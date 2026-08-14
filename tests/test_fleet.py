from __future__ import annotations

import argparse
import io
import json
import os
import subprocess
import sys
import tempfile
import time
import unittest
import uuid
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from unittest.mock import Mock, patch

from pikamux.cli import _setup_machine_candidates
from pikamux.consult import ConsultationError, ConsultationPolicy
from pikamux.fleet import (
    CAPABILITIES,
    PROTOCOL_NAME,
    PROTOCOL_VERSION,
    FleetError,
    FleetManager,
    RemoteConsultation,
    SSHTransport,
    discover_node_candidates,
    discover_ssh_candidates,
    discover_tailscale_candidates,
    handle_fleet_stdio,
    session_to_wire,
    suggest_local_machine_alias,
    validate_snapshot,
)
from pikamux.models import (
    Candidate,
    FleetNode,
    FleetSession,
    NodeCandidate,
    Session,
    Status,
)
from pikamux.monitor import (
    MonitorState,
    _capture_preview,
    _capture_remote_peek,
    _handle_key,
    _next_remote_node,
    render_monitor,
)
from pikamux.store import Store
from pikamux.ui import print_experts


def snapshot(
    node_id: str,
    *,
    name: str = "remote-work",
    session_id: str = "11111111-1111-4111-8111-111111111111",
    status: str = Status.WORKING.value,
) -> dict[str, object]:
    session = Session(
        "codex",
        session_id,
        name=name,
        cwd="/remote/project",
        transcript_path="/secret/transcript.jsonl",
        tmux_pane="%9",
        root_pid=999,
        status=status,
        live=True,
        home_state="exact-live",
        updated_at=10,
        last_activity_at=9,
    )
    return {
        "type": "snapshot",
        "protocol": PROTOCOL_NAME,
        "version": PROTOCOL_VERSION,
        "node_id": node_id,
        "machine": "atlas",
        "captured_at": 10,
        "sessions": [session_to_wire(session)],
        "profiles": [
            {
                "provider": "codex",
                "session_id": session_id,
                "scope": "Owns remote pricing infrastructure",
                "current_state": "Validating the rollout",
                "topics": ["pricing", "rollout", "validation"],
                "artifacts": ["/remote/project/result.md"],
                "updated_at": 10,
                "source": "interview",
            }
        ],
        "cards": [
            {
                "provider": "codex",
                "session_id": session_id,
                "status": "CURRENT",
                "detail": "matches remote transcript",
            }
        ],
    }


class FakeTransport:
    def __init__(self, *responses):
        self.responses = list(responses)
        self.requests = []
        self.exact = []

    def request(self, target, payload, *, mutating=False):
        self.requests.append((target, payload, mutating))
        response = self.responses.pop(0)
        if isinstance(response, BaseException):
            raise response
        return response

    def run_exact(self, node, arguments, *, tty):
        self.exact.append((node, arguments, tty))
        return 0


class FleetTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.store = Store(Path(self.temp.name) / "pika.db")
        self.store.initialize()

    def tearDown(self) -> None:
        self.temp.cleanup()

    def test_wire_session_never_exports_transcript_or_process_identity(self) -> None:
        session = Session(
            "codex",
            "thread-id",
            transcript_path="/secret/provider.jsonl",
            tmux_session="pika-c-1",
            tmux_pane="%4",
            root_pid=123,
        )
        wire = session_to_wire(session)
        self.assertNotIn("transcript_path", wire)
        self.assertNotIn("tmux_session", wire)
        self.assertNotIn("tmux_pane", wire)
        self.assertNotIn("root_pid", wire)
        self.assertNotIn("home_state", wire)

    def test_local_session_json_contract_has_no_fleet_fields(self) -> None:
        payload = Session("codex", "thread-id").to_dict()
        self.assertNotIn("node_id", payload)
        self.assertNotIn("machine", payload)
        self.assertNotIn("stale", payload)

    def test_node_identity_first_use_is_atomic(self) -> None:
        with ThreadPoolExecutor(max_workers=12) as pool:
            values = list(
                pool.map(lambda _value: self.store.local_node_id(), range(40))
            )
        self.assertEqual(len(set(values)), 1)
        self.assertEqual(str(uuid.UUID(values[0])), values[0])

    def test_ssh_discovery_is_file_only_and_ignores_patterns(self) -> None:
        ssh = Path(self.temp.name) / ".ssh"
        ssh.mkdir()
        (ssh / "config").write_text(
            "Host atlas\n  HostName 10.0.0.1\n"
            "Host *.internal !blocked\n  User ajain\n"
            "Include conf.d/*.conf\n"
        )
        (ssh / "conf.d").mkdir()
        (ssh / "conf.d" / "gpu.conf").write_text("Host gpu-box\n")
        with patch("pikamux.fleet.subprocess.run") as runner:
            values = discover_ssh_candidates(ssh)
        self.assertEqual([item.alias for item in values], ["atlas", "gpu-box"])
        runner.assert_not_called()

    def test_tailscale_discovery_offers_only_linux_compatible_peers(self) -> None:
        payload = {
            "Peer": {
                "a": {
                    "HostName": "linux-box",
                    "DNSName": "linux-box.ts.net.",
                    "OS": "linux",
                    "Online": True,
                },
                "b": {
                    "HostName": "phone",
                    "DNSName": "phone.ts.net.",
                    "OS": "iOS",
                    "Online": True,
                },
            }
        }
        completed = Mock(returncode=0, stdout=json.dumps(payload))
        with patch("pikamux.fleet.subprocess.run", return_value=completed):
            values = discover_tailscale_candidates()
        self.assertEqual([item.alias for item in values], ["linux-box"])

    def test_local_alias_prefers_tailscale_dns_over_kernel_hostname(self) -> None:
        completed = Mock(
            returncode=0,
            stdout=json.dumps(
                {
                    "Self": {
                        "HostName": "ip-172-31-61-171",
                        "DNSName": "rstudio-6.example.ts.net.",
                    }
                }
            ),
        )
        with patch("pikamux.fleet.subprocess.run", return_value=completed):
            self.assertEqual(suggest_local_machine_alias(), "rstudio-6")

    def test_discovery_makes_duplicate_suggested_aliases_unique(self) -> None:
        candidates = [
            NodeCandidate("worker", "worker-one", ("ssh-config",)),
            NodeCandidate("worker", "worker-two", ("tailscale",)),
        ]
        with (
            patch("pikamux.fleet.discover_ssh_candidates", return_value=candidates[:1]),
            patch(
                "pikamux.fleet.discover_tailscale_candidates",
                return_value=candidates[1:],
            ),
        ):
            values = discover_node_candidates(self.store)
        self.assertEqual([item.alias for item in values], ["worker", "worker-2"])

    def test_failed_initial_snapshot_adopts_nothing(self) -> None:
        remote_id = str(uuid.uuid4())
        hello = {
            "type": "hello",
            "protocol": PROTOCOL_NAME,
            "version": PROTOCOL_VERSION,
            "node_id": remote_id,
            "machine": "atlas",
            "package_version": "0.2.0",
            "capabilities": list(CAPABILITIES),
        }
        transport = FakeTransport(
            hello,
            FleetError("truncated snapshot", kind="incompatible"),
        )
        manager = FleetManager(self.store, transport)
        with self.assertRaisesRegex(FleetError, "truncated"):
            manager.add(NodeCandidate("atlas", "atlas", ("explicit",)))
        self.assertEqual(self.store.list_fleet_nodes(), [])

    def test_valid_empty_snapshot_replaces_old_cache(self) -> None:
        node_id = str(uuid.uuid4())
        node = FleetNode(node_id, "atlas", "atlas")
        self.store.upsert_fleet_node(node)
        self.store.put_remote_snapshot(node_id, snapshot(node_id))
        empty = snapshot(node_id)
        empty["sessions"] = []
        empty["profiles"] = []
        empty["cards"] = []
        manager = FleetManager(self.store, FakeTransport(empty))
        self.assertEqual(manager.refresh_node(node_id), [])
        self.assertEqual(manager.cached_sessions(node_id), [])

    def test_snapshot_accepts_explicit_unbound_placeholder_identity(self) -> None:
        node_id = str(uuid.uuid4())
        value = snapshot(node_id)
        value["sessions"] = [
            session_to_wire(
                Session(
                    "codex",
                    "unbound:%9",
                    name="unbound worker",
                    status=Status.UNBOUND.value,
                    live=True,
                )
            )
        ]
        validated = validate_snapshot(value, expected_node_id=node_id)
        self.assertEqual(validated["sessions"][0]["identity_kind"], "placeholder")

    def test_snapshot_rejects_unvalidated_expert_metadata(self) -> None:
        node_id = str(uuid.uuid4())
        value = snapshot(node_id)
        value["profiles"][0]["transcript_path"] = "/secret"
        with self.assertRaisesRegex(FleetError, "profile is malformed"):
            validate_snapshot(value, expected_node_id=node_id)

    def test_identity_change_quarantines_and_preserves_last_good(self) -> None:
        node_id = str(uuid.uuid4())
        other = str(uuid.uuid4())
        node = FleetNode(node_id, "atlas", "atlas")
        self.store.upsert_fleet_node(node)
        self.store.put_remote_snapshot(node_id, snapshot(node_id))
        manager = FleetManager(self.store, FakeTransport(snapshot(other)))
        with self.assertRaisesRegex(FleetError, "IDENTITY CHANGED"):
            manager.refresh_node(node_id)
        saved = self.store.get_fleet_node(node_id)
        self.assertEqual(saved.status, "quarantined")
        cached = manager.cached_sessions(node_id)
        self.assertEqual(len(cached), 1)
        self.assertTrue(cached[0].stale)

    def test_equal_provider_uuid_on_two_nodes_never_collides(self) -> None:
        ids = [str(uuid.uuid4()), str(uuid.uuid4())]
        for index, node_id in enumerate(ids):
            node = FleetNode(node_id, f"node-{index}", f"node-{index}")
            self.store.upsert_fleet_node(node)
            self.store.put_remote_snapshot(node_id, snapshot(node_id))
        manager = FleetManager(self.store, FakeTransport())
        sessions = manager.cached_sessions()
        self.assertEqual(len(sessions), 2)
        self.assertEqual(len({item.key for item in sessions}), 2)
        experts = manager.expert_matches("pricing")
        self.assertEqual(len(experts), 2)
        self.assertEqual(len({item.session.key for item in experts}), 2)

    def test_resolve_refreshes_then_attach_routes_only_provider_and_uuid(self) -> None:
        node_id = str(uuid.uuid4())
        node = FleetNode(node_id, "atlas", "atlas")
        self.store.upsert_fleet_node(node)
        transport = FakeTransport(snapshot(node_id), snapshot(node_id))
        manager = FleetManager(self.store, transport)
        remote = manager.resolve("remote-work@atlas")
        self.assertIsInstance(remote, FleetSession)
        with patch("builtins.print"):
            self.assertEqual(manager.attach(remote), 0)
        _node, arguments, tty = transport.exact[0]
        self.assertTrue(tty)
        self.assertIn("codex", arguments)
        self.assertIn("11111111-1111-4111-8111-111111111111", arguments)
        self.assertNotIn("remote-work", arguments)

    def test_scheduler_is_oldest_first_and_selection_only_affects_manual_sync(
        self,
    ) -> None:
        nodes = [
            FleetNode("b", "selected", "selected", last_attempt_at=90),
            FleetNode("a", "oldest", "oldest", last_attempt_at=10),
            FleetNode("c", "middle", "middle", last_attempt_at=50),
        ]
        automatic = _next_remote_node(
            nodes, selected_node_id="b", now=200, manual=False
        )
        manual = _next_remote_node(nodes, selected_node_id="b", now=200, manual=True)
        self.assertEqual(automatic.node_id, "a")
        self.assertEqual(manual.node_id, "b")

    def test_adopt_retry_reuses_persisted_request_id(self) -> None:
        node_id = str(uuid.uuid4())
        node = FleetNode(node_id, "atlas", "atlas")
        self.store.upsert_fleet_node(node)
        candidate = Candidate("codex", "thread-id", "remote-work")
        adopted = Session("codex", "thread-id", name="remote-work")
        transport = FakeTransport(
            FleetError("connection dropped", kind="outcome_unknown"),
        )
        manager = FleetManager(self.store, transport)

        with self.assertRaisesRegex(FleetError, "OUTCOME UNKNOWN"):
            manager.adopt(node, candidate)
        pending = self.store.get_meta(f"fleet:pending-adopt:{node_id}:codex:thread-id")
        self.assertIsNotNone(pending)
        transport.responses.extend(
            [
                {
                    "type": "adopted",
                    "node_id": node_id,
                    "request_id": pending,
                    "session": session_to_wire(adopted),
                },
                snapshot(node_id, session_id="thread-id"),
            ]
        )

        self.assertEqual(
            manager.adopt(node, candidate).key,
            (candidate.provider, candidate.session_id),
        )
        request_ids = [item[1]["request_id"] for item in transport.requests[:2]]
        self.assertEqual(request_ids, [pending, pending])
        self.assertIsNone(
            self.store.get_meta(f"fleet:pending-adopt:{node_id}:codex:thread-id")
        )

    def test_untrack_retry_reuses_persisted_request_id(self) -> None:
        node_id = str(uuid.uuid4())
        node = FleetNode(node_id, "atlas", "atlas")
        self.store.upsert_fleet_node(node)
        remote = FleetSession(
            node_id,
            "atlas",
            Session("claude", "thread-id", name="remote-work"),
        )
        transport = FakeTransport(
            FleetError("connection dropped", kind="outcome_unknown"),
        )
        manager = FleetManager(self.store, transport)

        with self.assertRaisesRegex(FleetError, "OUTCOME UNKNOWN"):
            manager.untrack(remote)
        pending = self.store.get_meta(
            f"fleet:pending-untrack:{node_id}:claude:thread-id"
        )
        self.assertIsNotNone(pending)
        transport.responses.extend(
            [
                {
                    "type": "untracked",
                    "node_id": node_id,
                    "request_id": pending,
                    "panes_cleared": 1,
                },
                snapshot(node_id, session_id="other-thread"),
            ]
        )

        self.assertEqual(manager.untrack(remote), 1)
        request_ids = [item[1]["request_id"] for item in transport.requests[:2]]
        self.assertEqual(request_ids, [pending, pending])
        self.assertIsNone(
            self.store.get_meta(f"fleet:pending-untrack:{node_id}:claude:thread-id")
        )

    def test_confirmed_mutation_survives_followup_snapshot_failure(self) -> None:
        node_id = str(uuid.uuid4())
        node = FleetNode(node_id, "atlas", "atlas")
        self.store.upsert_fleet_node(node)
        adopted = Session("codex", "thread-id", name="remote-work")
        request_id = str(uuid.uuid4())
        transport = FakeTransport(
            {
                "type": "adopted",
                "node_id": node_id,
                "request_id": request_id,
                "session": session_to_wire(adopted),
            },
            FleetError("snapshot timeout", kind="unreachable"),
        )
        manager = FleetManager(self.store, transport)
        result = manager.adopt(
            node,
            Candidate("codex", "thread-id", "remote-work"),
            request_id=request_id,
        )
        self.assertEqual(result.key, adopted.key)
        self.assertEqual(self.store.get_fleet_node(node_id).status, "unreachable")
        self.assertIsNone(
            self.store.get_meta(f"fleet:pending-adopt:{node_id}:codex:thread-id")
        )

    def test_mutation_receipt_must_match_node_and_request(self) -> None:
        node_id = str(uuid.uuid4())
        node = FleetNode(node_id, "atlas", "atlas")
        self.store.upsert_fleet_node(node)
        request_id = str(uuid.uuid4())
        transport = FakeTransport(
            {
                "type": "adopted",
                "node_id": str(uuid.uuid4()),
                "request_id": request_id,
                "session": session_to_wire(Session("codex", "thread-id")),
            }
        )
        manager = FleetManager(self.store, transport)
        with self.assertRaisesRegex(FleetError, "wrong node identity"):
            manager.adopt(
                node,
                Candidate("codex", "thread-id", "remote-work"),
                request_id=request_id,
            )
        self.assertEqual(
            self.store.get_meta(f"fleet:pending-adopt:{node_id}:codex:thread-id"),
            request_id,
        )

    def test_node_removal_clears_pending_mutation_replays(self) -> None:
        node_id = str(uuid.uuid4())
        self.store.upsert_fleet_node(FleetNode(node_id, "atlas", "atlas"))
        keys = [
            f"fleet:pending-adopt:{node_id}:codex:a",
            f"fleet:pending-untrack:{node_id}:claude:b",
        ]
        for key in keys:
            self.store.set_meta(key, str(uuid.uuid4()))
        self.store.delete_fleet_node(node_id)
        self.assertEqual([self.store.get_meta(key) for key in keys], [None, None])

    def test_setup_yes_without_explicit_machine_never_discovers_or_connects(
        self,
    ) -> None:
        pika = Mock()
        args = argparse.Namespace(
            no_machines=False,
            dry_run=False,
            machine=[],
            yes=True,
        )
        self.assertEqual(_setup_machine_candidates(pika, args), [])
        pika.fleet.discover.assert_not_called()

    def test_discovery_report_explains_passive_candidate_filtering(self) -> None:
        manager = FleetManager(self.store)
        candidate = NodeCandidate(
            "atlas", "atlas.tail.test", ("tailscale",), os_name="linux"
        )
        with (
            patch.object(manager, "discover", return_value=[candidate]),
            patch("pikamux.fleet._ssh_config_files", return_value=[Path("config")]),
            patch(
                "pikamux.fleet.discover_ssh_candidates",
                return_value=[NodeCandidate("build", "build", ("ssh-config",))],
            ),
            patch(
                "pikamux.fleet.tailscale_discovery_summary",
                return_value={
                    "total": 7,
                    "compatible": 3,
                    "non_linux": 3,
                    "no_target": 1,
                    "error": None,
                },
            ),
        ):
            report = manager.discover_report()
        self.assertEqual(report.candidates, (candidate,))
        self.assertEqual(report.ssh_aliases, 1)
        self.assertEqual(report.ssh_config_files, 1)
        self.assertEqual(report.tailscale_total, 7)
        self.assertEqual(report.tailscale_compatible, 3)
        self.assertEqual(report.excluded_non_linux, 3)
        self.assertEqual(report.excluded_no_target, 1)

    def test_snapshot_rejects_duplicate_exact_identity(self) -> None:
        node_id = str(uuid.uuid4())
        value = snapshot(node_id)
        value["sessions"] = [value["sessions"][0], value["sessions"][0]]
        with self.assertRaisesRegex(FleetError, "repeats"):
            validate_snapshot(value, expected_node_id=node_id)

    def test_ssh_transport_rejects_option_and_newline_targets(self) -> None:
        transport = SSHTransport()
        for target in ("-ProxyCommand=bad", "host\nother"):
            with self.subTest(target=target), self.assertRaises(ValueError):
                transport.base(target)

    def test_ssh_payload_is_stdin_data_not_remote_command_syntax(self) -> None:
        response = Mock(returncode=0, stdout='{"type":"hello"}\n', stderr="")
        payload = {"op": "hello", "question": "$(touch /tmp/nope); rm -rf nope"}
        with patch.object(
            SSHTransport, "_bounded_run", return_value=response
        ) as runner:
            SSHTransport().request("ajain@atlas", payload)
        command = runner.call_args.args[0]
        self.assertEqual(command[-3:], ["pika", "_fleet", "--stdio"])
        self.assertFalse(any("touch" in item or "rm -rf" in item for item in command))
        self.assertEqual(json.loads(runner.call_args.args[1]), payload)

    def test_ssh_response_limit_is_enforced_while_streaming(self) -> None:
        transport = SSHTransport(overall_timeout=2)
        with (
            patch("pikamux.fleet.MAX_MESSAGE_BYTES", 10),
            self.assertRaisesRegex(FleetError, "safety limit"),
        ):
            transport._bounded_run(
                [sys.executable, "-c", "import sys; sys.stdout.write('x' * 20)"],
                b"",
            )

    def test_snapshot_cannot_be_stored_for_an_untrusted_node(self) -> None:
        with self.assertRaisesRegex(ValueError, "adopted fleet node"):
            self.store.put_remote_snapshot(str(uuid.uuid4()), {"type": "snapshot"})

    def test_protocol_snapshot_is_complete_jsonl_and_transcript_free(self) -> None:
        node_id = self.store.local_node_id()
        session = Session(
            "codex",
            "11111111-1111-4111-8111-111111111111",
            name="local",
            transcript_path="/secret/transcript.jsonl",
            tmux_pane="%1",
            root_pid=42,
            status=Status.PARKED.value,
        )
        pika = Mock()
        pika.store = self.store
        pika.refresh.return_value = [session]
        pika.expert_card_states.return_value = []
        stdin = io.StringIO(
            json.dumps(
                {
                    "op": "snapshot",
                    "protocol": PROTOCOL_NAME,
                    "version": PROTOCOL_VERSION,
                    "expected_node_id": node_id,
                }
            )
            + "\n"
        )
        stdout = io.StringIO()
        self.assertEqual(handle_fleet_stdio(pika, stdin, stdout), 0)
        lines = stdout.getvalue().splitlines()
        self.assertEqual(len(lines), 1)
        payload = json.loads(lines[0])
        self.assertEqual(payload["type"], "snapshot")
        encoded = json.dumps(payload)
        self.assertNotIn("transcript", encoded)
        self.assertNotIn("root_pid", encoded)
        self.assertNotIn("tmux_pane", encoded)

    def test_remote_untrack_request_is_idempotent_before_exact_lookup(self) -> None:
        node_id = str(uuid.uuid4())
        request_id = str(uuid.uuid4())
        session = Session("codex", "thread-id", name="local")
        receipts: dict[str, str] = {}
        untracked = False
        store = Mock()
        store.local_node_id.return_value = node_id
        store.get_meta.side_effect = lambda key: receipts.get(key)
        store.set_meta.side_effect = lambda key, value: receipts.__setitem__(key, value)
        store.is_untracked.side_effect = lambda _provider, _session_id: untracked
        store.get_session.return_value = session
        pika = Mock(store=store)
        pika.refresh.return_value = [session]

        def untrack_once(_session):
            nonlocal untracked
            untracked = True
            return 1

        pika.untrack.side_effect = untrack_once
        request = json.dumps(
            {
                "op": "untrack",
                "protocol": PROTOCOL_NAME,
                "version": PROTOCOL_VERSION,
                "expected_node_id": node_id,
                "provider": "codex",
                "session_id": "thread-id",
                "request_id": request_id,
            }
        )
        stdout = io.StringIO()
        handle_fleet_stdio(pika, io.StringIO(request + "\n" + request + "\n"), stdout)
        responses = [json.loads(line) for line in stdout.getvalue().splitlines()]
        self.assertEqual(responses[0], responses[1])
        pika.untrack.assert_called_once_with(session)

    def test_remote_card_freshness_is_remote_authority_plus_cache_staleness(
        self,
    ) -> None:
        node_id = str(uuid.uuid4())
        self.store.upsert_fleet_node(FleetNode(node_id, "atlas", "atlas"))
        self.store.put_remote_snapshot(node_id, snapshot(node_id))
        manager = FleetManager(self.store, FakeTransport())
        session = manager.cached_sessions()[0]
        self.assertEqual(session.card_status, "CURRENT")
        self.assertFalse(session.stale)
        self.store.mark_fleet_node_error(node_id, "unreachable", "timeout")
        stale = manager.cached_sessions()[0]
        self.assertEqual(stale.card_status, "CURRENT")
        self.assertTrue(stale.stale)

    def test_fresh_remote_experts_rank_before_stale_and_human_view_labels_cache(
        self,
    ) -> None:
        stale_id = str(uuid.uuid4())
        fresh_id = str(uuid.uuid4())
        for node_id, alias in ((stale_id, "stale"), (fresh_id, "fresh")):
            self.store.upsert_fleet_node(FleetNode(node_id, alias, alias))
            self.store.put_remote_snapshot(node_id, snapshot(node_id))
        self.store.mark_fleet_node_error(stale_id, "unreachable", "timeout")
        matches = FleetManager(self.store, FakeTransport()).expert_matches("pricing")
        self.assertEqual(
            [item.session.node_name for item in matches], ["fresh", "stale"]
        )
        output = io.StringIO()
        with patch("sys.stdout", output):
            print_experts(matches, query="pricing")
        self.assertIn("CACHED", output.getvalue())

    def test_remote_rows_never_trigger_automatic_pane_preview(self) -> None:
        session = FleetSession(
            str(uuid.uuid4()),
            "atlas",
            Session("codex", "thread-id", name="remote", tmux_session="remote"),
        )
        pika = Mock()
        key, lines, error = _capture_preview(pika, session)
        self.assertEqual(key, session.key)
        self.assertEqual(lines, [])
        self.assertIsNone(error)
        pika.capture.assert_not_called()
        pika.tmux.capture.assert_not_called()

    def test_remote_peek_returns_content_before_any_acknowledgement(self) -> None:
        session = FleetSession(
            str(uuid.uuid4()),
            "atlas",
            Session(
                "codex",
                "thread-id",
                name="remote",
                status=Status.READY.value,
                unread=True,
            ),
        )
        pika = Mock()
        pika.capture.return_value = "important result"
        _key, lines, error = _capture_remote_peek(pika, session)
        self.assertEqual(lines, ["important result"])
        self.assertIsNone(error)
        pika.acknowledge.assert_not_called()

    def test_stale_remote_row_cannot_act_as_current(self) -> None:
        session = FleetSession(
            str(uuid.uuid4()),
            "atlas",
            Session(
                "codex",
                "thread-id",
                name="remote",
                transcript_path=None,
                status=Status.NEEDS_YOU.value,
            ),
            stale=True,
            remote_error="timeout",
        )
        state = MonitorState(sessions=[session], selected_key=session.key)
        pika = Mock()
        for key in ("enter", "ask", "peek", "untrack"):
            with self.subTest(key=key):
                action, target = _handle_key(key, pika, state)
                self.assertEqual(action, "continue")
                self.assertIsNone(target)
        frame = render_monitor(state, width=120, height=28, color=False)
        self.assertIn("CACHED", frame.plain)

    def test_selection_key_includes_node_identity(self) -> None:
        session_id = "same-id"
        first = FleetSession(
            str(uuid.uuid4()),
            "atlas",
            Session("codex", session_id, name="same"),
        )
        second = FleetSession(
            str(uuid.uuid4()),
            "gpu",
            Session("codex", session_id, name="same"),
        )
        state = MonitorState(sessions=[first, second], selected_key=first.key)
        self.assertEqual(state.selected().key, first.key)
        state.move(1)
        self.assertEqual(state.selected().key, second.key)


