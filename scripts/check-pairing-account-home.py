#!/usr/bin/env python3
"""Disposable, loopback-only enrollment and real SSH authentication check.

Requires an absolute candidate binary and non-root sshd support. Never starts a
remote command/session, changes the installed service, or uses an existing key.
--home-parent can exercise an administrator-owned mount above the account home.
All tokens, keys, child output and SSH diagnostics stay inside owned fixtures.
"""
import argparse
import base64
import hashlib
import http.client
import json
import os
from pathlib import Path
import pwd
import select
import signal
import socket
import ssl
import subprocess
import sys
import tempfile
import time


def require(condition, message):
    if not condition:
        raise RuntimeError(message)


def stop(child):
    if child.poll() is None:
        os.killpg(child.pid, signal.SIGTERM)
        try:
            child.wait(timeout=2)
        except subprocess.TimeoutExpired:
            os.killpg(child.pid, signal.SIGKILL)
            child.wait(timeout=2)


def launch(command, env, children, **kwargs):
    child = subprocess.Popen(command, env=env, stdin=subprocess.DEVNULL,
                             start_new_session=True, **kwargs)
    children.append(child)
    return child


def json_line(child):
    deadline = time.monotonic() + 10
    data = b""
    while time.monotonic() < deadline:
        if select.select([child.stdout], [], [], 0.1)[0]:
            block = os.read(child.stdout.fileno(), 4096)
            require(block, "Candidate exited before advertising pairing")
            data += block
            require(len(data) <= 16384, "Candidate pairing output exceeded bound")
            if b"\n" in data:
                return json.loads(data.split(b"\n", 1)[0])
    raise RuntimeError("Candidate did not advertise pairing within 10 seconds")


