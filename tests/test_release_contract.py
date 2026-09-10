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
    documents = [ROOT / name for name in ("README.md", "CONTRIBUTING.md", "SECURITY.md", "CHANGELOG.md")]
    documents.extend((ROOT / "docs").glob("*.md"))
    for document in documents:
        for target in re.findall(r"\]\(([^)]+)\)", document.read_text()):
            if "://" in target or target.startswith("#"):
                continue
            assert (document.parent / target.split("#", 1)[0]).exists(), (document.name, target)


def test_no_maintainer_home_dependency_in_tests():
    for path in (ROOT / "tests").glob("test_*.py"):
        # Assemble the sentinel so this test does not trip on its own source.
        assert "/mnt/" + "ebs1/" not in path.read_text(), path.name


@pytest.mark.skipif(not (ROOT / ".git").exists(), reason="gitignore checks need a checkout")
def test_local_state_and_credentials_are_ignored():
    paths = [".env", "auth.json", "pika.db", "pika.db-wal", "session.jsonl",
             "private.pem", "id_ed25519", "IMPLEMENTATION_STATUS.md", "audit.local.md"]
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
