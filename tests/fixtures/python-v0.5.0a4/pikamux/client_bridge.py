from __future__ import annotations

import hmac
import json
import os
import re
import socket
import subprocess
import time
import uuid
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable

from .paths import config_home

BRIDGE_PROTOCOL = "pikamux-client-launch"
BRIDGE_VERSION = 2
DEFAULT_LOCAL_PORT = 47653
DEFAULT_REMOTE_PORT = 47654
MAX_BRIDGE_MESSAGE_BYTES = 16 * 1024
_PROVIDERS = {"codex", "claude", "opencode"}
_ALIAS = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]{0,62}$")
_REQUEST_FIELDS = {
    "type",
    "protocol",
    "version",
    "request_id",
    "client_id",
    "token",
    "source_node_id",
    "target_node_id",
    "provider",
    "session_id",
}
_RECEIPT_FIELDS = {
    "type",
    "protocol",
    "version",
    "request_id",
    "target_node_id",
    "provider",
    "session_id",
    "detail",
}
_PAIR_FIELDS = {
    "type",
    "protocol",
    "version",
    "expected_node_id",
    "client_id",
    "client_label",
    "token",
    "port",
}


class ClientBridgeError(RuntimeError):
    pass


class ClientBridgeUnavailable(ClientBridgeError):
    pass


@dataclass(frozen=True, slots=True)
class ClientNode:
    node_id: str
    alias: str
    ssh_target: str
    token: str


@dataclass(frozen=True, slots=True)
class ClientLaunchReceipt:
    request_id: str
    target_node_id: str
    provider: str
    session_id: str
    detail: str


def client_config_path() -> Path:
    override = os.environ.get("PIKA_CLIENT_CONFIG")
    return Path(override).expanduser() if override else config_home() / "client.json"


def _uuid(value: object, label: str) -> str:
    try:
        return str(uuid.UUID(str(value)))
    except (ValueError, TypeError, AttributeError):
        raise ClientBridgeError(f"Invalid {label}") from None


def _conversation_identity(provider: str, value: object) -> str:
    if provider == "opencode":
        if (
            not isinstance(value, str)
            or not value.startswith("ses_")
            or not 8 <= len(value) <= 128
            or not value[4:].isalnum()
        ):
            raise ClientBridgeError("Invalid conversation identity")
        return value
    return _uuid(value, "conversation identity")


def _token(value: object) -> str:
    if not isinstance(value, str) or len(value) != 64:
        raise ClientBridgeError("Invalid bridge token")
    try:
        bytes.fromhex(value)
    except ValueError:
        raise ClientBridgeError("Invalid bridge token") from None
    return value.casefold()


def _ssh_target(value: object) -> str:
    if (
        not isinstance(value, str)
        or not value
        or value.startswith("-")
        or any(character in value for character in "\r\n\x00")
        or len(value) > 255
    ):
        raise ClientBridgeError("Invalid SSH target")
    return value


def load_client_config(path: Path | None = None) -> dict[str, Any]:
    target = path or client_config_path()
    try:
        value = json.loads(target.read_text())
    except FileNotFoundError:
        return {"version": 1, "client_id": str(uuid.uuid4()), "nodes": {}}
    except (OSError, ValueError) as exc:
        raise ClientBridgeError(
            f"Cannot read Pika client configuration: {exc}"
        ) from exc
    if not isinstance(value, dict) or value.get("version") != 1:
        raise ClientBridgeError("Unsupported Pika client configuration")
    value["client_id"] = _uuid(value.get("client_id"), "client identity")
    nodes = value.get("nodes")
    if not isinstance(nodes, dict):
        raise ClientBridgeError("Pika client node configuration is malformed")
    return value


def write_client_config(value: dict[str, Any], path: Path | None = None) -> None:
    target = path or client_config_path()
    target.parent.mkdir(parents=True, exist_ok=True)
    temporary = target.with_name(f".{target.name}.{os.getpid()}.tmp")
    temporary.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n")
    try:
        os.chmod(temporary, 0o600)
    except OSError:
        pass
    os.replace(temporary, target)


