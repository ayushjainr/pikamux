#!/usr/bin/env python3
"""Build a private, inspectable installation bundle; never publish or overwrite one."""
from __future__ import annotations

import argparse
from email.parser import BytesParser
import hashlib
import json
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import zipfile


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def validate_wheel(path: Path) -> str:
    with zipfile.ZipFile(path) as wheel:
        metadata = [name for name in wheel.namelist() if name.endswith(".dist-info/METADATA")]
        if len(metadata) != 1:
            raise ValueError("Expected exactly one wheel METADATA entry")
        value = BytesParser().parsebytes(wheel.read(metadata[0]))
        version = value.get("Version", "")
        if value.get("Name") != "pikamux" or not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+(?:(?:a|b|rc)[0-9]+)?", version):
            raise ValueError("Unexpected wheel package name/version")
        if path.name != f"pikamux-{version}-py3-none-any.whl":
            raise ValueError("Wheel filename must match its package version and universal platform")
        if "pikamux/installation.py" not in wheel.namelist():
            raise ValueError("Wheel is missing the Pika installation module")
    return version


def build_release(output: Path, *, source: Path, artifacts: Path | None = None, uv: str = "uv") -> Path:
    """Stage a validated bundle into a new directory. Existing directories are refused."""
    output = output.absolute()
    if output.exists() or output.is_symlink():
        raise FileExistsError(f"Output already exists; choose a fresh directory: {output}")
    installer = source / "install.sh"
    if not installer.is_file():
        raise ValueError("Source is missing install.sh")
    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="pika-release-build-", dir=output.parent) as temporary:
        staging = Path(temporary)
        if artifacts is None:
            artifacts = staging / "dist"
            subprocess.run([uv, "build", "--project", str(source), "--out-dir", str(artifacts)], check=True)
        wheels = list(artifacts.glob("*.whl"))
        sources = list(artifacts.glob("*.tar.gz"))
        if len(wheels) != 1 or len(sources) != 1:
            raise ValueError("Expected exactly one wheel and one source distribution")
        version = validate_wheel(wheels[0])
        if sources[0].name != f"pikamux-{version}.tar.gz":
            raise ValueError("Source distribution version must match wheel")
        bundle = staging / "bundle"
        bundle.mkdir()
        for artifact in (wheels[0], sources[0], installer):
            shutil.copyfile(artifact, bundle / artifact.name)
        (bundle / "install.sh").chmod(0o755)
        manifest = {"schema": 1, "version": version, "wheel": wheels[0].name, "sha256": sha256(wheels[0])}
        (bundle / "pika-release.json").write_text(json.dumps(manifest, indent=2) + "\n", encoding="utf-8")
        checksums = "".join(f"{sha256(path)}  {path.name}\n" for path in sorted(bundle.iterdir()))
        (bundle / "SHA256SUMS").write_text(checksums, encoding="utf-8")
        # mkdir is the final no-overwrite reservation, including against races with another builder.
        output.mkdir()
        for path in sorted(bundle.iterdir()):
            shutil.copy2(path, output / path.name)
    return output


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True, help="Fresh, nonexistent output directory")
    parser.add_argument("--artifacts", type=Path, help="Use already built wheel + sdist instead of building")
    parser.add_argument("--uv", default="uv", help="uv executable used for the build")
    args = parser.parse_args()
    try:
        output = build_release(args.output, source=Path(__file__).resolve().parents[1], artifacts=args.artifacts, uv=args.uv)
    except (OSError, ValueError, zipfile.BadZipFile, subprocess.CalledProcessError) as exc:
        parser.exit(1, f"Pika release bundle: {exc}\n")
    print(f"Private release bundle ready: {output}\nNothing uploaded or published.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
