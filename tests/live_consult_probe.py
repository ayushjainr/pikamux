"""Explicit, bounded installed-CLI consultation probe. Spends provider quota.

Stores receipts locally, never changes a parent or its watch state. This is not
part of pytest. Inspect answers against independently selected evidence.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import signal
import subprocess
import time


def digest(path):
    if not path:
        return None
    result = hashlib.sha256()
    with open(path, "rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            result.update(chunk)
    return result.hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--pika", default="pika")
    parser.add_argument("--target", required=True)
    parser.add_argument("--question", required=True)
    parser.add_argument("--parent-path")
    parser.add_argument("--output", required=True)
    parser.add_argument("--timeout", type=float, default=120)
    parser.add_argument("--fast", action="store_true")
    args = parser.parse_args()
    output = Path(args.output)
    output.mkdir(parents=True, exist_ok=True)
    before = digest(args.parent_path)
    command = [args.pika, "ask", args.target, "--jsonl"]
    if args.fast:
        command.append("--fast")
    started = time.monotonic()
    process = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                               stderr=subprocess.PIPE, text=True, start_new_session=True)
    request = json.dumps({"question": args.question}) + '\n{"close":true}\n'
    timed_out = False
    try:
        stdout, stderr = process.communicate(request, timeout=args.timeout)
    except subprocess.TimeoutExpired:
        timed_out = True
        process.send_signal(signal.SIGINT)
        try:
            stdout, stderr = process.communicate(timeout=15)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGTERM)
            stdout, stderr = process.communicate(timeout=5)
    events = []
    for line in stdout.splitlines():
        try:
            events.append(json.loads(line))
        except ValueError:
            events.append({"type": "invalid_output", "line": line})
    after = digest(args.parent_path)
    summary = {
        "target": args.target, "pid": process.pid,
        "elapsed_seconds": round(time.monotonic() - started, 3),
        "returncode": process.returncode, "timed_out": timed_out,
        "answers": sum(event.get("type") == "answer" for event in events),
        "cleanup_confirmed": any(event.get("type") == "closed" and event.get("discarded") is True for event in events),
        "parent_sha256_before": before, "parent_sha256_after": after,
        "parent_bytes_unchanged": before == after if before is not None else None,
        "usage": "not available in this CLI receipt; no cost inferred",
    }
    (output / "events.json").write_text(json.dumps(events, indent=2) + "\n")
    (output / "stderr.txt").write_text(stderr)
    (output / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    print(json.dumps(summary, indent=2))
    return int(timed_out or process.returncode != 0 or summary["answers"] != 1 or not summary["cleanup_confirmed"])


if __name__ == "__main__":
    raise SystemExit(main())
