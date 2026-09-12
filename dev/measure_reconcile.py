"""Measure full local reconciliation through the public list command."""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import tempfile
import time
import uuid
from pathlib import Path

from measure_board import percentile
from measure_board_steady import seed


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("native", type=Path)
    parser.add_argument("python_reference", type=Path)
    parser.add_argument("--samples", type=int, default=30)
    parser.add_argument("--rows", type=int, default=200)
    parser.add_argument("--remote-nodes", type=int, default=1)
    args = parser.parse_args()
    if args.samples < 1:
        raise SystemExit("--samples must be positive")
    if args.rows < 0:
        raise SystemExit("--rows must be non-negative")
    if not 1 <= args.remote_nodes <= 20:
        raise SystemExit("--remote-nodes must be between 1 and 20")
    project = Path(__file__).resolve().parents[1]
    programs = {
        "native": args.native.resolve(strict=True),
        "python": args.python_reference.resolve(strict=True),
    }
    results: dict[str, list[float]] = {label: [] for label in programs}
    with tempfile.TemporaryDirectory(prefix="pika-reconcile-benchmark-") as temporary:
        root = Path(temporary)
        environments: dict[str, dict[str, str]] = {}
        for index, label in enumerate(programs):
            _, environment = seed(
                root / f"{index}-{label}",
                project,
                args.rows,
                args.remote_nodes,
            )
            environment["PIKA_TMUX_SOCKET"] = f"pika-reconcile-{uuid.uuid4()}"
            environments[label] = environment
        for index in range(args.samples):
            order = ("native", "python") if index % 2 == 0 else ("python", "native")
            for label in order:
                started = time.perf_counter()
                completed = subprocess.run(
                    [str(programs[label]), "list", "--no-usage"],
                    env=environments[label],
                    stdout=subprocess.DEVNULL,
                    stderr=subprocess.PIPE,
                    timeout=10,
                )
                results[label].append((time.perf_counter() - started) * 1000)
                if completed.returncode:
                    raise RuntimeError(
                        f"{label}: {completed.stderr.decode(errors='replace')}"
                    )
    report = {}
    for label, timings in results.items():
        report[label] = {
            "samples": len(timings),
            "p50_ms": round(percentile(timings, 0.50), 3),
            "p95_ms": round(percentile(timings, 0.95), 3),
            "max_ms": round(max(timings), 3),
        }
    print(json.dumps(report, indent=2, sort_keys=True))


if __name__ == "__main__":
    main()
