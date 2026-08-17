from __future__ import annotations

import argparse
import json
import os
import secrets
import socket
import subprocess
import sys
import uuid
from pathlib import Path
from typing import Any

from . import __version__
from .client_bridge import (
    BRIDGE_PROTOCOL,
    BRIDGE_VERSION,
    DEFAULT_LOCAL_PORT,
    DEFAULT_REMOTE_PORT,
    ClientBridgeError,
    ClientLaunchBridge,
    client_alias,
    client_bridge_running,
    client_config_path,
    client_nodes,
    load_client_config,
    make_pair_request,
    serve_client_bridge,
    write_client_config,
)

FLEET_PROTOCOL = "pikamux-fleet"
FLEET_VERSION = 1


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="pika",
        description="Pika client cockpit and secure local-window bridge.",
    )
    parser.add_argument("--version", action="version", version=f"pikamux {__version__}")
    sub = parser.add_subparsers(dest="command")
    setup = sub.add_parser(
        "setup", help="pair this client with a trusted Pika machine over SSH"
    )
    setup.add_argument("ssh_target")
    setup.add_argument("--alias")
    setup.add_argument(
        "--ssh-executable", default="ssh.exe" if os.name == "nt" else "ssh"
    )
    setup.add_argument("--remote-port", type=int, default=DEFAULT_REMOTE_PORT)
    setup.add_argument(
        "--no-start",
        action="store_true",
        help="pair only; do not start the local bridge in the background",
    )

    bridge = sub.add_parser("bridge", help="manage the local new-window bridge")
    bridge_sub = bridge.add_subparsers(dest="bridge_command")
    serve = bridge_sub.add_parser(
        "serve", help="serve paired launch requests on loopback"
    )
    serve.add_argument("--host", default="127.0.0.1")
    serve.add_argument("--port", type=int, default=DEFAULT_LOCAL_PORT)
    serve.add_argument("--terminal-executable", default="wt.exe")
    serve.add_argument(
        "--ssh-executable", default="ssh.exe" if os.name == "nt" else "ssh"
    )
    start = bridge_sub.add_parser(
        "start", help="start the local bridge in the background"
    )
    start.add_argument("--host", default="127.0.0.1")
    start.add_argument("--port", type=int, default=DEFAULT_LOCAL_PORT)
    start.add_argument("--terminal-executable", default="wt.exe")
    start.add_argument(
        "--ssh-executable", default="ssh.exe" if os.name == "nt" else "ssh"
    )
    bridge_sub.add_parser("status", help="show paired nodes and bridge instructions")
    return parser


def _ssh_json(
    ssh_target: str,
    remote_arguments: list[str],
    payload: dict[str, Any],
    *,
    executable: str,
    timeout: float = 20.0,
) -> dict[str, Any]:
    if (
        not ssh_target
        or ssh_target.startswith("-")
        or any(character in ssh_target for character in "\r\n\x00")
    ):
        raise ClientBridgeError("Invalid SSH target")
    line = json.dumps(payload, ensure_ascii=False, separators=(",", ":")) + "\n"
    try:
        result = subprocess.run(
            [
                executable,
                "-T",
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=8",
                ssh_target,
                "pika",
                *remote_arguments,
            ],
            input=line,
            capture_output=True,
            text=True,
            timeout=timeout,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired) as exc:
        raise ClientBridgeError(f"SSH pairing failed: {exc}") from exc
    if result.returncode != 0:
        detail = (result.stderr or result.stdout).strip()
        raise ClientBridgeError(detail or f"SSH pairing exited {result.returncode}")
    lines = [item for item in result.stdout.splitlines() if item.strip()]
    if len(lines) != 1:
        raise ClientBridgeError("Remote Pika returned a malformed pairing receipt")
    try:
        response = json.loads(lines[0])
    except ValueError:
        raise ClientBridgeError(
            "Remote Pika returned non-JSON pairing output"
        ) from None
    if not isinstance(response, dict):
        raise ClientBridgeError("Remote Pika pairing response is not an object")
    return response


def _hello(ssh_target: str, *, executable: str) -> dict[str, Any]:
    response = _ssh_json(
        ssh_target,
        ["_fleet", "--stdio"],
        {"op": "hello", "protocol": FLEET_PROTOCOL, "version": FLEET_VERSION},
        executable=executable,
    )
    if (
        response.get("type") != "hello"
        or response.get("protocol") != FLEET_PROTOCOL
        or response.get("version") != FLEET_VERSION
    ):
        raise ClientBridgeError("Remote machine did not prove a compatible Pika node")
    try:
        node_id = str(uuid.UUID(str(response.get("node_id"))))
    except ValueError:
        raise ClientBridgeError(
            "Remote Pika returned an invalid node identity"
        ) from None
    machine = response.get("machine")
    if not isinstance(machine, str) or not machine or len(machine) > 63:
        raise ClientBridgeError("Remote Pika returned an invalid machine name")
    return {**response, "node_id": node_id, "machine": machine}


