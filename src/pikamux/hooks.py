from __future__ import annotations

import json
import os
import time
from pathlib import Path
from typing import Any

from .models import Candidate, Session, Status
from .processes import (
    process_environment,
    process_start_time,
    provider_ancestor,
    provider_process,
)
from .providers import (
    ClaudeProvider,
    CodexProvider,
    OpenCodeProvider,
    codex_transcript_metadata,
    codex_worker_originator,
    opencode_native_placeholder_title,
)
from .setup_hooks import hook_spec_fingerprint
from .store import Store, load_config
from .status_projection import project_status
from .tmux import Tmux, TmuxError
from .ui import provider_identity_label, provider_label, terminal_text


def _same_path(left: object, right: object) -> bool:
    if not left or not right:
        return False
    try:
        return Path(str(left)).resolve() == Path(str(right)).resolve()
    except OSError:
        return str(left) == str(right)


def _launch_hook_mismatch(
    pending: dict[str, Any],
    *,
    provider: str,
    thread_id: str,
    pane_id: str | None,
    cwd: object,
    parent_session_id: str | None = None,
) -> str | None:
    if str(pending["provider"]) != provider:
        return f"expected provider {pending['provider']}, observed {provider}"
    expected_id = str(pending.get("expected_session_id") or "")
    if expected_id and expected_id != thread_id:
        return (
            f"expected {provider_identity_label(provider)} {expected_id}, "
            f"observed {thread_id}"
        )
    if str(pending["provider"]) == "codex" and parent_session_id:
        return f"expected a new root thread, observed child of {parent_session_id}"
    expected_pane = str(pending.get("tmux_pane") or "")
    if expected_pane and expected_pane != str(pane_id or ""):
        return f"expected pane {expected_pane}, observed {pane_id or 'none'}"
    if not _same_path(pending.get("cwd"), cwd):
        return f"expected cwd {pending.get('cwd')}, observed {cwd or 'none'}"
    return None


def _record_launch_conflict(
    store: Store,
    launch_token: str,
    provider: str,
    session_id: str,
    detail: str,
) -> None:
    store.set_meta(f"launch_binding_error:{launch_token}", detail)
    store.set_meta(
        f"launch_binding_competitor:{launch_token}",
        json.dumps([provider, session_id]),
    )
    binding = store.get_launch_binding(launch_token)
    if binding is None:
        return
    winner = store.get_session(*binding)
    if winner is None or binding == (provider, session_id):
        return
    store.capture_identity_interruption(*winner.key)
    now = time.time()
    store.record_status_observation(
        *winner.key,
        kind="safety",
        status=Status.OPEN_TWICE.value,
        unread=True,
        attention_reason="identity",
        error=(
            "launch token observed competing provider identities: "
            f"{binding[0]}:{binding[1][:8]}, {provider}:{session_id[:8]}"
        ),
        observed_at=now,
        source="launch-conflict",
    )
    store.update_session(
        *winner.key,
        status=Status.OPEN_TWICE.value,
        unread=True,
        error=(
            "launch token observed competing provider identities: "
            f"{binding[0]}:{binding[1][:8]}, {provider}:{session_id[:8]}"
        ),
        attention_reason="identity",
        last_event_at=now,
        last_activity_at=now,
    )


def _apply_launch_conflict(
    store: Store, launch_token: str, winner: Session
) -> None:
    raw = store.get_meta(f"launch_binding_competitor:{launch_token}")
    if not raw:
        return
    try:
        provider, session_id = json.loads(raw)
    except (TypeError, ValueError):
        return
    if (provider, session_id) == winner.key:
        return
    store.capture_identity_interruption(*winner.key)
    now = time.time()
    conflict_status = (
        Status.OPEN_TWICE.value
        if provider == winner.provider
        else Status.ERROR.value
    )
    conflict_error = (
        "launch token observed competing provider identities: "
        f"{winner.provider}:{winner.session_id[:8]}, "
        f"{provider}:{str(session_id)[:8]}"
    )
    store.record_status_observation(
        *winner.key,
        kind="safety",
        status=conflict_status,
        unread=True,
        attention_reason="identity",
        error=conflict_error,
        observed_at=now,
        source="launch-conflict",
    )
    store.update_session(
        *winner.key,
        status=conflict_status,
        unread=True,
        error=conflict_error,
        attention_reason="identity",
        last_event_at=now,
        last_activity_at=now,
    )


