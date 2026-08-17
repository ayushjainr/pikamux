from __future__ import annotations

import json
import os
import stat
import time
import uuid
from dataclasses import asdict, dataclass
from datetime import datetime, timezone
from pathlib import Path

from .core import Pika
from .executables import configured_executable
from .models import Status
from .paths import config_path, database_path
from .processes import process_start_time, provider_process, shared_provider_process
from .setup_hooks import codex_hooks_enabled, hook_spec_fingerprint, hooks_installed
from .store import LIVE_OWNER_LEASE_SECONDS, load_config


@dataclass(slots=True)
class Check:
    name: str
    level: str
    message: str


def _owner_only(path: Path) -> bool:
    try:
        mode = stat.S_IMODE(path.stat().st_mode)
    except OSError:
        return False
    return mode & 0o077 == 0


def repair_stale_state(pika: Pika, *, older_than_seconds: float = 300) -> list[str]:
    """Remove only expired locks or launches with no surviving agent process."""
    now = time.time()
    pane_list = pika.tmux.list_panes()
    repairs: list[str] = []
    with pika.store.connect() as db:
        pending = db.execute(
            "SELECT * FROM pending_launches ORDER BY created_at"
        ).fetchall()
        reservations = db.execute(
            "SELECT * FROM launch_reservations ORDER BY created_at"
        ).fetchall()
    for row in pending:
        age = max(0, int(now - float(row["created_at"])))
        if age < older_than_seconds:
            continue
        pane_id = str(row["tmux_pane"] or "")
        tmux_session = str(row["tmux_session"] or "")
        token = str(row["launch_token"])
        candidates = [
            item
            for item in pane_list
            if (pane_id and item.pane_id == pane_id)
            or (tmux_session and item.session_name == tmux_session)
            or item.pika_launch_token == token
        ]
        live = any(
            provider_process(item.pane_pid, str(row["provider"])) for item in candidates
        )
        if live:
            continue
        pika.store.delete_pending(token)
        pika.store.delete_meta(f"attached_launch:{token}")
        repairs.append(
            f"removed pending launch {token} ({age}s old; no live "
            f"{row['provider']} process in "
            f"{pane_id or tmux_session or 'any tagged pane'})"
        )
    for row in reservations:
        age = max(0, int(now - float(row["created_at"])))
        if age < older_than_seconds:
            continue
        owner_pid = row["owner_pid"]
        owner_start = row["owner_start_time"]
        if owner_pid is None or owner_start is None:
            # Legacy reservations cannot be proven stale and stay fail-closed.
            continue
        if process_start_time(int(owner_pid)) == int(owner_start):
            continue
        pika.store.release_resume(
            str(row["provider"]), str(row["session_id"]), str(row["token"])
        )
        repairs.append(
            f"removed resume reservation {row['provider']}:{row['session_id']} "
            f"token {row['token']} ({age}s old)"
        )
    return repairs


