from __future__ import annotations

import os
from pathlib import Path


def config_home() -> Path:
    override = os.environ.get("PIKA_CONFIG_HOME")
    if override:
        return Path(override).expanduser()
    base = Path(os.environ.get("XDG_CONFIG_HOME", Path.home() / ".config"))
    return base / "pika"


def state_home() -> Path:
    override = os.environ.get("PIKA_STATE_HOME")
    if override:
        return Path(override).expanduser()
    base = Path(os.environ.get("XDG_STATE_HOME", Path.home() / ".local" / "state"))
    return base / "pika"


def config_path() -> Path:
    return config_home() / "config.json"


def database_path() -> Path:
    override = os.environ.get("PIKA_DB_PATH")
    if override:
        return Path(override).expanduser()
    return state_home() / "pika.db"


def codex_home() -> Path:
    return Path(os.environ.get("CODEX_HOME", Path.home() / ".codex")).expanduser()


def claude_home() -> Path:
    return Path(
        os.environ.get("CLAUDE_CONFIG_DIR", Path.home() / ".claude")
    ).expanduser()


def opencode_data_home() -> Path:
    override = os.environ.get("OPENCODE_DATA_HOME")
    if override:
        return Path(override).expanduser()
    base = Path(os.environ.get("XDG_DATA_HOME", Path.home() / ".local" / "share"))
    return base / "opencode"


def opencode_config_home() -> Path:
    override = os.environ.get("OPENCODE_CONFIG_DIR")
    if override:
        return Path(override).expanduser()
    base = Path(os.environ.get("XDG_CONFIG_HOME", Path.home() / ".config"))
    return base / "opencode"
