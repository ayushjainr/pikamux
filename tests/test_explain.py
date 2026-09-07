from __future__ import annotations

import io
import json
import unittest
from contextlib import redirect_stdout
from unittest.mock import Mock

from pikamux.cli import _explain, _normalize_argv
from pikamux.explain import explain_session
from pikamux.models import FleetSession, Session, Status
from pikamux.status_projection import StatusObservation, project_status


class ExplainTests(unittest.TestCase):
    def test_lifecycle_matrix_uses_actual_projection(self):
        for status, live, unread, expected, rule in [
            ("WORKING", True, False, "WORKING", "lifecycle"),
            ("WORKING", False, False, "PARKED", "working_process_gone"),
            ("READY", True, True, "READY", "lifecycle"),
            ("READY", False, False, "PARKED", "completed_and_collected_process_gone"),
            ("NEEDS YOU", True, True, "NEEDS YOU", "lifecycle"),
            ("ERROR", False, True, "ERROR", "lifecycle"),
        ]:
            with self.subTest(status=status, live=live):
                fact = StatusObservation("lifecycle", status, unread, "question", None, 10, "provider-hook")
                s = Session("codex", "abc", status=status, live=live, unread=unread)
                result = explain_session(s, [fact], now=20)
                self.assertEqual(result["state"], expected)
                self.assertEqual(result["rule"], rule)
                self.assertEqual(result["evidence"][0]["age_seconds"], 10)
                self.assertTrue(result["evidence"][0]["winner"])

    def test_identity_beats_newer_working(self):
        facts = [
            StatusObservation("safety", "OPEN TWICE", True, "identity", "Two owners", 9, "runtime"),
            StatusObservation("lifecycle", "WORKING", False, None, None, 10, "provider"),
        ]
        result = explain_session(Session("codex", "abc", live=True), facts, now=20)
        self.assertEqual(result["rule"], "safety_precedence")
        self.assertEqual([f["kind"] for f in result["evidence"] if f["winner"]], ["safety"])

    def test_completed_live_process_is_not_conflicting_evidence(self):
        s = Session("codex", "abc", status="READY", live=True, unread=True)
        fact = StatusObservation("lifecycle", "READY", True, "completed", None, 10, "hook")
        result = explain_session(s, [fact], now=20)
        self.assertEqual(result["state"], "READY")
        self.assertIn("remain open", result["summary"])
        self.assertFalse(s.needs_attention)

    def test_stale_remote_explanation_does_not_claim_fresh_evidence(self):
        remote = FleetSession("node", "offline", Session("claude", "abc", status="NEEDS YOU"), stale=True, seen_at=10)
        report = explain_session(remote, now=100)
        self.assertTrue(report["freshness"]["stale"])
        self.assertEqual(report["next_action"], "pika sync offline")
        self.assertEqual(report["evidence"], [])

    def test_cli_local_uses_metadata_no_usage(self):
        s = Session("codex", "abc", name="test")
        pika = Mock()
        pika.refresh.return_value = [s]
        pika.store.status_observations.return_value = ()
        with redirect_stdout(io.StringIO()) as output:
            self.assertEqual(_explain(pika, "test", as_json=True), 0)
        self.assertEqual(json.loads(output.getvalue())["session_id"], "abc")
        pika.refresh.assert_called_once_with(usage=False)
        pika.consultation.assert_not_called()
        self.assertEqual(_normalize_argv(["explain", "test", "--json"])[0], "explain")

    def test_remote_explain_does_not_require_ssh(self):
        pika = Mock()
        pika.fleet.resolve.return_value = FleetSession("node", "offline", Session("codex", "abc"), stale=True)
        with redirect_stdout(io.StringIO()):
            _explain(pika, "abc@offline", as_json=True)
        pika.fleet.resolve.assert_called_once_with("abc@offline", fresh=False)
        pika.refresh.assert_not_called()


if __name__ == "__main__":
    unittest.main()