def client_nodes(config: dict[str, Any]) -> dict[str, ClientNode]:
    result: dict[str, ClientNode] = {}
    raw_nodes = config.get("nodes")
    if not isinstance(raw_nodes, dict):
        raise ClientBridgeError("Pika client node configuration is malformed")
    for raw_id, value in raw_nodes.items():
        if not isinstance(value, dict):
            raise ClientBridgeError("Pika client node entry is malformed")
        node_id = _uuid(raw_id, "node identity")
        alias = client_alias(value.get("alias"))
        result[node_id] = ClientNode(
            node_id=node_id,
            alias=alias,
            ssh_target=_ssh_target(value.get("ssh_target")),
            token=_token(value.get("token")),
        )
    return result


def client_alias(value: object) -> str:
    if not isinstance(value, str) or not _ALIAS.fullmatch(value):
        raise ClientBridgeError("Pika client node alias is malformed")
    return value


def make_launch_request(
    *,
    client_id: str,
    token: str,
    source_node_id: str,
    target_node_id: str,
    provider: str,
    session_id: str,
    request_id: str | None = None,
) -> dict[str, Any]:
    return validate_launch_request(
        {
            "type": "open",
            "protocol": BRIDGE_PROTOCOL,
            "version": BRIDGE_VERSION,
            "request_id": request_id or str(uuid.uuid4()),
            "client_id": client_id,
            "token": token,
            "source_node_id": source_node_id,
            "target_node_id": target_node_id,
            "provider": provider,
            "session_id": session_id,
        }
    )


def make_pair_request(
    *,
    expected_node_id: str,
    client_id: str,
    client_label: str,
    token: str,
    port: int = DEFAULT_REMOTE_PORT,
) -> dict[str, Any]:
    return validate_pair_request(
        {
            "type": "pair",
            "protocol": BRIDGE_PROTOCOL,
            "version": BRIDGE_VERSION,
            "expected_node_id": expected_node_id,
            "client_id": client_id,
            "client_label": client_label,
            "token": token,
            "port": port,
        }
    )


def validate_pair_request(value: object) -> dict[str, Any]:
    if not isinstance(value, dict) or set(value) != _PAIR_FIELDS:
        raise ClientBridgeError("Malformed client pairing request")
    if (
        value.get("type") != "pair"
        or value.get("protocol") != BRIDGE_PROTOCOL
        or value.get("version") != BRIDGE_VERSION
    ):
        raise ClientBridgeError("Incompatible client pairing request")
    label = value.get("client_label")
    if (
        not isinstance(label, str)
        or not label
        or len(label) > 63
        or any(not character.isprintable() for character in label)
    ):
        raise ClientBridgeError("Invalid client label")
    port = value.get("port")
    if isinstance(port, bool) or not isinstance(port, int) or not 1024 <= port <= 65535:
        raise ClientBridgeError("Invalid reverse bridge port")
    return {
        **value,
        "expected_node_id": _uuid(value.get("expected_node_id"), "node identity"),
        "client_id": _uuid(value.get("client_id"), "client identity"),
        "token": _token(value.get("token")),
        "client_label": label,
        "port": port,
    }


def validate_launch_request(value: object) -> dict[str, Any]:
    if not isinstance(value, dict) or set(value) != _REQUEST_FIELDS:
        raise ClientBridgeError("Malformed client launch request")
    if (
        value.get("type") != "open"
        or value.get("protocol") != BRIDGE_PROTOCOL
        or value.get("version") != BRIDGE_VERSION
    ):
        raise ClientBridgeError("Incompatible client launch request")
    provider = value.get("provider")
    if provider not in _PROVIDERS:
        raise ClientBridgeError("Invalid provider")
    return {
        **value,
        "request_id": _uuid(value.get("request_id"), "request identity"),
        "client_id": _uuid(value.get("client_id"), "client identity"),
        "token": _token(value.get("token")),
        "source_node_id": _uuid(value.get("source_node_id"), "source node identity"),
        "target_node_id": _uuid(value.get("target_node_id"), "target node identity"),
        "session_id": _conversation_identity(provider, value.get("session_id")),
    }


