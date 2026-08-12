from __future__ import annotations

import difflib
import hashlib
import json
import os
import re
import shlex
import shutil
import subprocess
import sys
import tempfile
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

from .paths import claude_home, codex_home, config_path
from .store import DEFAULT_CONFIG

CODEX_EVENTS = (
    "SessionStart",
    "UserPromptSubmit",
    "PermissionRequest",
    "PostToolUse",
    "Stop",
    "SessionEnd",
)
CLAUDE_EVENTS = (
    "SessionStart",
    "UserPromptSubmit",
    "PermissionRequest",
    "PostToolUse",
    "Notification",
    "Stop",
    "StopFailure",
    "SessionEnd",
)
CLAUDE_NOTIFICATION_MATCHER = (
    "permission_prompt|idle_prompt|elicitation_dialog|agent_needs_input|agent_completed"
)
FEATURE_HEADER = re.compile(r"^\s*\[features\]\s*(?:#.*)?$")
SECTION_HEADER = re.compile(r"^\s*\[[^]]+\]\s*(?:#.*)?$")
HOOKS_KEY = re.compile(
    r"^(?P<indent>\s*)hooks\s*=\s*(?P<value>[^#\n]+?)(?P<comment>\s*#.*)?$"
)


@dataclass(slots=True)
class FileChange:
    path: Path
    before: str
    after: str

    @property
    def changed(self) -> bool:
        return self.before != self.after

    def diff(self) -> str:
        return "".join(
            difflib.unified_diff(
                self.before.splitlines(keepends=True),
                self.after.splitlines(keepends=True),
                fromfile=str(self.path),
                tofile=str(self.path),
            )
        )


def _read_json(path: Path) -> dict[str, Any]:
    if not path.exists():
        return {}
    try:
        value = json.loads(path.read_text())
    except (OSError, ValueError) as exc:
        raise ValueError(f"Cannot safely merge invalid JSON at {path}: {exc}") from exc
    if not isinstance(value, dict):
        raise TypeError(f"Cannot safely merge non-object JSON at {path}")
    return value


def _handler_command(provider: str) -> str:
    return shlex.join([sys.executable, "-m", "pikamux", "hook", "--provider", provider])


def _handler_timeout(event: str) -> int:
    return 3 if event == "SessionEnd" else 5


def _contains_handler(groups: Any, provider: str, event: str) -> bool:
    if not isinstance(groups, list):
        return False
    for group in groups:
        if not isinstance(group, dict):
            continue
        matcher = group.get("matcher")
        expected_matcher = (
            CLAUDE_NOTIFICATION_MATCHER
            if provider == "claude" and event == "Notification"
            else None
        )
        if expected_matcher is not None:
            if matcher != expected_matcher:
                continue
        elif matcher not in {None, "", "*"}:
            continue
        for handler in group.get("hooks", []):
            if not isinstance(handler, dict):
                continue
            command = str(handler.get("command") or "")
            if (
                handler.get("type") == "command"
                and command == _handler_command(provider)
                and handler.get("timeout") == _handler_timeout(event)
                and _command_available(command)
            ):
                return True
    return False


def _is_pika_group(group: Any, provider: str) -> bool:
    if not isinstance(group, dict):
        return False
    handlers = group.get("hooks")
    if not isinstance(handlers, list):
        return False
    for handler in handlers:
        if not isinstance(handler, dict):
            continue
        command = str(handler.get("command") or "")
        if "pikamux" in command and f"--provider {provider}" in command:
            return True
        args = handler.get("args")
        if isinstance(args, list) and "pikamux" in args and provider in args:
            return True
    return False


def _command_available(command: str) -> bool:
    try:
        executable = shlex.split(command)[0]
    except (ValueError, IndexError):
        return False
    if os.path.sep in executable:
        return Path(executable).is_file() and os.access(executable, os.X_OK)
    return shutil.which(executable) is not None


def _normalize_legacy_claude_handlers(groups: Any) -> None:
    """Migrate the short-lived command-plus-args development format."""
    if not isinstance(groups, list):
        return
    needle = ["-m", "pikamux", "hook", "--provider", "claude"]
    for group in groups:
        if not isinstance(group, dict):
            continue
        for handler in group.get("hooks", []):
            if not isinstance(handler, dict):
                continue
            args = handler.get("args")
            if not isinstance(args, list) or not all(item in args for item in needle):
                continue
            handler["command"] = shlex.join(
                [str(handler.get("command") or sys.executable), *map(str, args)]
            )
            handler.pop("args", None)


