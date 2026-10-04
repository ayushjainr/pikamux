#!/usr/bin/env python3
"""Exercise the real offline assistant beneath a disposable Linux account home.

Run only as the disposable CI account whose parent is foreign-owned. No provider,
fleet, real user history, installed command, or model quota is used.
"""
import argparse
import json
import os
from pathlib import Path
import pwd
import subprocess
import tempfile
import time


def require(value, message):
    if not value:
        raise RuntimeError(message)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--home", type=Path, required=True)
    parser.add_argument("--foreign-child", type=Path, required=True)
    parser.add_argument("--external", type=Path, required=True)
    args = parser.parse_args()
    require(os.geteuid() != 0, "Use the disposable account, not root")
    require(args.home == Path(pwd.getpwuid(os.geteuid()).pw_dir),
            "Fixture must be the native account home")
    require(args.home.parent.stat().st_uid not in (0, os.geteuid()),
            "Fixture needs a foreign-owned mount ancestor")
    require(args.binary.is_absolute() and args.binary.is_file(), "Missing binary")
    with tempfile.TemporaryDirectory(prefix="assistant-journey-", dir=args.home) as name:
        root = Path(name)
        for folder in ("home", "state", "config", "cache", "data", "tmp", "tmux",
                       "codex", "claude", "opencode", "bin"):
            (root / folder).mkdir(mode=0o700)
        marker = root / "unexpected-provider"
        for command in ("codex", "claude", "opencode", "muse", "ssh", "tmux", "tailscale"):
            executable = root / "bin" / command
            executable.write_text('#!/bin/sh\nprintf x >> "$PROBE_MARKER"\nexit 97\n')
            executable.chmod(0o700)
        env = {
            "HOME": str(root / "home"), "PATH": f"{root / 'bin'}:/usr/bin:/bin",
            "XDG_STATE_HOME": str(root / "state"), "XDG_CONFIG_HOME": str(root / "config"),
            "XDG_CACHE_HOME": str(root / "cache"), "XDG_DATA_HOME": str(root / "data"),
            "PIKA_STATE_HOME": str(root / "state"), "PIKA_CONFIG_HOME": str(root / "config"),
            "PIKA_DB_PATH": str(root / "board.sqlite"), "PIKA_UPDATE_CHECK": "0",
            "CODEX_HOME": str(root / "codex"), "CLAUDE_CONFIG_DIR": str(root / "claude"),
            "OPENCODE_DATA_HOME": str(root / "opencode"),
            "OPENCODE_CONFIG_DIR": str(root / "opencode"),
            "TMPDIR": str(root / "tmp"), "TMUX_TMPDIR": str(root / "tmux"),
            "PIKA_TMUX_SOCKET": "assistant-mount-probe", "PROBE_MARKER": str(marker),
            "SHELL": "/bin/sh", "LANG": "C.UTF-8",
        }

        def run(*extra, success=True, overrides=None):
            result = subprocess.run([str(args.binary), "pika", "--json", *extra],
                                    cwd=root, env={**env, **(overrides or {})},
                                    capture_output=True, text=True, timeout=20)
            require((result.returncode == 0) == success,
                    f"Unexpected exit {result.returncode}: {result.stderr[:1000]}")
            deadline = time.monotonic() + 3
            while (root / "state/assistant/view.sock").exists() and time.monotonic() < deadline:
                time.sleep(0.02)
            require(not (root / "state/assistant/view.sock").exists(), "Host did not exit")
            require(not marker.exists(), "Offline journey invoked an external provider")
            return json.loads(result.stdout) if success else result.stderr

        saved = run("--remember", "Disposable account mount continuity probe")
        before = run()
        require(saved["profile_id"] == before["profile_id"], "Profile changed after saving")
        require(any(record["id"] == saved["saved"] and
                    record["body"] == "Disposable account mount continuity probe"
                    for record in before["records"]), "Exact saved instruction missing")
        require(before["state"] == "not_enabled", "Offline entry enabled a provider")
        # Provider children change HOME. Native account ownership still governs.
        after = run(overrides={"HOME": str(root / "codex")})
        require(after["profile_id"] == before["profile_id"] and
                after["records"] == before["records"], "Private HOME lost identity/history")
        (root / "state").chmod(0o775)
        try:
            error = run(success=False)
            require("not trusted" in error, "Writable ancestor was not rejected by storage")
        finally:
            (root / "state").chmod(0o700)
        recovered = run()
        require(recovered["profile_id"] == before["profile_id"] and
                recovered["records"] == before["records"], "Recovery altered identity/history")
        require(not (root / "board.sqlite").exists(), "Offline entry touched board state")
        for parent in (args.foreign_child, args.external):
            require(parent.parent.stat().st_uid not in (0, os.geteuid()),
                    "Negative fixture needs a foreign ancestor")
            with tempfile.TemporaryDirectory(prefix="blocked-profile-", dir=parent) as blocked:
                # If provider HOME were used as the trust boundary, both of
                # these unsafe roots would incorrectly be accepted.
                error = run(success=False, overrides={"HOME": blocked, "PIKA_STATE_HOME": blocked})
                require("not trusted" in error, "Foreign below-home/external root was accepted")
    print("PASS: native account mount, private HOME, persisted memory, unsafe modes/owners and recovery")


if __name__ == "__main__":
    main()
