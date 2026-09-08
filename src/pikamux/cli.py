from __future__ import annotations

import argparse
import json
import os
import shlex
import shutil
import sqlite3
import sys
import termios
import time
import tty
from dataclasses import asdict
from pathlib import Path

from . import __version__
from .client_bridge import (
    BRIDGE_PROTOCOL,
    BRIDGE_VERSION,
    MAX_BRIDGE_MESSAGE_BYTES,
    ClientBridgeError,
    validate_pair_request,
)
from .consult import (
    Consultation,
    ConsultationError,
    consultation_for,
    consultation_policy,
)
from .core import (
    PENDING_LAUNCH_GRACE_SECONDS,
    OutsideLiveConflict,
    Pika,
    PikaError,
    SharedLeaseConflict,
)
from .doctor import repair_stale_state, run_doctor
from .executables import (
    PROVIDER_NAMES,
    executable_available,
    executable_version,
    provider_compatibility_error,
    provider_version_supported,
    setup_executables,
    setup_runtime_path,
)
from .expert_schedule import LAUNCHD_NAME, SERVICE_NAME, TIMER_NAME, activate_timer
from .processes import can_signal_exact_process
from .explain import explain_session
from .fleet import (
    REMOTE_INSTALL_ARGV,
    bundled_release,
    FleetError,
    handle_fleet_stdio,
    machine_alias,
    suggest_alias,
    suggest_local_machine_alias,
)
from .hooks import handle_hook, handle_process_exit, hook_stdout
from .models import Candidate, FleetSession, NodeCandidate, PendingLaunch, Session, Status
from .monitor import run_monitor
from .paths import config_path, database_path, codex_home
from .skill_package import install_skill, skill_text
from .processes import process_tty
from .setup_hooks import (
    apply_changes,
    hook_spec_fingerprint,
    hooks_installed,
    proposed_changes,
)
from .store import load_config, write_config
from .tmux import TmuxError
from .ui import (
    choose_fleet_candidates,
    choose_node_candidates,
    print_experts,
    print_fleet_sessions,
    print_machines,
    print_node_candidates,
    print_node_discovery_report,
    print_sessions,
    provider_identity_label,
    provider_label,
    terminal_text,
)

PUBLIC_COMMANDS = {
    "activity",
    "ask",
    "expert",
    "experts",
    "explain",
    "open",
    "list",
    "next",
    "peek",
    "wait",
    "new",
    "adopt",
    "untrack",
    "setup",
    "doctor",
    "machines",
    "machine",
    "sync",
    "skill",
    "update",
}
INTERNAL_COMMANDS = {
    "_enter",
    "recover-closed",
    "hook",
    "_process-exit",
    "_peek-popup",
    "_fleet",
    "_fleet-open",
    "_fleet-ask",
    "_client-pair",
}
ADVANCED_COMMANDS = {"open", "new", "adopt"}


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="pika",
        description=(
            "Bring back the exact Codex, Claude, or OpenCode conversation, even after "
            "the terminal is gone."
        ),
        epilog="Use `pika NAME` to find, protect, attach, resume, or create safely.",
    )
    parser.add_argument("--version", action="version", version=f"pikamux {__version__}")
    sub = parser.add_subparsers(
        dest="command",
        metavar=(
            "{activity,ask,expert,experts,explain,skill,list,next,peek,wait,untrack,setup,doctor,"
            "machines,sync,update}"
        ),
    )

    open_parser = sub.add_parser("open", help=argparse.SUPPRESS)
    update_parser = sub.add_parser("update", help="safely update an installer-managed Pika")
    update_parser.add_argument("--check", action="store_true", help="check without installing")
    update_parser.add_argument("--bundle", type=Path, help="use a locally supplied release bundle")
    open_parser.add_argument("name")

    # v0.4.3 printed this exact command in recovery receipts. Keep a hidden,
    # safe compatibility route while the daily product remains just `pika NAME`.
    legacy_recover = sub.add_parser("recover-closed", help=argparse.SUPPRESS)
    legacy_recover.add_argument("name")

    ask_parser = sub.add_parser(
        "ask",
        help="ask an ephemeral multi-turn side question without changing the parent",
    )
    ask_parser.add_argument("name")
    ask_parser.add_argument("question", nargs="*")
    ask_parser.add_argument(
        "--jsonl",
        action="store_true",
        help="keep one consultation open using JSON-lines requests and responses",
    )
    ask_parser.add_argument(
        "--fast",
        action="store_true",
        help=(
            "use the benchmarked Luna-medium Codex profile "
            "(not Codex Fast service tier)"
        ),
    )

    expert_parser = sub.add_parser(
        "expert", help="inspect, refresh, publish, or clear thread profiles"
    )
    expert_sub = expert_parser.add_subparsers(dest="expert_command", required=True)
    publish_parser = expert_sub.add_parser(
        "publish", help="publish this exact conversation's thread profile"
    )
    publish_parser.add_argument(
        "--scope",
        "--summary",
        dest="summary",
        required=True,
        help="durable mandate across the thread, not a recent-work recap",
    )
    publish_parser.add_argument(
        "--now",
        dest="current_state",
        required=True,
        help="current objective, stage, blocker, decision, or next step",
    )
    publish_parser.add_argument(
        "--topic", action="append", required=True, help="repeat or comma-separate"
    )
    publish_parser.add_argument("--artifact", action="append", default=[])
    update_parser = expert_sub.add_parser(
        "update", help="publish a small current-work change from this exact conversation"
    )
    update_parser.add_argument("--now", "--current-state", dest="current_state", required=True)
    update_parser.add_argument("--json", action="store_true")
    expert_sub.add_parser("clear", help="remove this conversation's thread profile")
    refresh_parser = expert_sub.add_parser(
        "refresh", help="interview exact conversations and refresh thread profiles"
    )
    refresh_parser.add_argument("name", nargs="?")
    refresh_mode = refresh_parser.add_mutually_exclusive_group()
    refresh_mode.add_argument(
        "--all",
        action="store_true",
        help="refresh all missing or changed thread profiles now",
    )
    refresh_mode.add_argument(
        "--due",
        action="store_true",
        help="refresh only when weekly quota is near reset with >10%% left",
    )
    refresh_parser.add_argument("--provider", choices=PROVIDER_NAMES)
    refresh_parser.add_argument("--json", action="store_true")
    status_parser = expert_sub.add_parser(
        "status", help="show which thread profiles are current, stale, or missing"
    )
    status_parser.add_argument("--json", action="store_true")

    experts_parser = sub.add_parser(
        "experts",
        help="find exact conversation threads by topic, project, or artifact",
    )
    experts_parser.add_argument("query", nargs="*")
    experts_parser.add_argument("--json", action="store_true")

    explain_parser = sub.add_parser("explain", help="show the evidence behind a conversation's state")
    explain_parser.add_argument("name")
    explain_parser.add_argument("--json", action="store_true")

    skill_parser = sub.add_parser("skill", help="show or install the bundled agent-convo skill")
    skill_sub = skill_parser.add_subparsers(dest="skill_command", required=True)
    skill_sub.add_parser("show", help="print the bundled skill")
    skill_install = skill_sub.add_parser("install", help="install with a backup of any previous SKILL.md")
    skill_install.add_argument("path", nargs="?", help="destination skill directory (default: Codex agent-convo directory)")
    skill_install.add_argument("--json", action="store_true")

    list_parser = sub.add_parser("list", help="list all tracked conversations")
    list_parser.add_argument("--json", action="store_true")
    list_parser.add_argument(
        "--no-usage", action="store_true", help="skip token and cost calculation"
    )

    activity_parser = sub.add_parser(
        "activity", help="show the transcript-free attention history"
    )
    activity_parser.add_argument("--limit", type=int, default=20)
    activity_parser.add_argument("--json", action="store_true")
    list_parser.add_argument(
        "--all-machines",
        action="store_true",
        help="include cache-only snapshots from adopted Pika machines",
    )

    sub.add_parser("next", help="open the oldest conversation needing attention")

    peek_parser = sub.add_parser(
        "peek", help="view recent pane output without attaching"
    )
    peek_parser.add_argument("name")
    peek_parser.add_argument("--lines", type=int)
    peek_parser.add_argument(
        "--ack",
        action="store_true",
        help="mark an unread READY result as seen, including in scripts",
    )

    wait_parser = sub.add_parser(
        "wait", help="wait until a conversation needs attention"
    )
    wait_parser.add_argument("name")
    wait_parser.add_argument(
        "--for",
        dest="wait_for",
        choices=("any", "needs-you", "ready", "error"),
        default="any",
    )
    wait_parser.add_argument("--timeout", type=float)
    wait_parser.add_argument("--json", action="store_true")

    new_parser = sub.add_parser("new", help=argparse.SUPPRESS)
    new_parser.add_argument("name")
    new_parser.add_argument("--agent", choices=PROVIDER_NAMES)
    new_parser.add_argument("--cwd")

    adopt_parser = sub.add_parser("adopt", help=argparse.SUPPRESS)
    adopt_parser.add_argument("target", nargs="?", metavar="TMUX_TARGET_OR_NAME")
    adopt_parser.add_argument("--name")

    untrack_parser = sub.add_parser(
        "untrack",
        help="stop watching a conversation without stopping or archiving it",
    )
    untrack_parser.add_argument("name")

    setup_parser = sub.add_parser("setup", help="preview and install lifecycle hooks")
    setup_parser.add_argument("--default-provider", choices=PROVIDER_NAMES)
    setup_parser.add_argument("--codex-executable")
    setup_parser.add_argument("--claude-executable")
    setup_parser.add_argument("--opencode-executable")
    setup_parser.add_argument(
        "--machine-alias",
        help="human name for this Pika node (for example devbox)",
    )
    setup_parser.add_argument(
        "--yes", action="store_true", help="apply the displayed changes"
    )
    setup_parser.add_argument("--dry-run", action="store_true")
    setup_parser.add_argument(
        "--skip-walkthrough", action="store_true",
        help="skip the first-conversation recovery walkthrough",
    )
    setup_parser.add_argument("--no-import", action="store_true")
    setup_parser.add_argument("--install-bundle", type=Path, help="verified local release bundle for approved remote installs")
    setup_parser.add_argument("--import-all", action="store_true")
    setup_parser.add_argument(
        "--browse-all", action="store_true",
        help="include generated or unconfirmed titles in setup choices; does not adopt them automatically",
    )
    setup_parser.add_argument(
        "--no-machines", action="store_true", help="skip passive machine discovery"
    )
    setup_parser.add_argument(
        "--machine",
        action="append",
        default=[],
        metavar="SSH_TARGET",
        help="explicitly handshake with this SSH target; repeatable",
    )
    setup_parser.add_argument(
        "--remote-import-all",
        action="store_true",
        help="adopt every eligible conversation on explicitly selected machines",
    )

    machines_parser = sub.add_parser(
        "machines", aliases=["machine"], help="discover and manage trusted remote Pika nodes"
    )
    machine_sub = machines_parser.add_subparsers(dest="machines_command")
    machine_list = machine_sub.add_parser("list", help="list trusted machines")
    machine_list.add_argument("--json", action="store_true")
    machine_discover = machine_sub.add_parser(
        "discover", help="passively list SSH and Tailscale candidates"
    )
    machine_discover.add_argument("--json", action="store_true")
    machine_add = machine_sub.add_parser("add", help="verify and trust one machine")
    machine_add.add_argument("ssh_target")
    machine_add.add_argument("--alias")
    machine_remove = machine_sub.add_parser(
        "remove", help="forget local trust and cache; remote Pika is untouched"
    )
    machine_remove.add_argument("machine")
    machine_upgrade = machine_sub.add_parser(
        "upgrade", help="install this coordinator's pinned Pika release remotely"
    )
    machine_upgrade.add_argument("machine")
    machine_upgrade.add_argument("--bundle", type=Path, help="transfer a verified local release bundle over SSH")
    machine_upgrade.add_argument(
        "--yes", action="store_true", help="run the displayed pinned install command"
    )
    machine_ignore = machine_sub.add_parser(
        "ignore", help="dismiss a passive discovery candidate"
    )
    machine_ignore.add_argument("ssh_target")

    sync_parser = sub.add_parser("sync", help="refresh one trusted machine now")
    sync_parser.add_argument("machine")

    doctor_parser = sub.add_parser("doctor", help="verify Pika recoverability")
    doctor_parser.add_argument("--json", action="store_true")
    doctor_parser.add_argument(
        "--verbose", action="store_true", help="show every diagnostic check"
    )
    doctor_parser.add_argument(
        "--repair-stale",
        action="store_true",
        help="remove confirmed stale launch locks older than five minutes",
    )

    hook_parser = sub.add_parser("hook", help=argparse.SUPPRESS)
    hook_parser.add_argument("--provider", required=True, choices=PROVIDER_NAMES)

    exit_parser = sub.add_parser("_process-exit", help=argparse.SUPPRESS)
    exit_parser.add_argument("--provider", required=True, choices=PROVIDER_NAMES)
    exit_parser.add_argument("--session-id")
    exit_parser.add_argument("--launch-token")
    exit_parser.add_argument("--owner-token")
    exit_parser.add_argument("--code", required=True, type=int)

    popup_parser = sub.add_parser("_peek-popup", help=argparse.SUPPRESS)
    popup_parser.add_argument("--target", required=True)
    popup_parser.add_argument("--lines", required=True, type=int)
    popup_parser.add_argument("--name", required=True)
    popup_parser.add_argument("--provider", required=True, choices=PROVIDER_NAMES)
    popup_parser.add_argument("--session-id", required=True)

    fleet_parser = sub.add_parser("_fleet", help=argparse.SUPPRESS)
    fleet_parser.add_argument("--stdio", action="store_true", required=True)

    fleet_open = sub.add_parser("_fleet-open", help=argparse.SUPPRESS)
    fleet_open.add_argument("--expected-node-id", required=True)
    fleet_open.add_argument("--provider", required=True, choices=PROVIDER_NAMES)
    fleet_open.add_argument("--session-id", required=True)

    fleet_ask = sub.add_parser("_fleet-ask", help=argparse.SUPPRESS)
    fleet_ask.add_argument("--expected-node-id", required=True)
    fleet_ask.add_argument("--provider", required=True, choices=PROVIDER_NAMES)
    fleet_ask.add_argument("--session-id", required=True)
    fleet_ask.add_argument("--fast", action="store_true")
    client_pair = sub.add_parser("_client-pair", help=argparse.SUPPRESS)
    client_pair.add_argument("--stdio", action="store_true", required=True)
    enter_parser = sub.add_parser("_enter", help=argparse.SUPPRESS)
    enter_parser.add_argument("name")
    # argparse otherwise renders hidden implementation commands as
    # ``==SUPPRESS==`` entries in the public command list.
    sub._choices_actions = [
        action
        for action in sub._choices_actions
        if action.dest not in INTERNAL_COMMANDS | ADVANCED_COMMANDS
    ]
    return parser


