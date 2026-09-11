"""Cheap source and opt-in distribution checks; no providers or live state."""
from __future__ import annotations

import os
import re
import shlex
import subprocess
import tarfile
import zipfile
from pathlib import Path
from unittest.mock import patch

import pytest

from pikamux import __version__
from pikamux.fleet import REMOTE_INSTALL_ARGV, SSHTransport


ROOT = Path(__file__).resolve().parents[1]


def test_readme_install_command_needs_no_version_selection():
    readme = (ROOT / "README.md").read_text()
    commands = [line for line in readme.splitlines() if line.startswith("curl ")]
    assert commands == ["curl -fsSL https://raw.githubusercontent.com/ayushjainr/pikamux/main/install.sh | bash"]
    assert "Install the current alpha," not in readme


def test_readme_uses_one_inline_video_without_production_labels():
    readme = (ROOT / "README.md").read_text()
    assert len(re.findall(
        r"^https://github\.com/user-attachments/assets/[0-9a-f-]+$", readme, re.M,
    )) == 1
    assert "Illustrative demo" in readme
    for phrase in ("with music", "with sound", "campaign-overview-v", "pika-with-music"):
        assert phrase not in readme.lower()


def test_remote_bootstrap_uses_matching_public_tag_without_github_ssh_credentials():
    url = f"https://github.com/ayushjainr/pikamux/releases/download/v{__version__}/install.sh"
    assert REMOTE_INSTALL_ARGV[:4] == ('bash', '-o', 'pipefail', '-c')
    assert url in shlex.split(REMOTE_INSTALL_ARGV[-1])[0]
    assert f'--version v{__version__} --no-setup' in shlex.split(REMOTE_INSTALL_ARGV[-1])[0]
    with patch("pikamux.fleet.subprocess.run") as run:
        run.return_value = subprocess.CompletedProcess([], 0, "installed", "")
        assert SSHTransport().install("buildbox") == (0, "installed")
    assert run.call_args.args[0][-len(REMOTE_INSTALL_ARGV):] == list(REMOTE_INSTALL_ARGV)
    assert run.call_args.kwargs["timeout"] == 180


def test_package_and_runtime_versions_match():
    metadata = (ROOT / "pyproject.toml").read_text()
    assert re.search(r'^version = "([^"]+)"', metadata, re.M).group(1) == __version__


def test_documentation_links_resolve():
    if (ROOT / ".git").exists():
        tracked = subprocess.run(["git", "ls-files", "-z", "*.md"], cwd=ROOT,
                                 capture_output=True, text=True, check=True).stdout
        documents = [ROOT / name for name in tracked.split("\0") if name]
        published = set(subprocess.run(["git", "ls-files", "-z"], cwd=ROOT,
                                      capture_output=True, text=True, check=True).stdout.split("\0"))
    else:
        documents = [*ROOT.glob("*.md"), *(ROOT / "docs").glob("*.md")]
        published = None
    for document in documents:
        for target in re.findall(r"\]\(([^)]+)\)", document.read_text()):
            if "://" in target or target.startswith("#"):
                continue
            path = (document.parent / target.split("#", 1)[0]).resolve()
            assert path.exists(), (document.name, target)
            if published is not None and path.is_file():
                assert path.relative_to(ROOT).as_posix() in published, (document.name, target)


def test_user_documentation_has_no_editorial_handoff_notes():
    documents = [ROOT / name for name in ("README.md", "SECURITY.md", "CONTRIBUTING.md", "DESIGN.md",
                 "docs/guide.md",
                 "docs/first-consultation.md", "docs/installing.md", "docs/releasing.md")]
    forbidden = ("the user rejected", "the user approved", "narrative revision pending",
                 "rory-inspired", "hopkins-inspired", "agent review score",
                 "public installer is pinned", "prepared candidate is")
    for document in documents:
        text = document.read_text().lower()
        assert not any(phrase in text for phrase in forbidden), document.name


def test_no_maintainer_home_dependency_in_tests():
    for path in (ROOT / "tests").glob("test_*.py"):
        # Assemble the sentinel so this test does not trip on its own source.
        assert "/mnt/" + "ebs1/" not in path.read_text(), path.name


@pytest.mark.skipif(not (ROOT / ".git").exists(), reason="gitignore checks need a checkout")
def test_local_state_and_credentials_are_ignored():
    paths = [".env", "auth.json", "pika.db", "pika.db-wal", "session.jsonl",
             "private.pem", "id_ed25519", "IMPLEMENTATION_STATUS.md", "audit.local.md",
             "docs/launch-playbook.md", "docs/campaign-brief.md", "docs/campaign-posts.md",
             "docs/campaign-rollout.md", "media/launch/SCRIPT.md", "media/launch/STORYBOARD.md",
             "media/launch/CROSS_PROJECT_CAPTURE.md", "media/launch/CROSS_PROJECT_FILM.md",
             "media/launch/CAMPAIGN_VIDEO.md", "media/launch/CAMPAIGN_OVERVIEW_STORYBOARD.md"]
    result = subprocess.run(
        ["git", "check-ignore", "--no-index", "--stdin"], input="\n".join(paths),
        cwd=ROOT, text=True, capture_output=True, check=True,
    )
    assert set(result.stdout.splitlines()) == set(paths)


@pytest.mark.skipif(not os.environ.get("PIKA_RELEASE_DIST"), reason="run after uv build")
def test_distributions_contain_only_public_artifacts():
    directory = Path(os.environ["PIKA_RELEASE_DIST"])
    wheels = list(directory.glob("*.whl"))
    sources = list(directory.glob("*.tar.gz"))
    assert wheels and sources, "build both wheel and sdist"
    for archive in wheels + sources:
        if archive.suffix == ".whl":
            with zipfile.ZipFile(archive) as handle:
                names = handle.namelist()
        else:
            with tarfile.open(archive) as handle:
                names = [member.name for member in handle.getmembers() if member.isfile()]
        assert any(name.endswith("skills/agent-convo/SKILL.md") for name in names)
        assert any(name.endswith("/LICENSE") for name in names)
        for name in names:
            assert not any(part in {".git", ".env", "__pycache__", "release-audit"} for part in Path(name).parts), name
            assert not name.endswith((".pyc", ".db", ".sqlite", ".jsonl", ".pem", ".key", ".local.md")), name
            assert "IMPLEMENTATION_STATUS" not in name and "pika-backup" not in name and "pika-skill-backup" not in name, name
            assert Path(name).name != "launch-playbook.md", name
            assert not (Path(name).name.startswith("campaign-") and name.endswith(".md")), name
