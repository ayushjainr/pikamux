from __future__ import annotations

import json
import os
import select
import shutil
import subprocess
import time
from dataclasses import dataclass
from datetime import datetime
from pathlib import Path
from typing import Any

from .paths import claude_home

WEEK_MINUTES = 7 * 24 * 60
OBSERVATION_MAX_AGE_SECONDS = 30 * 60


@dataclass(frozen=True, slots=True)
class QuotaSnapshot:
    provider: str
    used_percent: float
    reset_at: int
    observed_at: float
    source: str

    @property
    def remaining_percent(self) -> float:
        return max(0.0, 100.0 - self.used_percent)


def read_provider_quota(provider: str) -> QuotaSnapshot | None:
    if provider == "codex":
        return read_codex_quota()
    if provider == "claude":
        return read_claude_quota()
    return None


def read_codex_quota() -> QuotaSnapshot | None:
    """Read the authenticated account snapshot without starting a model turn."""
    executable = shutil.which("codex")
    if executable is None:
        return None
    process: subprocess.Popen[str] | None = None
    try:
        process = subprocess.Popen(
            [executable, "app-server", "--stdio"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
            bufsize=1,
            env=os.environ.copy(),
        )
        _rpc_request(
            process,
            1,
            "initialize",
            {
                "clientInfo": {
                    "name": "pikamux",
                    "title": "Pika quota observer",
                    "version": "0.2.0",
                },
                "capabilities": {"experimentalApi": True},
            },
            timeout=10,
        )
        _rpc_send(process, {"method": "initialized"})
        result = _rpc_request(
            process, 2, "account/rateLimits/read", None, timeout=10
        )
    except (OSError, ValueError, TimeoutError):
        return None
    finally:
        if process is not None:
            process.terminate()
            try:
                process.wait(timeout=1)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=1)
    return parse_codex_quota(result, observed_at=time.time())


def parse_codex_quota(
    data: dict[str, Any], *, observed_at: float
) -> QuotaSnapshot | None:
    snapshot = data.get("rateLimits")
    if not isinstance(snapshot, dict):
        return None
    windows = [
        value
        for value in (snapshot.get("primary"), snapshot.get("secondary"))
        if isinstance(value, dict)
    ]
    weekly = next(
        (
            value
            for value in windows
            if _weekly_window(value)
        ),
        None,
    )
    if weekly is None:
        return None
    used = _number(weekly.get("usedPercent"))
    reset = _integer(weekly.get("resetsAt"))
    if (
        used is None
        or not 0 <= used <= 100
        or reset is None
        or reset <= observed_at
    ):
        return None
    return QuotaSnapshot("codex", used, reset, observed_at, "account RPC")


def read_claude_quota(
    *, path: Path | None = None, now: float | None = None
) -> QuotaSnapshot | None:
    """Read Claude Code's local subscription-utilization observation."""
    now = time.time() if now is None else now
    target = path or claude_home().with_suffix(".json")
    try:
        data = json.loads(target.read_text())
    except (OSError, ValueError):
        return None
    if not isinstance(data, dict):
        return None
    cached = data.get("cachedUsageUtilization")
    if not isinstance(cached, dict):
        return None
    observed = _number(cached.get("fetchedAtMs"))
    utilization = cached.get("utilization")
    weekly = utilization.get("seven_day") if isinstance(utilization, dict) else None
    if not isinstance(weekly, dict) or observed is None:
        return None
    observed /= 1000
    if now - observed > OBSERVATION_MAX_AGE_SECONDS or observed > now + 60:
        return None
    used = _number(weekly.get("utilization"))
    reset = _iso_timestamp(weekly.get("resets_at"))
    if used is None or not 0 <= used <= 100 or reset is None or reset <= now:
        return None
    return QuotaSnapshot("claude", used, reset, observed, "Claude usage cache")


def _rpc_send(process: subprocess.Popen[str], message: dict[str, Any]) -> None:
    if process.stdin is None:
        raise OSError("app-server input unavailable")
    process.stdin.write(json.dumps(message, separators=(",", ":")) + "\n")
    process.stdin.flush()


def _rpc_request(
    process: subprocess.Popen[str],
    request_id: int,
    method: str,
    params: dict[str, Any] | None,
    *,
    timeout: float,
) -> dict[str, Any]:
    _rpc_send(
        process,
        {"method": method, "id": request_id, "params": params},
    )
    if process.stdout is None:
        raise OSError("app-server output unavailable")
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        ready, _, _ = select.select(
            [process.stdout], [], [], min(1.0, deadline - time.monotonic())
        )
        if not ready:
            if process.poll() is not None:
                break
            continue
        line = process.stdout.readline()
        if not line:
            break
        try:
            message = json.loads(line)
        except ValueError:
            continue
        if not isinstance(message, dict):
            continue
        if message.get("id") == request_id and "method" not in message:
            if message.get("error"):
                raise ValueError(str(message["error"]))
            result = message.get("result")
            return result if isinstance(result, dict) else {}
        if "id" in message and "method" in message:
            _rpc_send(
                process,
                {
                    "id": message["id"],
                    "error": {"code": -32000, "message": "non-interactive observer"},
                },
            )
    raise TimeoutError(f"{method} timed out")


def _number(value: object) -> float | None:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    return float(value)


def _weekly_window(value: dict[str, Any]) -> bool:
    minutes = _number(value.get("windowDurationMins"))
    return minutes is not None and minutes >= WEEK_MINUTES


def _integer(value: object) -> int | None:
    number = _number(value)
    return int(number) if number is not None else None


def _iso_timestamp(value: object) -> int | None:
    if not isinstance(value, str):
        return None
    try:
        return int(datetime.fromisoformat(value.replace("Z", "+00:00")).timestamp())
    except ValueError:
        return None
