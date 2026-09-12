"""Compare native and frozen-Python hook latency in disposable state."""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import tempfile
import time
import uuid
from pathlib import Path


PAYLOAD = (
    b'{"session_id":"11111111-1111-4111-8111-111111111111",'
    b'"hook_event_name":"Stop","cwd":"/synthetic/project",'
    b'"session_title":"hook_bench","source":"bench"}\n'
)


def percentile(values: list[float], fraction: float) -> float:
    ordered = sorted(values)
    return ordered[int((len(ordered) - 1) * fraction)]


def result(values: list[float]) -> dict[str, float | int]:
    return {
        "samples": len(values),
        "p50_ms": round(percentile(values, 0.50), 3),
        "p95_ms": round(percentile(values, 0.95), 3),
        "max_ms": round(max(values), 3),
    }


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("native", type=Path)
    parser.add_argument("python_reference", type=Path)
    parser.add_argument("--samples", type=int, default=100)
    args = parser.parse_args()
    if args.samples < 1:
        raise SystemExit("--samples must be positive")
    programs = {
        "native": args.native.resolve(strict=True),
        "python": args.python_reference.resolve(strict=True),
    }
    timings: dict[str, list[float]] = {name: [] for name in programs}
    with tempfile.TemporaryDirectory(prefix="pika-hook-benchmark-") as temporary:
        root = Path(temporary)
        environments = {}
        for label in programs:
            base = root / label
            for child in ("home", "config", "state", "codex", "claude", "opencode"):
                (base / child).mkdir(parents=True, exist_ok=True)
            environments[label] = {
                **os.environ,
                "HOME": str(base / "home"),
                "XDG_CONFIG_HOME": str(base / "config"),
                "XDG_STATE_HOME": str(base / "state"),
                "PIKA_CONFIG_HOME": str(base / "config" / "pika"),
                "PIKA_STATE_HOME": str(base / "state" / "pika"),
                "PIKA_DB_PATH": str(base / "state" / "pika" / "pika.db"),
                "CODEX_HOME": str(base / "codex"),
                "CLAUDE_CONFIG_DIR": str(base / "claude"),
                "OPENCODE_DATA_HOME": str(base / "opencode"),
                "OPENCODE_CONFIG_DIR": str(base / "opencode"),
                "PIKA_TMUX_SOCKET": f"pika-hook-{label}-{uuid.uuid4()}",
                "PYTHONDONTWRITEBYTECODE": "1",
            }
        for index in range(args.samples):
            order = ("native", "python") if index % 2 == 0 else ("python", "native")
            for label in order:
                started = time.perf_counter()
                completed = subprocess.run(
                    [str(programs[label]), "hook", "--provider", "codex"],
                    input=PAYLOAD,
                    env=environments[label],
                    stdout=subprocess.DEVNULL,
                    stderr=subprocess.PIPE,
                    timeout=5,
                )
                timings[label].append((time.perf_counter() - started) * 1000)
                if completed.returncode:
                    raise RuntimeError(
                        f"{label} hook failed: {completed.stderr.decode(errors='replace')}"
                    )
    print(json.dumps({name: result(values) for name, values in timings.items()}, indent=2))


if __name__ == "__main__":
    main()
