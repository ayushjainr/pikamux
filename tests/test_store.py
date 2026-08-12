from __future__ import annotations

import tempfile
import time
import unittest
from pathlib import Path
from unittest.mock import patch

from pikamux.models import Session, Status, Usage
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

    def test_attach_history_and_pending_launches(self) -> None:
        self.store.initialize()
        self.store.record_attach("codex", "one")
        self.store.record_attach("claude", "two")
        self.assertEqual(self.store.previous_attached(), ("codex", "one"))
        self.store.add_pending("token", "codex", "named", "/tmp", "pika-c-token", "%1")
        self.assertEqual(self.store.find_pending_for_pane("%1")["name"], "named")
        self.store.delete_pending("token")
        self.assertIsNone(self.store.get_pending("token"))

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
                "UPDATE launch_reservations "
                "SET owner_pid=?, owner_start_time=?",
                (99999999, 1),
            )
        self.assertTrue(self.store.reserve_resume("codex", "abc", "dead-owner"))

    def test_fast_hook_binding_cannot_be_resurrected_as_pending(self) -> None:
        self.store.add_pending("launch", "codex", "fast", "/tmp")
        self.store.bind_launch("launch", "codex", "exact-uuid")
        binding = self.store.finalize_pending_pane("launch", "home", "%1")
        self.assertEqual(binding, ("codex", "exact-uuid"))
        self.assertIsNone(self.store.get_pending("launch"))

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
        self.store.delete_live_owner("codex", "exact-uuid", pid=123)
        self.assertEqual(
            self.store.get_live_owners("codex", "exact-uuid"), [(456, 1002)]
        )
        self.store.delete_live_owner("codex", "exact-uuid")
        self.assertEqual(self.store.get_live_owners("codex", "exact-uuid"), [])

    def test_live_owner_requires_process_start_identity(self) -> None:
        with patch("pikamux.store.process_start_time", return_value=None):
            self.assertFalse(self.store.set_live_owner("codex", "exact-uuid", 123))
        self.assertEqual(self.store.get_live_owners("codex", "exact-uuid"), [])

    def test_existing_database_migrates_attention_reason_column(self) -> None:
        self.store.initialize()
        with self.store.connect() as db:
            db.execute("ALTER TABLE sessions DROP COLUMN attention_reason")
        migrated = Store(self.db_path)
        migrated.initialize()
        with migrated.connect() as db:
            columns = {
                row["name"] for row in db.execute("PRAGMA table_info(sessions)")
            }
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


if __name__ == "__main__":
    unittest.main()
