"""Measure cached-board first frame and key response in disposable Pika state."""

from __future__ import annotations

import argparse
import fcntl
import json
import os
import pty
import select
import signal
import subprocess
import struct
import sys
import tempfile
import termios
import time
import uuid
from pathlib import Path
from typing import Optional


COMPLETE_FRAME_MARKER = b"q leave"
REMOTE_SENTINEL = b"remote_offline_fixture"


def percentile(values: list[float], fraction: float) -> float:
    ordered = sorted(values)
    return ordered[int((len(ordered) - 1) * fraction)]


def read_until(
    fd: int, marker: bytes, timeout: float, required: Optional[bytes] = None
) -> bytes:
    deadline = time.monotonic() + timeout
    pending = bytearray()
    while time.monotonic() < deadline:
        ready, _, _ = select.select([fd], [], [], min(0.1, deadline - time.monotonic()))
        if not ready:
            continue
        try:
            pending.extend(os.read(fd, 65536))
        except OSError as error:
            raise RuntimeError("board exited before rendering") from error
        if marker in pending and (required is None or required in pending):
            return bytes(pending)
        if len(pending) > 1_048_576:
            del pending[:-len(marker)]
    tail = bytes(pending[-4096:]).decode("utf-8", "replace")
    requirement = f" and {required!r}" if required is not None else ""
    raise TimeoutError(
        f"board did not render {marker!r}{requirement}; tail={tail!r}"
    )


def stop(pid: int, fd: int) -> None:
    try:
        os.write(fd, b"q")
    except OSError:
        pass
    try:
        os.close(fd)
    except OSError:
        pass
    deadline = time.monotonic() + 0.25
    while time.monotonic() < deadline:
        found, _ = os.waitpid(pid, os.WNOHANG)
        if found == pid:
            return
        time.sleep(0.01)
    os.kill(pid, signal.SIGTERM)
    deadline = time.monotonic() + 2
    while time.monotonic() < deadline:
        found, _ = os.waitpid(pid, os.WNOHANG)
        if found == pid:
            return
        time.sleep(0.01)
    os.kill(pid, signal.SIGKILL)
    os.waitpid(pid, 0)


def spawn_board(
    program: Path,
    environment: dict[str, str],
    complete_marker: bytes = COMPLETE_FRAME_MARKER,
) -> tuple[int, int, float]:
    started = time.monotonic()
    pid, fd = pty.fork()
    if pid == 0:
        os.chdir(Path(__file__).resolve().parents[1])
        os.execve(str(program), [str(program)], environment)
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 35, 150, 0, 0))
    try:
        read_until(fd, complete_marker, 10, REMOTE_SENTINEL)
    except BaseException:
        stop(pid, fd)
        raise
    return pid, fd, (time.monotonic() - started) * 1000


def measure(
    program: Path,
    environment: dict[str, str],
    samples: int,
    input_samples: int,
    complete_marker: bytes = COMPLETE_FRAME_MARKER,
) -> dict[str, float | int]:
    first_frames = []
    for _ in range(samples):
        pid, fd, elapsed = spawn_board(program, environment, complete_marker)
        first_frames.append(elapsed)
        stop(pid, fd)

    pid, fd, _ = spawn_board(program, environment, complete_marker)
    inputs = []
    try:
        for index in range(input_samples):
            # Drain a completed prior redraw before starting the next sample.
            while select.select([fd], [], [], 0)[0]:
                os.read(fd, 65536)
            started = time.monotonic()
            os.write(fd, b"\x1b[B" if index % 2 == 0 else b"\x1b[A")
            read_until(fd, complete_marker, 2)
            inputs.append((time.monotonic() - started) * 1000)
    finally:
        stop(pid, fd)
    return {
        "first_frame_samples": len(first_frames),
        "first_frame_p50_ms": round(percentile(first_frames, 0.50), 3),
        "first_frame_p95_ms": round(percentile(first_frames, 0.95), 3),
        "first_frame_max_ms": round(max(first_frames), 3),
        "input_samples": len(inputs),
        "input_p50_ms": round(percentile(inputs, 0.50), 3),
        "input_p95_ms": round(percentile(inputs, 0.95), 3),
        "input_p99_ms": round(percentile(inputs, 0.99), 3),
        "input_max_ms": round(max(inputs), 3),
    }


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("binary", type=Path)
    parser.add_argument("--python-reference", type=Path)
    parser.add_argument("--samples", type=int, default=30)
    parser.add_argument("--input-samples", type=int, default=1000)
    args = parser.parse_args()
    if args.samples < 1 or args.input_samples < 1:
        raise SystemExit("sample counts must be positive")
    root = Path(__file__).resolve().parents[1]
    binary = args.binary.resolve(strict=True)
    with tempfile.TemporaryDirectory(prefix="pika-board-benchmark-") as temporary:
        sandbox = Path(temporary)
        for child in ("home", "config", "state", "cache", "tmp", "bin"):
            (sandbox / child).mkdir()
        fake_ssh = sandbox / "bin" / "ssh"
        fake_ssh.write_text("#!/bin/sh\n/bin/sleep 2\nexit 255\n")
        fake_ssh.chmod(0o700)
        database = sandbox / "state" / "pika.db"
        seed_environment = {
            "HOME": str(sandbox / "home"),
            "PATH": f"{sandbox / 'bin'}:/usr/bin:/bin",
            "PYTHONPATH": str(root / "tests" / "fixtures" / "python-v0.5.0a4"),
            "PYTHONDONTWRITEBYTECODE": "1",
            "PIKA_CONFIG_HOME": str(sandbox / "config" / "pika"),
            "PIKA_STATE_HOME": str(sandbox / "state"),
            "TMPDIR": str(sandbox / "tmp"),
        }
        subprocess.run(
            [
                sys.executable,
                str(root / "tests" / "fixtures" / "mixed_runtime_driver.py"),
                "seed-board",
                str(database),
                "200",
            ],
            env=seed_environment,
            check=True,
            stdout=subprocess.DEVNULL,
        )
        environment = {
            **os.environ,
            **seed_environment,
            "TERM": "xterm-256color",
            "XDG_CONFIG_HOME": str(sandbox / "config"),
            "XDG_STATE_HOME": str(sandbox / "state"),
            "XDG_CACHE_HOME": str(sandbox / "cache"),
            "PIKA_DB_PATH": str(database),
            "PIKA_TMUX_SOCKET": f"pika-benchmark-{uuid.uuid4()}",
            "PIKA_UPDATE_CHECK": "0",
        }
        result = {
            "native": measure(binary, environment, args.samples, args.input_samples)
        }
        if args.python_reference:
            result["python"] = measure(
                args.python_reference.resolve(strict=True),
                environment,
                args.samples,
                args.input_samples,
                b"q quit",
            )
        print(json.dumps(result, indent=2, sort_keys=True))


if __name__ == "__main__":
    main()
