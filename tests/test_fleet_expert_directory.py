"""Expert retention must cross nodes without resurrecting board inventory."""
import io
import json
import tempfile
import time
import unittest
import uuid
from pathlib import Path
from unittest.mock import patch

from pikamux.core import Pika
from pikamux.fleet import FleetManager, FleetError, PROTOCOL_NAME, PROTOCOL_VERSION, handle_fleet_stdio, validate_snapshot
from pikamux.models import ExpertProfile, FleetNode, Session
from pikamux.store import Store


class Provider:
    name = "codex"
    def hidden_session_ids(self):
        return set()


class FleetExpertDirectoryTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        root = Path(self.temp.name)
        self.store = Store(root / "remote.db")
        self.session = Session("codex", str(uuid.uuid4()), name="retained", transcript_path=str(root / "thread.jsonl"))
        Path(self.session.transcript_path).write_text("fixture\n")
        self.store.upsert_session(self.session)
        self.store.put_expert_profile(ExpertProfile("codex", self.session.session_id, "Return methodology", ("returns",), current_state="Review complete"))
        self.store.untrack_session(*self.session.key)
        self.pika = Pika(self.store, provider_map={"codex": Provider()})
        self.local = Store(root / "hub.db")

    def snapshot(self, extended):
        request = {"op": "snapshot", "protocol": PROTOCOL_NAME, "version": PROTOCOL_VERSION}
        if extended:
            request["expert_directory"] = True
        output = io.StringIO()
        with patch.object(self.pika, "refresh", return_value=[]):
            self.assertEqual(handle_fleet_stdio(self.pika, io.StringIO(json.dumps(request) + "\n"), output), 0)
        payload = json.loads(output.getvalue())
        return validate_snapshot(payload)

    def test_unwatched_remote_expert_is_findable_but_not_on_board(self):
        payload = self.snapshot(True)
        self.assertEqual(payload["sessions"], [])
        self.assertEqual(len(payload["expert_sessions"]), 1)
        self.assertNotIn("transcript_path", payload["expert_sessions"][0])
        self.assertFalse(payload["cards"][0]["watched"])
        self.assertEqual(payload["cards"][0]["availability"], "source-available")
        node = FleetNode(payload["node_id"], "remote", "remote", status="ready")
        self.local.upsert_fleet_node(node)
        self.local.put_remote_snapshot(node.node_id, payload)
        fleet = FleetManager(self.local)
        self.assertEqual(fleet.cached_sessions(), [])
        matches = fleet.expert_matches("returns")
        self.assertEqual(len(matches), 1)
        self.assertFalse(matches[0].to_dict()["watched"])
        self.assertEqual(matches[0].to_dict()["availability"], "source-available")
        found = fleet.resolve(self.session.session_id + "@remote", fresh=False, include_experts=True)
        self.assertEqual(found.session_id, self.session.session_id)
        self.assertTrue(self.store.is_untracked(*self.session.key))

    def test_old_client_gets_unchanged_snapshot_envelope(self):
        payload = self.snapshot(False)
        self.assertNotIn("expert_sessions", payload)
        self.assertEqual(payload["profiles"], [])

    def test_archived_expert_not_exported(self):
        with patch.object(self.pika.providers["codex"], "hidden_session_ids", return_value={self.session.session_id}):
            payload = self.snapshot(True)
        self.assertEqual(payload["expert_sessions"], [])
        self.assertEqual(payload["profiles"], [])

    def test_source_unavailable_does_not_become_available_across_wire(self):
        Path(self.session.transcript_path).unlink()
        payload = self.snapshot(True)
        self.assertEqual(payload["cards"][0]["availability"], "source-unavailable")

    def test_stale_snapshot_cannot_claim_remote_work_is_current(self):
        payload = self.snapshot(True)
        node = FleetNode(payload["node_id"], "remote", "remote", status="unreachable")
        self.local.upsert_fleet_node(node)
        self.local.put_remote_snapshot(node.node_id, payload)
        self.local.mark_fleet_node_error(node.node_id, "unreachable", "connection refused")
        match = FleetManager(self.local).expert_matches("returns")[0].to_dict()
        self.assertEqual(match["availability"], "machine-unreachable")
        self.assertEqual(match["current_state_status"], "UNKNOWN")

    def test_malformed_extension_rejected(self):
        payload = self.snapshot(True)
        payload["cards"][0]["watched"] = "false"
        with self.assertRaises(FleetError):
            validate_snapshot(payload)

    def test_active_leaf_preserved_across_snapshot_and_expert_route(self):
        active = str(uuid.uuid4())
        self.store.update_session(*self.session.key, active_thread_id=active)
        payload = self.snapshot(True)
        self.assertEqual(payload["expert_sessions"][0]["active_thread_id"], active)
        node = FleetNode(payload["node_id"], "remote", "remote", status="ready")
        self.local.upsert_fleet_node(node)
        self.local.put_remote_snapshot(node.node_id, payload)
        result = FleetManager(self.local).resolve(self.session.session_id + "@remote", fresh=False, include_experts=True)
        self.assertEqual(result.provider_thread_id, active)