def run_doctor(
    pika: Pika,
    *,
    as_json: bool = False,
    verbose: bool = False,
    repairs: list[str] | None = None,
) -> bool:
    checks: list[Check] = []
    if pika.tmux.available():
        checks.append(Check("tmux", "ok", "tmux is available"))
    else:
        checks.append(Check("tmux", "error", "tmux is not installed or cannot execute"))
    sessions = pika.refresh()
    for error in pika.discovery_errors:
        checks.append(Check("provider discovery", "warn", error))
    config = load_config()
    required_providers = {session.provider for session in sessions}
    required_providers.add(str(config.get("default_provider") or "codex"))
    for name, provider in pika.providers.items():
        version = provider.version()
        executable = configured_executable(name, config=config)
        required = name in required_providers
        checks.append(
            Check(
                name,
                "ok" if version or not required else "error",
                (f"{version} · {executable}" if version else None)
                or (
                    f"{name} is not on PATH"
                    if required
                    else f"{name} is not installed (not currently required)"
                ),
            )
        )
        installed = hooks_installed(name)
        message = "Pika lifecycle hooks installed"
        hook_level = "ok" if installed or not required else "warn"
        if installed and required:
            if name == "codex" and not codex_hooks_enabled():
                hook_level = "warn"
                message += "; Codex hooks are disabled in config.toml"
            else:
                fingerprint = hook_spec_fingerprint(name)
                observation = pika.store.get_hook_observation(name)
                if observation and observation.get("fingerprint") == fingerprint:
                    age = int(
                        max(0, time.time() - float(observation["observed_at"]))
                    )
                    message += (
                        f"; last {observation['event_name']} event {age}s ago "
                        f"for {str(observation['session_id'])[:8]}"
                    )
                elif pika.store.get_meta(f"hook_seen:{name}") == fingerprint:
                    hook_level = "warn"
                    message += (
                        "; previous event matched, but time and session proof "
                        "are unavailable—use Codex once"
                    )
                else:
                    hook_level = "warn"
                    message += (
                        "; current hook definition has not run—use the provider "
                        "once"
                    )
        checks.append(
            Check(
                f"{name} hooks",
                hook_level,
                message
                if installed
                else (
                    "run `pika setup`"
                    if required
                    else "not installed (provider is not currently required)"
                ),
            )
        )
    for label, path in (
        ("configuration", config_path()),
        ("state database", database_path()),
    ):
        if not path.exists():
            checks.append(Check(label, "warn", f"missing: {path}"))
        elif not _owner_only(path):
            checks.append(
                Check(label, "warn", f"permissions are not owner-only: {path}")
            )
        else:
            checks.append(Check(label, "ok", f"owner-only: {path}"))
        if label == "configuration" and path.exists():
            try:
                value = json.loads(path.read_text())
                if not isinstance(value, dict):
                    raise TypeError("top level must be a JSON object")
            except (OSError, TypeError, ValueError) as exc:
                checks.append(Check("configuration format", "error", str(exc)))
    recoverable = 0
    unbound = 0
    missing_cwd = 0
    invalid_session_ids: list[str] = []
    non_resumable: list[str] = []
    native_name_failures: list[str] = []
    duplicate_ids: list[str] = []
    seen: set[tuple[str, str]] = set()
    panes = pika.tmux.list_panes()
    for session in sessions:
        if pika.store.get_meta(
            f"native_name_error:{session.provider}:{session.session_id}"
        ):
            native_name_failures.append(f"{session.provider}:{session.session_id}")
        if session.key in seen:
            duplicate_ids.append(f"{session.provider}:{session.session_id}")
        seen.add(session.key)
        if session.status == Status.UNBOUND.value or session.session_id.startswith(
            "unbound:"
        ):
            unbound += 1
        else:
            valid_id = True
            try:
                parsed_id = uuid.UUID(session.session_id)
                if str(parsed_id) != session.session_id.lower():
                    raise ValueError
            except (ValueError, AttributeError):
                valid_id = False
                invalid_session_ids.append(f"{session.provider}:{session.session_id}")
            exact_live_pane = any(
                (pane.pika_provider, pane.pika_session_id) == session.key
                and pika.exact_pane_pid(session, pane)
                for pane in panes
            )
            provider = pika.providers.get(session.provider)
            resumable = bool(exact_live_pane)
            if provider and not resumable and valid_id:
                try:
                    checker = getattr(provider, "is_resumable", None)
                    resumable = bool(checker and checker(session.session_id))
                except (OSError, RuntimeError):
                    resumable = False
            if valid_id and not resumable:
                non_resumable.append(f"{session.provider}:{session.session_id}")
            if valid_id and resumable and session.cwd and Path(session.cwd).is_dir():
                recoverable += 1
            elif not session.cwd or not Path(session.cwd).is_dir():
                missing_cwd += 1
    pending_rows = []
    reservation_rows = []
    with pika.store.connect() as db:
        pending_rows = db.execute(
            "SELECT * FROM pending_launches ORDER BY created_at"
        ).fetchall()
        reservation_rows = db.execute(
            "SELECT * FROM launch_reservations ORDER BY created_at"
        ).fetchall()
    pending = len(pending_rows)
    reservations = len(reservation_rows)
    pane_groups: dict[tuple[str, str], list[str]] = {}
    for pane in panes:
        if pane.pika_provider and pane.pika_session_id:
            pane_groups.setdefault(
                (pane.pika_provider, pane.pika_session_id), []
            ).append(f"{pane.session_name}:{pane.pane_id}")
    duplicate_homes = [
        f"{provider}:{session_id} in {', '.join(homes)}"
        for (provider, session_id), homes in pane_groups.items()
        if len(homes) > 1
    ]
    mismatched_panes: list[str] = []
    for pane in panes:
        if pane.pika_provider not in {"codex", "claude"} or not pane.pika_session_id:
            continue
        expected = provider_process(pane.pane_pid, pane.pika_provider)
        other_provider = "claude" if pane.pika_provider == "codex" else "codex"
        other = provider_process(pane.pane_pid, other_provider)
        if not expected and other:
            mismatched_panes.append(
                f"{pane.session_name}:{pane.pane_id} is tagged {pane.pika_provider} "
                f"but contains {other_provider} PID {other}"
            )
    duplicate_processes: list[str] = []
    outside_tracked_processes: list[str] = []
    for session in sessions:
        provider = pika.providers.get(session.provider)
        active: set[int] = set()
        if provider:
            get_active = getattr(provider, "active_pids", None)
            if get_active:
                active.update(get_active(session.session_id))
        active.update(pika.identity_pids(session))
        pane_active: set[int] = set()
        for pane in panes:
            if (pane.pika_provider, pane.pika_session_id) == session.key:
                pid = provider_process(pane.pane_pid, session.provider)
                if pid:
                    if pid in pika.identity_pids(session):
                        active.add(pid)
                        pane_active.add(pid)
                    else:
                        mismatched_panes.append(
                            f"{pane.session_name}:{pane.pane_id} is tagged "
                            f"{session.provider}:{session.session_id} but PID {pid} "
                            "has no independent exact-UUID evidence"
                        )
        outside = active - pane_active
        if outside:
            outside_tracked_processes.append(
                f"{session.provider}:{session.session_id} PIDs "
                + ", ".join(map(str, sorted(outside)))
            )
        if len(active) > 1:
            duplicate_processes.append(
                f"{session.provider}:{session.session_id} PIDs "
                + ", ".join(map(str, sorted(active)))
            )
    tracked_keys = {session.key for session in sessions}
    hidden_live_owners: list[str] = []
    for (
        provider,
        session_id,
        pid,
        start_time,
        last_seen,
        owner_token,
    ) in pika.store.list_live_owners():
        if (provider, session_id) in tracked_keys:
            continue
        live_pid = (
            provider_process(pid, provider)
            if start_time is not None and process_start_time(pid) == start_time
            else None
        )
        if (
            live_pid
            and shared_provider_process(live_pid, provider)
            and time.time() - last_seen > LIVE_OWNER_LEASE_SECONDS
        ):
            live_pid = None
        if live_pid:
            hidden_live_owners.append(f"{provider}:{session_id} PID {live_pid}")
        else:
            pika.store.delete_live_owner(
                provider,
                session_id,
                pid=pid,
                owner_token=owner_token,
            )
    identity_duplicates = duplicate_ids + duplicate_homes + duplicate_processes
    if (
        identity_duplicates
        or mismatched_panes
        or invalid_session_ids
        or non_resumable
        or hidden_live_owners
        or outside_tracked_processes
    ):
        messages: list[str] = []
        if identity_duplicates:
            messages.append("duplicate owners: " + "; ".join(identity_duplicates))
        if mismatched_panes:
            messages.append("mismatched tmux homes: " + "; ".join(mismatched_panes))
        if invalid_session_ids:
            messages.append("invalid provider UUIDs: " + ", ".join(invalid_session_ids))
        if non_resumable:
            messages.append(
                "missing durable provider history: " + ", ".join(non_resumable)
            )
        if hidden_live_owners:
            messages.append(
                "live provider sessions outside Pika tracking: "
                + "; ".join(hidden_live_owners)
            )
        if outside_tracked_processes:
            messages.append(
                "tracked conversations running outside exact Pika tmux homes: "
                + "; ".join(outside_tracked_processes)
            )
        checks.append(
            Check(
                "identity",
                "error",
                "; ".join(messages),
            )
        )
    elif unbound or pending or reservations:
        now = time.time()
        pending_details = [
            f"{row['name']} token={str(row['launch_token'])[:8]} "
            f"{int(max(0, now - float(row['created_at'])))}s "
            f"pane={row['tmux_pane'] or '-'}"
            for row in pending_rows
        ]
        reservation_details = [
            f"{row['provider']}:{row['session_id']} token={row['token']} "
            f"{int(max(0, now - float(row['created_at'])))}s "
            f"owner_pid={row['owner_pid'] or '-'}"
            for row in reservation_rows
        ]
        detail = "; ".join(pending_details + reservation_details)
        checks.append(
            Check(
                "identity",
                "warn",
                f"{unbound} unbound session(s), {pending} pending launch(es), "
                f"{reservations} active resume(s)" + (f"; {detail}" if detail else ""),
            )
        )
    else:
        checks.append(
            Check(
                "identity", "ok", "all tracked conversations have exact provider UUIDs"
            )
        )
    if missing_cwd:
        checks.append(
            Check(
                "working directories",
                "warn",
                f"{missing_cwd} saved path(s) no longer exist",
            )
        )
    elif sessions:
        checks.append(
            Check("working directories", "ok", "all saved working directories exist")
        )
    if native_name_failures:
        checks.append(
            Check(
                "native names",
                "warn",
                "provider naming is still pending for "
                + ", ".join(native_name_failures),
            )
        )
    safe = not any(check.level != "ok" for check in checks)
    if as_json:
        print(
            json.dumps(
                {
                    "safe_to_disconnect": safe,
                    "recoverable_sessions": recoverable,
                    "tracked_sessions": len(sessions),
                    "repairs": repairs or [],
                    "checks": [asdict(check) for check in checks],
                },
                indent=2,
                sort_keys=True,
            )
        )
        return safe
    if verbose or not safe:
        for check in checks:
            mark = {"ok": "✓", "warn": "!", "error": "✗"}[check.level]
            print(f"{mark} {check.name}: {check.message}")
        print()
    if safe:
        live = sum(session.live for session in sessions)
        needs_you = sum(
            session.status == Status.NEEDS_YOU.value for session in sessions
        )
        parked = sum(session.status == Status.PARKED.value for session in sessions)
        verified_at = datetime.now(timezone.utc).strftime("%Y-%m-%d %H:%M UTC")
        if sessions:
            print(
                f"Recovery verified · {recoverable}/{len(sessions)} exact "
                f"conversation(s) · {verified_at}"
            )
        else:
            print(
                f"Pika setup verified · no conversations currently protected · "
                f"{verified_at}"
            )
        print(f"{live} live · {needs_you} needs you · {parked} parked")
        print("Safe to disconnect this terminal. Keep the tmux server running.")
        if os.environ.get("TMUX"):
            print("Detach with Ctrl-b d.")
    else:
        print(
            "Pika found recovery risks above. Resolve them before relying on terminal disconnects."
        )
    return safe
