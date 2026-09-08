"""Native macOS process inspection, without lossy parsing of ``ps`` output.

psutil uses Darwin's process APIs and preserves argv boundaries. Never cache
Process instances across calls: a stored Pika lease must be checked against a
fresh kernel observation, not a cached creation time. Access denied means no
identity evidence, not permission to guess from a title or terminal.
"""
from __future__ import annotations

import os

import psutil


def read(pid: int, attribute: str, default=None):
    if pid <= 0:
        return default
    try:
        process = psutil.Process(pid)
        if process.uids().real != os.getuid():
            return default
        value = getattr(process, attribute)()
        # is_running checks this object's creation time against a fresh PID
        # lookup, rejecting reuse during the observation as well as exit.
        return value if process.is_running() else default
    except (psutil.Error, OSError, ValueError):
        return default


def pids() -> list[int]:
    try:
        return psutil.pids()
    except (psutil.Error, OSError):
        return []


def children(pid: int) -> list[int]:
    return [child.pid for child in read(pid, "children", [])]


def start_time(pid: int) -> int | None:
    value = read(pid, "create_time")
    # Darwin creation time includes microseconds. Keep Store's integer lease
    # representation, without Linux's start-tick assumptions.
    return round(value * 1_000_000) if value is not None else None


def state(pid: int) -> str | None:
    return {
        psutil.STATUS_RUNNING: "R",
        psutil.STATUS_SLEEPING: "S",
        psutil.STATUS_DISK_SLEEP: "D",
        psutil.STATUS_STOPPED: "T",
        psutil.STATUS_TRACING_STOP: "t",
        psutil.STATUS_ZOMBIE: "Z",
        psutil.STATUS_DEAD: "X",
        psutil.STATUS_IDLE: "I",
    }.get(read(pid, "status"))
