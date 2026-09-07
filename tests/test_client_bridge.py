from __future__ import annotations

import argparse
import io
import tempfile
import unittest
import uuid
from pathlib import Path
from unittest.mock import Mock, patch

from pikamux.cli import _pair_client_bridge
from pikamux.client_bridge import (
    BRIDGE_PROTOCOL,
    BRIDGE_VERSION,
    ClientBridgeError,
    ClientBridgeUnavailable,
    ClientLaunchBridge,
    ClientLaunchReceipt,
    ClientNode,
    client_nodes,
    make_launch_request,
    make_pair_request,
    windows_terminal_command,
)
from pikamux.client_cli import _pair
from pikamux.core import Pika, PikaError
from pikamux.models import FleetSession, Session


class ClientBridgeTests(unittest.TestCase):
    def setUp(self) -> None:
        self.client_id = str(uuid.uuid4())
        self.source_id = str(uuid.uuid4())
        self.target_id = str(uuid.uuid4())
        self.session_id = str(uuid.uuid4())
        self.source_token = "ab" * 32
        self.config = {
            "version": 1,
            "client_id": self.client_id,
            "nodes": {
                self.source_id: {
                    "alias": "devbox",
                    "ssh_target": "devbox",
                    "token": self.source_token,
                },
                self.target_id: {
                    "alias": "gpu-box",
                    "ssh_target": "gpu-box",
                    "token": "cd" * 32,
                },
            },
        }

    def request(self, **overrides):
        values = {
            "client_id": self.client_id,
            "token": self.source_token,
            "source_node_id": self.source_id,
            "target_node_id": self.target_id,
            "provider": "codex",
            "session_id": self.session_id,
            "request_id": str(uuid.uuid4()),
        }
        values.update(overrides)
        return make_launch_request(**values)

    def test_command_uses_only_paired_node_and_exact_identity(self) -> None:
        node = ClientNode(self.target_id, "gpu-box", "developer@gpu-box", "cd" * 32)
        command = windows_terminal_command(
            node,
            provider="claude",
            session_id=self.session_id,
        )
        self.assertEqual(command[:4], ["wt.exe", "-w", "new", "new-tab"])
        self.assertIn("ssh.exe", command)
        self.assertIn("ClearAllForwardings=yes", command)
        self.assertIn("developer@gpu-box", command)
        self.assertIn("_fleet-open", command)
        self.assertIn(self.target_id, command)
        self.assertIn(self.session_id, command)
        self.assertNotIn("--continue", command)
        self.assertNotIn("--last", command)

        opencode_id = "ses_fdd613642ffeZLuODNNxjAL3h7"
        opencode = windows_terminal_command(
            node,
            provider="opencode",
            session_id=opencode_id,
        )
        self.assertIn("opencode", opencode)
        self.assertIn(opencode_id, opencode)

    def test_opencode_native_identity_survives_bridge_validation(self) -> None:
        session_id = "ses_fdd613642ffeZLuODNNxjAL3h7"
        request = self.request(provider="opencode", session_id=session_id)
        self.assertEqual(request["provider"], "opencode")
        self.assertEqual(request["session_id"], session_id)
        with self.assertRaisesRegex(ClientBridgeError, "conversation identity"):
            self.request(provider="opencode", session_id="ses_../../bad")

    def test_untrusted_command_text_and_non_uuid_identity_are_rejected(self) -> None:
        with self.assertRaisesRegex(ClientBridgeError, "conversation identity"):
            self.request(session_id="$(touch /tmp/nope)")
        malformed = {
            **self.config,
            "nodes": {
                self.target_id: {
                    "alias": "bad",
                    "ssh_target": "-oProxyCommand=bad",
                    "token": "cd" * 32,
                }
            },
        }
        with self.assertRaisesRegex(ClientBridgeError, "SSH target"):
            client_nodes(malformed)

    def test_authenticated_launch_is_deduplicated_by_request_id(self) -> None:
        launched: list[list[str]] = []
        bridge = ClientLaunchBridge(self.config, launcher=launched.append)
        request = self.request()
        first = bridge.handle(request)
        second = bridge.handle(request)
        self.assertEqual(first, second)
        self.assertEqual(len(launched), 1)
        self.assertEqual(first["type"], "launched")
        self.assertEqual(first["target_node_id"], self.target_id)
        self.assertEqual(first["session_id"], self.session_id)

    def test_unknown_source_or_target_fails_closed_without_launch(self) -> None:
        launcher = Mock()
        bridge = ClientLaunchBridge(self.config, launcher=launcher)
        with self.assertRaisesRegex(ClientBridgeError, "Source Pika node"):
            bridge.handle(self.request(token="ef" * 32))
        with self.assertRaisesRegex(ClientBridgeError, "Target Pika node"):
            bridge.handle(self.request(target_node_id=str(uuid.uuid4())))
        launcher.assert_not_called()

    def test_running_bridge_reloads_atomically_replaced_pairings(self) -> None:
        launched: list[list[str]] = []
        first = {
            **self.config,
            "nodes": {self.source_id: self.config["nodes"][self.source_id]},
        }
        bridge = ClientLaunchBridge(first, launcher=launched.append)
        with self.assertRaisesRegex(ClientBridgeError, "Target Pika node"):
            bridge.handle(self.request())
        bridge.reload(self.config)
        receipt = bridge.handle(self.request())
        self.assertEqual(receipt["target_node_id"], self.target_id)
        self.assertEqual(len(launched), 1)

    def test_pika_routes_local_and_fleet_rows_by_node_uuid(self) -> None:
        source = self.source_id
        bridge_config = {
            "client_bridges": [
                {
                    "client_id": self.client_id,
                    "token": self.source_token,
                    "port": 49000,
                }
            ]
        }
        pika = Pika.__new__(Pika)
        pika.store = Mock()
        pika.store.local_node_id.return_value = source
        receipt = ClientLaunchReceipt(
            str(uuid.uuid4()), self.target_id, "codex", self.session_id, "launched"
        )
        remote = FleetSession(
            self.target_id,
            "gpu-box",
            Session("codex", self.session_id, name="remote"),
        )
        with (
            patch("pikamux.core.load_config", return_value=bridge_config),
            patch.dict("pikamux.core.os.environ", {"SSH_CONNECTION": "client server"}),
            patch("pikamux.core.request_client_launch", return_value=receipt) as launch,
        ):
            self.assertEqual(pika.open_on_client(remote), receipt)
        request = launch.call_args.args[0]
        self.assertEqual(request["source_node_id"], source)
        self.assertEqual(request["target_node_id"], self.target_id)
        self.assertEqual(request["provider"], "codex")
        self.assertEqual(request["session_id"], self.session_id)

    def test_reachable_identity_rejection_does_not_fall_through(self) -> None:
        pika = Pika.__new__(Pika)
        pika.store = Mock()
        pika.store.local_node_id.return_value = self.source_id
        config = {
            "client_bridges": [
                {"client_id": self.client_id, "token": self.source_token}
            ]
        }
        with (
            patch("pikamux.core.load_config", return_value=config),
            patch.dict("pikamux.core.os.environ", {"SSH_CONNECTION": "yes"}),
            patch(
                "pikamux.core.request_client_launch",
                side_effect=ClientBridgeError("identity mismatch"),
            ),
            self.assertRaisesRegex(PikaError, "CLIENT WINDOW BLOCKED"),
        ):
            pika.open_on_client(Session("codex", self.session_id))

    def test_absent_reverse_tunnel_preserves_current_terminal_fallback(self) -> None:
        pika = Pika.__new__(Pika)
        pika.store = Mock()
        pika.store.local_node_id.return_value = self.source_id
        config = {
            "client_bridges": [
                {"client_id": self.client_id, "token": self.source_token}
            ]
        }
        with (
            patch("pikamux.core.load_config", return_value=config),
            patch.dict("pikamux.core.os.environ", {"SSH_CONNECTION": "yes"}),
            patch(
                "pikamux.core.request_client_launch",
                side_effect=ClientBridgeUnavailable("connection refused"),
            ),
        ):
            self.assertIsNone(
                pika.open_on_client(Session("codex", self.session_id))
            )

    def test_remote_pairing_persists_secret_with_identity_receipt(self) -> None:
        node_id = self.source_id
        request = make_pair_request(
            expected_node_id=node_id,
            client_id=self.client_id,
            client_label="my-laptop",
            token=self.source_token,
            port=49000,
        )
        pika = Mock()
        pika.store.local_node_id.return_value = node_id
        output = io.StringIO()
        with (
            patch("pikamux.cli.sys.stdin", io.StringIO(json_line(request))),
            patch("pikamux.cli.sys.stdout", output),
            patch("pikamux.cli.load_config", return_value={"version": 1}),
            patch("pikamux.cli.config_path", return_value=Path("/missing/config")),
            patch("pikamux.cli.write_config") as writer,
        ):
            self.assertEqual(_pair_client_bridge(pika), 0)
        saved = writer.call_args.args[0]["client_bridges"][0]
        self.assertEqual(saved["client_id"], self.client_id)
        self.assertEqual(saved["token"], self.source_token)
        receipt = json_load(output.getvalue())
        self.assertEqual(receipt["type"], "paired")
        self.assertEqual(receipt["node_id"], node_id)

    def test_client_setup_requires_two_identity_receipts_before_saving(self) -> None:
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        target = Path(temp.name) / "client.json"
        args = argparse.Namespace(
            ssh_target="devbox",
            alias=None,
            ssh_executable="ssh.exe",
            remote_port=49000,
            no_start=True,
        )
        hello = {"node_id": self.source_id, "machine": "devbox"}

        def paired(_target, _arguments, payload, **_kwargs):
            return {
                "type": "paired",
                "protocol": BRIDGE_PROTOCOL,
                "version": BRIDGE_VERSION,
                "node_id": self.source_id,
                "client_id": payload["client_id"],
                "port": 49000,
            }

        with (
            patch("pikamux.client_cli.client_config_path", return_value=target),
            patch("pikamux.client_cli.load_client_config", return_value={
                "version": 1, "client_id": self.client_id, "nodes": {}
            }),
            patch("pikamux.client_cli._hello", return_value=hello),
            patch("pikamux.client_cli._ssh_json", side_effect=paired),
            patch("pikamux.client_cli.write_client_config") as writer,
            patch("builtins.print"),
        ):
            self.assertEqual(_pair(args), 0)
        saved = writer.call_args.args[0]
        self.assertEqual(saved["nodes"][self.source_id]["ssh_target"], "devbox")
        self.assertEqual(len(saved["nodes"][self.source_id]["token"]), 64)


def json_line(value: dict[str, object]) -> str:
    import json

    return json.dumps(value) + "\n"


def json_load(value: str) -> dict[str, object]:
    import json

    return json.loads(value)


if __name__ == "__main__":
    unittest.main()
