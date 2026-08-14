from __future__ import annotations

import json
import os
import time
from typing import Any

from .models import Session, Status
from .processes import provider_ancestor, provider_process
from .providers import CodexProvider, codex_worker_originator
from .setup_hooks import hook_spec_fingerprint
from .store import Store, load_config
from .tmux import Tmux, TmuxError
from .ui import terminal_text


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
    session_id = str(data.get("session_id") or "")
    if not session_id:
        return None
    store.set_meta(f"hook_seen:{provider}", hook_spec_fingerprint(provider))
    if provider == "codex" and codex_worker_originator(
        session_id,
        data.get("transcript_path"),
        originator=data.get("originator"),
    ):
        # App-server automation workers are subordinate execution units, not
        # conversations.  Remove any record created by an earlier hook and
        # return before ownership, attention, or tmux identity can be changed.
        _repair_worker_pane_claim(provider, session_id, store, tmux)
        store.delete_session(provider, session_id)
        return None
    if store.is_untracked(provider, session_id):
        store.delete_live_owner(provider, session_id)
        return None
    owner_pid = provider_ancestor(os.getppid(), provider)
    if data.get("hook_event_name") == "SessionEnd":
        if owner_pid:
            store.delete_live_owner(provider, session_id, pid=owner_pid)
    elif owner_pid:
        store.set_live_owner(provider, session_id, owner_pid)
    pane_id = os.environ.get("TMUX_PANE")
    launch_token = os.environ.get("PIKA_LAUNCH_TOKEN")
    pending = store.get_pending(launch_token) if launch_token else None
    if pending is None and pane_id:
        pending = store.find_pending_for_pane(pane_id)
        if pending:
            launch_token = str(pending["launch_token"])
    existing = store.get_session(provider, session_id)
    placeholder = store.get_session(provider, f"unbound:{pane_id}") if pane_id else None
    pane = tmux.get_pane(pane_id) if pane_id else None
    provider_name = data.get("session_title")
    desired_name = os.environ.get("PIKA_NAME")
    name = (
        str(provider_name)
        if provider_name
        else (str(pending["name"]) if pending else None)
        or (existing.name if existing else None)
        or (placeholder.name if placeholder else None)
        or desired_name
        or (pane.pika_name if pane else None)
    )
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
    newly_actionable = not (
        existing
        and existing.unread
        and existing.status == status
        and existing.attention_reason == attention_reason
        and existing.error == error
    )
    now = time.time()
    session = Session(
        provider=provider,
        session_id=session_id,
        name=name,
        cwd=data.get("cwd")
        or (existing.cwd if existing else None)
        or (placeholder.cwd if placeholder else None)
        or (pending["cwd"] if pending else None),
        branch=existing.branch if existing else None,
        transcript_path=data.get("transcript_path")
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
        if pending or placeholder or (existing and existing.managed)
        else "external",
        managed=bool(pending or placeholder or (existing and existing.managed)),
        error=error,
        attention_reason=attention_reason,
        created_at=existing.created_at if existing else now,
        updated_at=now,
        last_event_at=(
            existing.last_event_at
            if data.get("hook_event_name") == "SessionEnd"
            and existing
            and existing.unread
            else now
        ),
        last_activity_at=now,
    )
    store.upsert_session(session)
    if store.is_untracked(provider, session_id):
        return None
    if placeholder and placeholder.session_id != session_id:
        store.delete_session(provider, placeholder.session_id)
    if pane:
        try:
            tmux.tag_pane(
                pane.pane_id,
                provider=provider,
                session_id=session_id,
                name=session.display_name,
                launch_token="",
            )
        except (OSError, TmuxError):
            pass
    if launch_token:
        store.bind_launch(launch_token, provider, session_id)
        store.delete_pending(launch_token)
        attach_key = f"attached_launch:{launch_token}"
        if store.get_meta(attach_key):
            store.record_attach(provider, session_id)
            store.delete_meta(attach_key)
    name_error_key = f"native_name_error:codex:{session_id}"
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
            named = CodexProvider().set_native_name(session_id, desired_name)
        except (OSError, RuntimeError):
            named = False
        if named:
            store.delete_meta(name_error_key)
        else:
            store.set_meta(name_error_key, desired_name)
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
            label = f"{terminal_text(session.display_name)} ({provider.title()})"
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
    store: Store | None = None,
) -> None:
    store = store or Store()
    if session_id and store.is_untracked(provider, session_id):
        if launch_token:
            store.delete_launch_binding(launch_token)
        return
    target: Session | None = None
    if session_id:
        target = store.get_session(provider, session_id)
    if target is None and launch_token:
        binding = store.get_launch_binding(launch_token)
        if binding and binding[0] == provider:
            target = store.get_session(*binding)
    if target is None and launch_token:
        pending = store.get_pending(launch_token)
        if pending:
            # A hook never supplied an exact UUID, so keep the pending record
            # for doctor to report rather than inventing an identity.
            return
    if target is None:
        return
    if code != 0:
        updates = {
            "status": Status.ERROR.value,
            "unread": True,
            "root_pid": None,
            "error": f"{provider} exited with status {code}",
            "attention_reason": "exited",
        }
    elif target.unread and target.status in {
        Status.READY.value,
        Status.NEEDS_YOU.value,
        Status.ERROR.value,
        Status.OPEN_TWICE.value,
    }:
        updates = {"root_pid": None}
    else:
        updates = {
            "status": Status.PARKED.value,
            "root_pid": None,
            "error": None,
            "attention_reason": None,
        }
    store.update_session(provider, target.session_id, **updates)
    if launch_token:
        store.delete_launch_binding(launch_token)