def post(descriptor, pin, path, payload):
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
    context.check_hostname = False
    context.verify_mode = ssl.CERT_NONE  # Explicit QR certificate pin below.
    with socket.create_connection(("127.0.0.1", descriptor["pair_port"]), 2) as raw:
        with context.wrap_socket(raw, server_hostname="localhost") as stream:
            require(hashlib.sha256(stream.getpeercert(binary_form=True)).digest() == pin,
                    "Pairing TLS certificate did not match QR pin")
            body = json.dumps(payload).encode()
            stream.sendall((f"POST {path} HTTP/1.1\r\nHost: localhost\r\n"
                            f"Content-Length: {len(body)}\r\n"
                            "Content-Type: application/json\r\nConnection: close\r\n\r\n"
                            ).encode() + body)
            response = http.client.HTTPResponse(stream)
            response.begin()
            result = response.read(16385)
            require(len(result) <= 16384, "Pairing response exceeded bound")
            return response.status, json.loads(result)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--home-parent", type=Path, default=Path("/tmp"))
    args = parser.parse_args()
    require(args.binary.is_absolute() and args.binary.is_file(),
            "--binary must be an absolute existing candidate executable")
    require(args.home_parent.is_absolute() and args.home_parent.is_dir(),
            "--home-parent must be an absolute existing directory")
    require(os.geteuid() != 0, "Run as the account being enrolled, not root")
    require(ssl.HAS_TLSv1_3, "Python's SSL runtime must support TLS 1.3")
    sshd = Path("/usr/sbin/sshd")
    require(sshd.is_file(), "Non-root sshd is unavailable: /usr/sbin/sshd missing")
    children = []
    with tempfile.TemporaryDirectory(prefix="pika-pair-ssh-", dir="/tmp") as anchor_dir, \
            tempfile.TemporaryDirectory(prefix="pika-pair-home-", dir=args.home_parent) as home_dir:
        anchor = Path(anchor_dir).resolve()
        home = Path(home_dir).resolve()
        env = {"PATH": "/usr/bin:/bin:/usr/sbin:/sbin", "HOME": str(home),
               "LANG": "C", "TERM": "dumb", "SHELL": "/bin/sh",
               "PIKA_UPDATE_CHECK": "0", "PIKA_TMUX_SOCKET": "pairing-test"}
        for name, suffix in {
            "XDG_CONFIG_HOME": "config", "XDG_CACHE_HOME": "cache",
            "XDG_STATE_HOME": "state", "XDG_DATA_HOME": "data", "TMPDIR": "tmp",
            "TMUX_TMPDIR": "tmux", "PIKA_CONFIG_HOME": "config/pika",
            "PIKA_STATE_HOME": "state/pika", "CODEX_HOME": "codex",
            "CLAUDE_CONFIG_DIR": "claude", "OPENCODE_DATA_HOME": "opencode/data",
            "OPENCODE_CONFIG_DIR": "opencode/config", "MUSE_CONFIG_DIR": "muse",
        }.items():
            directory = home / suffix
            directory.mkdir(parents=True, exist_ok=True, mode=0o700)
            env[name] = str(directory)
        env["PIKA_DB_PATH"] = str(home / "state/pika.db")
        ssh_dir = home / ".ssh"
        ssh_dir.mkdir(mode=0o700)
        authorization = ssh_dir / "authorized_keys"
        baseline = b"# disposable preexisting authorization sentinel\n"
        authorization.write_bytes(baseline)
        authorization.chmod(0o600)
        for name in ("host", "phone"):
            generated = subprocess.run(["/usr/bin/ssh-keygen", "-q", "-t", "ed25519",
                                        "-N", "", "-f", str(anchor / name)], env=env,
                                       capture_output=True, timeout=5)
            require(generated.returncode == 0, "Disposable Ed25519 key generation failed")
        key = " ".join((anchor / "phone.pub").read_text().split()[:2])
        host_key = " ".join((anchor / "host.pub").read_text().split()[:2])
        with socket.socket() as probe:
            probe.bind(("127.0.0.1", 0))
            port = probe.getsockname()[1]
        config = anchor / "sshd_config"
        config.write_text(f"Port {port}\nListenAddress 127.0.0.1\n"
                          f"HostKey {anchor}/host\nPidFile {anchor}/sshd.pid\n"
                          f"AllowUsers {pwd.getpwuid(os.geteuid()).pw_name}\n"
                          f"AuthorizedKeysFile {authorization}\nStrictModes yes\n"
                          "PasswordAuthentication no\nKbdInteractiveAuthentication no\n"
                          "PermitEmptyPasswords no\n"
                          "UsePAM no\nPubkeyAuthentication yes\nPermitRootLogin no\n"
                          "AllowTcpForwarding no\nX11Forwarding no\nLogLevel VERBOSE\n")
        try:
            server = launch([str(sshd), "-D", "-e", "-f", str(config)], env, children,
                            stdout=subprocess.DEVNULL, stderr=open(anchor / "sshd.log", "wb"))
            deadline = time.monotonic() + 5
            while True:
                require(server.poll() is None,
                        "Disposable non-root sshd could not start; inspect platform support")
                try:
                    with socket.create_connection(("127.0.0.1", port), 0.2):
                        break
                except OSError:
                    require(time.monotonic() < deadline, "Disposable sshd startup timed out")
                    time.sleep(0.05)
            command = [str(args.binary), "pair", "--json", "--address", "127.0.0.1",
                       "--ssh-port", str(port), "--host-key", str(anchor / "host.pub"),
                       "--expires", "30"]
            with open(anchor / "pair.log", "wb") as log:
                pairing = launch(command, env, children, stdout=subprocess.PIPE, stderr=log)
                advertised = json_line(pairing)
                descriptor = advertised["descriptor"]
                fragment = advertised["uri"].split("#", 1)[1]
                capability = base64.urlsafe_b64decode(fragment + "=" * (-len(fragment) % 4))
                require(len(capability) == 64, "QR capability has unexpected size")
                pin = capability[:32]
                require(pin.hex() == descriptor["tls_sha256"], "QR pin disagrees with descriptor")
                require(base64.urlsafe_b64encode(capability[32:]).decode().rstrip("=") ==
                        descriptor["token"], "QR token disagrees with descriptor")
                require(descriptor["ssh_host_key"] == host_key and
                        descriptor["username"] == pwd.getpwuid(os.geteuid()).pw_name,
                        "Pairing changed SSH account or host identity")
                status, _ = post(descriptor, pin, "/descriptor", {"token": "wrong"})
                require(status == 403 and authorization.read_bytes() == baseline,
                        "Wrong-token request was accepted or changed authorization")
                status, fetched = post(descriptor, pin, "/descriptor",
                                       {"token": descriptor["token"]})
                require(status == 200 and fetched == descriptor, "Descriptor exchange mismatch")
                status, receipt = post(descriptor, pin, "/pair",
                                       {"token": descriptor["token"], "public_key": key})
                require(status == 200 and receipt["public_key"] == key and
                        receipt["node_id"] == descriptor["node_id"] and
                        receipt["state"] == "paired", "Enrollment receipt mismatch")
                require(pairing.wait(timeout=5) == 0, "Pairing process did not finish successfully")
            enrolled = authorization.read_bytes()
            require(enrolled.startswith(baseline) and enrolled[len(baseline):].count(b"\n") == 1
                    and enrolled[len(baseline):].startswith(b'restrict,command="')
                    and (key + " pika-phone\n").encode() in enrolled,
                    "Enrollment did not preserve baseline and append exactly one restricted key")
            print("PASS: pinned TLS descriptor and enrollment; existing bytes preserved")

            # Each unsafe path must fail before publishing a capability or changing bytes.
            for unsafe in ("home-mode", "ssh-mode", "symlink", "hardlink"):
                saved = ssh_dir / "saved_authorizations"
                if unsafe == "home-mode":
                    home.chmod(0o777)
                elif unsafe == "ssh-mode":
                    ssh_dir.chmod(0o777)
                else:
                    authorization.rename(saved)
                    if unsafe == "symlink":
                        authorization.symlink_to(saved)
                    else:
                        os.link(saved, authorization)
                try:
                    rejected = launch(command, env, children, stdout=subprocess.PIPE,
                                      stderr=subprocess.PIPE)
                    rejected.communicate(timeout=10)
                    require(rejected.returncode != 0 and authorization.read_bytes() == enrolled,
                            f"Unsafe {unsafe} target did not fail closed")
                finally:
                    home.chmod(0o700)
                    ssh_dir.chmod(0o700)
                    if saved.exists():
                        authorization.unlink()
                        saved.rename(authorization)
            print("PASS: permissive home/SSH directory, symlink and hardlink rejected unchanged")

            known = anchor / "known_hosts"
            known.write_text(f"[127.0.0.1]:{port} {host_key}\n")
            with open(anchor / "ssh.log", "wb") as log:
                ssh = launch(["/usr/bin/ssh", "-vv", "-N", "-F", "/dev/null",
                              "-p", str(port), "-i", str(anchor / "phone"),
                              "-o", "IdentitiesOnly=yes", "-o", "IdentityAgent=none",
                              "-o", "BatchMode=yes", "-o", "StrictHostKeyChecking=yes",
                              "-o", f"UserKnownHostsFile={known}",
                              "-o", "GlobalKnownHostsFile=/dev/null",
                              "-o", "ConnectTimeout=5", "-o", "PreferredAuthentications=publickey",
                              f"{descriptor['username']}@127.0.0.1"], env, children,
                             stdout=subprocess.DEVNULL, stderr=log)
                deadline = time.monotonic() + 10
                while True:
                    diagnostics = (anchor / "ssh.log").read_bytes()
                    require(len(diagnostics) < 131072, "SSH diagnostics exceeded bound")
                    if b"Authenticated to " in diagnostics and b"publickey" in diagnostics:
                        break
                    require(ssh.poll() is None and time.monotonic() < deadline,
                            "SSH did not authenticate the enrolled key (non-root sshd/StrictModes limitation)")
                    time.sleep(0.05)
                stop(ssh)
            print("PASS: real pinned SSH public-key authentication; no command/session opened")
        finally:
            for child in reversed(children):
                stop(child)
    print("PASS: owned disposable fixtures and children cleaned up")


if __name__ == "__main__":
    try:
        main()
    except (RuntimeError, OSError, ValueError, KeyError, subprocess.TimeoutExpired) as error:
        print(f"FAIL: {error}", file=sys.stderr)
        sys.exit(1)