def _event_state(
    provider: str, data: dict[str, Any]
) -> tuple[str, bool, str | None, str | None]:
    event = str(data.get("hook_event_name") or "")
    tool_name = str(data.get("tool_name") or "")
    if event == "PreToolUse" and (
        (provider == "codex" and tool_name == "request_user_input")
        or (provider == "claude" and tool_name == "AskUserQuestion")
    ):
        return Status.NEEDS_YOU.value, True, None, "question"
    if event == "UserPromptSubmit":
        return Status.WORKING.value, False, None, None
    if event == "PermissionRequest":
        return Status.NEEDS_YOU.value, True, None, "permission"
    if event == "QuestionRequest":
        return Status.NEEDS_YOU.value, True, None, "question"
    if event in {"PermissionReply", "QuestionReply"}:
        return Status.WORKING.value, False, None, None
    if event == "Notification":
        kind = data.get("notification_type")
        if kind == "permission_prompt":
            return Status.NEEDS_YOU.value, True, None, "permission"
        if kind in {"agent_needs_input", "elicitation_dialog"}:
            return Status.NEEDS_YOU.value, True, None, "question"
        if kind in {"agent_completed", "idle_prompt"}:
            return Status.READY.value, True, None, "completed"
    if event == "Stop":
        background = data.get("background_tasks")
        if provider == "claude" and isinstance(background, list) and background:
            return Status.WORKING.value, False, None, None
        return Status.READY.value, True, None, "completed"
    if event == "StopFailure":
        error = str(data.get("error") or "Claude turn failed")
        return Status.ERROR.value, True, error, "failed"
    if event == "SessionEnd":
        return Status.PARKED.value, False, None, None
    if event == "SessionStart":
        return Status.READY.value, False, None, None
    return Status.WORKING.value, False, None, None


def _repair_worker_pane_claim(
    provider: str,
    session_id: str,
    store: Store,
    tmux: Tmux,
) -> None:
    """Undo only a stale Pika tag previously written for this worker UUID."""
    pane_id = os.environ.get("TMUX_PANE")
    if not pane_id:
        return
    try:
        pane = tmux.get_pane(pane_id)
    except (OSError, TmuxError):
        return
    if pane is None or (pane.pika_provider, pane.pika_session_id) != (
        provider,
        session_id,
    ):
        return
    previous = [
        item
        for item in store.list_sessions()
        if item.key != (provider, session_id)
        and item.tmux_pane == pane_id
        and not item.session_id.startswith("unbound:")
    ]
    try:
        if len(previous) == 1:
            owner = previous[0]
            tmux.tag_pane(
                pane_id,
                provider=owner.provider,
                session_id=owner.session_id,
                name=owner.display_name,
                launch_token="",
            )
        else:
            tmux.clear_pika_tags(pane_id)
    except (AttributeError, OSError, TmuxError):
        pass


