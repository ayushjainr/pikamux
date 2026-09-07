from __future__ import annotations

import unittest

from pikamux.models import Status
from pikamux.status_projection import StatusObservation, project_status


def fact(
    kind: str,
    status: str,
    *,
    at: float,
    unread: bool = False,
    reason: str | None = None,
    error: str | None = None,
) -> StatusObservation:
    return StatusObservation(kind, status, unread, reason, error, at, "test")


class StatusProjectionTests(unittest.TestCase):
    def test_safety_fact_remains_fail_closed_over_newer_lifecycle(self) -> None:
        projected = project_status(
            [
                fact(
                    "safety",
                    Status.OPEN_TWICE.value,
                    at=10,
                    unread=True,
                    reason="identity",
                    error="two exact roots",
                ),
                fact("lifecycle", Status.WORKING.value, at=20),
            ],
            live=True,
            home_state="exact-live",
        )
        self.assertEqual(projected.status, Status.OPEN_TWICE.value)
        self.assertEqual(projected.source, "test")

    def test_dead_process_cannot_remain_working(self) -> None:
        projected = project_status(
            [fact("lifecycle", Status.WORKING.value, at=20)],
            live=False,
            home_state="saved-idle",
        )
        self.assertEqual(projected.status, Status.PARKED.value)
        self.assertFalse(projected.unread)

    def test_needs_you_is_visible_even_when_home_is_unbound(self) -> None:
        projected = project_status(
            [
                fact(
                    "lifecycle",
                    Status.NEEDS_YOU.value,
                    at=20,
                    unread=True,
                    reason="question",
                )
            ],
            live=True,
            home_state="unbound",
        )
        self.assertEqual(projected.status, Status.NEEDS_YOU.value)

    def test_live_exact_process_clears_runtime_failure_in_projection(self) -> None:
        projected = project_status(
            [
                fact(
                    "runtime",
                    Status.ERROR.value,
                    at=10,
                    unread=True,
                    reason="exited",
                ),
                fact("lifecycle", Status.WORKING.value, at=9),
            ],
            live=True,
            home_state="exact-live",
        )
        self.assertEqual(projected.status, Status.WORKING.value)

    def test_unbound_is_derived_from_ownership_without_lifecycle(self) -> None:
        projected = project_status([], live=True, home_state="unbound")
        self.assertEqual(projected.status, Status.UNBOUND.value)

    def test_read_completed_conversation_without_process_is_parked(self) -> None:
        projected = project_status(
            [fact("lifecycle", Status.READY.value, at=20, unread=False)],
            live=False,
            home_state="no-live-home",
        )
        self.assertEqual(projected.status, Status.PARKED.value)


if __name__ == "__main__":
    unittest.main()