def _normalize_argv(argv: list[str]) -> list[str]:
    if not argv:
        return argv
    first = argv[0]
    if first == "ask" and "--" not in argv:
        options = [item for item in argv[1:] if item in {"--fast", "--jsonl"}]
        if options:
            positional = [
                item for item in argv[1:] if item not in {"--fast", "--jsonl"}
            ]
            return [first, *positional, *options]
    if first in PUBLIC_COMMANDS | INTERNAL_COMMANDS or first in {
        "-h",
        "--help",
        "--version",
    }:
        return argv
    return ["_enter", *argv]


def _confirm_shared_lease(pika: Pika, conflict: SharedLeaseConflict) -> int:
    session = conflict.session
    reopen_command = shlex.join(["pika", session.display_name])
    if not sys.stdin.isatty():
        raise PikaError(
            f"{conflict} Interactive confirmation is required; run exactly: "
            f"`{reopen_command}` in a terminal."
        ) from conflict
    print(
        f"pika: {terminal_text(session.display_name)} may still be open in another "
            f"{provider_label(session.provider)} client. Exit it everywhere before confirming; "
            "Pika will then resume the exact "
            f"{provider_identity_label(session.provider)} in its protected home.",
        file=sys.stderr,
    )
    answer = input(
        f"Have you exited {terminal_text(session.display_name)!r} in every "
        f"{provider_label(session.provider)} client? [y/N] "
    ).strip().casefold()
    if answer not in {"y", "yes"}:
        print(
            f"pika: No state changed. Exit it everywhere, then run exactly: "
            f"`{reopen_command}`.",
            file=sys.stderr,
        )
        return 1
    return pika.recover_after_closed_confirmation(session)


def _confirm_outside_live(pika: Pika, conflict: OutsideLiveConflict) -> int:
    """Offer one explicit, generation-pinned takeover without weakening identity."""
    session = conflict.session
    reopen_command = shlex.join(["pika", session.display_name])
    pid = conflict.process_identities[0][0]
    tty_name = process_tty(pid)
    location = f" · terminal {tty_name}" if tty_name else ""
    terminal_location = f" on terminal {tty_name}" if tty_name else ""
    if not can_signal_exact_process():
        raise PikaError(
            f"{provider_label(session.provider)} is already running{terminal_location} "
            f"(PID {pid}). This platform cannot safely stop a pinned PID generation. "
            "No process was stopped. In that client, run `/exit`, wait for the "
            f"shell prompt, then run exactly: `{reopen_command}`."
        ) from conflict
    if not sys.stdin.isatty():
        raise PikaError(
            f"{conflict} Interactive choice is required. In the original "
            f"{provider_label(session.provider)} client{terminal_location}, run `/exit`, wait for "
            f"the shell prompt, then run exactly: `{reopen_command}`."
        ) from conflict
    print(
        f"pika: exact {provider_label(session.provider)} conversation "
        f"{terminal_text(session.display_name)!r} is live outside Pika "
        f"(PID {pid}{location}).",
        file=sys.stderr,
    )
    print(
        "  1. Keep it there (recommended) — no process or state changes",
        file=sys.stderr,
    )
    print(
        "  2. Clean and attach here — request a graceful stop, then resume the "
        f"same exact {provider_identity_label(session.provider)} in Pika",
        file=sys.stderr,
    )
    print("  3. Cancel — no process or state changes", file=sys.stderr)
    answer = input("Choose 1-3 [1]: ").strip()
    if answer == "2":
        print(
            f"pika: stopping exact {provider_label(session.provider)} PID {pid}; this may "
            "interrupt its current turn. Pika will not force-kill it.",
            file=sys.stderr,
        )
        return pika.clean_and_attach(conflict)
    if answer in {"3", "q", "cancel"}:
        print("pika: Cancelled. No state changed.", file=sys.stderr)
        return 1
    print(
        f"pika: Keep using the existing {provider_label(session.provider)} client"
        f"{terminal_location}. To move it later, run `/exit`, wait for the shell prompt, "
        f"then run exactly: `{reopen_command}`.",
        file=sys.stderr,
    )
    return 1


def _open_with_shared_lease_confirmation(
    pika: Pika, session: Session | FleetSession
) -> int:
    try:
        return pika.open(session)
    except OutsideLiveConflict as exc:
        return _confirm_outside_live(pika, exc)
    except SharedLeaseConflict as exc:
        return _confirm_shared_lease(pika, exc)


def _select_named(
    pika: Pika,
    value: str,
    *,
    sessions: list[Session | FleetSession] | None = None,
) -> Session | FleetSession:
    if value == ".":
        return pika.current_repo(sessions)
    if value == "-":
        return pika.previous(sessions)
    if sessions is not None:
        matches = [
            item
            for item in sessions
            if item.session_id == value
            or (item.name and item.name.casefold() == value.casefold())
            or item.display_name.casefold() == value.casefold()
        ]
        if len(matches) == 1:
            return matches[0]
    if hasattr(type(pika), "resolve_target"):
        return pika.resolve_target(value)
    return pika.resolve(value)


