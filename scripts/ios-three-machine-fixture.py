#!/usr/bin/env python3
"""Disposable ordinary-SSH endpoints with protocol doubles, NOT Rust/providers.

Run with a fresh output directory. Stop via its stop marker; offline-beta stops
only Beta's listener and existing exec channels. Independent JSONL logs preserve
every exact open/send identity. No real provider or user transcript is read.
"""
import json
import os
import pathlib
import pwd
import select
import shlex
import subprocess
import sys
import tempfile
import time
import uuid


def endpoint(root, node, name):
    root = pathlib.Path(root)
    identity = {"nodeId": node, "provider": "codex", "threadId": "collision-thread"}
    injected = False
    def emit(frame):
        print(json.dumps(frame), flush=True)
    while not (root / "stop").exists() and not (name == "Beta" and (root / "offline-beta").exists()):
        if name == "Gamma" and (root / "gamma-disconnect-once").exists() and not (root / "gamma-disconnect-once-delivered").exists():
            (root / "gamma-disconnect-once-delivered").write_text("closed only the owned Gamma exec channel")
            break
        if name == "Gamma" and not injected and (root / "gamma-source-error").exists():
            emit({"v": 1, "event": "connection/error", "params": {"message": "Explicit unrelated Gamma protocol-fixture error without nodeId"}})
            (root / "gamma-source-error-delivered").write_text("delivered")
            injected = True
        if not select.select([sys.stdin], [], [], 0.2)[0]:
            continue
        line = sys.stdin.readline()
        if not line:
            break
        request = json.loads(line)
        method, params = request["method"], request.get("params", {})
        with (root / (name + ".jsonl")).open("a") as log:
            log.write(json.dumps({"method": method, "params": params, "node": node}) + "\n")
        result = {}
        if method == "hello":
            if name == "Gamma" and (root / "gamma-disconnect-once-delivered").exists():
                (root / "gamma-reconnected").write_text("new authenticated Gamma exec reached hello")
            result = {"nodeId": node, "name": "SSH fixture " + name, "capabilities": {"board": True, "codexShared": True}}
        elif method == "board/subscribe":
            emit({"v": 1, "event": "board/snapshot", "params": {"items": [{"identity": identity, "name": "Shared work", "machine": "SSH fixture " + name, "status": "READY", "observedAt": time.time()}], "observedAt": time.time(), "health": []}})
        elif method == "conversation/open":
            if params.get("identity") != identity:
                raise RuntimeError("WRONG EXACT OPEN DESTINATION: " + repr(params))
            if name == "Alpha" and (root / "alpha-open-hold").exists():
                (root / "alpha-open-held").write_text("held original Alpha response")
                while not (root / "alpha-open-release").exists() and not (root / "stop").exists():
                    time.sleep(0.1)
            result = {"identity": identity, "capabilities": {"read": True, "send": True}, "turns": {"data": [{"items": [{"id": "context", "type": "agentMessage", "text": "Protocol fixture context from " + name + ". No provider attached."}]}], "nextCursor": None}}
        elif method == "conversation/send":
            if params.get("identity") != identity or params.get("text") != "Exact destination " + name:
                raise RuntimeError("WRONG EXACT SEND DESTINATION: " + repr(params))
            result = {"identity": identity, "clientMessageId": params["clientMessageId"], "state": "accepted"}
            emit({"v": 1, "event": "conversation/event", "params": {"identity": identity, "method": "item/completed", "params": {"item": {"id": params["clientMessageId"], "type": "agentMessage", "text": "Protocol fixture received exact destination " + name}}}})
        else:
            emit({"v": 1, "id": request["id"], "error": {"code": "fixture_unsupported", "message": "Protocol fixture only"}})
            continue
        emit({"v": 1, "id": request["id"], "result": result})


def resume(root_path):
    """Restart ONLY explicitly retained disposable listeners, with saved pins."""
    root = pathlib.Path(root_path).resolve()
    if not root.name.startswith("pika-three-ssh-") or (root / "stop").exists():
        raise RuntimeError("Requires a retained disposable fixture without its stop marker")
    children, logs = [], []
    try:
        for name in ["Alpha", "Gamma"]:
            home = root / name
            config = root / (name + ".conf")
            subprocess.run(["/usr/sbin/sshd", "-t", "-f", str(config)], check=True)
            log = (root / (name + "-sshd.log")).open("a")
            logs.append(log)
            children.append(subprocess.Popen(["/usr/sbin/sshd", "-D", "-e", "-f", str(config)], stdout=log, stderr=log, env={"PATH": "/usr/bin:/bin", "HOME": str(home), "TMPDIR": str(home / "tmp")}))
        print("Resumed retained owned Alpha/Gamma only: " + str(root), flush=True)
        while not (root / "stop").exists():
            if any(child.poll() is not None for child in children):
                raise RuntimeError("An owned retained listener stopped unexpectedly")
            time.sleep(0.2)
    finally:
        for child in children:
            if child.poll() is None:
                child.terminate()
            child.wait(timeout=10)
        for log in logs:
            log.close()
        print("Stopped resumed owned listeners: " + str(root), flush=True)


