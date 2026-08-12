from __future__ import annotations

import tempfile
import time
import unittest
from pathlib import Path

from pikamux.core import Pika, PikaError
from pikamux.models import Session, Status
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


class CoreTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        store = Store(Path(self.temp.name) / "pika.db")
        self.pika = Pika(
            store=store, tmux=EmptyTmux(), provider_map={"codex": EmptyProvider()}
        )
        now = time.time()
        for session in (
            Session(
                provider="codex",
                session_id="11111111-1111",
                name="alpha",
                cwd="/tmp",
                status=Status.READY.value,
                unread=True,
                created_at=now,
                last_event_at=now,
                last_activity_at=now - 10,
            ),
            Session(
                provider="claude",
                session_id="22222222-2222",
                name="beta",
                cwd="/tmp",
                status=Status.NEEDS_YOU.value,
                unread=True,
                created_at=now,
                last_event_at=now,
                last_activity_at=now - 5,
            ),
        ):
            self.pika.store.upsert_session(session)

    def tearDown(self) -> None:
        self.temp.cleanup()

    def test_resolve_by_name_uuid_and_prefix(self) -> None:
        sessions = self.pika.store.list_sessions()
        self.assertEqual(
            self.pika.resolve("alpha", sessions).session_id, "11111111-1111"
        )
        self.assertEqual(self.pika.resolve("22222222", sessions).name, "beta")

    def test_missing_name_never_creates(self) -> None:
        count = len(self.pika.store.list_sessions())
        with self.assertRaises(PikaError):
            self.pika.resolve("gamma", self.pika.store.list_sessions())
        self.assertEqual(len(self.pika.store.list_sessions()), count)

    def test_next_prioritizes_needs_you_over_ready(self) -> None:
        selected = self.pika.next_attention(self.pika.store.list_sessions())
        self.assertEqual(selected.name if selected else None, "beta")

    def test_next_prioritizes_failure_over_completed_result(self) -> None:
        beta = self.pika.resolve("beta", self.pika.store.list_sessions())
        self.pika.store.update_session(*beta.key, status=Status.ERROR.value)
        selected = self.pika.next_attention(self.pika.store.list_sessions())
        self.assertEqual(selected.name if selected else None, "beta")

    def test_ready_acknowledgement_does_not_clear_needs_you(self) -> None:
        alpha = self.pika.resolve("alpha", self.pika.store.list_sessions())
        beta = self.pika.resolve("beta", self.pika.store.list_sessions())
        self.pika.acknowledge(alpha)
        self.pika.acknowledge(beta)
        self.assertFalse(self.pika.store.get_session(*alpha.key).unread)
        self.assertTrue(self.pika.store.get_session(*beta.key).unread)


if __name__ == "__main__":
    unittest.main()
