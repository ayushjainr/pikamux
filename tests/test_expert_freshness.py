from __future__ import annotations

import tempfile
import unittest
from dataclasses import replace
from pathlib import Path
from unittest.mock import patch

from pikamux.experts import expert_availability, profile_freshness, rank_experts
from pikamux.models import ExpertProfile, FleetSession, Session
from pikamux.store import Store


class ExpertFreshnessTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.transcript = self.root / "session.jsonl"
        self.transcript.write_text("history\n")
        self.store = Store(self.root / "pika.db")
        self.session = Session("codex", "one", name="research", transcript_path=str(self.transcript))
        self.store.upsert_session(self.session)
        stat = self.transcript.stat()
        self.profile = self.store.put_expert_profile(ExpertProfile(
            "codex", "one", "Owns the return methodology.", ("return methodology",),
            updated_at=100, current_state="Validating returns.",
            transcript_mtime_ns=stat.st_mtime_ns, transcript_size=stat.st_size,
        ))

    def test_small_work_update_preserves_durable_scope_and_deduplicates(self):
        self.transcript.write_text("history\ncheckpoint\n")
        stat = self.transcript.stat()
        with patch("pikamux.store.time.time", return_value=200):
            updated = self.store.put_expert_current_state(
                "codex", "one", "  Awaiting  the methodology decision. ",
                transcript_mtime_ns=stat.st_mtime_ns, transcript_size=stat.st_size,
            )
        self.assertEqual(updated.scope, self.profile.scope)
        self.assertEqual(updated.topics, self.profile.topics)
        self.assertEqual(updated.scope_updated_at, 100)
        self.assertEqual(updated.current_state_updated_at, 200)
        freshness = profile_freshness(self.session, updated, now=250)
        self.assertEqual(freshness["scope_age_seconds"], 150)
        self.assertEqual(freshness["current_state_age_seconds"], 50)
        self.assertEqual(freshness["current_state_status"], "CURRENT")
        with patch("pikamux.store.time.time", return_value=300):
            duplicate = self.store.put_expert_current_state(
                "codex", "one", "Awaiting the methodology decision.",
                transcript_mtime_ns=999, transcript_size=999,
            )
        self.assertEqual(duplicate, updated)
        self.assertEqual(self.store.get_expert_profile("codex", "one"), updated)
        self.assertEqual(self.store.list_activity_events(), [])

    def test_transcript_growth_changes_work_freshness_not_expertise_age(self):
        self.transcript.write_text("history\nmore work\n")
        state = profile_freshness(self.session, self.profile, now=200)
        self.assertEqual(state["scope_status"], "PUBLISHED")
        self.assertEqual(state["scope_updated_at"], 100)
        self.assertEqual(state["current_state_status"], "STALE")

    def test_scope_edit_does_not_make_old_work_fresh(self):
        self.transcript.write_text("a new stage\n")
        stat = self.transcript.stat()
        changed = self.store.put_expert_profile(replace(
            self.profile, summary="Owns return methodology and portfolio construction.",
            updated_at=300, transcript_mtime_ns=stat.st_mtime_ns, transcript_size=stat.st_size,
        ))
        self.assertEqual(changed.scope_updated_at, 300)
        self.assertEqual(changed.current_state_updated_at, 100)
        self.assertEqual(profile_freshness(self.session, changed)["current_state_status"], "STALE")

    def test_identical_full_publication_does_not_refresh_age(self):
        unchanged = self.store.put_expert_profile(replace(self.profile, updated_at=800))
        self.assertEqual(unchanged, self.profile)

    def test_legacy_profile_migration_keeps_original_publication_evidence(self):
        with self.store.connect() as db:
            for column in (
                "scope_updated_at", "current_state_updated_at",
                "current_state_mtime_ns", "current_state_size",
            ):
                db.execute(f"ALTER TABLE expert_profiles DROP COLUMN {column}")
        migrated = Store(self.store.path).get_expert_profile("codex", "one")
        self.assertEqual(migrated.scope_updated_at, 100)
        self.assertEqual(migrated.current_state_updated_at, 100)
        self.assertEqual(migrated.current_state_mtime_ns, self.profile.transcript_mtime_ns)
        self.assertEqual(profile_freshness(self.session, migrated)["current_state_status"], "CURRENT")

    def test_unwatch_keeps_expert_search_without_restoring_tracking(self):
        self.store.untrack_session("codex", "one")
        with patch("pikamux.experts.consultation_for", side_effect=AssertionError("no interviews")):
            matches = rank_experts(
                self.store.list_expert_profiles(),
                self.store.list_untracked_sessions(),
                "return methodology",
                untracked_keys=self.store.untracked_session_keys(),
            )
            payload = matches[0].to_dict()
        self.assertFalse(payload["watched"])
        self.assertTrue(payload["discoverable"])
        self.assertEqual(payload["availability"], "source-available")
        self.assertEqual(self.store.list_sessions(), [])
        self.transcript.unlink()
        self.assertEqual(matches[0].to_dict()["availability"], "source-unavailable")

    def test_archived_expertise_is_discoverable_but_not_available(self):
        archived = replace(self.session, transcript_path=str(self.root / "archived_sessions" / "one.jsonl"))
        match = rank_experts([self.profile], [archived])[0].to_dict()
        self.assertTrue(match["discoverable"])
        self.assertEqual(match["availability"], "archived")

    def test_unavailable_remote_never_uses_a_local_transcript(self):
        remote = FleetSession(session=self.session, node_id="remote", node_name="other", stale=True)
        with patch("pikamux.experts.transcript_fingerprint", side_effect=AssertionError("remote source is not local")):
            self.assertEqual(expert_availability(remote), "machine-unreachable")
            self.assertEqual(profile_freshness(remote, self.profile)["current_state_status"], "UNKNOWN")

    def test_remote_current_work_carries_authoritative_freshness(self):
        remote = FleetSession(
            session=self.session, node_id="remote", node_name="other",
            scope_updated_at=50, current_state_updated_at=150,
            current_state_status="STALE", availability="source-unavailable",
        )
        with patch("pikamux.experts.transcript_fingerprint", side_effect=AssertionError("remote source is not local")):
            self.assertEqual(expert_availability(remote), "source-unavailable")
            fields = profile_freshness(remote, self.profile, now=200)
        self.assertEqual(fields["scope_age_seconds"], 150)
        self.assertEqual(fields["current_state_age_seconds"], 50)
        self.assertEqual(fields["current_state_status"], "STALE")

    def test_work_publication_requires_profile_and_rejects_large_updates(self):
        with self.assertRaisesRegex(ValueError, "before a current-work"):
            self.store.put_expert_current_state("codex", "missing", "Work.")
        with self.assertRaisesRegex(ValueError, "600 characters"):
            self.store.put_expert_current_state("codex", "one", "x" * 601)
        self.assertEqual(self.store.get_expert_profile("codex", "one"), self.profile)


if __name__ == "__main__":
    unittest.main()