def _bare(pika: Pika) -> int:
    if sys.stdin.isatty() and sys.stdout.isatty():
        return run_monitor(pika)
    # Preserve a useful, finite representation when bare `pika` is redirected.
    # Stable automation should continue to prefer `pika list --json`.
    local = pika.refresh(usage=True)
    sessions = (
        pika.monitor_sessions(local)
        if hasattr(type(pika), "monitor_sessions")
        else local
    )
    if len(sessions) == len(local):
        print_sessions(local)
        _print_actions(pika, local)
    else:
        print_fleet_sessions(sessions)
    return 0


def _peek(pika: Pika, name: str, lines: int | None, *, ack: bool = False) -> int:
    session = _select_named(pika, name)
    lines = lines or int(load_config().get("peek_lines") or 200)
    human_view = sys.stdin.isatty() and sys.stdout.isatty()
    pane = None
    if not isinstance(session, FleetSession):
        pane = pika.tmux.get_pane(session.tmux_pane or session.tmux_session or "")
        if pane is None:
            raise PikaError(
                f"{session.display_name} has no surviving tmux pane to peek. "
                f"Use `pika {session.display_name}` to resurrect it."
            )
    if pane is not None and os.environ.get("TMUX") and human_view:
        result = pika.tmux.popup(
            pane.pane_id,
            lines,
            session.display_name,
            session.provider,
            session.session_id,
        )
    else:
        capture = getattr(pika, "capture", None)
        print(
            capture(session, lines)
            if callable(capture)
            else pika.tmux.capture(pane.pane_id, lines)
        )
        result = 0
    if result == 0 and session.status == Status.READY.value and (human_view or ack):
        pika.acknowledge(session)
    return result


def _ask(
    pika: Pika,
    name: str,
    question_parts: list[str],
    *,
    jsonl: bool = False,
    fast: bool = False,
) -> int:
    session = (
        pika.resolve_expert_target(name)
        if hasattr(type(pika), "resolve_expert_target")
        else _select_named(pika, name)
    )
    return _ask_session(pika, session, question_parts, jsonl=jsonl, fast=fast)


def _ask_session(
    pika: Pika,
    session: Session | FleetSession,
    question_parts: list[str],
    *,
    jsonl: bool = False,
    fast: bool = False,
) -> int:
    from .consult_reporting import ConsultationRun

    if not isinstance(session, FleetSession) and not session.transcript_path:
        raise PikaError(
            f"{session.display_name} has no durable provider transcript to consult"
        )
    initial = " ".join(question_parts).strip()
    interactive = sys.stdin.isatty() and sys.stdout.isatty()
    if not initial and not interactive and not jsonl:
        initial = sys.stdin.read().strip()
    if not initial and not interactive and not jsonl:
        raise PikaError("Provide a question as arguments or on stdin")

    def progress(payload: dict[str, object]) -> None:
        if jsonl:
            print(json.dumps(payload, ensure_ascii=False, sort_keys=True), flush=True)

    try:
        consultation = ConsultationRun.open(
            lambda: (
                pika.consultation(session, fast=fast)
                if hasattr(type(pika), "consultation")
                else consultation_for(session, fast=fast)
            ),
            on_event=progress,
        )
        if jsonl:
            return _ask_jsonl(consultation, session, initial)
        with consultation:
            print(
                f"SIDE · {terminal_text(session.display_name)} · "
                f"{provider_label(session.provider)} · parent {session.provider_thread_id[:8]} · "
                f"EPHEMERAL · {consultation.policy.label}"
            )
            pending = initial
            while True:
                if not pending:
                    try:
                        pending = input("side> ").strip()
                    except EOFError:
                        break
                if not pending:
                    if interactive:
                        continue
                    break
                if pending in {"/close", "/exit", "/quit"}:
                    break
                if pending == "/help":
                    print("Ask a follow-up, or use /close to discard the side chat.")
                    pending = ""
                    continue
                answer = consultation.ask(pending)
                safe_answer = "".join(
                    character
                    if character.isprintable() or character in {"\n", "\t"}
                    else "�"
                    for character in answer
                )
                print(f"\n{safe_answer}\n")
                pending = ""
                if not interactive:
                    break
    except (Exception, KeyboardInterrupt) as exc:
        if jsonl:
            try:
                policy = consultation_policy(session, fast=fast)
                receipt = {
                    "consultation_mode": policy.mode,
                    "requested_model": policy.model,
                    "requested_effort": policy.effort,
                }
            except ConsultationError:
                receipt = {}
            print(
                json.dumps(
                    {
                        "type": "error", "message": str(exc) or "Consultation interrupted",
                        **receipt, **getattr(exc, "receipt", {}),
                    },
                    ensure_ascii=False,
                    sort_keys=True,
                ),
                flush=True,
            )
            return 1
        raise PikaError(str(exc)) from exc
    print(
        "SIDE CLOSED · discarded · parent transcript unchanged · "
        f"{consultation.policy.label}"
    )
    return 0


def _ask_jsonl(consultation: Consultation, session: Session, initial: str) -> int:
    from .consult_reporting import ConsultationRun

    def emit(payload: dict[str, object]) -> None:
        print(json.dumps(payload, ensure_ascii=False, sort_keys=True), flush=True)

    if not isinstance(consultation, ConsultationRun):
        native = consultation
        consultation = ConsultationRun.open(lambda: native, on_event=emit)

    opened = {
        "type": "opened",
        "ephemeral": True,
        "provider": session.provider,
        "workstream_id": session.session_id,
        "parent_id": session.provider_thread_id,
        "name": session.display_name,
        **consultation.receipt(),
        **consultation.policy.receipt(),
    }

    def handle(question: str) -> None:
        answer = consultation.ask(question)
        emit(
            {
                "type": "answer",
                "text": answer,
                **consultation.receipt(),
                **consultation.policy.receipt(),
            }
        )

    result = 0
    try:
        emit(opened)
        if initial:
            handle(initial)
        for line in sys.stdin:
            if not line.strip():
                continue
            try:
                request = json.loads(line)
                if not isinstance(request, dict):
                    raise ValueError("Each JSONL request must be an object")
                if request.get("close") is True:
                    break
                question = request.get("question")
                if not isinstance(question, str) or not question.strip():
                    raise ValueError('Expected a non-empty "question" or {"close":true}')
            except ValueError as exc:
                error = ConsultationError(f"Invalid JSONL request: {exc}")
                error.receipt = {
                    **consultation.receipt(), "stage": "input",
                    "delivery": "not_sent", "retry_safe": False,
                }
                raise error from exc
            handle(question)
    except (Exception, KeyboardInterrupt) as exc:
        result = 1
        emit({
            "type": "error", "message": str(exc) or "Consultation interrupted",
            **consultation.policy.receipt(),
            **getattr(exc, "receipt", consultation.receipt()),
        })
    finally:
        try:
            consultation.close()
        except (Exception, KeyboardInterrupt) as exc:
            result = 1
            emit({
                "type": "error", "message": str(exc) or "Cleanup interrupted",
                **consultation.policy.receipt(),
                **getattr(exc, "receipt", consultation.receipt()),
            })
        emit({
            "type": "closed",
            "discarded": consultation.cleanup == "complete",
            "parent_transcript_unchanged": True,
            **consultation.receipt(),
            **consultation.policy.receipt(),
        })
    return result


def _expert(pika: Pika, args: argparse.Namespace) -> int:
    if args.expert_command == "update":
        profile = pika.publish_current_work(args.current_state)
        payload = {
            "provider": profile.provider,
            "session_id": profile.session_id,
            "current_state": profile.current_state,
            "current_state_updated_at": profile.current_state_updated_at,
            "scope_updated_at": profile.scope_updated_at,
        }
        if args.json:
            print(json.dumps(payload, sort_keys=True))
        else:
            print(
                f"CURRENT WORK SAVED · {provider_label(profile.provider)} · "
                f"{profile.session_id[:8]} · unchanged text retains its publication time"
            )
        return 0
    if args.expert_command == "publish":
        topics = [
            topic.strip()
            for value in args.topic
            for topic in value.split(",")
            if topic.strip()
        ]
        profile = pika.publish_expert(
            summary=args.summary,
            current_state=args.current_state,
            topics=topics,
            artifacts=args.artifact,
        )
        session = pika.store.get_session(*profile.key)
        name = session.display_name if session else profile.session_id[:8]
        print(
            f"THREAD PROFILE PUBLISHED · {terminal_text(name)} · "
            f"{provider_label(profile.provider)} · {profile.session_id[:8]} · "
            f"{len(profile.topics)} topics"
        )
        return 0
    if args.expert_command == "clear":
        session = pika.clear_current_expert()
        print(
            f"THREAD PROFILE CLEARED · {terminal_text(session.display_name)} · "
            f"{provider_label(session.provider)} · {session.session_id[:8]}"
        )
        return 0
    if args.expert_command == "status":
        states = pika.expert_card_states()
        if args.json:
            print(
                json.dumps(
                    [item.to_dict() for item in states], indent=2, sort_keys=True
                )
            )
        elif not states:
            print("No resumable Pika conversations are eligible for thread profiles.")
        else:
            for item in states:
                print(
                    f"{item.status:<8} · {provider_label(item.session.provider):<8} · "
                    f"{terminal_text(item.session.display_name)} · "
                    f"{item.session.session_id[:8]} · {terminal_text(item.detail)} · "
                    f"{terminal_text(item.to_dict()['availability'])}"
                )
        return 0
    if args.expert_command == "refresh":
        if args.due:
            results = pika.refresh_due_experts(provider_name=args.provider)
        elif args.all:
            sessions = [
                item
                for item in pika.refresh(usage=False)
                if args.provider is None or item.provider == args.provider
            ]
            if args.json:
                results = pika.bootstrap_experts(sessions)
            else:
                pending = [
                    item
                    for item in pika.expert_card_states(sessions)
                    if item.status in {"MISSING", "STALE"}
                ]
                results = []
                if pending:
                    print(
                        f"Building {len(pending)} exact thread profile(s) "
                        "from exact ephemeral interviews."
                    )
                for index, state in enumerate(pending, 1):
                    print(
                        f"  [{index}/{len(pending)}] "
                        f"{terminal_text(state.session.display_name)}…",
                        flush=True,
                    )
                    result = pika.bootstrap_experts([state.session])
                    results.extend(result)
                    _print_expert_refresh_results(result)
                return 1 if any(item.status == "FAILED" for item in results) else 0
        else:
            if args.provider:
                raise PikaError("--provider is only valid with --all or --due")
            session = (
                pika.resolve_expert_target(args.name)
                if args.name
                else pika.current_exact_session()
            )
            if isinstance(session, FleetSession):
                node = pika.store.get_fleet_node(session.node_id)
                target = node.ssh_target if node else session.node_name
                raise PikaError(
                    "Thread interviews run on the authoritative machine. "
                    f"Run `ssh {shlex.quote(target)} pika expert refresh "
                    f"{shlex.quote(session.session_id)}`, then `pika sync "
                    f"{shlex.quote(session.node_name)}`."
                )
            results = pika.bootstrap_experts([session])
        _print_expert_refresh_results(results, as_json=args.json)
        return 1 if any(item.status == "FAILED" for item in results) else 0
    raise PikaError(f"Unknown expert command: {args.expert_command}")