def hook_spec_fingerprint(provider: str) -> str:
    events = CODEX_EVENTS if provider == "codex" else CLAUDE_EVENTS
    definition = [
        {
            "event": event,
            "command": _handler_command(provider),
            "timeout": _handler_timeout(event),
            "matcher": (
                CLAUDE_NOTIFICATION_MATCHER
                if provider == "claude" and event == "Notification"
                else None
            ),
        }
        for event in events
    ]
    return hashlib.sha256(
        json.dumps(definition, sort_keys=True, separators=(",", ":")).encode()
    ).hexdigest()


def codex_hooks_change(home: Path | None = None) -> FileChange:
    target = (home or codex_home()) / "hooks.json"
    before = target.read_text() if target.exists() else ""
    data = _read_json(target)
    data.setdefault(
        "description", "User lifecycle hooks, including Pikamux session tracking."
    )
    hooks = data.setdefault("hooks", {})
    if not isinstance(hooks, dict):
        raise TypeError(f"Cannot safely merge non-object hooks at {target}")
    command = _handler_command("codex")
    for event in CODEX_EVENTS:
        groups = hooks.setdefault(event, [])
        if not isinstance(groups, list):
            raise TypeError(f"Cannot safely merge non-list Codex hook event {event}")
        if _contains_handler(groups, "codex", event):
            continue
        groups[:] = [group for group in groups if not _is_pika_group(group, "codex")]
        timeout = _handler_timeout(event)
        groups.append(
            {
                "hooks": [
                    {
                        "type": "command",
                        "command": command,
                        "timeout": timeout,
                    }
                ]
            }
        )
    after = json.dumps(data, indent=2) + "\n"
    return FileChange(target, before, after)


def codex_config_change(home: Path | None = None) -> FileChange:
    target = (home or codex_home()) / "config.toml"
    before = target.read_text() if target.exists() else ""
    lines = before.splitlines(keepends=True)
    section_start: int | None = None
    section_end = len(lines)
    for index, line in enumerate(lines):
        stripped = line.rstrip("\n")
        if FEATURE_HEADER.match(stripped):
            if section_start is not None:
                raise ValueError(
                    f"Cannot safely merge duplicate [features] tables at {target}"
                )
            section_start = index
            continue
        if (
            section_start is not None
            and index > section_start
            and SECTION_HEADER.match(stripped)
        ):
            section_end = index
            break
    if section_start is None:
        prefix = before
        if prefix and not prefix.endswith("\n"):
            prefix += "\n"
        if prefix and not prefix.endswith("\n\n"):
            prefix += "\n"
        after = prefix + "[features]\nhooks = true\n"
        _validate_codex_config(after)
        return FileChange(target, before, after)
    found = False
    for index in range(section_start + 1, section_end):
        match = HOOKS_KEY.match(lines[index].rstrip("\n"))
        if match:
            suffix = match.group("comment") or ""
            lines[index] = f"{match.group('indent')}hooks = true{suffix}\n"
            found = True
            break
    if not found:
        lines.insert(section_end, "hooks = true\n")
    after = "".join(lines)
    _validate_codex_config(after)
    return FileChange(target, before, after)


def _validate_codex_config(value: str) -> None:
    executable = shutil.which("codex")
    if executable is None:
        return
    try:
        with tempfile.TemporaryDirectory(prefix="pika-codex-config-") as directory:
            Path(directory, "config.toml").write_text(value)
            environment = os.environ.copy()
            environment["CODEX_HOME"] = directory
            result = subprocess.run(
                [executable, "features", "list"],
                capture_output=True,
                text=True,
                timeout=5,
                check=False,
                env=environment,
            )
    except (OSError, subprocess.TimeoutExpired):
        return
    if result.returncode:
        message = result.stderr.strip() or result.stdout.strip()
        raise ValueError(f"Cannot safely merge invalid Codex config.toml: {message}")


