from __future__ import annotations

import sqlite3
import tempfile
import threading
import time
import unittest
from pathlib import Path
from unittest.mock import patch

from pikamux.models import ExpertProfile, Session, Status, Usage
from pikamux.store import Store


class StoreTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.db_path = Path(self.temp.name) / "state" / "pika.db"
        self.store = Store(self.db_path)

    def tearDown(self) -> None:
        self.temp.cleanup()

    def test_round_trip_and_owner_only_permissions(self) -> None:
        now = time.time()
        session = Session(
            provider="codex",
            session_id="abc",
            name="build-auth",
            cwd="/tmp",
            status=Status.READY.value,
            unread=True,
            attention_reason="completed",
            created_at=now,
            last_event_at=now,
            last_activity_at=now,
        )
        self.store.upsert_session(session)
        loaded = self.store.get_session("codex", "abc")
        self.assertIsNotNone(loaded)
        assert loaded is not None
        self.assertEqual(loaded.name, "build-auth")
        self.assertTrue(loaded.unread)
        self.assertEqual(loaded.attention_reason, "completed")
        self.assertEqual(self.db_path.stat().st_mode & 0o777, 0o600)
        self.assertEqual(self.db_path.parent.stat().st_mode & 0o777, 0o700)

    def test_concurrent_fresh_store_initialization_is_serialized(self) -> None:
        barrier = threading.Barrier(8)
        errors: list[BaseException] = []

        def initialize() -> None:
            try:
                barrier.wait()
                Store(self.db_path).initialize()
            except BaseException as exc:
                errors.append(exc)

        threads = [threading.Thread(target=initialize) for _ in range(8)]
        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join(10)
        self.assertFalse(any(thread.is_alive() for thread in threads))
        self.assertEqual(errors, [])
        self.assertEqual(
            Store(self.db_path).claim_monitor_handoff(100.0),
            (None, {}),
        )

    def test_concurrent_legacy_schema_migration_is_serialized(self) -> None:
        self.db_path.parent.mkdir(parents=True)
        with sqlite3.connect(self.db_path) as db:
            db.executescript(
                """
                CREATE TABLE session_events (
                    provider TEXT NOT NULL,
                    session_id TEXT NOT NULL,
                    event_at REAL NOT NULL,
                    status TEXT NOT NULL,
                    attention_reason TEXT,
                    error TEXT,
                    PRIMARY KEY (provider, session_id, event_at, status)
                );
                INSERT INTO session_events VALUES (
                    'codex','legacy',10.0,'READY','completed',NULL
                );
                """
            )
        barrier = threading.Barrier(8)
        errors: list[BaseException] = []

        def initialize() -> None:
            try:
                barrier.wait()
                Store(self.db_path).initialize()
            except BaseException as exc:
                errors.append(exc)

        threads = [threading.Thread(target=initialize) for _ in range(8)]
        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join(10)
        self.assertFalse(any(thread.is_alive() for thread in threads))
        self.assertEqual(errors, [])
        with Store(self.db_path).connect() as db:
            row = db.execute("SELECT event_id,status FROM session_events").fetchone()
        self.assertEqual((row["event_id"], row["status"]), (1, "READY"))

    def test_attach_history_and_pending_launches(self) -> None:
        self.store.initialize()
        self.store.record_attach("codex", "one")
        self.store.record_attach("claude", "two")
        self.assertEqual(self.store.previous_attached(), ("codex", "one"))
        self.store.add_pending("token", "codex", "named", "/tmp", "pika-c-token", "%1")
        self.assertEqual(self.store.find_pending_for_pane("%1")["name"], "named")
        self.store.delete_pending("token")
        self.assertIsNone(self.store.get_pending("token"))

    def test_monitor_visit_claim_is_atomic_and_returns_previous_success(self) -> None:
        self.assertIsNone(self.store.claim_monitor_visit(100.0))
        self.assertEqual(self.store.claim_monitor_visit(200.0), 100.0)
        self.assertEqual(self.store.get_meta("monitor:last_seen_at"), "200.0")

    def test_monitor_handoff_uses_committed_event_watermarks(self) -> None:
        previous, counts = self.store.claim_monitor_handoff(100.0)
        self.assertIsNone(previous)
        self.assertEqual(counts, {})

        # An event whose timestamp predates the visit but commits afterward is
        # still after the claimed database watermark and cannot fall through.
        self.store.upsert_session(
            Session(
                "codex",
                "late-commit",
                status=Status.READY.value,
                unread=True,
                last_event_at=50.0,
            )
        )
        previous, counts = self.store.claim_monitor_handoff(200.0)
        self.assertEqual(previous, 100.0)
        self.assertEqual(counts, {Status.READY.value: 1})

        previous, counts = self.store.claim_monitor_handoff(300.0)
        self.assertEqual(previous, 200.0)
        self.assertEqual(counts, {})

    def test_action_event_ledger_survives_later_status_changes(self) -> None:
        self.store.upsert_session(
            Session(
                "codex",
                "ledger",
                status=Status.READY.value,
                unread=True,
                attention_reason="completed",
                last_event_at=100.0,
            )
        )
        self.store.update_session(
            "codex", "ledger", status=Status.WORKING.value, unread=False
        )
        self.store.update_session(
            "codex",
            "ledger",
            status=Status.READY.value,
            unread=True,
            attention_reason="completed",
            last_event_at=200.0,
        )
        counts = self.store.attention_event_counts(since=50.0, until=250.0)
        self.assertEqual(counts[Status.READY.value], 2)

    def test_status_observations_reject_stale_replay_by_kind(self) -> None:
        self.store.record_status_observation(
            "codex",
            "observed",
            kind="lifecycle",
            status=Status.WORKING.value,
            unread=False,
            attention_reason=None,
            error=None,
            observed_at=200.0,
            source="hook",
        )
        changed = self.store.record_status_observation(
            "codex",
            "observed",
            kind="lifecycle",
            status=Status.READY.value,
            unread=True,
            attention_reason="completed",
            error=None,
            observed_at=100.0,
            source="provider-scan",
        )
        self.assertFalse(changed)
        observation = self.store.status_observations("codex", "observed")[0]
        self.assertEqual(observation.status, Status.WORKING.value)
        self.assertEqual(observation.source, "hook")

    def test_activity_feed_is_named_and_transcript_free(self) -> None:
        self.store.upsert_session(
            Session(
                "codex",
                "activity",
                name="returns_tracker",
                transcript_path="/secret/rollout.jsonl",
                status=Status.READY.value,
                unread=True,
                attention_reason="completed",
                last_event_at=100.0,
            )
        )
        event = self.store.list_activity_events(limit=1)[0]
        self.assertEqual(event.display_name, "returns_tracker")
        self.assertEqual(event.status, Status.READY.value)
        self.assertNotIn("transcript", event.to_dict())

    def test_identity_interruption_tracks_new_provider_lifecycle(self) -> None:
        self.store.upsert_session(
            Session("codex", "interrupted", status=Status.WORKING.value)
        )
        self.store.capture_identity_interruption("codex", "interrupted")
        self.store.update_session(
            "codex",
            "interrupted",
            status=Status.ERROR.value,
            unread=True,
            attention_reason="identity",
            error="duplicate Pika tmux homes",
        )
        self.store.upsert_session(
            Session(
                "codex",
                "interrupted",
                status=Status.READY.value,
                unread=True,
                attention_reason="completed",
                last_event_at=200.0,
            )
        )
        interrupted = self.store.get_identity_interruption("codex", "interrupted")
        self.assertEqual(interrupted["status"], Status.READY.value)
        self.assertTrue(interrupted["unread"])
        self.assertEqual(interrupted["last_event_at"], 200.0)

        # Reassert the identity fault, then repair from the latest saved state.
        self.store.update_session(
            "codex",
            "interrupted",
            status=Status.ERROR.value,
            unread=True,
            attention_reason="identity",
            error="duplicate Pika tmux homes",
        )
        self.assertTrue(
            self.store.restore_identity_interruption("codex", "interrupted", live=True)
        )
        restored = self.store.get_session("codex", "interrupted")
        self.assertEqual(restored.status if restored else None, Status.READY.value)
        self.assertTrue(restored.unread if restored else False)
        self.assertIsNone(self.store.get_identity_interruption("codex", "interrupted"))

    def test_identity_restore_cannot_overwrite_a_concurrent_lifecycle_winner(
        self,
    ) -> None:
        self.store.upsert_session(
            Session("codex", "winner", status=Status.WORKING.value)
        )
        self.store.capture_identity_interruption("codex", "winner")
        self.store.update_session(
            "codex",
            "winner",
            status=Status.READY.value,
            unread=True,
            attention_reason="completed",
            last_event_at=300.0,
        )
        self.assertFalse(
            self.store.restore_identity_interruption("codex", "winner", live=True)
        )
        winner = self.store.get_session("codex", "winner")
        self.assertEqual(winner.status if winner else None, Status.READY.value)
        self.assertTrue(winner.unread if winner else False)

    def test_identity_restore_downgrades_dead_work_to_parked(self) -> None:
        self.store.upsert_session(
            Session("codex", "dead-work", status=Status.WORKING.value)
        )
        self.store.capture_identity_interruption("codex", "dead-work")
        self.store.update_session(
            "codex",
            "dead-work",
            status=Status.ERROR.value,
            unread=True,
            attention_reason="identity",
            error="identity fault",
        )
        self.assertTrue(
            self.store.restore_identity_interruption("codex", "dead-work", live=False)
        )
        restored = self.store.get_session("codex", "dead-work")
        self.assertEqual(restored.status if restored else None, Status.PARKED.value)
        self.assertFalse(restored.unread if restored else True)

    def test_result_collection_is_one_shot_and_counts_remaining_atomically(
        self,
    ) -> None:
        for index in range(2):
            self.store.upsert_session(
                Session(
                    "codex",
                    f"result-{index}",
                    status=Status.READY.value,
                    unread=True,
                    last_event_at=100.0 + index,
                )
            )
        self.assertEqual(
            self.store.collect_result("codex", "result-0", expected_event_at=100.0),
            1,
        )
        observation = self.store.status_observations("codex", "result-0")[0]
        self.assertFalse(observation.unread)
        self.assertIsNone(
            self.store.collect_result("codex", "result-0", expected_event_at=100.0)
        )
        self.assertIsNone(
            self.store.collect_result("codex", "result-1", expected_event_at=999.0)
        )
        self.assertTrue(self.store.get_session("codex", "result-1").unread)

    def test_usage_cache_is_invalidated_by_source_change(self) -> None:
        source = Path(self.temp.name) / "rollout.jsonl"
        source.write_text("{}\n")
        usage = Usage(model="gpt-5.4", input_tokens=2, output_tokens=3, total_tokens=5)
        self.store.put_cached_usage("codex", "abc", source, usage)
        loaded = self.store.get_cached_usage("codex", "abc", source)
        self.assertEqual(loaded.total_tokens if loaded else None, 5)
        source.write_text("{}\n{}\n")
        self.assertIsNone(self.store.get_cached_usage("codex", "abc", source))

    def test_resume_reservation_is_exclusive_and_releasable(self) -> None:
        self.assertTrue(self.store.reserve_resume("codex", "abc", "first"))
        self.assertFalse(self.store.reserve_resume("codex", "abc", "second"))
        with self.store.connect() as db:
            db.execute(
                "UPDATE launch_reservations SET created_at=?",
                (time.time() - 3600,),
            )
        self.assertFalse(self.store.reserve_resume("codex", "abc", "still-live"))
        self.store.release_resume("codex", "abc", "first")
        self.assertTrue(self.store.reserve_resume("codex", "abc", "second"))
        with self.store.connect() as db:
            db.execute(
                "UPDATE launch_reservations SET owner_pid=?, owner_start_time=?",
                (99999999, 1),
            )
        self.assertTrue(self.store.reserve_resume("codex", "abc", "dead-owner"))

    def test_fast_hook_binding_waits_for_home_certificate(self) -> None:
        self.store.add_pending("launch", "codex", "fast", "/tmp")
        self.store.bind_launch("launch", "codex", "exact-uuid")
        binding = self.store.finalize_pending_pane(
            "launch", "home", "%1", root_pid=123, root_pid_start=1001
        )
        self.assertEqual(binding, ("codex", "exact-uuid"))
        self.assertIsNotNone(self.store.get_pending("launch"))
        self.assertIsNone(self.store.get_recovery_owner("codex", "exact-uuid"))
        self.assertTrue(
            self.store.certify_launch(
                "launch", "codex", "exact-uuid", 123, 1001
            )
        )
        self.assertIsNone(self.store.get_pending("launch"))
        self.assertEqual(
            self.store.get_recovery_owner("codex", "exact-uuid"),
            (123, 1001, "launch"),
        )

    def test_launch_binding_certifies_the_pending_provider_pid_generation(self) -> None:
        self.store.add_pending("launch", "codex", "exact", "/tmp")
        self.store.finalize_pending_pane(
            "launch", "home", "%1", root_pid=123, root_pid_start=1001
        )

        self.assertTrue(self.store.bind_launch("launch", "codex", "exact-uuid"))
        self.assertTrue(
            self.store.certify_launch(
                "launch", "codex", "exact-uuid", 123, 1001
            )
        )
        self.assertEqual(
            self.store.get_recovery_owner("codex", "exact-uuid"),
            (123, 1001, "launch"),
        )

    def test_switch_launch_binding_moves_only_same_live_pid_generation(self) -> None:
        self.assertTrue(self.store.bind_launch("token", "opencode", "ses_old123"))
        self.store.set_recovery_owner(
            "opencode", "ses_old123", 123, 1001, "token"
        )
        with patch("pikamux.store.process_start_time", return_value=1001):
            self.assertTrue(
                self.store.switch_launch_binding(
                    "token", "opencode", "ses_old123", "ses_new123", 123
                )
            )
        self.assertEqual(
            self.store.get_launch_binding("token"), ("opencode", "ses_new123")
        )
        self.assertIsNone(self.store.get_recovery_owner("opencode", "ses_old123"))
        self.assertEqual(
            self.store.get_recovery_owner("opencode", "ses_new123"),
            (123, 1001, "token"),
        )

    def test_switch_launch_binding_rejects_pid_reuse_and_live_target_owner(self) -> None:
        self.assertTrue(self.store.bind_launch("token", "opencode", "ses_old123"))
        self.store.set_recovery_owner(
            "opencode", "ses_old123", 123, 1001, "token"
        )
        with patch("pikamux.store.process_start_time", return_value=2002):
            self.assertFalse(
                self.store.switch_launch_binding(
                    "token", "opencode", "ses_old123", "ses_new123", 123
                )
            )
        self.store.set_recovery_owner(
            "opencode", "ses_new123", 456, 3003, "other-token"
        )
        with patch(
            "pikamux.store.process_start_time",
            side_effect=lambda pid: {123: 1001, 456: 3003}.get(pid),
        ):
            self.assertFalse(
                self.store.switch_launch_binding(
                    "token", "opencode", "ses_old123", "ses_new123", 123
                )
            )
        self.assertEqual(
            self.store.get_launch_binding("token"), ("opencode", "ses_old123")
        )
        self.assertEqual(
            self.store.get_recovery_owner("opencode", "ses_new123"),
            (456, 3003, "other-token"),
        )

    def test_switch_launch_binding_rejects_fresh_target_lease_without_certificate(
        self,
    ) -> None:
        self.assertTrue(self.store.bind_launch("token", "opencode", "ses_old123"))
        self.store.set_recovery_owner(
            "opencode", "ses_old123", 123, 1001, "token"
        )
        with patch(
            "pikamux.store.process_start_time",
            side_effect=lambda pid: {123: 1001, 456: 2002}.get(pid),
        ):
            self.assertTrue(
                self.store.set_live_owner("opencode", "ses_new123", 456)
            )
            self.assertFalse(
                self.store.switch_launch_binding(
                    "token", "opencode", "ses_old123", "ses_new123", 123
                )
            )
        self.assertEqual(
            self.store.get_launch_binding("token"), ("opencode", "ses_old123")
        )
        self.assertEqual(
            self.store.get_live_owners("opencode", "ses_new123"), [(456, 2002)]
        )

    def test_live_owner_round_trip_and_delete(self) -> None:
        with patch(
            "pikamux.store.process_start_time",
            side_effect=lambda pid: {123: 1001, 456: 1002}.get(pid),
        ):
            self.assertTrue(self.store.set_live_owner("codex", "exact-uuid", 123))
            self.assertTrue(self.store.set_live_owner("codex", "exact-uuid", 456))
        self.assertEqual(
            self.store.get_live_owners("codex", "exact-uuid"),
            [(123, 1001), (456, 1002)],
        )
        leases = self.store.get_live_owner_leases("codex", "exact-uuid")
        self.assertEqual(
            [(pid, start) for pid, start, _seen, _token in leases],
            [
                (123, 1001),
                (456, 1002),
            ],
        )
        self.assertTrue(all(seen > 0 for _pid, _start, seen, _token in leases))
        self.store.delete_live_owner("codex", "exact-uuid", pid=123)
        self.assertEqual(
            self.store.get_live_owners("codex", "exact-uuid"), [(456, 1002)]
        )
        self.store.delete_live_owner("codex", "exact-uuid")
        self.assertEqual(self.store.get_live_owners("codex", "exact-uuid"), [])

    def test_live_owner_tokens_isolate_clients_sharing_one_app_server(self) -> None:
        with patch("pikamux.store.process_start_time", return_value=1001):
            self.assertTrue(
                self.store.set_live_owner(
                    "codex", "exact-uuid", 123, owner_token="pika-client"
                )
            )
            self.assertTrue(
                self.store.set_live_owner(
                    "codex", "exact-uuid", 123, owner_token="desktop-client"
                )
            )
        leases = self.store.get_live_owner_leases("codex", "exact-uuid")
        self.assertEqual(
            [token for _pid, _start, _seen, token in leases],
            ["desktop-client", "pika-client"],
        )

        self.store.delete_live_owner(
            "codex", "exact-uuid", owner_token="pika-client"
        )

        remaining = self.store.get_live_owner_leases("codex", "exact-uuid")
        self.assertEqual(
            [token for _pid, _start, _seen, token in remaining],
            ["desktop-client"],
        )

    def test_live_owner_requires_process_start_identity(self) -> None:
        with patch("pikamux.store.process_start_time", return_value=None):
            self.assertFalse(self.store.set_live_owner("codex", "exact-uuid", 123))
        self.assertEqual(self.store.get_live_owners("codex", "exact-uuid"), [])

    def test_untracking_blocks_hook_writes_and_retains_expert_card(self) -> None:
        session = Session("codex", "watched", name="researcher")
        self.store.upsert_session(session)
        self.store.put_expert_profile(
            ExpertProfile(
                "codex",
                "watched",
                "Owns the research pipeline.",
                ("research", "pipeline", "operations"),
                current_state="Monitoring the current run.",
            )
        )
        with patch("pikamux.store.process_start_time", return_value=100):
            self.assertTrue(self.store.set_live_owner("codex", "watched", 123))

        self.store.untrack_session("codex", "watched")

        self.assertTrue(self.store.is_untracked("codex", "watched"))
        self.assertIn(("codex", "watched"), self.store.untracked_session_keys())
        self.assertEqual(self.store.list_sessions(), [])
        hidden = self.store.get_session("codex", "watched")
        self.assertIsNotNone(hidden)
        self.assertEqual(hidden.status if hidden else None, Status.PARKED.value)
        self.assertEqual(
            [item.session_id for item in self.store.list_untracked_sessions()],
            ["watched"],
        )
        self.assertIsNotNone(self.store.get_expert_profile("codex", "watched"))
        self.assertEqual(self.store.get_live_owners("codex", "watched"), [])
        self.store.upsert_session(session)
        self.assertEqual(self.store.list_sessions(), [])
        with patch("pikamux.store.process_start_time", return_value=100):
            self.assertFalse(self.store.set_live_owner("codex", "watched", 123))

        self.store.restore_tracking("codex", "watched")
        self.store.upsert_session(session)
        self.assertEqual(
            [item.session_id for item in self.store.list_sessions()],
            ["watched"],
        )

    def test_expert_profile_round_trip_replaces_and_follows_session_lifetime(
        self,
    ) -> None:
        self.store.upsert_session(Session("codex", "expert", name="researcher"))
        first = self.store.put_expert_profile(
            ExpertProfile(
                "codex",
                "expert",
                "Built the factor attribution pipeline.",
                ("factor attribution", "portfolio analytics"),
                ("reports/attribution.md",),
                current_state="Validating the production handoff.",
            )
        )
        self.assertGreater(first.updated_at, 0)
        loaded = self.store.get_expert_profile("codex", "expert")
        self.assertEqual(loaded.summary if loaded else None, first.summary)
        self.assertEqual(
            loaded.topics if loaded else None,
            ("factor attribution", "portfolio analytics"),
        )
        self.assertEqual(
            loaded.current_state if loaded else None,
            "Validating the production handoff.",
        )
        replacement = self.store.put_expert_profile(
            ExpertProfile(
                "codex",
                "expert",
                "Owns the production attribution implementation.",
                ("production attribution",),
                current_state="No active blocker; awaiting the next run.",
            )
        )
        self.assertEqual(
            self.store.list_expert_profiles()[0].summary,
            replacement.summary,
        )
        self.store.delete_session("codex", "expert")
        self.assertIsNone(self.store.get_expert_profile("codex", "expert"))

    def test_expert_profile_requires_a_tracked_session(self) -> None:
        with self.assertRaisesRegex(ValueError, "tracked Pika session"):
            self.store.put_expert_profile(
                ExpertProfile("codex", "missing", "Unknown", ("topic",))
            )

    def test_existing_database_migrates_attention_reason_column(self) -> None:
        self.store.initialize()
        with self.store.connect() as db:
            db.execute("ALTER TABLE sessions DROP COLUMN attention_reason")
        migrated = Store(self.db_path)
        migrated.initialize()
        with migrated.connect() as db:
            columns = {row["name"] for row in db.execute("PRAGMA table_info(sessions)")}
        self.assertIn("attention_reason", columns)

    def test_existing_database_migrates_live_owner_start_time_column(self) -> None:
        self.store.initialize()
        with self.store.connect() as db:
            db.execute("ALTER TABLE live_owners DROP COLUMN start_time")
        migrated = Store(self.db_path)
        migrated.initialize()
        with migrated.connect() as db:
            columns = {
                row["name"] for row in db.execute("PRAGMA table_info(live_owners)")
            }
        self.assertIn("start_time", columns)

    def test_existing_database_migrates_live_owner_client_tokens(self) -> None:
        self.store.initialize()
        with self.store.connect() as db:
            db.executescript(
                """
                ALTER TABLE live_owners RENAME TO live_owners_current;
                CREATE TABLE live_owners (
                    provider TEXT NOT NULL,
                    session_id TEXT NOT NULL,
                    pid INTEGER NOT NULL,
                    start_time INTEGER,
                    last_seen REAL NOT NULL,
                    PRIMARY KEY (provider, session_id, pid)
                );
                INSERT INTO live_owners VALUES ('codex','legacy',123,456,789.0);
                DROP TABLE live_owners_current;
                """
            )
        migrated = Store(self.db_path)
        migrated.initialize()
        with migrated.connect() as db:
            columns = {
                row["name"] for row in db.execute("PRAGMA table_info(live_owners)")
            }
            owner_pk = [
                row["name"]
                for row in db.execute("PRAGMA table_info(live_owners)")
                if row["pk"]
            ]
        self.assertIn("owner_token", columns)
        self.assertEqual(
            owner_pk, ["provider", "session_id", "pid", "owner_token"]
        )
        self.assertEqual(
            migrated.get_live_owner_leases("codex", "legacy")[0][3], ""
        )

    def test_existing_database_migrates_expert_freshness_columns(self) -> None:
        self.store.initialize()
        with self.store.connect() as db:
            db.execute("ALTER TABLE expert_profiles DROP COLUMN transcript_size")
            db.execute("ALTER TABLE expert_profiles DROP COLUMN transcript_mtime_ns")
            db.execute("ALTER TABLE expert_profiles DROP COLUMN source")
            db.execute("ALTER TABLE expert_profiles DROP COLUMN current_state")
        migrated = Store(self.db_path)
        migrated.initialize()
        with migrated.connect() as db:
            columns = {
                row["name"]
                for row in db.execute("PRAGMA table_info(expert_profiles)")
            }
        self.assertTrue(
            {
                "source",
                "transcript_mtime_ns",
                "transcript_size",
                "current_state",
            }.issubset(columns)
        )


if __name__ == "__main__":
    unittest.main()