def _print_expert_refresh_results(results, *, as_json: bool = False) -> None:
    if as_json:
        print(
            json.dumps([item.to_dict() for item in results], indent=2, sort_keys=True)
        )
        return
    if not results:
        print("Thread profiles already current.")
        return
    for item in results:
        subject = terminal_text(item.name or provider_label(item.provider))
        quota = (
            f" · {item.remaining_percent:.0f}% left"
            if item.remaining_percent is not None
            else ""
        )
        policy = (
            f" · {item.model} · {item.effort}"
            if item.model and item.effort
            else " · provider native"
            if item.consultation_mode == "provider-native"
            else ""
        )
        print(
            f"{item.status} · {subject} · {provider_label(item.provider)}"
            f"{quota}{policy} · {terminal_text(item.detail)}"
        )


def _experts(pika: Pika, query_parts: list[str], *, as_json: bool) -> int:
    query = " ".join(query_parts).strip()
    print_experts(pika.expert_matches(query), query=query, as_json=as_json)
    return 0


def _explain(pika: Pika, name: str, *, as_json: bool) -> int:
    # Remote explanations must remain available even when SSH is unavailable.
    remote = pika.fleet.resolve(name, fresh=False) if "@" in name else None
    if remote is not None:
        session = remote
        facts = ()
    else:
        session = _select_named(pika, name, sessions=pika.refresh(usage=False))
        facts = pika.store.status_observations(*session.key)
    report = explain_session(session, facts)
    if as_json:
        print(json.dumps(report, ensure_ascii=False, sort_keys=True))
    else:
        print(f"{terminal_text(session.display_name)} · {report['state']}")
        print(terminal_text(report["summary"]))
        freshness = report["freshness"]
        if freshness["age_seconds"] is not None:
            print(f"Evidence age: {int(freshness['age_seconds'])}s · {freshness['basis']}")
        for fact in report["evidence"]:
            marker = "WINNER" if fact["winner"] else "observed"
            print(terminal_text(f"  {marker} · {fact['kind']} · {fact['status']} · {fact['source']} · {int(fact['age_seconds'])}s ago"))
        if report["next_action"]:
            print("Next: " + terminal_text(report["next_action"]))
    return 0


def _activity(pika: Pika, *, limit: int, as_json: bool) -> int:
    # Reconciliation may add a newly-derived actionable transition before the
    # immutable ledger is read. No transcript content enters this surface.
    pika.refresh(usage=False)
    events = pika.store.list_activity_events(limit=limit)
    if as_json:
        print(json.dumps([item.to_dict() for item in events], indent=2, sort_keys=True))
        return 0
    if not events:
        print("No Pika attention events yet.")
        return 0
    print("PIKA ACTIVITY · transcript-free · newest first")
    for item in events:
        timestamp = time.strftime("%Y-%m-%d %H:%M:%S", time.localtime(item.event_at))
        reason = item.error or item.attention_reason or item.status.casefold()
        print(
            f"{timestamp} · {item.status:<10} · "
            f"{provider_label(item.provider):<8} · "
            f"{terminal_text(item.display_name)} · {terminal_text(reason)}"
        )
    return 0


def _print_actions(pika: Pika, sessions: list[Session]) -> None:
    if not sessions:
        return
    hints: list[str] = []
    next_session = pika.next_attention(sessions)
    if next_session:
        reason = next_session.attention_reason or next_session.status.casefold()
        hints.append(
            f"Next: {terminal_text(next_session.display_name)} — "
            f"{terminal_text(reason)} · "
            "continue with `pika next`"
        )
    try:
        cwd = Path.cwd().resolve()
        root = pika._git_root(cwd)
        here = [
            item
            for item in sessions
            if item.cwd
            and (
                Path(item.cwd).resolve() == cwd
                or pika._git_root(Path(item.cwd).resolve()) == root
            )
        ]
    except OSError:
        here = []
    if here:
        noun = "conversation" if len(here) == 1 else "conversations"
        hints.append(f"Here: {len(here)} {noun} · open with `pika .`")
    previous = pika.store.previous_attached()
    previous_session = pika.store.get_session(*previous) if previous else None
    if previous_session:
        command = shlex.join(["pika", "open", previous_session.session_id])
        hints.append(
            f"Previous: {terminal_text(previous_session.display_name)} · "
            f"return with `{command}` or `pika -`"
        )
    if hints:
        print()
        for hint in hints[:2]:
            print(hint)


def _wait(pika: Pika, args: argparse.Namespace) -> int:
    initial = _select_named(pika, args.name)
    started = time.monotonic()
    next_reconcile = started + 5
    while True:
        now = time.monotonic()
        if now >= next_reconcile:
            sessions = pika.refresh(usage=False)
            session = next((item for item in sessions if item.key == initial.key), None)
            next_reconcile = now + 5
        else:
            session = pika.store.get_session(*initial.key)
        if session is None:
            raise PikaError("The tracked conversation disappeared while waiting")
        matches = {
            "any": session.needs_attention or (session.status == Status.READY.value and session.unread),
            "needs-you": session.status == Status.NEEDS_YOU.value,
            "ready": session.status == Status.READY.value and session.unread,
            "error": session.status in {Status.ERROR.value, Status.OPEN_TWICE.value}
            and session.unread,
        }[args.wait_for]
        if matches:
            if args.json:
                print(json.dumps(session.to_dict(), indent=2, sort_keys=True))
            else:
                reason = (
                    f" — {terminal_text(session.attention_reason)}"
                    if session.attention_reason
                    else ""
                )
                print(
                    f"{terminal_text(session.display_name)}: "
                    f"{terminal_text(session.status)}{reason}"
                )
            return 0
        if args.timeout is not None and time.monotonic() - started >= args.timeout:
            if args.json:
                print(
                    json.dumps(
                        {"timed_out": True, "session": session.to_dict()},
                        indent=2,
                        sort_keys=True,
                    )
                )
            else:
                print(
                    f"Timed out waiting for {terminal_text(session.display_name)}",
                    file=sys.stderr,
                )
            return 124
        time.sleep(0.5)


def _machines(pika: Pika, args: argparse.Namespace) -> int:
    command = args.machines_command or "list"
    if command == "list":
        print_machines(
            pika.fleet.nodes(),
            local_name=machine_alias(
                str(load_config().get("machine_alias") or suggest_local_machine_alias())
            ),
            local_node_id=pika.store.get_meta("fleet:node_id"),
            as_json=getattr(args, "json", False),
        )
        return 0
    if command == "discover":
        report = pika.fleet.discover_report()
        candidates = list(report.candidates)
        if args.json:
            print(
                json.dumps(
                    [asdict(item) for item in candidates], indent=2, sort_keys=True
                )
            )
        else:
            print_node_discovery_report(report)
            print("\nPassive discovery made no SSH connections and changed no machine.")
        return 0
    if command == "add":
        candidate = NodeCandidate(
            alias=args.alias or suggest_alias(args.ssh_target),
            ssh_target=args.ssh_target,
            sources=("explicit",),
        )
        try:
            node = pika.fleet.add(candidate, alias=args.alias)
        except FleetError as exc:
            if exc.kind == "missing":
                preview = shlex.join(REMOTE_INSTALL_ARGV)
                raise PikaError(
                    f"Pika is missing on {candidate.ssh_target}. Nothing was installed. "
                    f"Run this there, then retry: `{preview}`"
                ) from exc
            raise PikaError(str(exc)) from exc
        print(
            f"ADDED · {node.alias} · node {node.node_id[:8]} · "
            "identity and protocol verified"
        )
        return 0
    if command == "remove":
        node = pika.store.get_fleet_node(args.machine)
        if node is None:
            raise PikaError(f"Unknown Pika machine {args.machine!r}")
        pika.store.delete_fleet_node(node.node_id)
        print(
            f"REMOVED · {node.alias} · local trust and cache deleted · "
            "remote Pika untouched"
        )
        return 0
    if command == "upgrade":
        node = pika.store.get_fleet_node(args.machine)
        if node is None:
            raise PikaError(f"Unknown Pika machine {args.machine!r}")
        bundle = getattr(args, 'bundle', None) or bundled_release()
        preview = (
            f"Transfer verified release from {bundle}; install user-locally without running setup"
            if bundle else shlex.join(REMOTE_INSTALL_ARGV)
        )
        print(f"PINNED REMOTE UPGRADE · {node.alias}")
        print(f"Exact command: {preview}")
        approved = args.yes
        if not approved and sys.stdin.isatty():
            approved = input(
                f"Upgrade Pika on {node.alias}? [y/N] "
            ).strip().casefold() in {
                "y",
                "yes",
            }
        if not approved:
            print("Nothing installed.")
            return 0
        # Do not mutate whichever machine an SSH alias happens to resolve to.
        # New optional capabilities must not prevent identifying an older node.
        pika.fleet.verify_node_identity(node)
        code, detail = (
            pika.fleet.transport.install(node.ssh_target, bundle=bundle)
            if bundle else pika.fleet.transport.install(node.ssh_target)
        )
        if code:
            raise PikaError(f"Remote upgrade failed on {node.alias}: {detail}")
        pika.fleet.verify_node_identity(node)
        refreshed = pika.fleet.add(
            NodeCandidate(node.alias, node.ssh_target, node.sources), alias=node.alias
        )
        print(
            f"UPGRADED · {refreshed.alias} · pika {refreshed.package_version} · "
            f"node {refreshed.node_id[:8]} · identity reverified"
        )
        return 0
    if command == "ignore":
        pika.store.ignore_node_candidate(args.ssh_target)
        print(
            f"IGNORED · {terminal_text(args.ssh_target)} · no SSH connection was made"
        )
        return 0
    raise PikaError(f"Unknown machines command {command!r}")