def handle_hook(
    provider: str, data: dict[str, Any], store: Store | None = None
) -> dict[str, Any] | None:
    if os.environ.get("PIKA_EPHEMERAL") == "1":
        return None
    store = store or Store()
    store.initialize()
    tmux = Tmux()
    reported_session_id = str(data.get("session_id") or "")
    if not reported_session_id:
        return None
    transcript_path = data.get("transcript_path")
    transcript_metadata = (
        codex_transcript_metadata(transcript_path) if provider == "codex" else {}
    )
    thread_id = str(
        transcript_metadata.get("id")
        or transcript_metadata.get("session_id")
        or reported_session_id
    )
    if provider == "claude":
        try:
            worker_origin = ClaudeProvider().worker_originator(
                thread_id, str(transcript_path) if transcript_path else None
            )
        except (OSError, RuntimeError):
            worker_origin = None
        if worker_origin:
            _repair_worker_pane_claim(provider, thread_id, store, tmux)
            store.delete_session(provider, thread_id)
            return None
        expected_session_id = str(os.environ.get("PIKA_SESSION_ID") or "")
        if expected_session_id and thread_id != expected_session_id:
            # A Claude process launched from inside a managed conversation can
            # inherit its parent's PIKA_NAME, pane and owner variables. The new
            # provider UUID is a separate identity and must not rename itself,
            # claim the parent's pane, or enter the operational inventory.
            _repair_worker_pane_claim(provider, thread_id, store, tmux)
            return None
    if provider == "codex" and codex_worker_originator(
        thread_id,
        transcript_path,
        originator=data.get("originator"),
    ):
        # App-server automation workers are subordinate execution units, not
        # conversations.  Remove any record created by an earlier hook and
        # return before ownership, attention, or tmux identity can be changed.
        _repair_worker_pane_claim(provider, thread_id, store, tmux)
        store.delete_session(provider, thread_id)
        return None
    if provider == "opencode":
        try:
            worker_origin = OpenCodeProvider().worker_originator(
                thread_id, str(transcript_path) if transcript_path else None
            )
        except (OSError, RuntimeError):
            worker_origin = None
        if worker_origin:
            _repair_worker_pane_claim(provider, thread_id, store, tmux)
            store.delete_session(provider, thread_id)
            return None
    expected_provider = str(os.environ.get("PIKA_PROVIDER") or "")
    if expected_provider and expected_provider != provider:
        # A provider launched inside another managed provider inherits its
        # parent's name, pane and ownership variables. Until immutable worker
        # metadata is available, quarantine the child hook at this boundary:
        # it must not rename itself, enter inventory, or claim the parent home.
        _repair_worker_pane_claim(provider, thread_id, store, tmux)
        return None
    fingerprint = hook_spec_fingerprint(provider)
    store.set_meta(f"hook_seen:{provider}", fingerprint)
    store.record_hook_observation(
        provider,
        fingerprint,
        str(data.get("hook_event_name") or "unknown"),
        thread_id,
        source=str(data.get("source") or transcript_metadata.get("source") or "")
        or None,
        managed=bool(os.environ.get("PIKA_LAUNCH_TOKEN")),
    )
    pane_id = os.environ.get("TMUX_PANE")
    existing = store.get_session_by_thread(provider, thread_id)
    if (
        provider == "opencode"
        and data.get("hook_event_name") == "SessionEnd"
        and data.get("deleted") is True
    ):
        canonical = existing.session_id if existing else thread_id
        store.delete_live_owner(provider, canonical)
        store.delete_recovery_owner(provider, canonical)
        pane = tmux.get_pane(pane_id) if pane_id else None
        if pane and (pane.pika_provider, pane.pika_session_id) == (
            provider,
            canonical,
        ):
            try:
                tmux.clear_pika_tags(pane.pane_id)
            except (AttributeError, OSError, TmuxError):
                pass
        store.delete_session(provider, canonical)
        return None
    provider_candidate = None
    if provider in {"codex", "opencode"} and existing is None:
        try:
            if provider == "codex":
                provider_candidate = CodexProvider().thread_candidate(
                    thread_id, str(transcript_path) if transcript_path else None
                )
            else:
                matches = OpenCodeProvider().find_candidates(thread_id)
                provider_candidate = matches[0] if matches else None
        except (OSError, RuntimeError):
            provider_candidate = None
        if not isinstance(provider_candidate, Candidate):
            provider_candidate = None
        if provider_candidate and provider_candidate.parent_session_id:
            parent = store.get_session_by_thread(
                provider, provider_candidate.parent_session_id
            )
            same_cwd = bool(
                parent
                and (
                    not parent.cwd
                    or not provider_candidate.cwd
                    or parent.cwd == provider_candidate.cwd
                )
            )
            same_home = bool(
                parent
                and pane_id
                and parent.tmux_pane
                and parent.tmux_pane == pane_id
            )
            if parent and same_home and same_cwd:
                # Codex can switch one live TUI from a parent UUID to a fork
                # without replacing the `codex resume <parent>` process.  The
                # immutable lineage plus the exact inherited tmux pane proves
                # that this is the same Pika home; a renamed fork is expected
                # and must not be split into a second parked conversation.
                if parent.status == Status.WORKING.value:
                    # A second UUID started while the stable home's current
                    # thread is still working. Keep the pane bound to its
                    # canonical Pika identity and fail closed on the one row.
                    store.capture_identity_interruption(*parent.key)
                    now = time.time()
                    conflict_error = (
                        "multiple active Codex continuation threads: "
                        f"{parent.provider_thread_id[:8]}, {thread_id[:8]}"
                    )
                    store.record_status_observation(
                        *parent.key,
                        kind="safety",
                        status=Status.OPEN_TWICE.value,
                        unread=True,
                        attention_reason="identity",
                        error=conflict_error,
                        observed_at=now,
                        source="continuation-conflict",
                    )
                    store.update_session(
                        *parent.key,
                        status=Status.OPEN_TWICE.value,
                        unread=True,
                        error=conflict_error,
                        attention_reason="identity",
                        last_event_at=now,
                        last_activity_at=now,
                    )
                    return None
                existing = parent
    canonical_session_id = existing.session_id if existing else thread_id
    if store.is_untracked(provider, canonical_session_id) or store.is_untracked(
        provider, thread_id
    ):
        store.delete_live_owner(provider, canonical_session_id)
        return None
    launch_token = os.environ.get("PIKA_LAUNCH_TOKEN")
    owner_token = os.environ.get("PIKA_OWNER_TOKEN") or ""
    owner_pid = provider_ancestor(os.getppid(), provider)
    managed_root_switch = False
    pending = store.get_pending(launch_token) if launch_token else None
    pane_pending = store.find_pending_for_pane(pane_id) if pane_id else None
    if pane_pending and (
        not launch_token or launch_token != str(pane_pending["launch_token"])
    ):
        # Pane location is context, not launch identity. A provider hook must
        # carry the exact token Pika injected before it can bind, tag, or
        # delete a pending launch.
        if data.get("hook_event_name") != "SessionEnd":
            _record_launch_conflict(
                store,
                str(pane_pending["launch_token"]),
                provider,
                canonical_session_id,
                "refused launch hook: missing or wrong PIKA_LAUNCH_TOKEN",
            )
        return None
    if (
        launch_token
        and pending is None
        and store.get_launch_binding(launch_token) is None
    ):
        # Unknown/stale environment values cannot mint a launch binding.
        launch_token = None
    if pending:
        mismatch = _launch_hook_mismatch(
            pending,
            provider=provider,
            thread_id=thread_id,
            pane_id=pane_id,
            cwd=data.get("cwd"),
            parent_session_id=(
                str(provider_candidate.parent_session_id)
                if provider_candidate and provider_candidate.parent_session_id
                else None
            ),
        )
        if mismatch:
            assert launch_token is not None
            if data.get("hook_event_name") != "SessionEnd":
                _record_launch_conflict(
                    store,
                    launch_token,
                    provider,
                    canonical_session_id,
                    "refused launch hook: " + mismatch,
                )
            return None
    if launch_token:
        binding = store.get_launch_binding(launch_token)
        if binding and binding != (provider, canonical_session_id):
            managed_root_switch = bool(
                provider == "opencode"
                and data.get("hook_event_name") != "SessionEnd"
                and owner_pid
                and binding[0] == provider
                and store.switch_launch_binding(
                    launch_token,
                    provider,
                    binding[1],
                    canonical_session_id,
                    owner_pid,
                )
            )
            if not managed_root_switch:
                if data.get("hook_event_name") != "SessionEnd":
                    _record_launch_conflict(
                        store,
                        launch_token,
                        provider,
                        canonical_session_id,
                        f"refused competing {provider}:{canonical_session_id}",
                    )
                return None
        elif not store.bind_launch(launch_token, provider, canonical_session_id):
            if data.get("hook_event_name") != "SessionEnd":
                _record_launch_conflict(
                    store,
                    launch_token,
                    provider,
                    canonical_session_id,
                    f"refused competing {provider}:{canonical_session_id}",
                )
            return None
    if data.get("hook_event_name") == "SessionEnd":
        if owner_pid:
            store.delete_live_owner(
                provider,
                canonical_session_id,
                pid=owner_pid,
                owner_token=owner_token,
            )
    elif owner_pid:
        if provider == "opencode":
            store.delete_other_live_owner_sessions(
                provider, owner_pid, canonical_session_id
            )
        store.set_live_owner(
            provider,
            canonical_session_id,
            owner_pid,
            owner_token=owner_token,
        )
    if provider == "opencode" and data.get("hook_event_name") == "SessionHeartbeat":
        return None
    placeholder = store.get_session(provider, f"unbound:{pane_id}") if pane_id else None
    pane = tmux.get_pane(pane_id) if pane_id else None
    provider_name = data.get("session_title")
    switched_candidate_name = (
        provider_candidate.name
        if provider_candidate
        and existing
        and thread_id != existing.session_id
        else None
    )
    desired_name = os.environ.get("PIKA_NAME")
    if provider == "claude" and str(os.environ.get("PIKA_SESSION_ID") or "") != thread_id:
        # PIKA_NAME is authoritative for Claude only alongside the exact UUID
        # injected by Pika. A name on its own is ambient shell state.
        desired_name = None
    opencode_desired_name = (
        str(data.get("desired_name") or "") or None
        if "desired_name" in data
        else desired_name
    )
    preserve_requested_name = bool(
        provider == "opencode"
        and data.get("hook_event_name") == "SessionStart"
        and opencode_desired_name
        and (
            data.get("native_name_error")
            or provider_name != opencode_desired_name
        )
    )
    if preserve_requested_name:
        name = str(opencode_desired_name)
    else:
        name = (
            str(provider_name)
            if provider_name
            else (str(pending["name"]) if pending else None)
            or switched_candidate_name
            or (existing.name if existing else None)
            or (provider_candidate.name if provider_candidate else None)
            or (placeholder.name if placeholder else None)
            or desired_name
            or (pane.pika_name if pane else None)
        )
    if (
        provider == "opencode"
        and existing is None
        and pending is None
        and placeholder is None
        and not (pane and pane.pika_provider and pane.pika_session_id)
        and not opencode_desired_name
        and opencode_native_placeholder_title(name)
    ):
        # OpenCode assigns every untouched root a non-empty timestamp title.
        # That is provider scaffolding, not a user's request to watch it in
        # Pika. Keep the live-owner lease recorded above for duplicate safety,
        # but do not create an operational inventory row until the root is
        # renamed or deliberately launched/adopted by Pika.
        return None
    # Avoid pulling every unnamed IDE/background conversation into Pika.
    if (
        existing is None
        and pending is None
        and placeholder is None
        and not pane_id
        and not name
    ):
        return None
    status, unread, error, attention_reason = _event_state(provider, data)
    if pane and pane.attached and status == Status.READY.value:
        unread = False
    if data.get("hook_event_name") == "SessionEnd" and existing and existing.unread:
        status, unread = existing.status, existing.unread
        attention_reason = existing.attention_reason
        error = existing.error
    now = time.time()
    observation_at = (
        existing.last_event_at
        if data.get("hook_event_name") == "SessionEnd"
        and existing
        and existing.unread
        else now
    )
    store.record_status_observation(
        provider,
        canonical_session_id,
        kind="lifecycle",
        status=status,
        unread=unread,
        attention_reason=attention_reason,
        error=error,
        observed_at=observation_at,
        source=f"hook:{data.get('hook_event_name') or 'unknown'}",
    )
    if data.get("hook_event_name") != "SessionEnd":
        store.clear_status_observation(provider, canonical_session_id, "runtime")
    projection = project_status(
        store.status_observations(provider, canonical_session_id),
        live=data.get("hook_event_name") != "SessionEnd",
        home_state="unknown",
        fallback_status=status,
        fallback_unread=unread,
        fallback_reason=attention_reason,
        fallback_error=error,
        fallback_at=observation_at,
    )
    status = projection.status
    unread = projection.unread
    attention_reason = projection.attention_reason
    error = projection.error
    newly_actionable = not (
        existing
        and existing.unread
        and existing.status == status
        and existing.attention_reason == attention_reason
        and existing.error == error
    )
    session = Session(
        provider=provider,
        session_id=canonical_session_id,
        active_thread_id=(
            thread_id if thread_id != canonical_session_id else None
        ),
        name=name,
        cwd=data.get("cwd")
        or (existing.cwd if existing else None)
        or (placeholder.cwd if placeholder else None)
        or (pending["cwd"] if pending else None),
        branch=existing.branch if existing else None,
        transcript_path=transcript_path
        or (provider_candidate.transcript_path if provider_candidate else None)
        or (existing.transcript_path if existing else None),
        tmux_session=pane.session_name
        if pane
        else (existing.tmux_session if existing else None)
        or (placeholder.tmux_session if placeholder else None),
        tmux_pane=pane.pane_id
        if pane
        else (existing.tmux_pane if existing else None)
        or (placeholder.tmux_pane if placeholder else pane_id),
        root_pid=(provider_process(pane.pane_pid, provider) if pane else owner_pid),
        status=status,
        unread=unread,
        model=data.get("model") or (existing.model if existing else None),
        source="managed"
        if pending or placeholder or managed_root_switch or (existing and existing.managed)
        else "external",
        managed=bool(
            pending
            or placeholder
            or managed_root_switch
            or (existing and existing.managed)
        ),
        error=error,
        attention_reason=attention_reason,
        created_at=existing.created_at if existing else now,
        updated_at=now,
        last_event_at=projection.observed_at,
        last_activity_at=now,
    )
    store.upsert_session(session)
    if launch_token:
        _apply_launch_conflict(store, launch_token, session)
    if store.is_untracked(provider, canonical_session_id):
        return None
    if placeholder and placeholder.session_id != canonical_session_id:
        store.delete_session(provider, placeholder.session_id)
    pane_tagged = False
    if pane:
        try:
            tmux.tag_pane(
                pane.pane_id,
                provider=provider,
                session_id=canonical_session_id,
                name=session.display_name,
                launch_token="",
            )
        except (OSError, TmuxError):
            pass
        else:
            pane_tagged = True
    if launch_token:
        # A provider UUID is known, but a pending launch is not complete until
        # its physical home carries the same exact identity. Certify only the
        # provider PID generation captured by Pika's launcher, and only after
        # the pane tag succeeds. A later hook can retry a transient tmux failure.
        certified = False
        if pending is not None and pane_tagged and pane is not None:
            saved_pid = pending.get("root_pid")
            saved_start = pending.get("root_pid_start")
            pane_pid = provider_process(pane.pane_pid, provider)
            environment = process_environment(pane_pid) if pane_pid else {}
            if (
                saved_pid is not None
                and saved_start is not None
                and pane_pid == int(saved_pid)
                and process_start_time(pane_pid) == int(saved_start)
                and environment.get("PIKA_LAUNCH_TOKEN") == launch_token
                and environment.get("PIKA_PROVIDER") == provider
            ):
                certified = store.certify_launch(
                    launch_token,
                    provider,
                    canonical_session_id,
                    pane_pid,
                    int(saved_start),
                )
        if pending is None or certified:
            store.delete_pending(launch_token)
        attach_key = f"attached_launch:{launch_token}"
        if store.get_meta(attach_key):
            store.record_attach(provider, canonical_session_id)
            store.delete_meta(attach_key)
    name_error_key = f"native_name_error:{provider}:{thread_id}"
    if (
        provider == "codex"
        and (
            data.get("hook_event_name") == "SessionStart"
            or store.get_meta(name_error_key)
        )
        and desired_name
        and not provider_name
    ):
        try:
            named = CodexProvider().set_native_name(thread_id, desired_name)
        except (OSError, RuntimeError):
            named = False
        if named:
            store.delete_meta(name_error_key)
        else:
            store.set_meta(name_error_key, desired_name)
    if (
        provider == "opencode"
        and data.get("hook_event_name") == "SessionStart"
        and opencode_desired_name
    ):
        if (
            data.get("native_name_error")
            or provider_name != opencode_desired_name
        ):
            store.set_meta(name_error_key, opencode_desired_name)
        else:
            store.delete_meta(name_error_key)
    if (
        unread
        and newly_actionable
        and not (pane and pane.attached)
        and status
        in {
            Status.NEEDS_YOU.value,
            Status.READY.value,
            Status.ERROR.value,
        }
    ):
        config = load_config()
        if config.get("alerts") == "tmux":
            label = f"{terminal_text(session.display_name)} ({provider_label(provider)})"
            reason = {
                "permission": "permission requested",
                "question": "question waiting",
                "completed": "completed",
                "failed": "failed",
                "exited": "exited",
            }.get(attention_reason, status)
            try:
                tmux.display_alert(f"Pika: {label} — {reason}")
            except (OSError, TmuxError):
                pass
    if (
        provider == "claude"
        and data.get("hook_event_name") == "SessionStart"
        and desired_name
        and not provider_name
    ):
        return {
            "hookSpecificOutput": {
                "hookEventName": "SessionStart",
                "sessionTitle": desired_name,
            }
        }
    return None


