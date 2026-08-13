from __future__ import annotations

import argparse
import json
import os
import shlex
import sqlite3
import sys
import termios
import time
import tty
from pathlib import Path

from . import __version__
from .consult import Consultation, ConsultationError, consultation_for
from .core import Pika, PikaError
from .doctor import repair_stale_state, run_doctor
from .expert_schedule import TIMER_NAME, activate_timer
from .hooks import handle_hook, handle_process_exit, hook_stdout
from .models import Session, Status
from .monitor import run_monitor
from .paths import config_path, database_path
from .setup_hooks import (
    apply_changes,
    hook_spec_fingerprint,
    hooks_installed,
    proposed_changes,
)
from .store import load_config
from .tmux import TmuxError
from .ui import choose_candidates, print_experts, print_sessions, terminal_text

PUBLIC_COMMANDS = {
    "ask",
    "expert",
    "experts",
    "open",
    "list",
    "next",
    "peek",
    "wait",
    "new",
    "adopt",
    "setup",
    "doctor",
}
INTERNAL_COMMANDS = {"hook", "_process-exit", "_peek-popup"}


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="pika",
        description="Persistent Identity Keeper for Codex and Claude sessions in tmux.",
    )
    parser.add_argument("--version", action="version", version=f"pikamux {__version__}")
    sub = parser.add_subparsers(
        dest="command",
        metavar="{open,ask,expert,experts,list,next,peek,wait,new,adopt,setup,doctor}",
    )

    open_parser = sub.add_parser("open", help="open a named conversation")
    open_parser.add_argument("name")

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

    expert_parser = sub.add_parser(
        "expert", help="inspect, refresh, publish, or clear expert cards"
    )
    expert_sub = expert_parser.add_subparsers(dest="expert_command", required=True)
    publish_parser = expert_sub.add_parser(
        "publish", help="publish a provenance-bound expert card from this Pika pane"
    )
    publish_parser.add_argument("--summary", required=True)
    publish_parser.add_argument(
        "--topic", action="append", required=True, help="repeat or comma-separate"
    )
    publish_parser.add_argument("--artifact", action="append", default=[])
    expert_sub.add_parser("clear", help="remove this conversation's expert card")
    refresh_parser = expert_sub.add_parser(
        "refresh", help="interview exact conversations and refresh their expert cards"
    )
    refresh_parser.add_argument("name", nargs="?")
    refresh_mode = refresh_parser.add_mutually_exclusive_group()
    refresh_mode.add_argument(
        "--all", action="store_true", help="refresh all missing or changed cards now"
    )
    refresh_mode.add_argument(
        "--due",
        action="store_true",
        help="refresh only when weekly quota is near reset with >10%% left",
    )
    refresh_parser.add_argument("--provider", choices=("codex", "claude"))
    refresh_parser.add_argument("--json", action="store_true")
    status_parser = expert_sub.add_parser(
        "status", help="show which expert cards are current, stale, or missing"
    )
    status_parser.add_argument("--json", action="store_true")

    experts_parser = sub.add_parser(
        "experts", help="find UUID-bound experts by topic, project, or artifact"
    )
    experts_parser.add_argument("query", nargs="*")
    experts_parser.add_argument("--json", action="store_true")

    list_parser = sub.add_parser("list", help="list all tracked conversations")
    list_parser.add_argument("--json", action="store_true")
    list_parser.add_argument(
        "--no-usage", action="store_true", help="skip token and cost calculation"
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

    new_parser = sub.add_parser("new", help="start a new managed conversation")
    new_parser.add_argument("name")
    new_parser.add_argument("--agent", choices=("codex", "claude"))
    new_parser.add_argument("--cwd")

    adopt_parser = sub.add_parser("adopt", help="adopt a running agent pane")
    adopt_parser.add_argument("target", nargs="?")
    adopt_parser.add_argument("--name")

    setup_parser = sub.add_parser("setup", help="preview and install lifecycle hooks")
    setup_parser.add_argument("--default-provider", choices=("codex", "claude"))
    setup_parser.add_argument(
        "--yes", action="store_true", help="apply the displayed changes"
    )
    setup_parser.add_argument("--dry-run", action="store_true")
    setup_parser.add_argument("--no-import", action="store_true")
    setup_parser.add_argument("--import-all", action="store_true")

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
    hook_parser.add_argument("--provider", required=True, choices=("codex", "claude"))

    exit_parser = sub.add_parser("_process-exit", help=argparse.SUPPRESS)
    exit_parser.add_argument("--provider", required=True, choices=("codex", "claude"))
    exit_parser.add_argument("--session-id")
    exit_parser.add_argument("--launch-token")
    exit_parser.add_argument("--code", required=True, type=int)

    popup_parser = sub.add_parser("_peek-popup", help=argparse.SUPPRESS)
    popup_parser.add_argument("--target", required=True)
    popup_parser.add_argument("--lines", required=True, type=int)
    popup_parser.add_argument("--name", required=True)
    popup_parser.add_argument("--provider", required=True, choices=("codex", "claude"))
    popup_parser.add_argument("--session-id", required=True)
    # argparse otherwise renders hidden implementation commands as
    # ``==SUPPRESS==`` entries in the public command list.
    sub._choices_actions = [
        action
        for action in sub._choices_actions
        if action.dest not in INTERNAL_COMMANDS
    ]
    return parser


def _normalize_argv(argv: list[str]) -> list[str]:
    if not argv:
        return argv
    first = argv[0]
    if first in PUBLIC_COMMANDS | INTERNAL_COMMANDS or first in {
        "-h",
        "--help",
        "--version",
    }:
        return argv
    return ["open", *argv]


def _select_named(
    pika: Pika, value: str, *, sessions: list[Session] | None = None
) -> Session:
    if value == ".":
        return pika.current_repo(sessions)
    if value == "-":
        return pika.previous(sessions)
    return pika.resolve(value, sessions)


def _bare(pika: Pika) -> int:
    if sys.stdin.isatty() and sys.stdout.isatty():
        return run_monitor(
            pika,
            ask_handler=lambda session: _ask_session(
                session, [], jsonl=False
            ),
        )
    # Preserve a useful, finite representation when bare `pika` is redirected.
    # Stable automation should continue to prefer `pika list --json`.
    sessions = pika.refresh(usage=True)
    print_sessions(sessions)
    _print_actions(pika, sessions)
    return 0


def _peek(pika: Pika, name: str, lines: int | None, *, ack: bool = False) -> int:
    session = _select_named(pika, name)
    lines = lines or int(load_config().get("peek_lines") or 200)
    pane = pika.tmux.get_pane(session.tmux_pane or session.tmux_session or "")
    if pane is None:
        raise PikaError(
            f"{session.display_name} has no surviving tmux pane to peek. Use `pika {session.display_name}` to resurrect it."
        )
    human_view = sys.stdin.isatty() and sys.stdout.isatty()
    if os.environ.get("TMUX") and human_view:
        result = pika.tmux.popup(
            pane.pane_id,
            lines,
            session.display_name,
            session.provider,
            session.session_id,
        )
    else:
        print(pika.tmux.capture(pane.pane_id, lines))
        result = 0
    if result == 0 and session.status == Status.READY.value and (human_view or ack):
        pika.acknowledge(session)
    return result


def _ask(
    pika: Pika, name: str, question_parts: list[str], *, jsonl: bool = False
) -> int:
    session = _select_named(pika, name)
    return _ask_session(session, question_parts, jsonl=jsonl)


def _ask_session(
    session: Session, question_parts: list[str], *, jsonl: bool = False
) -> int:
    if not session.transcript_path:
        raise PikaError(
            f"{session.display_name} has no durable provider transcript to consult"
        )
    initial = " ".join(question_parts).strip()
    interactive = sys.stdin.isatty() and sys.stdout.isatty()
    if not initial and not interactive and not jsonl:
        initial = sys.stdin.read().strip()
    if not initial and not interactive and not jsonl:
        raise PikaError("Provide a question as arguments or on stdin")

    try:
        with consultation_for(session) as consultation:
            if jsonl:
                return _ask_jsonl(consultation, session, initial)
            print(
                f"SIDE · {terminal_text(session.display_name)} · "
                f"{session.provider.title()} · parent {session.session_id[:8]} · "
                "EPHEMERAL"
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
    except ConsultationError as exc:
        if jsonl:
            print(
                json.dumps(
                    {"type": "error", "message": str(exc)},
                    ensure_ascii=False,
                    sort_keys=True,
                ),
                flush=True,
            )
            return 1
        raise PikaError(str(exc)) from exc
    print("SIDE CLOSED · discarded · parent transcript unchanged")
    return 0


def _ask_jsonl(consultation: Consultation, session: Session, initial: str) -> int:
    def emit(payload: dict[str, object]) -> None:
        print(json.dumps(payload, ensure_ascii=False, sort_keys=True), flush=True)

    emit(
        {
            "type": "opened",
            "ephemeral": True,
            "provider": session.provider,
            "parent_id": session.session_id,
            "name": session.display_name,
        }
    )

    def handle(question: str) -> None:
        emit({"type": "answer", "text": consultation.ask(question)})

    if initial:
        handle(initial)
    for line in sys.stdin:
        if not line.strip():
            continue
        try:
            request = json.loads(line)
        except ValueError as exc:
            raise ConsultationError(f"Invalid JSONL request: {exc}") from exc
        if not isinstance(request, dict):
            raise ConsultationError("Each JSONL request must be an object")
        if request.get("close") is True:
            break
        question = request.get("question")
        if not isinstance(question, str) or not question.strip():
            raise ConsultationError(
                'Each JSONL request needs a non-empty "question" or {"close":true}'
            )
        handle(question)
    emit(
        {
            "type": "closed",
            "discarded": True,
            "parent_transcript_unchanged": True,
        }
    )
    return 0


def _expert(pika: Pika, args: argparse.Namespace) -> int:
    if args.expert_command == "publish":
        topics = [
            topic.strip()
            for value in args.topic
            for topic in value.split(",")
            if topic.strip()
        ]
        profile = pika.publish_expert(
            summary=args.summary,
            topics=topics,
            artifacts=args.artifact,
        )
        session = pika.store.get_session(*profile.key)
        name = session.display_name if session else profile.session_id[:8]
        print(
            f"EXPERT CARD PUBLISHED · {terminal_text(name)} · "
            f"{profile.provider.title()} · {profile.session_id[:8]} · "
            f"{len(profile.topics)} topics"
        )
        return 0
    if args.expert_command == "clear":
        session = pika.clear_current_expert()
        print(
            f"EXPERT CARD CLEARED · {terminal_text(session.display_name)} · "
            f"{session.provider.title()} · {session.session_id[:8]}"
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
            print("No resumable Pika conversations are eligible for expert cards.")
        else:
            for item in states:
                print(
                    f"{item.status:<8} · {item.session.provider.title():<6} · "
                    f"{terminal_text(item.session.display_name)} · "
                    f"{item.session.session_id[:8]} · {terminal_text(item.detail)}"
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
                        f"Building {len(pending)} provenance-bound expert card(s) "
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
                _select_named(pika, args.name)
                if args.name
                else pika.current_exact_session()
            )
            results = pika.bootstrap_experts([session])
        _print_expert_refresh_results(results, as_json=args.json)
        return 1 if any(item.status == "FAILED" for item in results) else 0
    raise PikaError(f"Unknown expert command: {args.expert_command}")


def _print_expert_refresh_results(results, *, as_json: bool = False) -> None:
    if as_json:
        print(
            json.dumps(
                [item.to_dict() for item in results], indent=2, sort_keys=True
            )
        )
        return
    if not results:
        print("Expert cards already current.")
        return
    for item in results:
        subject = terminal_text(item.name or item.provider.title())
        quota = (
            f" · {item.remaining_percent:.0f}% left"
            if item.remaining_percent is not None
            else ""
        )
        print(
            f"{item.status} · {subject} · {item.provider.title()}"
            f"{quota} · {terminal_text(item.detail)}"
        )


def _experts(pika: Pika, query_parts: list[str], *, as_json: bool) -> int:
    query = " ".join(query_parts).strip()
    print_experts(pika.expert_matches(query), query=query, as_json=as_json)
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
            "any": session.needs_attention,
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


def _setup(pika: Pika, args: argparse.Namespace) -> int:
    print("Pika commissioning · exact recovery for Codex + Claude")
    print("Preview first · existing settings retained · backups before writes\n")
    selected = []
    if not args.no_import and not args.dry_run:
        candidates = [
            item for item in pika.discover_import_candidates() if item.name or item.live
        ]
        tracked = {session.key for session in pika.store.list_sessions()}
        candidates = [
            item
            for item in candidates
            if (item.provider, item.session_id) not in tracked
        ]
        if args.import_all:
            selected = candidates
        elif args.yes or not sys.stdin.isatty():
            if candidates:
                print(
                    f"Pika found {len(candidates)} import candidate(s); "
                    "rerun interactively to choose them or use `--import-all`.\n"
                )
        else:
            selected = choose_candidates(candidates)
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
        answer = input("Default agent: 1 for Codex, 2 for Claude [1]: ").strip()
        default_provider = "claude" if answer == "2" else "codex"
    default_provider = default_provider or config.get("default_provider") or "codex"
    print(f"Default for new conversations: {str(default_provider).title()}\n")
    changes = proposed_changes(str(default_provider))
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
    timer_in_scope = any(change.path.name == TIMER_NAME for change in changes)
    if timer_in_scope:
        active, detail = activate_timer()
        print(f"Expert refresh timer {'active' if active else 'inactive'} · {detail}")
    if changed:
        inactive = [
            provider
            for provider in ("codex", "claude")
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
    print("\nCommissioning status")
    observed: dict[str, bool] = {}
    active_hooks: dict[str, bool] = {}
    for provider in ("codex", "claude"):
        active = hooks_installed(provider)
        active_hooks[provider] = active
        observed[provider] = pika.store.get_meta(
            f"hook_seen:{provider}"
        ) == hook_spec_fingerprint(provider)
        print(
            f"  {provider.title():<6} hooks {'✓' if active else '✗'}  "
            f"observed {'✓' if observed[provider] else '○'}"
        )
    commissioned = all(
        active_hooks[provider] and observed[provider]
        for provider in ("codex", "claude")
    )
    if commissioned:
        print("\nPika commissioned · both agent integrations active and observed.")
    elif (
        active_hooks["codex"]
        and not observed["codex"]
        and active_hooks["claude"]
        and observed["claude"]
    ):
        print(
            "\nOne required proof remains: Codex → `/hooks` → trust Pika → "
            "send one prompt → `pika doctor`."
        )
    else:
        incomplete = []
        for provider in ("codex", "claude"):
            if not active_hooks[provider]:
                incomplete.append(f"{provider.title()} activation")
            if not observed[provider]:
                incomplete.append(f"{provider.title()} observation")
        print("\nPika not yet commissioned · pending: " + ", ".join(incomplete) + ".")
    if args.no_import:
        return 0
    for candidate in selected:
        pika.import_candidate(candidate)
    if selected:
        print(f"Adopted {len(selected)} existing conversation(s).")
    tracked = pika.refresh(usage=False)
    pending_cards = [
        item
        for item in pika.expert_card_states(tracked)
        if item.status in {"MISSING", "STALE"}
    ]
    if pending_cards:
        print(
            f"\nBuilding {len(pending_cards)} provenance-bound expert card(s) "
            "from exact ephemeral interviews."
        )
        for index, state in enumerate(pending_cards, 1):
            print(
                f"  [{index}/{len(pending_cards)}] "
                f"{terminal_text(state.session.display_name)}…",
                flush=True,
            )
            _print_expert_refresh_results(
                pika.bootstrap_experts([state.session]), as_json=False
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
    print(pika.tmux.capture(args.target, args.lines))
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
        return pika.open(session)
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


def run(argv: list[str] | None = None) -> int:
    argv = _normalize_argv(list(sys.argv[1:] if argv is None else argv))
    args = _parser().parse_args(argv)
    if args.command == "hook":
        return _hook(args)
    if args.command == "_process-exit":
        handle_process_exit(
            args.provider,
            args.code,
            session_id=args.session_id,
            launch_token=args.launch_token,
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
    if args.command is None:
        return _bare(pika)
    if args.command == "open":
        return pika.open(_select_named(pika, args.name))
    if args.command == "ask":
        return _ask(pika, args.name, args.question, jsonl=args.jsonl)
    if args.command == "expert":
        return _expert(pika, args)
    if args.command == "experts":
        return _experts(pika, args.query, as_json=args.json)
    if args.command == "list":
        sessions = pika.refresh(usage=not args.no_usage)
        print_sessions(sessions, as_json=args.json)
        if not args.json:
            _print_actions(pika, sessions)
        return 0
    if args.command == "next":
        session = pika.next_attention()
        if session is None:
            print("No Pika session currently needs attention.")
            return 0
        return pika.open(session)
    if args.command == "peek":
        return _peek(pika, args.name, args.lines, ack=args.ack)
    if args.command == "wait":
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
            print("Building its UUID-bound expert card…", flush=True)
        _print_expert_refresh_results(pika.bootstrap_experts([session]))
        return 0
    if args.command == "setup":
        return _setup(pika, args)
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
    except (PikaError, TmuxError, TypeError, ValueError) as exc:
        print(f"pika: {terminal_text(exc)}", file=sys.stderr)
        raise SystemExit(1) from exc
    except KeyboardInterrupt:
        print("\npika: interrupted", file=sys.stderr)
        raise SystemExit(130)