def _pair(args: argparse.Namespace) -> int:
    if not 1024 <= args.remote_port <= 65535:
        raise ClientBridgeError("Reverse bridge port must be between 1024 and 65535")
    config = load_client_config()
    hello = _hello(args.ssh_target, executable=args.ssh_executable)
    node_id = hello["node_id"]
    client_id = str(config["client_id"])
    token = secrets.token_hex(32)
    label = socket.gethostname().split(".", 1)[0] or "pika-client"
    request = make_pair_request(
        expected_node_id=node_id,
        client_id=client_id,
        client_label=label,
        token=token,
        port=args.remote_port,
    )
    receipt = _ssh_json(
        args.ssh_target,
        ["_client-pair", "--stdio"],
        request,
        executable=args.ssh_executable,
    )
    expected = {
        "type": "paired",
        "protocol": BRIDGE_PROTOCOL,
        "version": BRIDGE_VERSION,
        "node_id": node_id,
        "client_id": client_id,
        "port": args.remote_port,
    }
    if receipt != expected:
        raise ClientBridgeError(
            "Remote Pika pairing receipt failed identity validation"
        )

    nodes = config.setdefault("nodes", {})
    assert isinstance(nodes, dict)
    nodes[node_id] = {
        "alias": client_alias(args.alias or hello["machine"]),
        "ssh_target": args.ssh_target,
        "token": token,
        "remote_port": args.remote_port,
    }
    write_client_config(config)
    print(
        f"PAIRED · {nodes[node_id]['alias']} · node {node_id[:8]} · "
        "exact launches only"
    )
    print("\nAdd this to the matching Host block in your local SSH config:")
    print(
        f"  RemoteForward 127.0.0.1:{args.remote_port} "
        f"127.0.0.1:{DEFAULT_LOCAL_PORT}"
    )
    print("\nThen keep the local bridge available with:")
    if not getattr(args, "no_start", False):
        _start_bridge(
            host="127.0.0.1",
            port=DEFAULT_LOCAL_PORT,
            terminal_executable="wt.exe",
            ssh_executable=args.ssh_executable,
        )
    print("  pika bridge start")
    print(
        "Reconnect SSH after adding the forward. Enter in the remote Pika board "
        "will then launch a new local Windows Terminal window."
    )
    return 0


def _start_bridge(
    *,
    host: str,
    port: int,
    terminal_executable: str,
    ssh_executable: str,
) -> int:
    if client_bridge_running(host=host, port=port):
        print(f"CLIENT BRIDGE READY · loopback {host}:{port}")
        return 0
    log_path = client_config_path().with_name("client-bridge.log")
    log_path.parent.mkdir(parents=True, exist_ok=True)
    command = [
        sys.executable,
        "-m",
        "pikamux",
        "bridge",
        "serve",
        "--host",
        host,
        "--port",
        str(port),
        "--terminal-executable",
        terminal_executable,
        "--ssh-executable",
        ssh_executable,
    ]
    flags = 0
    kwargs: dict[str, Any] = {}
    if os.name == "nt":
        flags = getattr(subprocess, "CREATE_NEW_PROCESS_GROUP", 0) | getattr(
            subprocess, "DETACHED_PROCESS", 0
        )
    else:
        kwargs["start_new_session"] = True
    with log_path.open("ab") as log:
        subprocess.Popen(
            command,
            stdin=subprocess.DEVNULL,
            stdout=log,
            stderr=log,
            close_fds=True,
            creationflags=flags,
            **kwargs,
        )
    for _attempt in range(20):
        if client_bridge_running(host=host, port=port):
            print(f"CLIENT BRIDGE STARTED · loopback {host}:{port}")
            return 0
        import time

        time.sleep(0.05)
    raise ClientBridgeError(f"Client bridge did not start; inspect {log_path}")


def _status(config_path: Path | None = None) -> int:
    config = load_client_config(config_path)
    nodes = client_nodes(config)
    bridge_state = (
        "READY" if client_bridge_running() else "STOPPED · pika bridge start"
    )
    print(f"Pika client · bridge {bridge_state} · {len(nodes)} paired machine(s)")
    for node in sorted(nodes.values(), key=lambda item: item.alias.casefold()):
        print(f"  {node.alias:<20} node {node.node_id[:8]} · {node.ssh_target}")
    if not nodes:
        print("Pair one with `pika setup SSH_HOST`.")
    else:
        print("Start local window routing with `pika bridge start`.")
    return 0


def run(argv: list[str] | None = None) -> int:
    args = _parser().parse_args(sys.argv[1:] if argv is None else argv)
    if args.command is None:
        return _status()
    if args.command == "setup":
        return _pair(args)
    if args.command == "bridge":
        if args.bridge_command in {None, "status"}:
            return _status()
        if args.bridge_command == "serve":
            config = load_client_config()
            bridge = ClientLaunchBridge(
                config,
                terminal_executable=args.terminal_executable,
                ssh_executable=args.ssh_executable,
            )
            print(
                f"Pika client bridge · loopback {args.host}:{args.port} · "
                f"{len(bridge.nodes)} paired machine(s)"
            )
            print("Leave this running; Ctrl-C stops local window routing.")
            serve_client_bridge(
                bridge,
                host=args.host,
                port=args.port,
                config_loader=load_client_config,
            )
            return 0
        if args.bridge_command == "start":
            return _start_bridge(
                host=args.host,
                port=args.port,
                terminal_executable=args.terminal_executable,
                ssh_executable=args.ssh_executable,
            )
    raise ClientBridgeError("Unknown Pika client command")


def main() -> None:
    try:
        raise SystemExit(run())
    except ClientBridgeError as exc:
        print(f"pika: {exc}", file=sys.stderr)
        raise SystemExit(1) from exc
    except KeyboardInterrupt:
        print("\npika: client bridge stopped", file=sys.stderr)
        raise SystemExit(130)


if __name__ == "__main__":
    main()
