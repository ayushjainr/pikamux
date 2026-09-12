"""Measure warm-board process-tree RSS, idle CPU, and terminal output."""

from __future__ import annotations

import argparse
import json
import os
import select
import shutil
import subprocess
import sys
import tempfile
import time
import uuid
from pathlib import Path

from measure_board import spawn_board, stop


def percentile(values: list[float], fraction: float) -> float:
    ordered = sorted(values)
    return ordered[int((len(ordered) - 1) * fraction)]


def process_tree_metrics(root_pid: int) -> tuple[int, float]:
    output = subprocess.run(
        ["ps", "-axo", "pid=,ppid=,rss=,%cpu="],
        check=True,
        stdout=subprocess.PIPE,
        text=True,
    ).stdout
    rows: dict[int, tuple[int, int, float]] = {}
    for line in output.splitlines():
        fields = line.split()
        if len(fields) != 4:
            continue
        try:
            pid, parent, rss, cpu = int(fields[0]), int(fields[1]), int(fields[2]), float(fields[3])
        except ValueError:
            continue
        rows[pid] = (parent, rss, cpu)
    tree = {root_pid}
    changed = True
    while changed:
        changed = False
        for pid, (parent, _, _) in rows.items():
            if parent in tree and pid not in tree:
                tree.add(pid)
                changed = True
    present = [rows[pid] for pid in tree if pid in rows]
    return sum(row[1] for row in present), sum(row[2] for row in present)


def seed(root: Path, project: Path) -> tuple[Path, dict[str, str]]:
    for child in (
        "home",
        "config",
        "state",
        "cache",
        "tmp",
        "codex",
        "claude",
        "opencode",
        "bin",
    ):
        (root / child).mkdir(parents=True, exist_ok=True)
    fake_ssh = root / "bin" / "ssh"
    fake_ssh.write_text("#!/bin/sh\n/bin/sleep 2\nexit 255\n")
    fake_ssh.chmod(0o700)
    database = root / "state" / "pika.db"
    tmux = shutil.which("tmux")
    path_parts = [str(root / "bin")]
    if tmux:
        path_parts.append(str(Path(tmux).parent))
    path_parts.extend(["/usr/bin", "/bin"])
    path = os.pathsep.join(path_parts)
    base = {
        "HOME": str(root / "home"),
        "PATH": path,
        "PYTHONPATH": str(project / "tests" / "fixtures" / "python-v0.5.0a4"),
        "PYTHONDONTWRITEBYTECODE": "1",
        "PIKA_CONFIG_HOME": str(root / "config" / "pika"),
        "PIKA_STATE_HOME": str(root / "state"),
        "PIKA_DB_PATH": str(database),
        "CODEX_HOME": str(root / "codex"),
        "CLAUDE_CONFIG_DIR": str(root / "claude"),
        "OPENCODE_DATA_HOME": str(root / "opencode"),
        "OPENCODE_CONFIG_DIR": str(root / "opencode"),
        "TMPDIR": str(root / "tmp"),
    }
    subprocess.run(
        [
            os.environ.get("PYTHON", "python3"),
            str(project / "tests" / "fixtures" / "mixed_runtime_driver.py"),
            "seed-board",
            str(database),
            "200",
        ],
        env=base,
        check=True,
        stdout=subprocess.DEVNULL,
    )
    return database, {
        **os.environ,
        **base,
        "TERM": "xterm-256color",
        "XDG_CONFIG_HOME": str(root / "config"),
        "XDG_STATE_HOME": str(root / "state"),
        "XDG_CACHE_HOME": str(root / "cache"),
        "PIKA_TMUX_SOCKET": f"pika-steady-{uuid.uuid4()}",
        "PIKA_UPDATE_CHECK": "0",
    }