def hook_stdout(provider: str, result: dict[str, Any] | None) -> str:
    if result is not None:
        return json.dumps(result)
    # Codex Stop/UserPromptSubmit/SessionStart require JSON when output is
    # present. An empty object is a valid no-decision response for every event.
    return "{}" if provider == "codex" else ""


def handle_process_exit(
    provider: str,
    code: int,
    *,
    session_id: str | None = None,
    launch_token: str | None = None,
    owner_token: str | None = None,
    store: Store | None = None,
) -> None:
    store = store or Store()
    target: Session | None = None
    # OpenCode can switch roots without replacing its process. The wrapper's
    # launch-time --session-id is then stale, while the atomically moved launch
    # binding names the root this exact client owned when it exited.
    if launch_token:
        binding = store.get_launch_binding(launch_token)
        if binding and binding[0] == provider:
            target = store.get_session(*binding)
    if target is None and session_id:
        target = store.get_session(provider, session_id)
    if target is not None and store.is_untracked(*target.key):
        if launch_token:
            store.delete_launch_binding(launch_token)
        return
    if target is None and launch_token:
        pending = store.get_pending(launch_token)
        if pending:
            # A hook never supplied an exact UUID, so keep the pending record
            # for doctor to report rather than inventing an identity.
            return
    if target is None:
        return
    # The wrapper is stronger evidence than an expiring hook: this exact Pika
    # client has returned. Revoke only its claim so a genuine app/second-client
    # claim remains fail-closed. Wrappers from releases before owner tokens
    # clear the legacy undifferentiated claim as a one-time compatibility path.
    store.delete_live_owner(
        provider,
        target.session_id,
        owner_token=owner_token,
    )
    store.delete_recovery_owner(provider, target.session_id)
    clean_exit = code in {0, 130}
    now = time.time()
    if not clean_exit:
        exit_error = f"{provider} exited with status {code}"
        store.record_status_observation(
            *target.key,
            kind="runtime",
            status=Status.ERROR.value,
            unread=True,
            attention_reason="exited",
            error=exit_error,
            observed_at=now,
            source="process-exit",
        )
        updates = {
            "status": Status.ERROR.value,
            "unread": True,
            "root_pid": None,
            "error": exit_error,
            "attention_reason": "exited",
            "last_event_at": now,
        }
    elif target.unread and target.status in {
        Status.READY.value,
        Status.NEEDS_YOU.value,
        Status.ERROR.value,
        Status.OPEN_TWICE.value,
    }:
        updates = {"root_pid": None}
    else:
        store.clear_status_observation(*target.key, "runtime")
        store.record_status_observation(
            *target.key,
            kind="lifecycle",
            status=Status.PARKED.value,
            unread=False,
            attention_reason=None,
            error=None,
            observed_at=now,
            source="process-exit",
        )
        updates = {
            "status": Status.PARKED.value,
            "root_pid": None,
            "error": None,
            "attention_reason": None,
            "last_event_at": now,
        }
    store.update_session(provider, target.session_id, **updates)
    if launch_token:
        store.delete_launch_binding(launch_token)