def _sync_machine(pika: Pika, machine: str) -> int:
    node = pika.store.get_fleet_node(machine)
    if node is None:
        raise PikaError(f"Unknown Pika machine {machine!r}")
    try:
        sessions = pika.fleet.refresh_node(node.node_id)
    except FleetError as exc:
        labels = {
            "unreachable": "UNREACHABLE",
            "auth": "SSH TRUST OR AUTH FAILED",
            "incompatible": "INCOMPATIBLE",
            "quarantined": "NODE IDENTITY CHANGED",
        }
        raise PikaError(
            f"{labels.get(exc.kind, 'SYNC FAILED')} · {node.alias} · {exc}"
        ) from exc
    print(
        f"SYNCED · {node.alias} · {len(sessions)} conversations · node {node.node_id[:8]}"
    )
    return 0


def _setup_machine_candidates(
    pika: Pika, args: argparse.Namespace
) -> list[NodeCandidate]:
    if getattr(args, "no_machines", False) or args.dry_run:
        return []
    explicit = getattr(args, "machine", [])
    if explicit:
        discovered = {
            item.ssh_target.casefold(): item for item in pika.fleet.discover()
        }
        return [
            discovered.get(
                target.casefold(),
                NodeCandidate(
                    alias=suggest_alias(target),
                    ssh_target=target,
                    sources=("explicit",),
                ),
            )
            for target in explicit
        ]
    if args.yes or not sys.stdin.isatty():
        return []
    print("Pika machines · 1/2 FIND")
    print("Discovery is passive; only selected machines receive an SSH handshake.")
    report = pika.fleet.discover_report()
    return choose_node_candidates(list(report.candidates), report=report)


def _add_setup_machine(pika: Pika, candidate: NodeCandidate, *, bundle: Path | None = None):
    try:
        return pika.fleet.add(candidate)
    except FleetError as exc:
        if exc.kind != "missing":
            print(
                f"NOT ADDED · {candidate.alias} · {terminal_text(exc)}",
                file=sys.stderr,
            )
            return None
        bundle = bundle or bundled_release()
        preview = (f'Transfer verified release from {bundle}; install without running setup'
                   if bundle else shlex.join(REMOTE_INSTALL_ARGV))
        print(f"PIKA MISSING · {candidate.alias}")
        print(f"Exact remote install preview: {preview}")
        if not sys.stdin.isatty():
            print("Nothing installed · rerun interactively to approve this command.")
            return None
        answer = input(f"Install Pika on {candidate.alias}? [y/N] ").strip().casefold()
        if answer not in {"y", "yes"}:
            print("Nothing installed.")
            return None
        code, detail = (pika.fleet.transport.install(candidate.ssh_target, bundle=bundle)
                        if bundle else pika.fleet.transport.install(candidate.ssh_target))
        if code:
            print(f"INSTALL FAILED · {candidate.alias} · {detail}", file=sys.stderr)
            return None
        try:
            return pika.fleet.add(candidate)
        except FleetError as retry_error:
            print(
                f"INSTALLED BUT NOT ADDED · {candidate.alias} · {retry_error}",
                file=sys.stderr,
            )
            return None


def _setup_coverage(
    pika: Pika, *, untracked_keys: set[tuple[str, str]]
) -> list[Session]:
    """Lead setup's closing ledger with the protection outcome, not mechanics."""
    def noun(value: int, singular: str) -> str:
        return singular if value == 1 else singular + "s"

    sessions = list(pika.store.list_sessions())
    exact = [item for item in sessions if not item.session_id.startswith("unbound:")]
    providers = {item.provider for item in exact}
    needs_action = sum(
        item.needs_attention or item.status == Status.UNBOUND.value for item in sessions
    )
    node_reader = getattr(pika.store, "list_fleet_nodes", None)
    node_value = node_reader() if callable(node_reader) else []
    nodes = list(node_value) if isinstance(node_value, (list, tuple)) else []

    archived = 0
    provider_map = getattr(pika, "providers", {})
    if isinstance(provider_map, dict):
        for provider in provider_map.values():
            hidden = getattr(provider, "hidden_session_ids", None)
            if not callable(hidden):
                continue
            try:
                archived += len(hidden())
            except (OSError, RuntimeError, TypeError):
                continue

    print("\nYOUR PIKA COVERAGE")
    if exact:
        print(
            f"  {len(exact)} exact {noun(len(exact), 'conversation')} under Pika"
        )
    else:
        print("  No exact conversations protected yet · use `pika NAME`")
    print(
        f"  {len(providers)} {noun(len(providers), 'provider')} · "
        f"{1 + len(nodes)} {noun(1 + len(nodes), 'machine')}"
    )
    if needs_action:
        print(
            f"  {needs_action} {noun(needs_action, 'conversation')} "
            f"{'needs' if needs_action == 1 else 'need'} action now"
        )
    else:
        print("  No protection decisions pending")

    ignored: list[str] = []
    if archived:
        ignored.append(f"{archived} archived")
    if untracked_keys:
        ignored.append(f"{len(untracked_keys)} explicitly untracked")
    if isinstance(provider_map, dict) and "codex" in provider_map:
        ignored.append("automation workers filtered by provenance")
    if ignored:
        print("  Ignored on purpose · " + " · ".join(ignored))
    return sessions


def _offer_recovery_rehearsal(
    pika: Pika,
    sessions: list[Session],
    *,
    commissioned: bool,
    automatic: bool,
) -> None:
    """Prove continuity independently of the lifecycle commissioning ledger."""
    if (
        automatic
        or os.environ.get("TMUX")
        or not sys.stdin.isatty()
    ):
        return
    blocked = {
        Status.NEEDS_YOU.value,
        Status.READY.value,
        Status.ERROR.value,
        Status.OPEN_TWICE.value,
        Status.UNBOUND.value,
        Status.STARTING.value,
    }
    eligible = [
        item
        for item in sessions
        if item.status not in blocked
        and item.cwd
        and Path(item.cwd).is_dir()
        and item.provider in pika.providers
    ]
    if not eligible:
        return
    session = sorted(
        eligible,
        key=lambda item: (
            item.status != Status.PARKED.value,
            -item.last_activity_at,
        ),
    )[0]
    print("\nPROVE CONTINUITY NOW?")
    if not commissioned:
        print(
            "  This checks conversation recovery only. Missing integration "
            "observations still need proof in `pika doctor`."
        )
    print(
        f"  Pika can open {terminal_text(session.display_name)} without sending "
        "a prompt."
    )
    print(
        "  Detach with Ctrl-b d; setup will verify the same "
        f"{provider_identity_label(session.provider)} when you return."
    )
    answer = input("Run the recovery rehearsal? [y/N] ").strip().casefold()
    if answer not in {"y", "yes"}:
        print(
            "Try it later: `pika "
            f"{terminal_text(session.display_name)}` → detach → run the same command."
        )
        return

    expected_thread_id = session.provider_thread_id
    command = shlex.join(["pika", session.session_id])
    try:
        result = pika.open(session)
    except PikaError as exc:
        print(f"REHEARSAL PAUSED · {terminal_text(exc)}")
        print(f"Next: {command}")
        return
    if result:
        print(
            f"REHEARSAL INCOMPLETE · attach exited {result} · "
            f"retry with: {command}"
        )
        return

    try:
        refreshed = pika.refresh(usage=False)
        current = next((item for item in refreshed if item.key == session.key), None)
        panes = pika.tmux.list_panes()
        exact = bool(
            current
            and current.provider_thread_id == expected_thread_id
            and any(
                (pane.pika_provider, pane.pika_session_id) == session.key
                and pika.exact_pane_pid(current, pane)
                for pane in panes
            )
        )
    except (PikaError, TmuxError, OSError) as exc:
        print(f"REHEARSAL INCOMPLETE · verification failed: {terminal_text(exc)}")
        print(f"Next: {command}")
        return
    if exact:
        print(
            f"CONTINUITY PROVEN · {terminal_text(session.display_name)} · same "
            f"exact {provider_label(session.provider)} conversation · "
            f"id {expected_thread_id[:8]}"
        )
    else:
        print(
            "REHEARSAL INCOMPLETE · exact live identity is not yet proven · "
            f"next: {command}"
        )


