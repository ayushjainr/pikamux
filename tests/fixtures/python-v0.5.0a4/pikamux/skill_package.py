"""Install the version-matched Pika skill, preserving existing local resources."""
from __future__ import annotations

import os
import shutil
import tempfile
from importlib.resources import files
from pathlib import Path


def skill_text() -> str:
    return files("pikamux").joinpath("skills", "agent-convo", "SKILL.md").read_text(encoding="utf-8")


def install_skill(target: Path) -> dict[str, str | bool | None]:
    target = target.expanduser().resolve()
    target.mkdir(parents=True, exist_ok=True)
    destination = target / "SKILL.md"
    content = skill_text()
    if destination.exists() and destination.read_text(encoding="utf-8") == content:
        return {"path": str(destination), "changed": False, "backup": None}
    backup = None
    if destination.exists():
        # A nested SKILL.md is discovered as another active skill by hosts.
        # Keep recoverable bytes without creating another skill directory.
        descriptor, filename = tempfile.mkstemp(
            prefix="SKILL-", suffix=".pika-backup", dir=target
        )
        os.close(descriptor)
        backup = Path(filename)
        shutil.copy2(destination, backup)
    descriptor, staged = tempfile.mkstemp(prefix=".pika-skill-", dir=target)
    try:
        with os.fdopen(descriptor, "w", encoding="utf-8") as stream:
            stream.write(content)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(staged, destination)
    finally:
        if os.path.exists(staged):
            os.unlink(staged)
    return {"path": str(destination), "changed": True, "backup": str(backup) if backup else None}