def measure(
    program: Path,
    environment: dict[str, str],
    seconds: int,
    complete_marker: bytes,
) -> dict[str, float | int]:
    pid, fd, first_frame = spawn_board(program, environment, complete_marker)
    rss: list[float] = []
    cpu: list[float] = []
    output_bytes = 0
    deadline = time.monotonic() + seconds
    next_sample = time.monotonic()
    try:
        while time.monotonic() < deadline:
            ready, _, _ = select.select([fd], [], [], min(0.25, deadline - time.monotonic()))
            if ready:
                try:
                    output_bytes += len(os.read(fd, 65536))
                except OSError:
                    break
            if time.monotonic() >= next_sample:
                tree_rss, tree_cpu = process_tree_metrics(pid)
                rss.append(float(tree_rss))
                cpu.append(tree_cpu)
                next_sample += 1.0
    finally:
        stop(pid, fd)
    return {
        "seconds": seconds,
        "samples": len(rss),
        "first_frame_ms": round(first_frame, 3),
        "rss_p50_mib": round(percentile(rss, 0.50) / 1024, 3),
        "rss_p95_mib": round(percentile(rss, 0.95) / 1024, 3),
        "rss_max_mib": round(max(rss) / 1024, 3),
        "cpu_mean_percent": round(sum(cpu) / len(cpu), 3),
        "cpu_p50_percent": round(percentile(cpu, 0.50), 3),
        "cpu_p95_percent": round(percentile(cpu, 0.95), 3),
        "cpu_tail_mean_percent": round(
            sum(cpu[len(cpu) // 2 :]) / len(cpu[len(cpu) // 2 :]), 3
        ),
        "terminal_output_bytes_after_first_frame": output_bytes,
    }


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("native", type=Path)
    parser.add_argument("python_reference", type=Path)
    parser.add_argument("--seconds", type=int, default=300)
    parser.add_argument("--runs", type=int, default=1)
    parser.add_argument("--program", choices=("native", "python", "both"), default="both")
    args = parser.parse_args()
    if args.seconds < 10:
        raise SystemExit("--seconds must be at least 10")
    if args.runs < 1:
        raise SystemExit("--runs must be at least 1")
    project = Path(__file__).resolve().parents[1]
    programs = {
        "native": args.native.resolve(strict=True),
        "python": args.python_reference.resolve(strict=True),
    }
    if args.program != "both":
        programs = {args.program: programs[args.program]}
    results: dict[str, list[dict[str, float | int]]] = {
        label: [] for label in programs
    }
    with tempfile.TemporaryDirectory(prefix="pika-steady-benchmark-") as temporary:
        root = Path(temporary)
        configured = list(programs.items())
        for run in range(args.runs):
            # Alternate order so machine drift does not systematically favour
            # either implementation while keeping contenders non-concurrent.
            ordered = configured if run % 2 == 0 else list(reversed(configured))
            for label, program in ordered:
                _, environment = seed(root / f"{run}-{label}", project)
                complete_marker = b"q quit" if label == "python" else b"q leave"
                results[label].append(
                    measure(program, environment, args.seconds, complete_marker)
                )
                print(
                    f"completed steady-state run {run + 1}/{args.runs}: {label}",
                    file=sys.stderr,
                    flush=True,
                )
    summary = {
        label: {
            "runs": len(rows),
            "cpu_mean_percent_across_runs": round(
                sum(float(row["cpu_mean_percent"]) for row in rows) / len(rows), 3
            ),
            "cpu_worst_run_percent": max(
                float(row["cpu_mean_percent"]) for row in rows
            ),
            "rss_p95_worst_run_mib": max(float(row["rss_p95_mib"]) for row in rows),
            "rss_max_mib": max(float(row["rss_max_mib"]) for row in rows),
        }
        for label, rows in results.items()
    }
    print(
        json.dumps(
            {"run_seconds": args.seconds, "results": results, "summary": summary},
            indent=2,
            sort_keys=True,
        )
    )


if __name__ == "__main__":
    main()
