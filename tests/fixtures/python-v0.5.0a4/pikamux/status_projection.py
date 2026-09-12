from __future__ import annotations

from dataclasses import dataclass
from typing import Iterable

from .models import Status


@dataclass(frozen=True, slots=True)
class StatusObservation:
    """One durable fact used to derive a conversation's public state."""

    kind: str
    status: str
    unread: bool
    attention_reason: str | None
    error: str | None
    observed_at: float
    source: str


@dataclass(frozen=True, slots=True)
class StatusProjection:
    """The display state derived from durable facts and current liveness."""

    status: str
    unread: bool
    attention_reason: str | None
    error: str | None
    observed_at: float
    source: str
    kind: str = "fallback"
    rule: str = "fallback"


def _newest(
    observations: Iterable[StatusObservation], kind: str
) -> StatusObservation | None:
    matches = [item for item in observations if item.kind == kind]
    return max(matches, key=lambda item: item.observed_at) if matches else None


def _projection(
    fact: StatusObservation,
    *,
    status: str | None = None,
    unread: bool | None = None,
    attention_reason: str | None = None,
    error: str | None = None,
    rule: str = "lifecycle",
) -> StatusProjection:
    return StatusProjection(
        status=status or fact.status,
        unread=fact.unread if unread is None else unread,
        attention_reason=(
            fact.attention_reason
            if attention_reason is None and status is None
            else attention_reason
        ),
        error=fact.error if error is None and status is None else error,
        observed_at=fact.observed_at,
        source=fact.source,
        kind=fact.kind,
        rule=rule,
    )


def project_status(
    observations: Iterable[StatusObservation],
    *,
    live: bool,
    home_state: str,
    fallback_status: str = Status.PARKED.value,
    fallback_unread: bool = False,
    fallback_reason: str | None = None,
    fallback_error: str | None = None,
    fallback_at: float = 0.0,
) -> StatusProjection:
    """Derive display state; stored labels are observations, never final truth.

    Safety evidence is fail-closed until reconciliation explicitly clears it.
    Runtime failure matters only while the exact agent is not live. Provider
    lifecycle then wins over ownership so an external agent asking a question
    is still visible as NEEDS YOU rather than being buried under UNBOUND.
    """

    facts = tuple(observations)
    safety = _newest(facts, "safety")
    if safety and safety.status in {
        Status.OPEN_TWICE.value,
        Status.ERROR.value,
    }:
        return _projection(safety, rule="safety_precedence")

    runtime = _newest(facts, "runtime")
    if (
        runtime
        and not live
        and runtime.status in {Status.ERROR.value, Status.OPEN_TWICE.value}
    ):
        return _projection(runtime, rule="runtime_failure_without_live_process")

    lifecycle = _newest(facts, "lifecycle")
    if lifecycle:
        if lifecycle.status in {
            Status.NEEDS_YOU.value,
            Status.ERROR.value,
            Status.OPEN_TWICE.value,
        }:
            return _projection(lifecycle)
        if lifecycle.status == Status.WORKING.value:
            if live:
                return _projection(lifecycle)
            return _projection(
                lifecycle,
                status=Status.PARKED.value,
                unread=False,
                attention_reason=None,
                error=None,
                rule="working_process_gone",
            )
        if lifecycle.status == Status.READY.value:
            if live or lifecycle.unread:
                return _projection(lifecycle)
            return _projection(
                lifecycle,
                status=Status.PARKED.value,
                unread=False,
                attention_reason=None,
                error=None,
                rule="completed_and_collected_process_gone",
            )
        if lifecycle.status == Status.PARKED.value:
            return _projection(lifecycle, unread=False)

    if home_state == "unbound":
        return StatusProjection(
            Status.UNBOUND.value,
            False,
            None,
            None,
            fallback_at,
            "ownership",
            "ownership",
            "unbound_without_lifecycle",
        )
    if live:
        return StatusProjection(
            Status.READY.value,
            False,
            None,
            None,
            fallback_at,
            "process",
            "runtime",
            "live_without_turn_evidence",
        )
    if fallback_status in {
        Status.NEEDS_YOU.value,
        Status.READY.value,
        Status.ERROR.value,
        Status.OPEN_TWICE.value,
    }:
        return StatusProjection(
            fallback_status,
            fallback_unread,
            fallback_reason,
            fallback_error,
            fallback_at,
            "legacy",
            "legacy",
            "legacy_observation",
        )
    return StatusProjection(
        Status.PARKED.value,
        False,
        None,
        None,
        fallback_at,
        "process",
        "runtime",
        "no_live_process",
    )
