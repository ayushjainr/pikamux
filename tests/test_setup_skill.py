"""Skill installation participates in setup's existing preview and write batch."""
import sys

import pytest

from pikamux import setup_hooks as setup
from pikamux.skill_package import skill_text


@pytest.fixture
def homes(tmp_path, monkeypatch):
    roots = {provider: tmp_path / provider for provider in ("codex", "claude", "opencode")}
    monkeypatch.setattr(setup, "codex_home", lambda: roots["codex"])
    monkeypatch.setattr(setup, "claude_home", lambda: roots["claude"])
    monkeypatch.setattr(setup, "opencode_config_home", lambda: roots["opencode"])
    monkeypatch.setattr(setup, "config_path", lambda: tmp_path / "pika/config.json")
    monkeypatch.setattr(setup, "unit_contents", lambda **_: {})
    return roots


def test_setup_proposes_skills_for_each_installed_provider_without_writing(homes):
    changes = setup.proposed_changes("codex", provider_executables={name: sys.executable for name in homes})
    skills = [change for change in changes if change.path.name == "SKILL.md"]
    assert {change.path for change in skills} == {
        root / "skills/agent-convo/SKILL.md" for root in homes.values()
    }
    assert all(change.changed and change.after == skill_text() for change in skills)
    assert not any(root.exists() for root in homes.values())
    setup.apply_changes(changes)
    assert all(change.path.read_text() == skill_text() for change in skills)


def test_absent_provider_does_not_receive_a_skill(homes):
    changes = setup.skill_setup_changes({"claude": sys.executable, "codex": "/missing/provider"})
    assert len(changes) == 1
    assert changes[0].path == homes["claude"] / "skills/agent-convo/SKILL.md"


def test_setup_skill_backup_preserves_resources_and_repeat_is_idempotent(homes):
    directory = homes["codex"] / "skills/agent-convo"
    directory.mkdir(parents=True)
    (directory / "SKILL.md").write_text("custom instructions\n")
    (directory / "notes.md").write_text("keep resource\n")
    changes = setup.skill_setup_changes({"codex": sys.executable})
    assert "custom instructions" in changes[0].diff()
    backups = setup.apply_changes(changes)
    assert len(backups) == 1 and backups[0].read_text() == "custom instructions\n"
    assert list(directory.rglob("SKILL.md")) == [directory / "SKILL.md"]
    assert (directory / "notes.md").read_text() == "keep resource\n"
    again = setup.skill_setup_changes({"codex": sys.executable})
    assert not again[0].changed
    assert setup.apply_changes(again) == []


@pytest.mark.parametrize("level", ["skills", "agent-convo", "SKILL.md"])
def test_setup_preserves_externally_managed_skill_symlinks(homes, tmp_path, level):
    target = homes["codex"] / "skills/agent-convo/SKILL.md"
    link = {"skills": target.parent.parent, "agent-convo": target.parent, "SKILL.md": target}[level]
    link.parent.mkdir(parents=True)
    external = tmp_path / "external"
    if level == "SKILL.md":
        external.write_text("external skill\n")
    else:
        external.mkdir()
    link.symlink_to(external)
    changes = setup.skill_setup_changes({"codex": sys.executable})
    assert len(changes) == 1 and not changes[0].changed
    assert "externally managed" in changes[0].notice
    assert setup.apply_changes(changes) == []
    assert link.is_symlink()
    if level == "SKILL.md":
        assert external.read_text() == "external skill\n"
    else:
        assert list(external.iterdir()) == []


@pytest.mark.parametrize("race", ["edit", "symlink"])
def test_changed_skill_preflight_stops_the_batch_before_any_write(homes, tmp_path, race):
    changes = setup.skill_setup_changes({"codex": sys.executable})
    target = changes[0].path
    target.parent.mkdir(parents=True)
    if race == "edit":
        target.write_text("new user edit")
    else:
        external = tmp_path / "external"
        external.write_text("external edit")
        target.symlink_to(external)
    config = tmp_path / "other-config"
    with pytest.raises(ValueError, match="Re-run pika setup"):
        setup.apply_changes([setup.FileChange(config, "", "planned"), *changes])
    assert not config.exists()
    assert target.read_text() == ("new user edit" if race == "edit" else "external edit")
