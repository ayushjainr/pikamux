from __future__ import annotations

import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from pikamux.core import Pika
from pikamux.consult import DEFAULT_CODEX_EFFORT, DEFAULT_CODEX_MODEL
from pikamux.models import ExpertProfile, Session
from pikamux.quota import QuotaSnapshot
from pikamux.store import Store


class EmptyTmux:
    def list_panes(self):
        return []


class EmptyProvider:
    name = "codex"

    def discover(self):
        return []

    def usage(self, _session, _store):
        return None


class ExpertRefreshTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        root = Path(self.temporary.name)
        self.transcript = root / "thread.jsonl"
        self.transcript.write_text("history\n")
        self.store = Store(root / "pika.db")
        self.session = Session(
            "codex",
            "exact-uuid",
            name="identity",
            transcript_path=str(self.transcript),
            last_activity_at=100,
        )
        self.store.upsert_session(self.session)
        self.pika = Pika(
            self.store, EmptyTmux(), {"codex": EmptyProvider()}
        )

    def _profile(self) -> ExpertProfile:
        stat = self.transcript.stat()
        return ExpertProfile(
            "codex",
            "exact-uuid",
            "Owns exact identity recovery.",
            ("session identity",),
            (),
            source="interview",
            transcript_mtime_ns=stat.st_mtime_ns,
            transcript_size=stat.st_size,
            current_state="Exact recovery is verified and currently stable.",
        )

    def test_due_refresh_uses_expiring_quota_once(self) -> None:
        now = 1_000.0
        quota = QuotaSnapshot("codex", 80, 2_000, now, "test")
        with (
            patch("pikamux.core.read_provider_quota", return_value=quota),
            patch(
                "pikamux.core.interview_profile", return_value=self._profile()
            ) as ask,
        ):
            first = self.pika.refresh_due_experts(now=now)
            second = self.pika.refresh_due_experts(now=now)
        self.assertEqual(first[0].status, "REFRESHED")
        self.assertEqual(first[0].consultation_mode, "default")
        self.assertEqual(first[0].model, DEFAULT_CODEX_MODEL)
        self.assertEqual(first[0].effort, DEFAULT_CODEX_EFFORT)
        self.assertEqual(second[0].status, "CURRENT")
        ask.assert_called_once()

    def test_failed_card_is_not_retried_in_the_same_cycle(self) -> None:
        now = 1_000.0
        quota = QuotaSnapshot("codex", 80, 2_000, now, "test")
        with (
            patch("pikamux.core.read_provider_quota", return_value=quota),
            patch(
                "pikamux.core.interview_profile", side_effect=RuntimeError("boom")
            ) as ask,
        ):
            first = self.pika.refresh_due_experts(now=now)
            second = self.pika.refresh_due_experts(now=now)
        self.assertEqual(first[0].status, "FAILED")
        self.assertIsNone(first[0].model)
        self.assertIsNone(first[0].effort)
        self.assertEqual(second[0].status, "DEFERRED")
        ask.assert_called_once()

    def test_reserve_and_missing_telemetry_make_no_call(self) -> None:
        now = 1_000.0
        with patch("pikamux.core.interview_profile") as ask:
            with patch(
                "pikamux.core.read_provider_quota",
                return_value=QuotaSnapshot("codex", 90, 2_000, now, "test"),
            ):
                reserved = self.pika.refresh_due_experts(now=now)
            with patch("pikamux.core.read_provider_quota", return_value=None):
                unknown = self.pika.refresh_due_experts(now=now)
        self.assertEqual(reserved[0].status, "DEFERRED")
        self.assertEqual(unknown[0].status, "UNKNOWN")
        ask.assert_not_called()

    def test_outside_final_six_hours_waits(self) -> None:
        now = 1_000.0
        quota = QuotaSnapshot("codex", 1, int(now + 7 * 60 * 60), now, "test")
        with (
            patch("pikamux.core.read_provider_quota", return_value=quota),
            patch("pikamux.core.interview_profile") as ask,
        ):
            result = self.pika.refresh_due_experts(now=now)
        self.assertEqual(result[0].status, "WAITING")
        ask.assert_not_called()


if __name__ == "__main__":
    unittest.main()
