from __future__ import annotations

import os
import shlex
import shutil
import subprocess
import sys
from pathlib import Path


SERVICE_NAME = "pika-expert-refresh.service"
TIMER_NAME = "pika-expert-refresh.timer"


def unit_directory() -> Path:
    base = Path(os.environ.get("XDG_CONFIG_HOME", Path.home() / ".config"))
    return base / "systemd" / "user"


def unit_contents(*, runtime_path: str | None = None) -> dict[Path, str]:
    directory = unit_directory()
    command = shlex.join(
        [sys.executable, "-m", "pikamux", "expert", "refresh", "--due", "--json"]
    )
    service_path = _systemd_escape(runtime_path or _service_path())
    return {
        directory / SERVICE_NAME: (
            "[Unit]\n"
            "Description=Refresh one stale Pika thread profile per provider\n\n"
            "[Service]\n"
            "Type=oneshot\n"
            f'Environment="PATH={service_path}"\n'
            f"ExecStart={command}\n"
            "Nice=10\n"
            "TimeoutStartSec=30min\n"
        ),
        directory / TIMER_NAME: (
            "[Unit]\n"
            "Description=Check whether Pika thread profiles are due for refresh\n\n"
            "[Timer]\n"
            "OnBootSec=10min\n"
            "OnUnitActiveSec=10min\n"
            "RandomizedDelaySec=2min\n"
            "Persistent=true\n\n"
            "[Install]\n"
            "WantedBy=timers.target\n"
        ),
    }


def _service_path() -> str:
    directories: list[str] = []
    for name in ("codex", "claude", "opencode", "node", "tmux", "git", "bash"):
        executable = shutil.which(name)
        if executable:
            directories.append(str(Path(executable).resolve().parent))
            directories.append(str(Path(executable).parent))
    directories.extend(os.defpath.split(os.pathsep))
    return os.pathsep.join(dict.fromkeys(directories))


def _systemd_escape(value: str) -> str:
    return value.replace("\\", "\\\\").replace('"', '\\"').replace("%", "%%")


def activate_timer() -> tuple[bool, str]:
    executable = shutil.which("systemctl")
    if executable is None:
        return False, "systemctl unavailable; run `pika expert refresh --due` manually"
    for command in (
        [executable, "--user", "daemon-reload"],
        [executable, "--user", "enable", "--now", TIMER_NAME],
    ):
        try:
            result = subprocess.run(
                command, capture_output=True, text=True, timeout=15, check=False
            )
        except (OSError, subprocess.TimeoutExpired) as exc:
            return False, f"could not activate expert refresh timer: {exc}"
        if result.returncode:
            detail = (result.stderr or result.stdout).strip()
            return False, detail or "systemd user timer activation failed"
    return True, "10-minute quota-aware expert refresh timer active"
