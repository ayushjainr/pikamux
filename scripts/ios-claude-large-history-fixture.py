#!/usr/bin/env python3
"""Synthetic Claude JSONL -> actual Pika mobile -> disposable loopback SSH.

No provider is launched. --prepare creates/adopts only this fixture; --serve
holds only its SSH listener until stop or 15 minutes. Existing homes are unused.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import pwd
import select
import shlex
import socket
import subprocess
import tempfile
import time
import uuid


def digest(path):
    value = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            value.update(chunk)
    return value.hexdigest()


def environment(root):
    env = {"PATH": str(root / "bin") + ":/usr/bin:/bin", "PIKA_UPDATE_CHECK": "0"}
    for key, folder in {
        "HOME": "home", "XDG_CONFIG_HOME": "config", "XDG_DATA_HOME": "data",
        "XDG_STATE_HOME": "state", "XDG_CACHE_HOME": "cache", "TMPDIR": "tmp",
        "TMUX_TMPDIR": "tmp", "PIKA_CONFIG_HOME": "config/pika", "PIKA_STATE_HOME": "state/pika",
        "CODEX_HOME": "codex", "CLAUDE_CONFIG_DIR": "claude", "OPENCODE_DATA_HOME": "data/opencode",
        "OPENCODE_CONFIG_DIR": "config/opencode",
    }.items():
        directory = root / folder
        directory.mkdir(parents=True, exist_ok=True)
        env[key] = str(directory)
    env["PIKA_DB_PATH"] = str(root / "state/pika/pika.db")
    env["PIKA_TMUX_SOCKET"] = "synthetic-large-history-" + root.name
    return env


def visible_role(index):
    return "assistant" if index == 90 or index % 2 else "user"


def large_code_body():
    return '雪🦀\\"\tlet value = 42; ' * 16000


def visible_text(index):
    text = f"Synthetic large Claude {visible_role(index)} {index:03d}"
    if index == 88:
        text += "\n" + 'Literal user line 雪🦀 \\"\t\n' * 12000
        # One extended grapheme can be arbitrarily large. A character-count
        # preview limit must not hand this entire unit to native text layout.
        text += "\nSingle extended grapheme:\ne" + "\u0301" * 200000
        assert len(text.encode()) > 64 * 1024
    if index == 90:
        text += "\n```rust\n" + large_code_body() + "\n```\n"
    if index == 91:
        text += "\n**Mixed long reply**\n```text\ntiny fence cannot bypass the reader\n```\n"
        text += 'Original mixed prose 雪🦀 \\" escaped line.\n' * 32000
        assert len(text.encode()) > 1024 * 1024
    return text


def source(root, thread, mib):
    folder = root / "claude/projects/synthetic-only"
    folder.mkdir(parents=True)
    path = folder / (thread + ".jsonl")
    parent = None
    padding = "SYNTHETIC_HIDDEN_TOOL_DATA_" + "x" * (2 * 1024 * 1024)
    with path.open("w") as stream:
        def emit(record):
            line = json.dumps(record, separators=(",", ":")) + "\n"
            stream.write(line)
        for index in range(4):
            tool = "synthetic_tool_" + str(index)
            for role, content in [
                ("assistant", [{"type": "tool_use", "id": tool, "name": "SyntheticNeverExecuted", "input": {}}]),
                ("user", [{"type": "tool_result", "tool_use_id": tool, "content": padding if index % 2 == 0 else [{"type": "text", "text": padding}]}]),
                ("assistant", [{"type": "thinking", "thinking": padding, "signature": "synthetic"}]),
                ("user", [{"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": padding}}]),
            ]:
                identity = str(uuid.uuid5(uuid.UUID(thread), "tool:" + str(index) + ":" + content[0]["type"]))
                emit({"type": role, "uuid": identity, "parentUuid": parent, "sessionId": thread,
                      "isSidechain": False, "entrypoint": "cli", "cwd": str(root),
                      "message": {"role": role, "content": content}})
                parent = identity
        while stream.tell() < mib * 1024 * 1024:
            emit({"type": "file-history-snapshot", "sessionId": thread,
                  "snapshot": {"trackedFileBackups": {}}, "syntheticPadding": padding})
        for index in range(120):
            role = visible_role(index)
            identity = str(uuid.uuid5(uuid.UUID(thread), "visible:" + str(index)))
            emit({"type": role, "uuid": identity, "parentUuid": parent, "sessionId": thread,
                  "isSidechain": False, "entrypoint": "cli", "cwd": str(root),
                  "message": {"role": role, "content": [{"type": "text", "text": visible_text(index)}]}})
            parent = identity
        emit({"type": "custom-title", "sessionId": thread, "customTitle": "Synthetic large Claude history"})
    return path


def prepare(binary, mib):
    root = Path(tempfile.mkdtemp(prefix="pika-claude-large-ui-")).resolve()
    (root / "bin").mkdir()
    deny = root / "bin/denied"
    deny.write_text("#!/bin/sh\nexit 97\n")
    deny.chmod(0o700)
    for command in ["claude", "codex", "opencode", "muse", "tmux", "ssh", "curl"]:
        (root / "bin" / command).symlink_to(deny)
    env = environment(root)
    thread = str(uuid.uuid4())
    transcript = source(root, thread, mib)
    subprocess.run([binary, "adopt", thread], env=env, cwd=root, check=True, timeout=15,
                   stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    ready = {"root": str(root), "binary": binary, "env": env, "threadId": thread,
             "source": str(transcript), "sourceBytes": transcript.stat().st_size,
             "sourceSha256": digest(transcript), "mib": mib}
    (root / "ready.json").write_text(json.dumps(ready))
    (root / "ready.json").chmod(0o600)
    print(json.dumps(ready), flush=True)


def serve(root, binary=None, configure_only=False):
    root = Path(root).resolve()
    assert root.name.startswith("pika-claude-large-ui-")
    ready = json.loads((root / "ready.json").read_text())
    assert ready["root"] == str(root)
    if binary:
        ready["binary"] = str(Path(binary).resolve())
    transcript = Path(ready["source"])
    assert digest(transcript) == ready["sourceSha256"]
    for name in ["client-key", "host-key"]:
        if not (root / name).exists():
            subprocess.run(["/usr/bin/ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", str(root / name)], check=True)
    with socket.socket() as reservation:
        reservation.bind(("127.0.0.1", 0))
        port = reservation.getsockname()[1]
    command = shlex.join(["/usr/bin/env", "-i"] + [key + "=" + value for key, value in ready["env"].items()] + [ready["binary"], "_mobile"])
    bridge = root / "bridge.sh"
    bridge.write_text("#!/bin/sh\nexec " + command + "\n")
    bridge.chmod(0o700)
    key = (root / "client-key.pub").read_text().strip()
    authorized = root / "authorized_keys"
    authorized.write_text(f'restrict,command="{bridge}" {key}\n')
    authorized.chmod(0o600)
    config = root / "sshd_config"
    config.write_text(f"Port {port}\nListenAddress 127.0.0.1\nHostKey {root / 'host-key'}\nPidFile {root / 'sshd.pid'}\nAuthorizedKeysFile {authorized}\nStrictModes no\nPasswordAuthentication no\nKbdInteractiveAuthentication no\nUsePAM no\nAllowUsers {pwd.getpwuid(os.getuid()).pw_name}\nAllowTcpForwarding no\nAllowAgentForwarding no\nX11Forwarding no\nPermitTunnel no\nPermitTTY no\nPermitUserRC no\nPermitUserEnvironment no\nSetEnv HOME={root / 'home'} XDG_CONFIG_HOME={root / 'config'}\nForceCommand {bridge}\n")
    subprocess.run(["/usr/sbin/sshd", "-t", "-f", str(config)], check=True)
    fingerprint = subprocess.check_output(["/usr/bin/ssh-keygen", "-lf", str(root / "host-key.pub"), "-E", "sha256"], text=True).split()[1]
    integration = {"address": "127.0.0.1", "port": port, "username": pwd.getpwuid(os.getuid()).pw_name,
                   "clientKeyPath": str(root / "client-key"), "fingerprint": fingerprint,
                   "threadId": ready["threadId"], "threadName": "Synthetic large Claude history",
                   "expectedContext": "Synthetic large Claude user 118", "finalResponse": "Synthetic large Claude assistant 119",
                   "expectedCodeSha256": hashlib.sha256(large_code_body().encode()).hexdigest(),
                   "expectedLargeUserSha256": hashlib.sha256(visible_text(88).encode()).hexdigest(),
                   "expectedLargeReplySha256": hashlib.sha256(visible_text(91).encode()).hexdigest(),
                   "mode": "largeHistory", "storeId": str(uuid.uuid4())}
    (root / "integration.json").write_text(json.dumps(integration))
    if configure_only:
        print("SYNTHETIC_LARGE_HISTORY_CONFIGURED " + str(root / "integration.json"), flush=True)
        return
    with (root / "sshd.log").open("w") as log:
        child = subprocess.Popen(["/usr/sbin/sshd", "-D", "-e", "-f", str(config)], env=ready["env"], stdout=log, stderr=log)
        try:
            print("SYNTHETIC_LARGE_HISTORY_READY " + str(root / "integration.json"), flush=True)
            deadline = time.monotonic() + 900
            while not (root / "stop").exists() and time.monotonic() < deadline:
                assert child.poll() is None, "Owned SSH listener exited"
                time.sleep(0.2)
        finally:
            child.terminate()
            try:
                child.wait(timeout=5)
            except subprocess.TimeoutExpired:
                child.kill()
                child.wait(timeout=5)
            unchanged = digest(transcript) == ready["sourceSha256"]
            cleanup = {"listenerPid": child.pid, "listenerExit": child.returncode, "sourceUnchanged": unchanged,
                       "sourceSha256": digest(transcript), "port": port}
            (root / "cleanup.json").write_text(json.dumps(cleanup))
            assert unchanged, "Synthetic source changed during read-only journey"


def check(root, binary, expect_record_limit=False):
    """Actual endpoint evidence, with no SSH/provider/model execution."""
    root = Path(root).resolve()
    ready = json.loads((root / "ready.json").read_text())
    assert root.name.startswith("pika-claude-large-ui-") and ready["root"] == str(root)
    version = subprocess.check_output([binary, "--version"], env=ready["env"], cwd=root,
                                      timeout=5, text=True).strip()
    binary_sha256 = digest(Path(binary))
    child = subprocess.Popen([binary, "_mobile"], env=ready["env"], cwd=root,
                             stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                             text=True, bufsize=1)
    started = time.monotonic()
    latencies = []
    def request(method, params):
        request_started = time.monotonic()
        serial = str(uuid.uuid4())
        child.stdin.write(json.dumps({"v": 1, "id": serial, "method": method, "params": params}) + "\n")
        child.stdin.flush()
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            assert select.select([child.stdout], [], [], max(0, deadline-time.monotonic()))[0], "Endpoint deadline"
            line = child.stdout.readline()
            assert line, "Endpoint exited"
            value = json.loads(line)
            if value.get("id") == serial:
                latencies.append({"method": method, "seconds": time.monotonic()-request_started})
                return value
        raise AssertionError("Endpoint deadline")
    try:
        hello = request("hello", {})
        identity = {"nodeId": hello["result"]["nodeId"], "provider": "claude", "threadId": ready["threadId"]}
        opened = request("conversation/open", {"identity": identity})
        if expect_record_limit:
            assert "individual history record exceeds 256 KiB" in json.dumps(opened.get("error")), opened
            assert "result" not in opened
            evidence = {"binary": binary, "version": version, "expectedFormerRecordLimit": opened["error"],
                        "binarySha256": binary_sha256,
                        "sourceSha256": ready["sourceSha256"], "elapsedSeconds": time.monotonic()-started}
            assert digest(Path(ready["source"])) == ready["sourceSha256"]
            (root / "endpoint-former-record-limit.json").write_text(json.dumps(evidence))
            print(json.dumps(evidence), flush=True)
            return
        assert "error" not in opened, opened.get("error")
        evidence = {"binary": binary, "binarySha256": binary_sha256, "version": version, "sourceSha256": ready["sourceSha256"]}
        if "error" not in opened:
            assert opened["result"]["capabilities"]["send"] is False
            turns = opened["result"]["turns"]
            pages = [turns["data"]]
            cursor = turns.get("nextCursor")
            while cursor:
                older = request("conversation/history", {"identity": identity, "cursor": cursor})
                assert "error" not in older, older
                turns = older["result"]["turns"]
                assert turns["order"] == "chronological"
                pages.insert(0, turns["data"])
                cursor = turns.get("nextCursor")
                assert len(pages) <= 4
            texts = [turn["items"][0]["text"] for page in pages for turn in page]
            identities = [turn["items"][0]["id"] for page in pages for turn in page]
            assert len(identities) == len(set(identities))
            expected = [visible_text(i) for i in range(120)]
            assert texts == expected, (len(texts), texts[:2], texts[-2:])
            evidence["pageSizes"] = [len(page) for page in pages]
            evidence["visibleMessages"] = len(texts)
            evidence["visibleTextSha256"] = [hashlib.sha256(text.encode()).hexdigest() for text in texts]
            reopened = request("conversation/open", {"identity": identity})
            assert "error" not in reopened, reopened.get("error")
            assert reopened["result"]["turns"]["data"] == pages[-1]
            evidence["reopenedLatestExact"] = True
        evidence["elapsedSeconds"] = time.monotonic()-started
        evidence["requestLatencies"] = latencies
        assert digest(Path(ready["source"])) == ready["sourceSha256"]
        assert digest(Path(binary)) == binary_sha256
        (root / ("endpoint-" + str(uuid.uuid4()) + ".json")).write_text(json.dumps(evidence))
        print(json.dumps(evidence), flush=True)
    finally:
        child.terminate()
        try:
            child.wait(timeout=5)
        except subprocess.TimeoutExpired:
            child.kill(); child.wait(timeout=5)


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary")
    parser.add_argument("--mib", type=int, default=20, choices=[20, 40, 160])
    parser.add_argument("--serve")
    parser.add_argument("--check")
    parser.add_argument("--expect-record-limit", action="store_true")
    parser.add_argument("--configure-only", action="store_true")
    args = parser.parse_args()
    if args.check:
        assert args.binary and Path(args.binary).is_file()
        check(args.check, str(Path(args.binary).resolve()), args.expect_record_limit)
    elif args.serve:
        serve(args.serve, args.binary, args.configure_only)
    else:
        assert args.binary and Path(args.binary).is_file(), "Explicit built native Pika required"
        prepare(str(Path(args.binary).resolve()), args.mib)
