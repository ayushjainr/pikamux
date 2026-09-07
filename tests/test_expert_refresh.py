from __future__ import annotations

import tempfile
import unittest
from pathlib import Path
from dataclasses import replace
from unittest.mock import patch

from pikamux.core import Pika, PikaError
from pikamux.consult import DEFAULT_CODEX_EFFORT, DEFAULT_CODEX_MODEL
from pikamux.experts import card_state, profile_freshness
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

    def test_unwatched_experts_remain_searchable_and_resolve_without_rewatch(self):
        self.store.put_expert_profile(self._profile())
        self.store.untrack_session(*self.session.key)
        with (
            patch.object(self.pika, "refresh", side_effect=AssertionError("lookup must stay metadata-only")),
            patch("pikamux.core.interview_profile", side_effect=AssertionError("lookup must not interview")),
        ):
            matches = self.pika.expert_matches("session identity")
            self.assertEqual(len(matches), 1)
            self.assertFalse(matches[0].to_dict()["watched"])
            self.assertEqual(matches[0].to_dict()["availability"], "source-available")
            target = self.pika.resolve_expert_target("identity")
            self.assertEqual(target.key, self.session.key)
            self.assertFalse(self.pika.expert_card_states()[0].watched)
        self.assertTrue(self.store.is_untracked(*self.session.key))
        self.assertEqual(self.store.list_sessions(), [])

    def test_identical_interview_revalidates_without_republishing_or_repeat_spend(self):
        original = self.store.put_expert_profile(replace(self._profile(), updated_at=100))
        self.transcript.write_text("history\nmore work, unchanged mandate and objective\n")
        self.assertEqual(card_state(self.session, original).status, "STALE")
        with patch("pikamux.core.interview_profile", return_value=self._profile()) as interview:
            refreshed = self.pika.refresh_expert(self.session)
            self.assertEqual(card_state(self.session, refreshed).status, "CURRENT")
            with patch("pikamux.core.read_provider_quota", side_effect=AssertionError("synced card needs no quota check")):
                next_cycle = self.pika.refresh_due_experts(now=5000)
        interview.assert_called_once()
        self.assertEqual(next_cycle[0].status, "CURRENT")
        self.assertEqual(refreshed.updated_at, original.updated_at)
        self.assertEqual(refreshed.scope_updated_at, original.scope_updated_at)
        self.assertEqual(refreshed.current_state_updated_at, original.current_state_updated_at)
        self.assertEqual(profile_freshness(self.session, refreshed)["current_state_status"], "CURRENT")
        self.assertEqual(self.store.get_expert_profile(*self.session.key), refreshed)

    def test_changed_interview_scope_also_revalidates_unchanged_current_work(self):
        original = self.store.put_expert_profile(replace(self._profile(), updated_at=100))
        self.transcript.write_text("history\nverified broader ownership, same current objective\n")
        interview_card = replace(self._profile(), summary="Owns exact identity and multi-machine recovery.", updated_at=200)
        with patch("pikamux.core.interview_profile", return_value=interview_card):
            refreshed = self.pika.refresh_expert(self.session)
        self.assertEqual(refreshed.scope_updated_at, 200)
        self.assertEqual(refreshed.current_state_updated_at, original.current_state_updated_at)
        self.assertEqual(card_state(self.session, refreshed).status, "CURRENT")
        self.assertEqual(profile_freshness(self.session, refreshed)["current_state_status"], "CURRENT")

    def test_identical_self_publication_cannot_revalidate_interviewed_content(self):
        original = self.store.put_expert_profile(replace(self._profile(), updated_at=100))
        self.transcript.write_text("history\nnew work\n")
        repeated = replace(self._profile(), source="self", updated_at=200)
        self.assertEqual(self.store.put_expert_profile(repeated), original)
        self.assertEqual(card_state(self.session, original).status, "STALE")

    def test_unavailable_expert_stays_discoverable_but_resolution_does_not_restore(self):
        self.store.put_expert_profile(self._profile())
        self.store.untrack_session(*self.session.key)
        self.transcript.unlink()
        match = self.pika.expert_matches("identity")[0]
        self.assertEqual(match.to_dict()["availability"], "source-unavailable")
        with self.assertRaisesRegex(PikaError, "No question was sent"):
            self.pika.resolve_expert_target(self.session.session_id)
        self.assertTrue(self.store.is_untracked(*self.session.key))

    def test_archived_expert_is_excluded_and_never_resurrected(self):
        self.store.put_expert_profile(self._profile())
        self.store.untrack_session(*self.session.key)
        with patch.object(
            self.pika.providers["codex"], "hidden_session_ids",
            return_value={self.session.session_id}, create=True,
        ):
            self.assertEqual(self.pika.expert_matches(), [])
            self.assertEqual(self.pika.expert_card_states(), [])
            with self.assertRaisesRegex(PikaError, "archived"):
                self.pika.resolve_expert_target("identity")
        self.assertTrue(self.store.is_untracked(*self.session.key))

    def test_ambiguous_unwatched_name_cannot_silently_choose_a_provider(self):
        self.store.untrack_session(*self.session.key)
        other = Session("claude", "second-uuid", name="identity", transcript_path=str(self.transcript))
        self.store.upsert_session(other)
        self.store.untrack_session(*other.key)
        with patch("pikamux.ui.sys.stdin.isatty", return_value=False):
            with self.assertRaisesRegex(ValueError, "Multiple continuations"):
                self.pika.resolve_expert_target("identity")
        self.assertEqual(len(self.store.list_untracked_sessions()), 2)

    def test_current_work_command_uses_exact_calling_identity_and_no_model(self):
        from pikamux.cli import _expert, _parser

        original = self.store.put_expert_profile(self._profile())
        args = _parser().parse_args(["expert", "update", "--now", "Awaiting review.", "--json"])
        with (
            patch.object(self.pika, "current_exact_session", return_value=self.session) as exact,
            patch("pikamux.core.interview_profile", side_effect=AssertionError("no interview")),
            patch("builtins.print") as output,
        ):
            self.assertEqual(_expert(self.pika, args), 0)
        exact.assert_called_once()
        self.assertIn("Awaiting review.", output.call_args.args[0])
        changed = self.store.get_expert_profile(*self.session.key)
        self.assertEqual(changed.scope_updated_at, original.scope_updated_at)
        self.assertEqual(changed.current_state, "Awaiting review.")

    def test_current_work_cannot_bypass_exact_identity(self):
        original = self.store.put_expert_profile(self._profile())
        with patch.object(self.pika, "current_exact_session", side_effect=PikaError("cannot prove identity")):
            with self.assertRaisesRegex(PikaError, "cannot prove"):
                self.pika.publish_current_work("Counterfeit update.")
        self.assertEqual(self.store.get_expert_profile(*self.session.key), original)

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
