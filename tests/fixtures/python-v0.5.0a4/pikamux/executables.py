from __future__ import annotations

import json
import os
import plistlib
import re
import shutil
import subprocess
import sys
from pathlib import Path
from typing import Any

from .paths import config_path


PROVIDER_NAMES = ("codex", "claude", "opencode")
MINIMUM_PROVIDER_VERSIONS = {"opencode": (1, 18, 21)}
_PATH_LINE = re.compile(r'^Environment="PATH=(?P<value>.*)"$')
_SEMANTIC_VERSION = re.compile(r"(?<!\d)(\d+)\.(\d+)\.(\d+)(?!\d)")


def _raw_config() -> dict[str, Any]:
    try:
        value = json.loads(config_path().read_text())
    except (OSError, ValueError):
        return {}
    return value if isinstance(value, dict) else {}


def configured_executable(
    provider: str, *, config: dict[str, Any] | None = None
) -> str | None:
    """Return Pika's pinned provider command, falling back only for old configs."""
    values = (config if config is not None else _raw_config()).get(
        "provider_executables"
    )
    if isinstance(values, dict):
        saved = values.get(provider)
        if isinstance(saved, str) and saved.strip():
            return str(Path(saved).expanduser())
    return shutil.which(provider)


def executable_available(value: str | None) -> bool:
    return bool(value and Path(value).is_file() and os.access(value, os.X_OK))


def executable_version(value: str | None) -> str | None:
    if not executable_available(value):
        return None
    try:
        result = subprocess.run(
            [str(value), "--version"],
            capture_output=True,
            text=True,
            timeout=3,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired):
        return None
    return (result.stdout or result.stderr).strip() or None


def provider_compatibility_error(provider: str, version: str | None) -> str | None:
    """Explain a provider version Pika has not certified, if any."""
    minimum = MINIMUM_PROVIDER_VERSIONS.get(provider)
    if minimum is None:
        return None
    match = _SEMANTIC_VERSION.search(version or "")
    required = ".".join(map(str, minimum))
    if match is None:
        return (
            f"Pika requires {provider} >= {required}; could not verify version "
            f"from {version!r}"
        )
    found = tuple(int(part) for part in match.groups())
    if found < minimum:
        return f"Pika requires {provider} >= {required}; found {version}"
    return None


def provider_version_supported(provider: str, version: str | None) -> bool:
    return version is not None and provider_compatibility_error(provider, version) is None


def _service_path() -> str | None:
    if sys.platform == "darwin":
        from .expert_schedule import LAUNCHD_NAME, unit_directory

        try:
            value = plistlib.loads((unit_directory() / LAUNCHD_NAME).read_bytes())
            path = value.get("EnvironmentVariables", {}).get("PATH")
            return path if isinstance(path, str) else None
        except (OSError, ValueError, plistlib.InvalidFileException, AttributeError):
            return None
    base = Path(os.environ.get("XDG_CONFIG_HOME", Path.home() / ".config"))
    target = base / "systemd" / "user" / "pika-expert-refresh.service"
    try:
        lines = target.read_text().splitlines()
    except OSError:
        return None
    for line in lines:
        match = _PATH_LINE.match(line.strip())
        if match:
            return match.group("value").replace("%%", "%").replace('\\"', '"')
    return None


def _from_path(provider: str, value: str | None) -> str | None:
    if not value:
        return None
    for directory in value.split(os.pathsep):
        candidate = Path(directory).expanduser() / provider
        if executable_available(str(candidate)):
            return str(candidate.absolute())
    return None


def setup_executables(
    config: dict[str, Any], *, overrides: dict[str, str | None] | None = None
) -> dict[str, str]:
    """Choose provider commands once and preserve the choice across shells."""
    overrides = overrides or {}
    existing = config.get("provider_executables")
    existing = existing if isinstance(existing, dict) else {}
    prior_service_path = _service_path()
    result: dict[str, str] = {}
    for provider in PROVIDER_NAMES:
        explicit = overrides.get(provider)
        saved = existing.get(provider)
        selected = (
            str(Path(explicit).expanduser().absolute())
            if explicit
            else str(Path(saved).expanduser())
            if isinstance(saved, str) and saved.strip()
            else _from_path(provider, prior_service_path)
            or shutil.which(provider)
        )
        if selected:
            result[provider] = selected
    return result


def setup_runtime_path(
    config: dict[str, Any], executables: dict[str, str]
) -> str:
    """Pin the PATH needed by provider wrappers used by scheduled refresh."""
    saved = config.get("provider_runtime_path")
    directories: list[str] = []
    for executable in executables.values():
        if not executable:
            continue
        path = Path(executable)
        directories.extend((str(path.parent), str(path.resolve().parent)))
    previous = (
        saved
        if isinstance(saved, str) and saved.strip()
        else _service_path()
    )
    if previous:
        directories.extend(str(previous).split(os.pathsep))
    else:
        node = shutil.which("node")
        if node:
            directories.append(str(Path(node).parent))
        directories.extend(os.defpath.split(os.pathsep))
    return os.pathsep.join(dict.fromkeys(directories))