def _first_conversation_recovery(
    pika: Pika, *, commissioned: bool,
) -> None:
    """Use the ordinary name workflow, then demonstrate an exact return."""
    if not sys.stdin.isatty() or os.environ.get("TMUX"):
        return
    print("\nYOUR FIRST CONVERSATION")
    print("Open an existing conversation by name, or give a new one a name.")
    print("Detach with Ctrl-b d to return here; the agent keeps running.")
    print("Pika will not send a prompt. Enter skips this walkthrough.")
    query = input("Conversation name: ").strip()
    if not query:
        print("Skipped · later, run `pika NAME`, detach, then run the same command.")
        return
    command = shlex.join(["pika", query])
    try:
        result = pika.enter(query)
    except (PikaError, FleetError) as exc:
        print(f"FIRST CONVERSATION PAUSED · {terminal_text(exc)}")
        print(f"When resolved, run exactly: {command}")
        return
    if result:
        print(f"FIRST CONVERSATION INCOMPLETE · exited {result} · retry: {command}")
        return
    # A name may collide across providers or be renamed during the visit.
    # Never certify whichever row happens to be returned first.
    matches = [
        item for item in pika.store.list_sessions()
        if item.session_id == query or item.provider_thread_id == query
        or item.display_name.casefold() == query.casefold()
    ]
    if len(matches) != 1:
        print(
            "Recovery proof pending · the visit did not identify one durable "
            "local conversation. No identity was guessed."
        )
        print(f"Next: {command}")
        return
    session = matches[0]
    print(f"\nYou can return any time with: {shlex.join(['pika', session.display_name])}")
    _offer_recovery_rehearsal(
        pika, [session], commissioned=commissioned, automatic=False,
    )


def _setup(pika: Pika, args: argparse.Namespace) -> int:
    print("Pika commissioning · exact recovery for Codex + Claude + OpenCode")
    print("Preview first · existing settings retained · backups before writes\n")
    if not args.dry_run:
        identity_loader = getattr(pika.store, "local_node_id", None)
        if callable(identity_loader):
            identity_loader()
    first_setup = not config_path().exists()
    explicit_machine_setup = bool(getattr(args, "machine", []))
    selected_machine_candidates: list[NodeCandidate] = []
    selected: list[Candidate] = []
    local_candidates: list[Candidate] = []
    original_tracked = [] if args.dry_run else pika.store.list_sessions()
    tracked_names = {session.key: session.display_name for session in original_tracked}
    browse_all = getattr(args, "browse_all", False)
    explicit_import = bool(args.import_all or getattr(args, "remote_import_all", False) or browse_all)
    routine_inventory = first_setup or explicit_import
    tracked_sessions = original_tracked
    untracked_keys = set() if args.dry_run else pika.store.untracked_session_keys()
    config = load_config()
    default_provider = args.default_provider
    first_interactive_setup = (
        default_provider is None
        and not config_path().exists()
        and not args.yes
        and not args.dry_run
        and sys.stdin.isatty()
    )
    if first_interactive_setup:
        answer = input(
            "Default agent: 1 for Codex, 2 for Claude, 3 for OpenCode [1]: "
        ).strip()
        default_provider = {"2": "claude", "3": "opencode"}.get(answer, "codex")
    default_provider = default_provider or config.get("default_provider") or "codex"
    print(f"Default for new conversations: {str(default_provider).title()}\n")
    configured_alias = config.get("machine_alias")
    alias = getattr(args, "machine_alias", None) or configured_alias
    if not alias:
        suggestion = suggest_local_machine_alias()
        if not args.yes and not args.dry_run and sys.stdin.isatty():
            answer = input(f"Name this Pika machine [{suggestion}]: ").strip()
            alias = answer or suggestion
        else:
            alias = suggestion
    alias = machine_alias(str(alias))
    print(f"This Pika machine: {alias}\n")
    selected_executables = setup_executables(
        config,
        overrides={
            "codex": getattr(args, "codex_executable", None),
            "claude": getattr(args, "claude_executable", None),
            "opencode": getattr(args, "opencode_executable", None),
        },
    )
    runtime_path = setup_runtime_path(config, selected_executables)
    print("Provider executables")
    provider_versions: dict[str, str | None] = {}
    compatibility_errors: dict[str, str | None] = {}
    for provider in PROVIDER_NAMES:
        executable = selected_executables.get(provider)
        version = executable_version(executable)
        provider_versions[provider] = version
        compatibility_errors[provider] = provider_compatibility_error(provider, version)
        state = (
            f"UNSUPPORTED · {compatibility_errors[provider]}"
            if version and compatibility_errors[provider]
            else version or "MISSING"
        )
        print(f"  {provider_label(provider):<8} {state} · {executable or 'not found'}")
    print()
    missing_default = selected_executables.get(str(default_provider))
    if not executable_available(missing_default):
        raise PikaError(
            f"Configured {default_provider} executable is unavailable: "
            f"{missing_default or 'not found'}. Supply --{default_provider}-executable."
        )
    default_compatibility = compatibility_errors.get(str(default_provider))
    if default_compatibility:
        raise PikaError(default_compatibility)
    changes = proposed_changes(
        str(default_provider),
        alias,
        provider_executables=selected_executables,
        provider_runtime_path=runtime_path,
    )
    changed = [item for item in changes if item.changed]
    if changed:
        print("Pika proposes these configuration changes:\n")
        for change in changed:
            print(change.diff())
    else:
        print("Pika hooks and configuration are already installed.")
    if args.dry_run:
        print("Dry run only · no files changed.")
        return 0
    apply = args.yes
    if changed and not apply:
        if not sys.stdin.isatty():
            print(
                "Re-run with `pika setup --yes` to apply these changes.",
                file=sys.stderr,
            )
            return 2
        answer = input("Apply these changes? [y/N] ").strip().lower()
        apply = answer in {"y", "yes"}
    if changed and not apply:
        print("No changes applied.")
        return 0
    backups = apply_changes(changes) if changed else []
    for backup in backups:
        print(f"Backup: {backup}")
    schedule_in_scope = any(
        change.path.name in {SERVICE_NAME, TIMER_NAME, LAUNCHD_NAME} for change in changes
    )
    if schedule_in_scope:
        active, detail = activate_timer()
        print(f"Expert refresh timer {'active' if active else 'inactive'} · {detail}")
    changed_names = {change.path.name for change in changed}
    hook_configuration_changed = bool(
        changed_names & {"hooks.json", "settings.json", "config.toml", "pika.js"}
    )
    if hook_configuration_changed:
        inactive = [
            provider
            for provider in PROVIDER_NAMES
            if not hooks_installed(provider)
        ]
        print("Pika hook definitions installed.")
        if "codex" in inactive:
            print(
                "Warning: Codex hooks remain disabled by effective configuration; "
                "`pika doctor` will stay unsafe."
            )
        else:
            print("In Codex, open `/hooks` once to review and trust them.")
        if "claude" in inactive:
            print(
                "Warning: Claude hooks are not active; run `pika doctor --verbose` "
                "before relying on attention state."
            )
        if "opencode" in inactive:
            print(
                "Warning: OpenCode plugin is not active; run `pika setup` again "
                "before relying on attention state."
            )
    print("\nCommissioning status")
    observed: dict[str, bool] = {}
    active_hooks: dict[str, bool] = {}
    available_executables = {
        provider: bool(
            executable_available(selected_executables.get(provider))
            and provider_version_supported(provider, provider_versions.get(provider))
        )
        for provider in PROVIDER_NAMES
    }
    pending_reader = getattr(pika.store, "list_pending", None)
    pending_value = pending_reader() if callable(pending_reader) else []
    pending_rows = (
        list(pending_value) if isinstance(pending_value, (list, tuple)) else []
    )
    overdue = {
        str(row["provider"]): row
        for row in pending_rows
        if time.time() - float(row["created_at"]) > PENDING_LAUNCH_GRACE_SECONDS
    }
    required_provider_names = {
        str(default_provider),
        *(session.provider for session in original_tracked),
    }
    commissioned_providers = [
        provider for provider in PROVIDER_NAMES if provider in required_provider_names
    ]
    for provider in PROVIDER_NAMES:
        active = hooks_installed(provider)
        active_hooks[provider] = active
        fingerprint = hook_spec_fingerprint(provider)
        observation_reader = getattr(pika.store, "get_hook_observation", None)
        observation = (
            observation_reader(provider) if callable(observation_reader) else None
        )
        if not isinstance(observation, dict):
            observation = None
        observed[provider] = bool(
            observation and observation.get("fingerprint") == fingerprint
        )
        if observed[provider] and observation:
            age = int(max(0, time.time() - float(observation["observed_at"])))
            proof = (
                f"{observation['event_name']} · id "
                f"{str(observation['session_id'])[:8]} · {age}s ago"
            )
        elif pika.store.get_meta(f"hook_seen:{provider}") == fingerprint:
            proof = "previous proof · time/session unavailable"
        else:
            proof = "not yet proven"
        launch = "DEGRADED" if provider in overdue else "healthy"
        scope = "required" if provider in commissioned_providers else "optional"
        print(
            f"  {provider_label(provider):<8} binary "
            f"{'✓' if available_executables[provider] else '✗'}  "
            f"hooks {'✓' if active else '✗'}  "
            f"observed {'✓' if observed[provider] else '○'} · {proof} · "
            f"launches {launch} · {scope}"
        )
    commissioned = all(
        available_executables[provider]
        and active_hooks[provider]
        and observed[provider]
        and provider not in overdue
        for provider in commissioned_providers
    )
    if commissioned:
        print(
            "\nPika commissioned · required integrations configured, observed, "
            "and healthy now."
        )
    elif (
        "codex" in commissioned_providers
        and active_hooks["codex"]
        and available_executables["codex"]
        and not observed["codex"]
        and all(
            active_hooks[provider]
            and available_executables[provider]
            and observed[provider]
            and provider not in overdue
            for provider in commissioned_providers
            if provider != "codex"
        )
        and "codex" not in overdue
    ):
        print(
            "\nOne required proof remains: Codex → `/hooks` → trust Pika → "
            "send one prompt → `pika doctor`."
        )
    else:
        incomplete = []
        for provider in commissioned_providers:
            if not available_executables[provider]:
                incomplete.append(f"{provider_label(provider)} executable/version proof")
            if not active_hooks[provider]:
                incomplete.append(f"{provider_label(provider)} activation")
            if not observed[provider]:
                incomplete.append(f"{provider_label(provider)} observation")
            if provider in overdue:
                row = overdue[provider]
                age = int(time.time() - float(row["created_at"]))
                incomplete.append(
                    f"{provider_label(provider)} launch {row['name']} identity pending {age}s"
                )
        print("\nPika not yet commissioned · pending: " + ", ".join(incomplete) + ".")
    if (
        first_setup and not args.yes and not getattr(args, "skip_walkthrough", False)
        and sys.stdin.isatty()
    ):
        _first_conversation_recovery(pika, commissioned=commissioned)

    # First establish the local return experience. Broader discovery is an
    # optional expansion, and explicit automation flags retain their behavior.
    if first_setup or explicit_machine_setup:
        if first_setup and not args.yes and sys.stdin.isatty():
            print("\nOPTIONAL · Add other machines or more conversations; Enter skips each choice.")
        selected_machine_candidates = _setup_machine_candidates(pika, args)
    if routine_inventory:
        tracked_sessions = pika.refresh(usage=False)
    if not args.no_import and (first_setup or explicit_import):
        discovered_local = [
            item for item in (
                pika.discover_import_candidates(include_unconfirmed=True)
                if browse_all else pika.discover_import_candidates()
            ) if browse_all or item.name or item.live
        ]
        tracked = {session.key for session in tracked_sessions}
        tracked.update(
            (session.provider, session.active_thread_id)
            for session in tracked_sessions if session.active_thread_id
        )
        suppressed = [
            item for item in discovered_local
            if (item.provider, item.session_id) in untracked_keys
        ]
        if suppressed:
            print(
                f"Pika is keeping {len(suppressed)} explicitly untracked "
                "conversation(s) out of setup choices:"
            )
            for item in suppressed[:3]:
                print(
                    f"  {provider_label(item.provider):<6} "
                    f"{terminal_text(item.display_name)} · {item.session_id[:8]}"
                )
            if len(suppressed) > 3:
                print(f"  … and {len(suppressed) - 3} more")
            print("Restore one explicitly with `pika <conversation-name>`.\n")
        local_candidates = [
            item for item in discovered_local
            if (item.provider, item.session_id) not in tracked
            and (item.provider, item.session_id) not in untracked_keys
        ]
        if args.import_all:
            selected = local_candidates
        elif args.yes or not sys.stdin.isatty():
            if local_candidates:
                print(
                    f"Pika found {len(local_candidates)} local import candidate(s); "
                    "rerun interactively to choose them or use `--import-all`.\n"
                )
    ready_nodes = []
    for candidate in selected_machine_candidates:
        install_bundle = getattr(args, 'install_bundle', None)
        node = (_add_setup_machine(pika, candidate, bundle=install_bundle)
                if install_bundle else _add_setup_machine(pika, candidate))
        if node is not None:
            ready_nodes.append(node)
            print(
                f"ADDED · {node.alias} · node {node.node_id[:8]} · "
                "identity and protocol verified"
            )

    remote_candidates: list[tuple[object, Candidate]] = []
    if not args.no_import:
        for node in ready_nodes:
            try:
                values = (
                    pika.fleet.remote_candidates(node, include_unconfirmed=True)
                    if browse_all else pika.fleet.remote_candidates(node)
                )
            except FleetError as exc:
                print(
                    f"REMOTE INVENTORY UNAVAILABLE · {node.alias} · {exc}",
                    file=sys.stderr,
                )
                continue
            remote_candidates.extend((node, item) for item in values)

    selected_remote: list[tuple[object, Candidate]] = []
    if (
        not args.no_import
        and not args.dry_run
        and sys.stdin.isatty()
        and not args.yes
        and not args.import_all
        and not getattr(args, "remote_import_all", False)
        and (first_setup or explicit_import or ready_nodes)
    ):
        def browse_titles():
            watched = {
                (item.provider, thread_id)
                for item in pika.store.list_sessions()
                for thread_id in (item.session_id, item.active_thread_id)
                if thread_id
            }
            ignored = pika.store.untracked_session_keys()
            values = [
                (None, item)
                for item in pika.discover_import_candidates(include_unconfirmed=True)
                if (item.provider, item.session_id) not in watched | ignored
            ]
            for node in ready_nodes:
                try:
                    values.extend(
                        (node, item) for item in pika.fleet.remote_candidates(
                            node, include_unconfirmed=True,
                        )
                    )
                except FleetError as exc:
                    print(f"REMOTE INVENTORY UNAVAILABLE · {node.alias} · {exc}", file=sys.stderr)
            return values

        values = [(None, item) for item in local_candidates] + remote_candidates
        chosen = (
            choose_fleet_candidates(values) if browse_all
            else choose_fleet_candidates(values, browse=browse_titles)
        )
        selected = [item for node, item in chosen if node is None]
        selected_remote = [(node, item) for node, item in chosen if node is not None]
    elif getattr(args, "remote_import_all", False):
        selected_remote = remote_candidates

    for candidate in selected:
        pika.import_candidate(candidate)
    if selected:
        for candidate in selected:
            print(
                f"ADDED HERE · {provider_label(candidate.provider)} · "
                f"{terminal_text(candidate.display_name)} · id {candidate.session_id[:8]}"
            )
        print(f"Added {len(selected)} existing conversation(s) to Pika.")
    for node, candidate in selected_remote:
        try:
            adopted = pika.fleet.adopt(node, candidate)
        except FleetError as exc:
            print(
                f"NOT ADOPTED · {node.alias} · {candidate.display_name} · {exc}",
                file=sys.stderr,
            )
            continue
        print(
                f"ADDED ON {node.alias} · {provider_label(adopted.provider)} · "
            f"{terminal_text(adopted.display_name)} · id {adopted.session_id[:8]}"
        )
        current_node = pika.store.get_fleet_node(node.node_id)
        if current_node is not None and current_node.status == "ready":
            print(
                "REMOTE PIKA UPDATED · local metadata cache refreshed · no agent moved"
            )
        else:
            print(
                "REMOTE ADOPTION PROVEN · cache reconciliation pending · no agent moved"
            )
    synchronized = pika.store.list_sessions() if selected else tracked_sessions
    renamed = [
        (tracked_names[session.key], session.display_name, session)
        for session in synchronized
        if session.key in tracked_names
        and tracked_names[session.key] != session.display_name
    ]
    if routine_inventory and renamed:
        print(f"Refreshed {len(renamed)} provider rename(s):")
        for old_name, new_name, session in renamed:
            print(
                f"  {provider_label(session.provider):<6} {session.session_id[:8]}  "
                f"{terminal_text(old_name)} → {terminal_text(new_name)}"
            )
    elif routine_inventory and tracked_sessions:
        print(f"Reconciled {len(tracked_sessions)} tracked conversation name(s).")
    sync_errors = getattr(pika, "discovery_errors", [])
    if isinstance(sync_errors, list) and sync_errors:
        print(
            "Name reconciliation partial · "
            + "; ".join(terminal_text(error) for error in sync_errors)
        )
    coverage_sessions = _setup_coverage(pika, untracked_keys=untracked_keys)
    if not args.no_import:
        print("\nThread profiles deferred · setup did not interview any agents.")
        print(
            "Missing or stale profiles remain visible in `pika expert status`; the "
            "quota-aware refresher handles them gradually. Use "
            "`pika expert refresh --all` only when you want to spend quota now."
        )
    if not first_setup:
        _offer_recovery_rehearsal(
            pika,
            coverage_sessions,
            commissioned=commissioned,
            automatic=bool(args.yes or getattr(args, "skip_walkthrough", False)),
        )
    return 0