def windows_terminal_command(
    node: ClientNode,
    *,
    provider: str,
    session_id: str,
    terminal_executable: str = "wt.exe",
    ssh_executable: str = "ssh.exe",
) -> list[str]:
    if provider not in _PROVIDERS:
        raise ClientBridgeError("Invalid provider")
    exact_session_id = _conversation_identity(provider, session_id)
    return [
        terminal_executable,
        "-w",
        "new",
        "new-tab",
        "--title",
        f"Pika · {node.alias} · {provider}-{exact_session_id[:8]}",
        ssh_executable,
        "-tt",
        "-o",
        "ClearAllForwardings=yes",
        node.ssh_target,
        "pika",
        "_fleet-open",
        "--expected-node-id",
        node.node_id,
        "--provider",
        provider,
        "--session-id",
        exact_session_id,
    ]


def _default_launcher(command: list[str]) -> None:
    flags = 0
    if os.name == "nt":
        flags = getattr(subprocess, "CREATE_NEW_PROCESS_GROUP", 0)
    subprocess.Popen(
        command,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        close_fds=True,
        creationflags=flags,
    )


class ClientLaunchBridge:
    """A loopback-only exact-identity launcher for an explicitly paired client."""

    def __init__(
        self,
        config: dict[str, Any],
        *,
        launcher: Callable[[list[str]], None] | None = None,
        terminal_executable: str = "wt.exe",
        ssh_executable: str = "ssh.exe",
    ) -> None:
        self.client_id = _uuid(config.get("client_id"), "client identity")
        self.nodes = client_nodes(config)
        self.launcher = launcher or _default_launcher
        self.terminal_executable = terminal_executable
        self.ssh_executable = ssh_executable
        self._receipts: dict[str, tuple[float, dict[str, Any]]] = {}

    def reload(self, config: dict[str, Any]) -> None:
        """Adopt an atomically replaced pairing file without restarting."""
        client_id = _uuid(config.get("client_id"), "client identity")
        if client_id != self.client_id:
            raise ClientBridgeError("Client identity changed while bridge was running")
        self.nodes = client_nodes(config)

    def handle(self, value: object) -> dict[str, Any]:
        if value == {
            "type": "ping",
            "protocol": BRIDGE_PROTOCOL,
            "version": BRIDGE_VERSION,
        }:
            return {
                "type": "pong",
                "protocol": BRIDGE_PROTOCOL,
                "version": BRIDGE_VERSION,
            }
        request = validate_launch_request(value)
        if request["client_id"] != self.client_id:
            raise ClientBridgeError("Client identity does not match this bridge")
        source = self.nodes.get(request["source_node_id"])
        if source is None or not hmac.compare_digest(source.token, request["token"]):
            raise ClientBridgeError("Source Pika node is not paired with this bridge")
        target = self.nodes.get(request["target_node_id"])
        if target is None:
            raise ClientBridgeError("Target Pika node is not paired on this client")

        now = time.monotonic()
        self._receipts = {
            key: item for key, item in self._receipts.items() if now - item[0] < 60
        }
        prior = self._receipts.get(request["request_id"])
        if prior is not None:
            return prior[1]

        command = windows_terminal_command(
            target,
            provider=request["provider"],
            session_id=request["session_id"],
            terminal_executable=self.terminal_executable,
            ssh_executable=self.ssh_executable,
        )
        self.launcher(command)
        receipt = {
            "type": "launched",
            "protocol": BRIDGE_PROTOCOL,
            "version": BRIDGE_VERSION,
            "request_id": request["request_id"],
            "target_node_id": target.node_id,
            "provider": request["provider"],
            "session_id": request["session_id"],
            "detail": (
                f"WINDOW LAUNCHED · {target.alias} · "
                f"id {request['session_id'][:8]}"
            ),
        }
        self._receipts[request["request_id"]] = (now, receipt)
        return receipt


def _read_message(connection: socket.socket) -> dict[str, Any]:
    payload = bytearray()
    while b"\n" not in payload:
        chunk = connection.recv(min(4096, MAX_BRIDGE_MESSAGE_BYTES + 1 - len(payload)))
        if not chunk:
            break
        payload.extend(chunk)
        if len(payload) > MAX_BRIDGE_MESSAGE_BYTES:
            raise ClientBridgeError("Client bridge request exceeded the safety limit")
    line, separator, trailing = bytes(payload).partition(b"\n")
    if not separator or trailing.strip():
        raise ClientBridgeError("Client bridge requires exactly one JSON line")
    try:
        value = json.loads(line)
    except (ValueError, UnicodeDecodeError):
        raise ClientBridgeError("Client bridge received invalid JSON") from None
    if not isinstance(value, dict):
        raise ClientBridgeError("Client bridge request must be an object")
    return value