def claude_settings_change(home: Path | None = None) -> FileChange:
    target = (home or claude_home()) / "settings.json"
    before = target.read_text() if target.exists() else ""
    data = _read_json(target)
    if data.get("disableAllHooks") is True:
        data["disableAllHooks"] = False
    hooks = data.setdefault("hooks", {})
    if not isinstance(hooks, dict):
        raise TypeError(f"Cannot safely merge non-object hooks at {target}")
    for event in CLAUDE_EVENTS:
        groups = hooks.setdefault(event, [])
        if not isinstance(groups, list):
            raise TypeError(f"Cannot safely merge non-list Claude hook event {event}")
        _normalize_legacy_claude_handlers(groups)
        if _contains_handler(groups, "claude", event):
            continue
        groups[:] = [group for group in groups if not _is_pika_group(group, "claude")]
        timeout = _handler_timeout(event)
        group: dict[str, Any] = {
            "hooks": [
                {
                    "type": "command",
                    "command": _handler_command("claude"),
                    "timeout": timeout,
                }
            ]
        }
        if event == "Notification":
            group["matcher"] = CLAUDE_NOTIFICATION_MATCHER
        groups.append(group)
    after = json.dumps(data, indent=2) + "\n"
    return FileChange(target, before, after)


def pika_config_change(default_provider: str) -> FileChange:
    target = config_path()
    before = target.read_text() if target.exists() else ""
    data = dict(DEFAULT_CONFIG)
    if before:
        try:
            existing = json.loads(before)
            if isinstance(existing, dict):
                data.update(existing)
        except ValueError as exc:
            raise ValueError(
                f"Cannot safely merge invalid JSON at {target}: {exc}"
            ) from exc
    data["default_provider"] = default_provider
    after = json.dumps(data, indent=2, sort_keys=True) + "\n"
    return FileChange(target, before, after)


def proposed_changes(default_provider: str) -> list[FileChange]:
    return [
        pika_config_change(default_provider),
        codex_hooks_change(),
        codex_config_change(),
        claude_settings_change(),
    ]


def apply_changes(changes: list[FileChange]) -> list[Path]:
    stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    backups: list[Path] = []
    for change in changes:
        if not change.changed:
            continue
        mode = 0o700 if change.path.parent == config_path().parent else 0o755
        change.path.parent.mkdir(parents=True, exist_ok=True, mode=mode)
        if change.path.parent == config_path().parent:
            try:
                os.chmod(change.path.parent, 0o700)
            except OSError:
                pass
        if change.path.exists():
            backup = change.path.with_name(f"{change.path.name}.pika-backup-{stamp}")
            suffix = 2
            while backup.exists():
                backup = change.path.with_name(
                    f"{change.path.name}.pika-backup-{stamp}-{suffix}"
                )
                suffix += 1
            shutil.copy2(change.path, backup)
            backups.append(backup)
        temporary = change.path.with_name(f".{change.path.name}.{os.getpid()}.tmp")
        temporary.write_text(change.after)
        os.chmod(temporary, 0o600)
        os.replace(temporary, change.path)
    return backups


def hooks_installed(provider: str) -> bool:
    try:
        if provider == "codex":
            data = _read_json(codex_home() / "hooks.json")
        elif provider == "claude":
            data = _read_json(claude_home() / "settings.json")
        else:
            return False
    except (TypeError, ValueError):
        return False
    hooks = data.get("hooks")
    if not isinstance(hooks, dict):
        return False
    events = CODEX_EVENTS if provider == "codex" else CLAUDE_EVENTS
    if provider == "claude" and data.get("disableAllHooks") is True:
        return False
    if provider == "codex" and not codex_hooks_enabled():
        return False
    return all(_contains_handler(hooks.get(event), provider, event) for event in events)


def codex_hooks_enabled(home: Path | None = None) -> bool:
    selected_home = home or codex_home()
    executable = shutil.which("codex")
    if executable:
        environment = os.environ.copy()
        environment["CODEX_HOME"] = str(selected_home)
        try:
            result = subprocess.run(
                [executable, "features", "list"],
                capture_output=True,
                text=True,
                timeout=5,
                check=False,
                env=environment,
            )
        except (OSError, subprocess.TimeoutExpired):
            result = None
        if result and result.returncode == 0:
            for line in result.stdout.splitlines():
                fields = line.split()
                if len(fields) >= 3 and fields[0] == "hooks":
                    return fields[-1].lower() == "true"
    target = selected_home / "config.toml"
    try:
        lines = target.read_text().splitlines()
    except OSError:
        return True
    in_features = False
    value: bool | None = None
    for line in lines:
        if FEATURE_HEADER.match(line):
            in_features = True
            continue
        if in_features and SECTION_HEADER.match(line):
            in_features = False
        if not in_features:
            continue
        match = HOOKS_KEY.match(line)
        if not match:
            continue
        raw = match.group("value").strip().lower()
        if raw == "true":
            value = True
        elif raw == "false":
            value = False
    return value is not False
