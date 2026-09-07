"""Local cold-process first-cache-frame benchmark; no provider or SSH calls.

Run with the installed tool Python, e.g.:
  ~/.local/share/uv/tools/pikamux/bin/python tests/benchmark_board.py
Includes process startup/imports, 200 persisted local rows, an offline machine,
terminal setup, cache load and first frame. Each child exits at first draw.
"""
from __future__ import annotations

import json
import os
from pathlib import Path
import select
import shutil
import signal
import subprocess
import sys
import tempfile
import time


def child(path: str) -> None:
    import fcntl
    import pty
    import struct
    import termios
    from unittest.mock import patch

    from pikamux import cli  # Include the real command's import graph.
    from pikamux.core import Pika
    from pikamux.models import FleetNode
    from pikamux.monitor import run_monitor
    from pikamux.store import Store

    class Fleet:
        def nodes(self):
            return [FleetNode("offline", "offline", "unreachable", status="offline")]

        def cached_sessions(self):
            return []

    pika = Pika(store=Store(Path(path)), fleet=Fleet())
    pika.refresh = lambda **_: (_ for _ in ()).throw(
        AssertionError("First frame must precede provider reconciliation")
    )

    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 35, 150, 0, 0))

    class FirstFrame(BaseException):
        pass

    def first_draw(_terminal, frame):
        assert "bench-" in frame and "LAST KNOWN" in frame
        print("FIRST_FRAME", flush=True)
        raise FirstFrame()

    try:
        with patch("pikamux.monitor._Terminal.draw", first_draw):
            run_monitor(pika, input_fd=slave, output_fd=slave)
    except FirstFrame:
        pass
    finally:
        os.close(master)
        os.close(slave)


def benchmark():
    from pikamux.models import Session
    from pikamux.store import Store

    with tempfile.TemporaryDirectory(prefix="pika-board-bench-") as directory:
        path = Path(directory) / "pika.db"
        store = Store(path)
        for index in range(200):
            store.upsert_session(Session("codex", f"bench-{index}", name=f"bench-{index:03}", status="WORKING"))
        samples = []
        for _ in range(30):
            started = time.perf_counter()
            process = subprocess.Popen(
                [sys.executable, __file__, "--child", str(path)],
                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
            )
            if not select.select([process.stdout], [], [], 5)[0]:
                process.kill()
                process.communicate()
                raise TimeoutError("First cache frame exceeded 5s")
            line = process.stdout.readline()
            duration = time.perf_counter() - started
            _stdout, stderr = process.communicate(timeout=5)
            if process.returncode or line.strip() != "FIRST_FRAME":
                raise RuntimeError(stderr or line)
            samples.append(duration * 1000)
    ranked = sorted(samples)
    result = {
        "samples": len(samples), "cached_sessions": 200,
        "cold_process": True, "offline_remote": True,
        "p50_ms": round(ranked[15], 2), "p95_ms": round(ranked[28], 2),
        "max_ms": round(max(samples), 2), "target_ms": 500,
        "python": sys.executable,
    }
    print(json.dumps(result, indent=2))
    if result["p95_ms"] >= 500:
        raise SystemExit(1)


def installed_benchmark(executable: str):
    """Exercise the actual installed bare command using an isolated state DB."""
    import fcntl
    import pty
    import struct
    import termios
    import uuid

    from pikamux.models import FleetNode, Session
    from pikamux.store import Store

    samples = []
    with tempfile.TemporaryDirectory(prefix="pika-installed-board-") as directory:
        root = Path(directory)
        store = Store(root / "pika.db")
        for index in range(200):
            store.upsert_session(Session("codex", str(uuid.uuid4()), name=f"bench-{index:03}", status="PARKED"))
        # Known unavailable machine: no first-frame handshake is warranted.
        store.upsert_fleet_node(FleetNode(
            str(uuid.uuid4()), "offline", "pika-benchmark.invalid",
            status="unreachable", last_attempt_at=time.time(), last_error="benchmark offline fixture",
        ))
        environment = dict(os.environ)
        environment.update({
            "PIKA_DB_PATH": str(store.path), "PIKA_CONFIG_HOME": str(root / "config"),
            "PIKA_STATE_HOME": str(root / "state"),
            "PIKA_TMUX_SOCKET": "pika-benchmark-" + uuid.uuid4().hex,
            "TERM": "xterm-256color", "NO_COLOR": "1",
        })
        # This fixture has its own Pika DB/config/tmux namespace. Provider
        # reconciliation can read host metadata, but cannot mutate those agents.
        for _ in range(30):
            master, slave = pty.openpty()
            fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 35, 150, 0, 0))
            started = time.perf_counter()
            process = subprocess.Popen(
                [executable], stdin=slave, stdout=slave, stderr=slave,
                env=environment, start_new_session=True,
            )
            data = b""
            deadline = time.monotonic() + 5
            try:
                while time.monotonic() < deadline:
                    if select.select([master], [], [], max(0, deadline - time.monotonic()))[0]:
                        data += os.read(master, 65536)
                        if b"bench-" in data and b"PIKA PLAYBOOK" in data and b"x stop watching" in data:
                            samples.append((time.perf_counter() - started) * 1000)
                            break
                else:
                    raise TimeoutError(f"Installed board missing first full frame: {data[-1000:]!r}")
                if b"LAST KNOWN" not in data:
                    raise AssertionError("Installed board did not display the persisted cache first")
                os.write(master, b"q")
                # Drain output so a second frame cannot block exit on PTY size.
                deadline = time.monotonic() + 3
                while process.poll() is None and time.monotonic() < deadline:
                    if select.select([master], [], [], .05)[0]:
                        os.read(master, 65536)
                if process.poll() is None:
                    raise TimeoutError("Installed board did not exit promptly")
                if process.returncode:
                    raise RuntimeError(f"Installed board exited {process.returncode}")
            finally:
                if process.poll() is None:
                    # Only the process group created for this benchmark run.
                    os.killpg(process.pid, signal.SIGTERM)
                    try:
                        process.wait(timeout=1)
                    except subprocess.TimeoutExpired:
                        os.killpg(process.pid, signal.SIGKILL)
                        process.wait(timeout=1)
                os.close(master)
                os.close(slave)
    ranked = sorted(samples)
    print(json.dumps({
        "samples": 30, "cached_sessions": 200, "installed_command": executable,
        "cold_process": True, "offline_remote": True, "full_pty_frame": True,
        "p50_ms": round(ranked[15], 2), "p95_ms": round(ranked[28], 2),
        "max_ms": round(max(samples), 2), "target_ms": 500,
    }, indent=2))
    if ranked[28] >= 500:
        raise SystemExit(1)


if __name__ == "__main__":
    if len(sys.argv) > 1 and sys.argv[1] == "--child":
        child(sys.argv[2])
    elif len(sys.argv) > 1 and sys.argv[1] == "--pika":
        installed_benchmark(sys.argv[2] if len(sys.argv) > 2 else shutil.which("pika"))
    else:
        benchmark()