def serve():
    root = pathlib.Path(tempfile.mkdtemp(prefix="pika-three-ssh-"))
    python = sys.executable
    script = pathlib.Path(__file__).resolve()
    subprocess.run(["/usr/bin/ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", str(root / "client")], check=True)
    machines, children, logs = [], [], []
    try:
        for index, name in enumerate(["Alpha", "Beta", "Gamma"]):
            node = str(uuid.uuid4())
            key = root / (name + "-host")
            subprocess.run(["/usr/bin/ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", str(key)], check=True)
            port = 59431 + index
            fingerprint = subprocess.check_output(["/usr/bin/ssh-keygen", "-lf", str(key) + ".pub", "-E", "sha256"], text=True).split()[1]
            home = root / name
            home.mkdir()
            # sshd launches the account shell before ForceCommand. SetEnv makes
            # that shell's startup HOME disposable too, not just Python's HOME.
            isolated = {"HOME": home, "ZDOTDIR": home, "XDG_CONFIG_HOME": home / "config", "XDG_STATE_HOME": home / "state", "XDG_DATA_HOME": home / "data", "XDG_CACHE_HOME": home / "cache", "CODEX_HOME": home / "codex", "CLAUDE_CONFIG_DIR": home / "claude", "OPENCODE_DATA_HOME": home / "opencode", "PIKA_CONFIG_HOME": home / "pika-config", "PIKA_STATE_HOME": home / "pika-state", "PIKA_DB_PATH": home / "unused.sqlite", "TMPDIR": home / "tmp", "TMUX_TMPDIR": home / "tmux"}
            for key_name, directory in isolated.items():
                if key_name != "PIKA_DB_PATH":
                    directory.mkdir(exist_ok=True)
            force = "/usr/bin/env -i " + " ".join(key + "=" + shlex.quote(str(value)) for key, value in isolated.items()) + " PIKA_TMUX_SOCKET=three-machine-fixture PIKA_UPDATE_CHECK=0 PATH=/usr/bin:/bin " + " ".join(shlex.quote(str(x)) for x in [python, script, "--endpoint", root, node, name])
            config = root / (name + ".conf")
            startup_env = " ".join(key + "=" + str(value) for key, value in isolated.items())
            config.write_text(f"Port {port}\nListenAddress 127.0.0.1\nHostKey {key}\nPidFile {root / (name + '.pid')}\nAuthorizedKeysFile {root / 'client.pub'}\nStrictModes no\nPasswordAuthentication no\nKbdInteractiveAuthentication no\nUsePAM no\nAllowUsers {pwd.getpwuid(os.getuid()).pw_name}\nAllowTcpForwarding no\nX11Forwarding no\nPermitTunnel no\nPermitTTY no\nPermitUserRC no\nPermitUserEnvironment no\nSetEnv {startup_env}\nForceCommand {force}\n")
            log = (root / (name + "-sshd.log")).open("w")
            logs.append(log)
            children.append(subprocess.Popen(["/usr/sbin/sshd", "-D", "-e", "-f", str(config)], stdout=log, stderr=log, env={"PATH": "/usr/bin:/bin", "HOME": str(home), "TMPDIR": str(home / "tmp")}))
            machines.append({"nodeId": node, "name": name, "port": port, "fingerprint": fingerprint})
        (root / "configuration.json").write_text(json.dumps({"root": str(root), "clientKeyPath": str(root / "client"), "username": pwd.getpwuid(os.getuid()).pw_name, "storeId": str(uuid.uuid4()), "machines": machines}))
        print(root, flush=True)
        offline = False
        while not (root / "stop").exists():
            if (root / "offline-beta").exists() and not offline:
                children[1].terminate()
                children[1].wait(timeout=10)
                offline = True
            time.sleep(0.2)
    finally:
        for child in children:
            if child.poll() is None:
                child.terminate()
            child.wait(timeout=10)
        for log in logs:
            log.close()
        print("Stopped owned listeners; generated keys/config/evidence retained at " + str(root), flush=True)


if __name__ == "__main__":
    if len(sys.argv) > 1 and sys.argv[1] == "--endpoint":
        endpoint(*sys.argv[2:])
    elif len(sys.argv) == 3 and sys.argv[1] == "--resume":
        resume(sys.argv[2])
    else:
        serve()
