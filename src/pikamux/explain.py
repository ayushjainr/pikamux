"""Transcript-free explanations from the same facts that project display state."""
from __future__ import annotations

import shlex
import time
from dataclasses import asdict
from typing import Iterable

from .models import FleetSession, Session, Status
from .status_projection import StatusObservation, project_status


_RULES = {
    "safety_precedence": "Unresolved identity evidence takes precedence over activity.",
    "runtime_failure_without_live_process": "A runtime failure remains unresolved and no live owner was found.",
    "working_process_gone": "The last turn started, but its process is no longer live.",
    "completed_and_collected_process_gone": "The completed result was collected and no process is live.",
    "unbound_without_lifecycle": "A live process has no verified Pika home or newer lifecycle evidence.",
    "live_without_turn_evidence": "A process is live; this alone does not establish an active turn.",
    "legacy_observation": "The saved state has no separate lifecycle evidence yet.",
    "no_live_process": "No live process or pending lifecycle event was found.",
}


def explain_session(
    session: Session | FleetSession,
    observations: Iterable[StatusObservation] = (),
    *,
    now: float | None = None,
) -> dict[str, object]:
    now = time.time() if now is None else now
    remote = isinstance(session, FleetSession)
    facts = tuple(observations) if not remote else ()
    projection = project_status(
        facts, live=session.live, home_state=session.home_state,
        fallback_status=session.status, fallback_unread=session.unread,
        fallback_reason=session.attention_reason, fallback_error=session.error,
        fallback_at=session.last_event_at,
    )
    # A cached remote label is a node's observation, not a locally proven state.
    rule = projection.rule if facts else "remote_snapshot" if remote else "saved_display"
    reason = projection.attention_reason if facts else session.attention_reason
    state = projection.status if facts else session.status
    if rule in {"remote_snapshot", "saved_display"}:
        summary = "Last reported state; underlying observations are not available here."
    elif projection.source == "legacy":
        summary = "Saved state carried forward during migration; the original provider event is unavailable."
    elif rule != "lifecycle":
        summary = _RULES.get(rule, "Derived from the recorded observations.")
    else:
        summary = {
            Status.NEEDS_YOU.value: "The provider requested " + ("permission." if reason == "permission" else "your input."),
            Status.WORKING.value: "The provider reported an active turn and its process is live.",
            Status.READY.value: "The provider reported completion; a live process can remain open afterward.",
            Status.ERROR.value: "The provider reported a failure requiring inspection.",
            Status.OPEN_TWICE.value: "Multiple owners were reported for the same conversation.",
            Status.PARKED.value: "The provider reported that this conversation is idle.",
        }.get(state, "The latest provider lifecycle observation determines this state.")
    evidence = []
    for fact in sorted(facts, key=lambda value: (value.kind, value.observed_at)):
        evidence.append({
            **asdict(fact),
            "age_seconds": max(0.0, now - fact.observed_at),
            "winner": fact.kind == projection.kind and fact.source == projection.source
            and fact.observed_at == projection.observed_at,
        })
    command = shlex.join(["pika", session.session_id + ("@" + session.node_name if remote else "")])
    action = command if state in {Status.NEEDS_YOU.value, Status.READY.value} else None
    if state in {Status.ERROR.value, Status.OPEN_TWICE.value, Status.UNBOUND.value}:
        action = session.error or f"Run {command} for the exact recovery steps."
    observed_at = session.seen_at if remote else projection.observed_at if facts else session.last_event_at
    freshness = {
        "basis": "remote_snapshot" if remote else "local_observations" if facts else "saved_display",
        "observed_at": observed_at or None,
        "age_seconds": max(0.0, now - observed_at) if observed_at else None,
        "stale": bool(remote and session.stale),
        "error": session.remote_error if remote else None,
    }
    if remote and session.stale:
        summary = "Remote state is stale; it cannot establish whether this agent needs you now."
        action = shlex.join(["pika", "sync", session.node_name])
    return {
        "schema_version": 1,
        "provider": session.provider, "session_id": session.session_id,
        "active_thread_id": session.provider_thread_id,
        "name": session.display_name,
        "node_id": session.node_id if remote else None,
        "state": state, "displayed_state": session.status,
        "summary": summary, "rule": rule,
        "reason": reason, "unread": projection.unread if facts else session.unread,
        "live": session.live, "home_state": session.home_state,
        "evidence": evidence, "freshness": freshness,
        "next_action": action,
    }