class FakeProcess:
    def __init__(
        self,
        lines: list[dict[str, object]],
        *,
        raw_stdout: bytes | None = None,
    ) -> None:
        self.stdin = io.BytesIO()
        stdout_read, stdout_write = os.pipe()
        payload = raw_stdout
        if payload is None:
            payload = b"".join((json.dumps(line) + "\n").encode() for line in lines)
        os.write(stdout_write, payload)
        os.close(stdout_write)
        self.stdout = os.fdopen(stdout_read, "rb", buffering=0)
        stderr_read, stderr_write = os.pipe()
        os.close(stderr_write)
        self.stderr = os.fdopen(stderr_read, "rb", buffering=0)
        self.returncode = None

    def poll(self):
        return self.returncode

    def wait(self, timeout=None):
        self.returncode = 0
        return 0

    def terminate(self):
        self.returncode = -15

    def kill(self):
        self.returncode = -9


class RemoteConsultationTests(unittest.TestCase):
    def test_two_turns_share_one_ssh_process_and_are_never_retried(self) -> None:
        node_id = str(uuid.uuid4())
        policy = ConsultationPolicy("default", "gpt-5.6-sol", "medium")
        process = FakeProcess(
            [
                {
                    "type": "opened",
                    "provider": "codex",
                    "parent_id": "parent-id",
                    **policy.receipt(),
                },
                {"type": "answer", "text": "one"},
                {"type": "answer", "text": "two"},
            ]
        )
        node = FleetNode(node_id, "atlas", "atlas")
        session = FleetSession(
            node_id,
            "atlas",
            Session("codex", "parent-id", name="expert"),
        )
        with patch("pikamux.fleet.subprocess.Popen", return_value=process) as popen:
            consultation = RemoteConsultation(node, session, policy, SSHTransport())
            self.assertEqual(consultation.ask("first"), "one")
            self.assertEqual(consultation.ask("follow-up"), "two")
            sent = process.stdin.getvalue()
            consultation.close()
        popen.assert_called_once()
        self.assertEqual(sent.count(b'"question"'), 2)

    def test_open_timeout_closes_the_ssh_process(self) -> None:
        node_id = str(uuid.uuid4())
        process = FakeProcess([])
        node = FleetNode(node_id, "atlas", "atlas")
        session = FleetSession(node_id, "atlas", Session("codex", "parent-id"))
        policy = ConsultationPolicy("default", "gpt-5.6-sol", "medium")
        with (
            patch("pikamux.fleet.subprocess.Popen", return_value=process),
            patch.object(
                RemoteConsultation,
                "_read_event",
                side_effect=ConsultationError("Remote side channel timed out"),
            ),
            self.assertRaisesRegex(ConsultationError, "timed out"),
        ):
            RemoteConsultation(node, session, policy, SSHTransport())
        self.assertEqual(process.returncode, 0)

    def test_partial_jsonl_frame_fails_without_blocking_readline(self) -> None:
        node_id = str(uuid.uuid4())
        process = FakeProcess([], raw_stdout=b'{"type":"opened"')
        node = FleetNode(node_id, "atlas", "atlas")
        session = FleetSession(node_id, "atlas", Session("codex", "parent-id"))
        policy = ConsultationPolicy("default", "gpt-5.6-sol", "medium")
        with (
            patch("pikamux.fleet.subprocess.Popen", return_value=process),
            self.assertRaisesRegex(ConsultationError, "partial JSONL frame"),
        ):
            RemoteConsultation(node, session, policy, SSHTransport())
        self.assertIsNotNone(process.returncode)

    def test_remote_consultation_frame_limit_is_enforced_incrementally(self) -> None:
        node_id = str(uuid.uuid4())
        process = FakeProcess([], raw_stdout=b"x" * 20)
        node = FleetNode(node_id, "atlas", "atlas")
        session = FleetSession(node_id, "atlas", Session("codex", "parent-id"))
        policy = ConsultationPolicy("default", "gpt-5.6-sol", "medium")
        with (
            patch("pikamux.fleet.subprocess.Popen", return_value=process),
            patch("pikamux.fleet.MAX_MESSAGE_BYTES", 10),
            self.assertRaisesRegex(ConsultationError, "safety limit"),
        ):
            RemoteConsultation(node, session, policy, SSHTransport())
        self.assertIsNotNone(process.returncode)

    def test_trickling_partial_frame_obeys_absolute_deadline_and_is_killed(
        self,
    ) -> None:
        consultation = object.__new__(RemoteConsultation)
        consultation.process = subprocess.Popen(
            [
                sys.executable,
                "-c",
                "import sys,time; sys.stdout.write('{'); sys.stdout.flush(); time.sleep(5)",
            ],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            bufsize=0,
        )
        consultation._stdout_buffer = bytearray()
        consultation._stderr_buffer = bytearray()
        started = time.monotonic()
        with self.assertRaisesRegex(ConsultationError, "timed out"):
            consultation._read_event(0.1)
        self.assertLess(time.monotonic() - started, 1.0)
        self.assertIsNotNone(consultation.process.poll())


if __name__ == "__main__":
    unittest.main()