def _hook(args: argparse.Namespace) -> int:
    try:
        data = json.load(sys.stdin)
        if not isinstance(data, dict):
            data = {}
        result = handle_hook(args.provider, data)
        output = hook_stdout(args.provider, result)
        if output:
            print(output)
    except Exception:  # noqa: BLE001 - provider hooks must always fail open
        # Observability must never break the provider's own workflow.
        if args.provider == "codex":
            print("{}")
    return 0


def _peek_popup(args: argparse.Namespace) -> int:
    pika = Pika()
    session = pika.store.get_session(args.provider, args.session_id)
    if session is None:
        raise PikaError("The selected Pika conversation is no longer tracked")
    if session.tmux_pane != args.target:
        raise PikaError("The selected conversation's pane changed; reopen its peek")
    print(pika.capture(session, args.lines))
    print(
        f"\n[{terminal_text(args.name)}] Enter attaches; Esc or q returns.",
        flush=True,
    )
    try:
        if sys.stdin.isatty():
            descriptor = sys.stdin.fileno()
            previous = termios.tcgetattr(descriptor)
            try:
                tty.setraw(descriptor)
                answer = sys.stdin.read(1)
            finally:
                termios.tcsetattr(descriptor, termios.TCSADRAIN, previous)
            print()
        else:
            answer = sys.stdin.readline(1)
    except (EOFError, OSError, termios.error):
        return 0
    if not answer:
        return 0
    if answer in {"\n", "\r"}:
        session = pika.store.get_session(args.provider, args.session_id)
        if session is None:
            raise PikaError("The selected Pika conversation is no longer tracked")
        return _open_with_shared_lease_confirmation(pika, session)
    return 0