def _send_message(connection: socket.socket, value: dict[str, Any]) -> None:
    encoded = (
        json.dumps(value, ensure_ascii=False, separators=(",", ":")).encode()
        + b"\n"
    )
    connection.sendall(encoded)


def serve_client_bridge(
    bridge: ClientLaunchBridge,
    *,
    host: str = "127.0.0.1",
    port: int = DEFAULT_LOCAL_PORT,
    config_loader: Callable[[], dict[str, Any]] | None = None,
) -> None:
    if host not in {"127.0.0.1", "::1", "localhost"}:
        raise ClientBridgeError("The Pika client bridge may listen only on loopback")
    family = socket.AF_INET6 if host == "::1" else socket.AF_INET
    with socket.create_server((host, port), family=family) as server:
        server.settimeout(0.5)
        while True:
            try:
                connection, _address = server.accept()
            except socket.timeout:
                continue
            with connection:
                connection.settimeout(1.0)
                try:
                    if config_loader is not None:
                        bridge.reload(config_loader())
                    response = bridge.handle(_read_message(connection))
                except (ClientBridgeError, OSError) as exc:
                    response = {
                        "type": "error",
                        "protocol": BRIDGE_PROTOCOL,
                        "version": BRIDGE_VERSION,
                        "message": str(exc),
                    }
                _send_message(connection, response)


def request_client_launch(
    request: dict[str, Any],
    *,
    host: str = "127.0.0.1",
    port: int = DEFAULT_REMOTE_PORT,
    timeout: float = 0.35,
) -> ClientLaunchReceipt:
    validated = validate_launch_request(request)
    encoded = (
        json.dumps(validated, ensure_ascii=False, separators=(",", ":")).encode()
        + b"\n"
    )
    try:
        with socket.create_connection((host, port), timeout=timeout) as connection:
            connection.settimeout(timeout)
            connection.sendall(encoded)
            response = _read_message(connection)
    except (OSError, TimeoutError) as exc:
        raise ClientBridgeUnavailable(f"Client bridge unavailable: {exc}") from exc
    if response.get("type") == "error":
        raise ClientBridgeError(
            str(response.get("message") or "Client bridge rejected the request")
        )
    if set(response) != _RECEIPT_FIELDS:
        raise ClientBridgeError("Malformed client launch receipt")
    if (
        response.get("type") != "launched"
        or response.get("protocol") != BRIDGE_PROTOCOL
        or response.get("version") != BRIDGE_VERSION
    ):
        raise ClientBridgeError("Incompatible client launch receipt")
    for field in ("request_id", "target_node_id", "provider", "session_id"):
        if response.get(field) != validated.get(field):
            raise ClientBridgeError("Client launch receipt identity mismatch")
    detail = response.get("detail")
    if not isinstance(detail, str) or not detail or len(detail) > 500:
        raise ClientBridgeError("Malformed client launch receipt detail")
    return ClientLaunchReceipt(
        request_id=validated["request_id"],
        target_node_id=validated["target_node_id"],
        provider=validated["provider"],
        session_id=validated["session_id"],
        detail=detail,
    )


def client_bridge_running(
    *,
    host: str = "127.0.0.1",
    port: int = DEFAULT_LOCAL_PORT,
    timeout: float = 0.2,
) -> bool:
    request = {
        "type": "ping",
        "protocol": BRIDGE_PROTOCOL,
        "version": BRIDGE_VERSION,
    }
    try:
        with socket.create_connection((host, port), timeout=timeout) as connection:
            connection.settimeout(timeout)
            _send_message(connection, request)
            response = _read_message(connection)
    except (OSError, TimeoutError, ClientBridgeError):
        return False
    return response == {
        "type": "pong",
        "protocol": BRIDGE_PROTOCOL,
        "version": BRIDGE_VERSION,
    }
