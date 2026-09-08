from __future__ import annotations

import hashlib
import importlib.util
import json
from pathlib import Path
import subprocess
import zipfile

import pytest


spec = importlib.util.spec_from_file_location("pika_build_release", Path(__file__).resolve().parents[1] / "scripts/build_release.py")
builder = importlib.util.module_from_spec(spec)
spec.loader.exec_module(builder)


@pytest.fixture
def inputs(tmp_path):
    source = tmp_path / "source"
    source.mkdir()
    (source / "install.sh").write_text("#!/bin/bash\nexit 0\n")
    artifacts = tmp_path / "artifacts"
    artifacts.mkdir()
    wheel = artifacts / "pikamux-0.5.0a1-py3-none-any.whl"
    with zipfile.ZipFile(wheel, "w") as archive:
        archive.writestr("pikamux-0.5.0a1.dist-info/METADATA", "Name: pikamux\nVersion: 0.5.0a1\n")
        archive.writestr("pikamux/installation.py", "")
    (artifacts / "pikamux-0.5.0a1.tar.gz").write_bytes(b"source distribution fixture")
    return source, artifacts


def test_bundle_manifest_and_checksums_match_staged_bytes(inputs, tmp_path):
    source, artifacts = inputs
    output = builder.build_release(tmp_path / "release", source=source, artifacts=artifacts)
    manifest = json.loads((output / "pika-release.json").read_text())
    assert manifest == {"schema": 1, "version": "0.5.0a1", "wheel": "pikamux-0.5.0a1-py3-none-any.whl", "sha256": hashlib.sha256((output / "pikamux-0.5.0a1-py3-none-any.whl").read_bytes()).hexdigest()}
    lines = (output / "SHA256SUMS").read_text().splitlines()
    assert len(lines) == 4
    for line in lines:
        digest, name = line.split("  ", 1)
        assert hashlib.sha256((output / name).read_bytes()).hexdigest() == digest
    assert (output / "install.sh").stat().st_mode & 0o111


def test_existing_output_is_not_overwritten(inputs, tmp_path):
    source, artifacts = inputs
    output = tmp_path / "release"
    output.mkdir()
    sentinel = output / "keep"
    sentinel.write_text("old release")
    with pytest.raises(FileExistsError):
        builder.build_release(output, source=source, artifacts=artifacts)
    assert sentinel.read_text() == "old release"
    assert list(output.iterdir()) == [sentinel]


def test_invalid_wheel_version_is_rejected_before_output(inputs, tmp_path):
    source, artifacts = inputs
    wheel = next(artifacts.glob("*.whl"))
    wheel.rename(artifacts / "pikamux-9.0-py3-none-any.whl")
    with pytest.raises(ValueError, match="filename"):
        builder.build_release(tmp_path / "release", source=source, artifacts=artifacts)
    assert not (tmp_path / "release").exists()


def test_failed_build_never_publishes_output(inputs, tmp_path, monkeypatch):
    source, _ = inputs
    def fail(*args, **kwargs):
        raise subprocess.CalledProcessError(1, "uv build")
    monkeypatch.setattr(builder.subprocess, "run", fail)
    with pytest.raises(subprocess.CalledProcessError):
        builder.build_release(tmp_path / "release", source=source)
    assert not (tmp_path / "release").exists()
    assert not list(tmp_path.glob("pika-release-build-*"))


def test_build_command_is_argument_safe(inputs, tmp_path, monkeypatch):
    source, artifacts = inputs
    calls = []
    def fake_run(command, **kwargs):
        calls.append((command, kwargs))
        target = Path(command[command.index("--out-dir") + 1])
        target.mkdir()
        for path in artifacts.iterdir():
            (target / path.name).write_bytes(path.read_bytes())
    monkeypatch.setattr(builder.subprocess, "run", fake_run)
    builder.build_release(tmp_path / "release", source=source, uv="/private tools/uv")
    assert calls[0][0][:4] == ["/private tools/uv", "build", "--project", str(source)]
    assert calls[0][1] == {"check": True}
