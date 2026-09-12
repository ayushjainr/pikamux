#!/usr/bin/env python3
"""Build Pika's one-use universal Python-to-native transition wheel."""

from __future__ import annotations

import base64
import hashlib
import json
from pathlib import Path
import re
import sys
import zipfile

TARGETS = {
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
    "aarch64-unknown-linux-musl",
    "x86_64-unknown-linux-musl",
}
MAX_WHEEL_BYTES = 64 * 1024 * 1024


def fail(message: str) -> "None":
    raise SystemExit(f"Pika bridge release: {message}")


def digest(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def record_hash(data: bytes) -> str:
    value = base64.urlsafe_b64encode(hashlib.sha256(data).digest()).rstrip(b"=").decode()
    return f"sha256={value}"


def main(argv: list[str]) -> int:
    if len(argv) != 4 + len(TARGETS):
        fail(
            "usage: package-python-bridge.py 0.5.0a5 NATIVE_VERSION OUTPUT "
            "TARGET=ARCHIVE [four supported targets]"
        )
    bridge_version, native_version, output_name, *pairs = argv[1:]
    if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+(?:(?:a|b|rc)[0-9]+)?", bridge_version):
        fail("bridge version must be accepted by the frozen Python updater")
    if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+(?:(?:a|b|rc)[0-9]+|-(?:alpha|beta|rc)\.[0-9]+)?", native_version):
        fail("invalid native version")
    output = Path(output_name)
    if output.exists():
        fail("output already exists; release bytes were not replaced")

    archives: dict[str, Path] = {}
    for pair in pairs:
        target, separator, raw_path = pair.partition("=")
        if not separator or target not in TARGETS or target in archives:
            fail(f"invalid or duplicate target mapping: {pair}")
        path = Path(raw_path)
        expected = f"pikamux-{native_version}-{target}.tar.gz"
        if path.name != expected or not path.is_file() or path.is_symlink():
            fail(f"expected a regular {expected}")
        archives[target] = path
    if set(archives) != TARGETS:
        fail("the universal bridge requires every supported Mac/Linux target")

    repository = Path(__file__).resolve().parent.parent
    package = repository / "bridge" / "pikamux_bridge"
    install = (repository / "scripts" / "install.sh").read_bytes()
    manifest = {
        "schema": 2,
        "package": "pikamux",
        "version": native_version,
        "channel": "stable" if re.fullmatch(r"\d+\.\d+\.\d+", native_version) else "preview",
        "artifacts": {},
    }
    bridge_init = (package / "__init__.py").read_text()
    bridge_init, substitutions = re.subn(
        r'__version__ = "[^"]+"',
        f'__version__ = "{bridge_version}"',
        bridge_init,
        count=1,
    )
    if substitutions != 1:
        fail("bridge version source marker is missing or ambiguous")
    files: dict[str, bytes] = {
        "pikamux_bridge/__init__.py": bridge_init.encode(),
        "pikamux_bridge/cli.py": (package / "cli.py").read_bytes(),
        "pikamux_bridge/agent-convo/SKILL.md": (
            repository / "assets" / "agent-convo" / "SKILL.md"
        ).read_bytes(),
        "pikamux_bridge/native/install.sh": install,
        "pikamux_bridge/native/pika-version": f"{native_version}\n".encode(),
        "pikamux_bridge/native/LICENSE": (repository / "LICENSE").read_bytes(),
        "pikamux_bridge/native/THIRD_PARTY.md": (repository / "THIRD_PARTY.md").read_bytes(),
    }
    for target, path in sorted(archives.items()):
        data = path.read_bytes()
        if not data or len(data) > 20 * 1024 * 1024:
            fail(f"native archive has invalid size: {target}")
        checksum = digest(data)
        manifest["artifacts"][target] = {
            "file": path.name,
            "sha256": checksum,
            "bytes": len(data),
        }
        files[f"pikamux_bridge/native/{path.name}"] = data
        files[f"pikamux_bridge/native/{path.name}.sha256"] = f"{checksum}\n".encode()
    files["pikamux_bridge/native/pika-native-release.json"] = (
        json.dumps(manifest, indent=2, sort_keys=True) + "\n"
    ).encode()

    dist = f"pikamux-{bridge_version}.dist-info"
    files[f"{dist}/LICENSE"] = (repository / "LICENSE").read_bytes()
    files[f"{dist}/THIRD_PARTY.md"] = (repository / "THIRD_PARTY.md").read_bytes()
    files[f"{dist}/METADATA"] = (
        "Metadata-Version: 2.1\n"
        "Name: pikamux\n"
        f"Version: {bridge_version}\n"
        "Summary: One-use Pika native transition bridge\n"
        "License: MIT\n"
        "Requires-Python: >=3.9\n\n"
    ).encode()
    files[f"{dist}/WHEEL"] = (
        "Wheel-Version: 1.0\nGenerator: pika-native-transition\n"
        "Root-Is-Purelib: true\nTag: py3-none-any\n"
    ).encode()
    files[f"{dist}/entry_points.txt"] = b"[console_scripts]\npika = pikamux_bridge.cli:main\n"
    record = f"{dist}/RECORD"
    rows = [f"{name},{record_hash(data)},{len(data)}" for name, data in sorted(files.items())]
    rows.append(f"{record},,")
    files[record] = ("\n".join(rows) + "\n").encode()

    output.parent.mkdir(parents=True, exist_ok=True)
    temporary = output.with_name(f".{output.name}.tmp")
    try:
        with zipfile.ZipFile(temporary, "x", compression=zipfile.ZIP_DEFLATED, compresslevel=9) as wheel:
            for name, data in sorted(files.items()):
                info = zipfile.ZipInfo(name, (2020, 1, 1, 0, 0, 0))
                info.compress_type = zipfile.ZIP_DEFLATED
                # Encode an explicit regular-file type, not only permission
                # bits, so the publication verifier can reject links and
                # special members without ambiguity.
                info.external_attr = 0o100644 << 16
                wheel.writestr(info, data, compresslevel=9)
        if temporary.stat().st_size > MAX_WHEEL_BYTES:
            fail("universal bridge wheel exceeds the frozen updater's 64 MiB limit")
        temporary.replace(output)
    finally:
        temporary.unlink(missing_ok=True)

    schema_one = {
        "schema": 1,
        "version": bridge_version,
        "wheel": output.name,
        "sha256": digest(output.read_bytes()),
    }
    (output.parent / "pika-release.json").write_text(
        json.dumps(schema_one, indent=2, sort_keys=True) + "\n"
    )
    (output.parent / "pika-version").write_text(f"{bridge_version}\n")
    for name in ("LICENSE", "THIRD_PARTY.md"):
        destination = output.parent / name
        if not destination.exists():
            destination.write_bytes((repository / name).read_bytes())
    checksum_file = output.parent / "SHA256SUMS"
    rows = []
    for path in sorted(output.parent.iterdir(), key=lambda item: item.name):
        if path.is_file() and not path.is_symlink() and path != checksum_file:
            rows.append(f"{digest(path.read_bytes())}  {path.name}")
    checksum_file.write_text("\n".join(rows) + "\n")
    print(f"Pika transition bridge: {output}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
