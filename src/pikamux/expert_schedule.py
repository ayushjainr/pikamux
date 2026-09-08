from __future__ import annotations

import os
import plistlib
import shlex
import shutil
import subprocess
import sys
from pathlib import Path


SERVICE_NAME = "pika-expert-refresh.service"
TIMER_NAME = "pika-expert-refresh.timer"
LAUNCHD_LABEL = "io.pikamux.expert-refresh"
LAUNCHD_NAME = LAUNCHD_LABEL + ".plist"
_MACOS = sys.platform == "darwin"


def unit_directory() -> Path:
    if _MACOS:
        return Path.home() / "Library" / "LaunchAgents"
    base = Path(os.environ.get("XDG_CONFIG_HOME", Path.home() / ".config"))
    return base / "systemd" / "user"


def unit_contents(*, runtime_path: str | None = None) -> dict[Path, str]:
    directory = unit_directory()
    if _MACOS:
        return {
            directory / LAUNCHD_NAME: plistlib.dumps({
                "Label": LAUNCHD_LABEL,
                "ProgramArguments": [
                    sys.executable, "-m", "pikamux", "expert", "refresh", "--due", "--json"
                ],
                "EnvironmentVariables": {"PATH": runtime_path or _service_path()},
                "StartInterval": 600,
                "ProcessType": "Background",
            }).decode()
        }
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
    if _MACOS:
        return _activate_launch_agent()
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


def _activate_launch_agent() -> tuple[bool, str]:
    executable = shutil.which("launchctl")
    if executable is None:
        return False, "launchctl unavailable; run `pika expert refresh --due` manually"
    domain = f"gui/{os.getuid()}"
    target = f"{domain}/{LAUNCHD_LABEL}"

    def run(*arguments: str):
        return subprocess.run(
            [executable, *arguments], capture_output=True, text=True,
            timeout=15, check=False,
        )

    try:
        loaded = run("print", target)
        if loaded.returncode == 0:
            # bootout would terminate an in-flight expert consultation. Keep
            # it running; launchd reads the updated plist on the next login.
            return True, (
                "existing expert refresh LaunchAgent left running; "
                "saved schedule changes take effect at your next desktop login"
            )
        commands = [
            ("enable", target),
            ("bootstrap", domain, str(unit_directory() / LAUNCHD_NAME)),
            ("print", target),
        ]
        for arguments in commands:
            result = run(*arguments)
            if result.returncode:
                detail = (result.stderr or result.stdout).strip()
                return False, (
                    f"launchd {arguments[0]} failed: {detail}. "
                    "Log in to the Mac desktop and rerun `pika setup`; "
                    "or run `pika expert refresh --due` manually."
                )
    except (OSError, subprocess.TimeoutExpired) as exc:
        return False, f"could not activate expert refresh LaunchAgent: {exc}"
    return True, "10-minute quota-aware expert refresh LaunchAgent active"