def _database_error_receipt(error: sqlite3.DatabaseError, *, as_json: bool) -> int:
    message = f"state database is unreadable: {database_path()} ({error})"
    if as_json:
        print(
            json.dumps(
                {
                    "safe_to_disconnect": False,
                    "recoverable_sessions": 0,
                    "tracked_sessions": None,
                    "checks": [
                        {"name": "state database", "level": "error", "message": message}
                    ],
                },
                indent=2,
                sort_keys=True,
            )
        )
    else:
        print(f"✗ state database: {message}")
        print(
            "\nPika cannot verify recovery while its state database is corrupt. "
            "Preserve the file for recovery before replacing it."
        )
    return 1


def _pair_client_bridge(pika: Pika) -> int:
    """Persist one authenticated client route received through SSH stdin."""
    raw = sys.stdin.readline(MAX_BRIDGE_MESSAGE_BYTES + 1)
    if not raw or len(raw.encode()) > MAX_BRIDGE_MESSAGE_BYTES:
        raise PikaError("Invalid client pairing request size")
    if sys.stdin.readline(1):
        raise PikaError("Client pairing accepts exactly one JSON line")
    try:
        request = validate_pair_request(json.loads(raw))
    except (ValueError, ClientBridgeError) as exc:
        raise PikaError(str(exc)) from exc
    node_id = pika.store.local_node_id()
    if request["expected_node_id"] != node_id:
        raise PikaError(
            "NODE IDENTITY CHANGED: expected "
            f"{request['expected_node_id'][:8]}, received {node_id[:8]}"
        )

    config = load_config()
    existing = config.get("client_bridges")
    bridges = (
        [item for item in existing if isinstance(item, dict)]
        if isinstance(existing, list)
        else []
    )
    bridge = {
        "client_id": request["client_id"],
        "label": request["client_label"],
        "host": "127.0.0.1",
        "port": request["port"],
        "token": request["token"],
        "timeout": 0.35,
        "enabled": True,
    }
    bridges = [
        item for item in bridges if item.get("client_id") != request["client_id"]
    ]
    bridges.append(bridge)
    config["client_bridges"] = bridges

    target = config_path()
    if target.exists():
        stamp = time.strftime("%Y%m%dT%H%M%SZ", time.gmtime())
        backup = target.with_name(f"{target.name}.pika-backup-{stamp}")
        suffix = 2
        while backup.exists():
            backup = target.with_name(
                f"{target.name}.pika-backup-{stamp}-{suffix}"
            )
            suffix += 1
        shutil.copy2(target, backup)
    write_config(config)
    print(
        json.dumps(
            {
                "type": "paired",
                "protocol": BRIDGE_PROTOCOL,
                "version": BRIDGE_VERSION,
                "node_id": node_id,
                "client_id": request["client_id"],
                "port": request["port"],
            },
            separators=(",", ":"),
        )
    )
    return 0


def run(argv: list[str] | None = None) -> int:
    argv = _normalize_argv(list(sys.argv[1:] if argv is None else argv))
    args = _parser().parse_args(argv)
    if args.command == 'machine':
        args.command = 'machines'
    if args.command == "update":
        from .installation import InstallError, update
        try:
            update(bundle=args.bundle, check=args.check)
        except (InstallError, OSError, ValueError) as exc:
            raise PikaError(str(exc)) from exc
        return 0
    if args.command == "skill":
        if args.skill_command == "show":
            print(skill_text(), end="")
        else:
            result = install_skill(Path(args.path) if args.path else codex_home() / "skills" / "agent-convo")
            if args.json:
                print(json.dumps(result, sort_keys=True))
            else:
                print(f"Agent Convo {'installed' if result['changed'] else 'already current'} · {result['path']}")
                if result["backup"]:
                    print(f"Backup: {result['backup']}")
        return 0
    if args.command == "hook":
        return _hook(args)
    if args.command == "_process-exit":
        handle_process_exit(
            args.provider,
            args.code,
            session_id=args.session_id,
            launch_token=args.launch_token,
            owner_token=args.owner_token,
        )
        return 0
    if args.command == "_peek-popup":
        return _peek_popup(args)
    try:
        pika = Pika()
    except sqlite3.DatabaseError as exc:
        if args.command == "doctor":
            return _database_error_receipt(exc, as_json=args.json)
        raise PikaError(f"Pika state database is unreadable: {exc}") from exc
    if args.command == "_client-pair":
        return _pair_client_bridge(pika)
    if args.command == "_fleet":
        return handle_fleet_stdio(pika, sys.stdin, sys.stdout)
    if args.command in {"_fleet-open", "_fleet-ask"}:
        actual_node_id = pika.store.local_node_id()
        if args.expected_node_id != actual_node_id:
            raise PikaError(
                "NODE IDENTITY CHANGED: expected "
                f"{args.expected_node_id[:8]}, received {actual_node_id[:8]}"
            )
        if args.command == "_fleet-ask":
            current = pika.resolve_expert_target(args.session_id)
            if current.provider != args.provider or current.session_id != args.session_id:
                raise PikaError("Exact remote consultation identity mismatch")
            return _ask_session(pika, current, [], jsonl=True, fast=args.fast)
        saved = pika.store.get_session(args.provider, args.session_id)
        if saved is None:
            raise PikaError("Exact remote session is not tracked on this Pika node")
        current = next(
            (item for item in pika.refresh() if item.key == saved.key),
            None,
        )
        if current is None:
            raise PikaError("Exact remote session disappeared during reconciliation")
        if args.command == "_fleet-open":
            return _open_with_shared_lease_confirmation(pika, current)
        return _ask_session(
            pika,
            current,
            [],
            jsonl=True,
            fast=args.fast,
        )
    if args.command is None:
        try:
            return _bare(pika)
        except OutsideLiveConflict as exc:
            return _confirm_outside_live(pika, exc)
        except SharedLeaseConflict as exc:
            return _confirm_shared_lease(pika, exc)
    if args.command == "_enter":
        try:
            return pika.enter(args.name)
        except OutsideLiveConflict as exc:
            return _confirm_outside_live(pika, exc)
        except SharedLeaseConflict as exc:
            return _confirm_shared_lease(pika, exc)
    if args.command == "open":
        return _open_with_shared_lease_confirmation(
            pika, _select_named(pika, args.name)
        )
    if args.command == "recover-closed":
        return _open_with_shared_lease_confirmation(
            pika, _select_named(pika, args.name)
        )
    if args.command == "ask":
        return _ask(
            pika,
            args.name,
            args.question,
            jsonl=args.jsonl,
            fast=args.fast,
        )
    if args.command == "expert":
        return _expert(pika, args)
    if args.command == "experts":
        return _experts(pika, args.query, as_json=args.json)
    if args.command == "explain":
        return _explain(pika, args.name, as_json=args.json)
    if args.command == "activity":
        return _activity(pika, limit=args.limit, as_json=args.json)
    if args.command == "list":
        local = pika.refresh(usage=not args.no_usage)
        if args.all_machines:
            print_fleet_sessions(pika.monitor_sessions(local), as_json=args.json)
        else:
            visible = [*local, *pika.pending_launches()]
            print_sessions(visible, as_json=args.json)
            if not args.json:
                _print_actions(pika, local)
        return 0
    if args.command == "next":
        session = pika.next_attention(pika.monitor_sessions(pika.refresh()))
        if session is None:
            print("No Pika session currently needs attention.")
            return 0
        if isinstance(session, PendingLaunch):
            return pika.open_pending(session)
        return _open_with_shared_lease_confirmation(pika, session)
    if args.command == "peek":
        return _peek(pika, args.name, args.lines, ack=args.ack)
    if args.command == "wait":
        selected = _select_named(pika, args.name)
        if isinstance(selected, FleetSession):
            raise PikaError(
                "Remote wait is not yet a durable stream; use `pika sync MACHINE` "
                "or the live monitor"
            )
        return _wait(pika, args)
    if args.command == "new":
        return pika.new(args.name, args.agent, args.cwd)
    if args.command == "adopt":
        session = pika.adopt(args.target, args.name)
        print(
            f"Adopted {terminal_text(session.display_name)} "
            f"({terminal_text(session.provider)}, {terminal_text(session.status)})."
        )
        if session.transcript_path:
            print("Building its exact thread profile…", flush=True)
        _print_expert_refresh_results(pika.bootstrap_experts([session]))
        return 0
    if args.command == "untrack":
        session = _select_named(pika, args.name)
        pika.untrack(session)
        print(
            f"Stopped watching {terminal_text(session.display_name)} "
            f"({session.provider}, {session.session_id[:8]})."
        )
        if isinstance(session, FleetSession):
            node = pika.store.get_fleet_node(session.node_id)
            target = node.ssh_target if node else session.node_name
            print(
                "The remote agent and provider conversation were left running and "
                f"unarchived. Re-adopt it with `pika setup --machine {shlex.quote(target)}`."
            )
        else:
            print(
                "The agent and provider conversation were left running and unarchived. "
                f"Use `pika {terminal_text(session.display_name)}` to track it again."
            )
        return 0
    if args.command == "setup":
        return _setup(pika, args)
    if args.command == "machines":
        return _machines(pika, args)
    if args.command == "sync":
        return _sync_machine(pika, args.machine)
    if args.command == "doctor":
        try:
            repairs = repair_stale_state(pika) if args.repair_stale else []
            if repairs and not args.json:
                for repair in repairs:
                    print(f"Repaired: {repair}")
            elif args.repair_stale and not args.json:
                print("No confirmed stale launch state found.")
            safe = run_doctor(
                pika,
                as_json=args.json,
                verbose=args.verbose,
                repairs=repairs,
            )
        except sqlite3.DatabaseError as exc:
            return _database_error_receipt(exc, as_json=args.json)
        return 0 if safe else 1
    raise PikaError(f"Unknown command: {args.command}")


def main() -> None:
    try:
        raise SystemExit(run())
    except (FleetError, PikaError, TmuxError, TypeError, ValueError) as exc:
        print(f"pika: {terminal_text(exc)}", file=sys.stderr)
        raise SystemExit(1) from exc
    except KeyboardInterrupt:
        print("\npika: interrupted", file=sys.stderr)
        raise SystemExit(130)
