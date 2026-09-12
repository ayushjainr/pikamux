#!/usr/bin/env bash
# Offline publication gate for an already assembled native or transition release.
set -euo pipefail

fail() { printf 'Pika release verification: %s\n' "$*" >&2; exit 1; }
[ "$#" -eq 1 ] || fail 'usage: verify-release.sh RELEASE_DIRECTORY'
pika_release=$1
[ -d "$pika_release" ] && [ ! -L "$pika_release" ] || fail 'release must be a real directory'

for pika_command in python3; do
    command -v "$pika_command" >/dev/null 2>&1 || fail "required command missing: $pika_command"
done
pika_script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)
pika_repo_dir=$(dirname "$pika_script_dir")

python3 - "$pika_release" "$pika_repo_dir" <<'PY'
import base64
import csv
import gzip
import hashlib
import io
import json
from pathlib import Path
import re
import struct
import sys
import tarfile
import zipfile

root = Path(sys.argv[1])
source = Path(sys.argv[2])
bridge_targets = {
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
    "aarch64-unknown-linux-musl",
    "x86_64-unknown-linux-musl",
}
native_targets = bridge_targets | {"x86_64-pc-windows-msvc"}
allowed_top_level = {"LICENSE", "SHA256SUMS", "THIRD_PARTY.md", "pika-version"}

def fail(message):
    raise SystemExit(f"Pika release verification: {message}")

def unique_json_object(pairs):
    value = {}
    for key, item in pairs:
        if key in value:
            fail(f"duplicate JSON key: {key}")
        value[key] = item
    return value

def parse_json(raw):
    return json.loads(raw, object_pairs_hook=unique_json_object)

def binary_shape(data, target):
    if len(data) < 64 * 1024 or len(data) > 50 * 1024 * 1024:
        fail(f"native executable has an invalid size: {target}")
    if target.endswith("apple-darwin"):
        if data[:4] != b"\xcf\xfa\xed\xfe":
            fail(f"native executable is not a 64-bit Mach-O: {target}")
        cpu = struct.unpack_from("<I", data, 4)[0]
        expected = 0x0100000C if target.startswith("aarch64") else 0x01000007
        if cpu != expected:
            fail(f"native executable architecture mismatch: {target}")
    elif target.endswith("linux-musl"):
        if data[:6] != b"\x7fELF\x02\x01":
            fail(f"native executable is not a little-endian ELF64: {target}")
        machine = struct.unpack_from("<H", data, 18)[0]
        expected = 183 if target.startswith("aarch64") else 62
        if machine != expected:
            fail(f"native executable architecture mismatch: {target}")
    else:
        if data[:2] != b"MZ" or len(data) < 0x40:
            fail("Windows executable is not PE/COFF")
        pe = struct.unpack_from("<I", data, 0x3C)[0]
        if pe + 6 > len(data) or data[pe:pe + 4] != b"PE\0\0":
            fail("Windows executable has no valid PE header")
        if struct.unpack_from("<H", data, pe + 4)[0] != 0x8664:
            fail("Windows executable is not x86_64")

def archive_executable(data, target):
    if target == "x86_64-pc-windows-msvc":
        try:
            with zipfile.ZipFile(io.BytesIO(data)) as archive:
                infos = archive.infolist()
                if len(infos) != 1 or infos[0].filename != "pika.exe":
                    fail(f"native archive has the wrong shape: {target}")
                info = infos[0]
                mode = (info.external_attr >> 16) & 0o170000
                if info.is_dir() or mode == 0o120000 or info.flag_bits & 1:
                    fail(f"native archive member is unsafe: {target}")
                if info.file_size > 50 * 1024 * 1024:
                    fail(f"native executable is oversized: {target}")
                data = archive.read(info)
        except zipfile.BadZipFile as exc:
            fail(f"native archive is not a ZIP file: {target}: {exc}")
    else:
        try:
            with gzip.GzipFile(fileobj=io.BytesIO(data)) as compressed:
                expanded = compressed.read(51 * 1024 * 1024 + 1)
            if len(expanded) > 51 * 1024 * 1024:
                fail(f"native archive expands beyond its safety limit: {target}")
            with tarfile.open(fileobj=io.BytesIO(expanded), mode="r:") as archive:
                members = archive.getmembers()
                if len(members) != 1 or members[0].name != "pika":
                    fail(f"native archive has the wrong shape: {target}")
                member = members[0]
                if not member.isfile() or not member.mode & 0o111:
                    fail(f"native archive executable is unsafe: {target}")
                if member.size > 50 * 1024 * 1024:
                    fail(f"native executable is oversized: {target}")
                stream = archive.extractfile(member)
                if stream is None:
                    fail(f"native archive executable is unreadable: {target}")
                data = stream.read(50 * 1024 * 1024 + 1)
                if len(data) != member.size:
                    fail(f"native archive executable size mismatch: {target}")
        except (tarfile.TarError, EOFError, OSError) as exc:
            fail(f"native archive is not a tar.gz file: {target}: {exc}")
    binary_shape(data, target)

def check_native(manifest_path, asset_root):
    try:
        if manifest_path.stat().st_size > 65536:
            fail("native manifest exceeds 64 KiB")
        raw = manifest_path.read_bytes()
        value = parse_json(raw)
        if set(value) != {"schema", "package", "version", "channel", "artifacts"}:
            fail("native manifest fields are not exact")
        if value["schema"] != 2 or value["package"] != "pikamux":
            fail("invalid native manifest identity")
        version = value["version"]
        if not isinstance(version, str) or not re.fullmatch(
            r"\d+\.\d+\.\d+(?:(?:a|b|rc)\d+|-(?:alpha|beta|rc)\.\d+)?", version
        ):
            fail("native version is invalid")
        expected_channel = "stable" if re.fullmatch(r"\d+\.\d+\.\d+", version) else "preview"
        if value["channel"] != expected_channel:
            fail("native channel/version mismatch")
        if set(value["artifacts"]) != native_targets:
            fail("native manifest must contain all supported release targets")
        for target, artifact in value["artifacts"].items():
            extension = "zip" if target == "x86_64-pc-windows-msvc" else "tar.gz"
            expected = f"pikamux-{value['version']}-{target}.{extension}"
            if set(artifact) != {"file", "sha256", "bytes"} or artifact["file"] != expected:
                fail(f"invalid native artifact row: {target}")
            path = asset_root / expected
            if (
                path.is_symlink()
                or not isinstance(artifact["bytes"], int)
                or isinstance(artifact["bytes"], bool)
                or artifact["bytes"] <= 0
                or artifact["bytes"] > 20 * 1024 * 1024
                or path.stat().st_size != artifact["bytes"]
            ):
                fail(f"native artifact size mismatch: {target}")
            data = path.read_bytes()
            checksum = hashlib.sha256(data).hexdigest()
            if checksum != artifact["sha256"]:
                fail(f"native artifact checksum mismatch: {target}")
            if (asset_root / f"{expected}.sha256").read_text() != checksum + "\n":
                fail(f"native sidecar mismatch: {target}")
            archive_executable(data, target)
        return value
    except (KeyError, OSError, TypeError, ValueError, json.JSONDecodeError) as exc:
        fail(f"invalid native release: {exc}")

native = root / "pika-native-release.json"
bridge = root / "pika-release.json"
if native.is_file():
    native_manifest = check_native(native, root)
    if (root / "pika-version").read_text() != native_manifest["version"] + "\n":
        fail("pika-version does not match the native manifest")
    allowed_top_level.update({"install.sh", native.name})
    for artifact in native_manifest["artifacts"].values():
        allowed_top_level.update({artifact["file"], f'{artifact["file"]}.sha256'})
if bridge.is_file():
    try:
        if bridge.stat().st_size > 65536:
            fail("bridge manifest exceeds 64 KiB")
        value = parse_json(bridge.read_text())
        if set(value) != {"schema", "version", "wheel", "sha256"} or value["schema"] != 1:
            fail("invalid schema-1 bridge manifest")
        version = value["version"]
        if not re.fullmatch(r"\d+\.\d+\.\d+(?:(?:a|b|rc)\d+)?", version):
            fail("bridge version is not accepted by the frozen updater")
        if value["wheel"] != f"pikamux-{version}-py3-none-any.whl":
            fail("bridge wheel/version mismatch")
        allowed_top_level.update({bridge.name, value["wheel"]})
        wheel_path = root / value["wheel"]
        if wheel_path.is_symlink() or wheel_path.stat().st_size > 64 * 1024 * 1024:
            fail("bridge wheel is missing or oversized")
        data = wheel_path.read_bytes()
        if hashlib.sha256(data).hexdigest() != value["sha256"]:
            fail("bridge wheel checksum mismatch")
        with zipfile.ZipFile(wheel_path) as wheel:
            listed = wheel.namelist()
            names = set(listed)
            if len(names) != len(listed):
                fail("bridge wheel contains duplicate member names")
            for info in wheel.infolist():
                parts = Path(info.filename).parts
                mode = (info.external_attr >> 16) & 0o170000
                if (
                    not info.filename
                    or info.filename.startswith(("/", "\\"))
                    or "\\" in info.filename
                    or ".." in parts
                    or mode == 0o120000
                ):
                    fail(f"unsafe bridge wheel member: {info.filename}")
            embedded = "pikamux_bridge/native/"
            required = {
                "pikamux_bridge/agent-convo/SKILL.md",
                embedded + "install.sh",
                embedded + "pika-version",
                embedded + "pika-native-release.json",
            }
            if not required.issubset(names):
                fail("bridge has no embedded native manifest")
            with wheel.open(embedded + "pika-native-release.json") as stream:
                payload = json.load(stream, object_pairs_hook=unique_json_object)
            if set(payload) != {"schema", "package", "version", "channel", "artifacts"}:
                fail("embedded native manifest fields are not exact")
            if payload["schema"] != 2 or payload["package"] != "pikamux":
                fail("embedded native manifest identity is invalid")
            native_version = payload["version"]
            if not isinstance(native_version, str) or not re.fullmatch(
                r"\d+\.\d+\.\d+(?:(?:a|b|rc)\d+|-(?:alpha|beta|rc)\.\d+)?",
                native_version,
            ):
                fail("embedded native version is invalid")
            expected_channel = (
                "stable" if re.fullmatch(r"\d+\.\d+\.\d+", native_version) else "preview"
            )
            if payload["channel"] != expected_channel:
                fail("embedded native channel/version mismatch")
            if set(payload["artifacts"]) != bridge_targets:
                fail("bridge does not embed every supported native target")
            for target, artifact in payload["artifacts"].items():
                expected = f"pikamux-{native_version}-{target}.tar.gz"
                if set(artifact) != {"file", "sha256", "bytes"} or artifact["file"] != expected:
                    fail(f"invalid embedded native artifact row: {target}")
                archive_name = embedded + expected
                sidecar_name = archive_name + ".sha256"
                if archive_name not in names or sidecar_name not in names:
                    fail("bridge is missing a declared native archive or sidecar")
                info = wheel.getinfo(archive_name)
                if (
                    not isinstance(artifact["bytes"], int)
                    or isinstance(artifact["bytes"], bool)
                    or artifact["bytes"] <= 0
                    or info.file_size != artifact["bytes"]
                    or info.file_size > 20 * 1024 * 1024
                ):
                    fail(f"embedded native artifact size mismatch: {target}")
                archive = wheel.read(info)
                archive_executable(archive, target)
                checksum = hashlib.sha256(archive).hexdigest()
                if checksum != artifact["sha256"]:
                    fail(f"embedded native artifact checksum mismatch: {target}")
                if wheel.read(sidecar_name) != (checksum + "\n").encode():
                    fail(f"embedded native artifact sidecar mismatch: {target}")

            record_name = f"pikamux-{version}.dist-info/RECORD"
            if record_name not in names:
                fail("bridge wheel has no RECORD")
            rows = list(csv.reader(io.StringIO(wheel.read(record_name).decode())))
            if len(rows) != len(names):
                fail("bridge wheel RECORD does not cover every member exactly once")
            recorded = set()
            for name, encoded_hash, encoded_size in rows:
                if name in recorded or name not in names:
                    fail("bridge wheel RECORD contains an invalid or duplicate member")
                recorded.add(name)
                if name == record_name:
                    if encoded_hash or encoded_size:
                        fail("bridge wheel RECORD self-row must be unhashed")
                    continue
                data = wheel.read(name)
                if encoded_size != str(len(data)) or not encoded_hash.startswith("sha256="):
                    fail(f"bridge wheel RECORD metadata mismatch: {name}")
                expected = base64.urlsafe_b64encode(hashlib.sha256(data).digest()).rstrip(b"=").decode()
                if encoded_hash.removeprefix("sha256=") != expected:
                    fail(f"bridge wheel RECORD checksum mismatch: {name}")
    except (KeyError, OSError, TypeError, ValueError, json.JSONDecodeError, zipfile.BadZipFile) as exc:
        fail(f"invalid transition bridge: {exc}")
if not native.is_file() and not bridge.is_file():
    fail("release contains neither a native nor bridge manifest")
entries = list(root.iterdir())
unsafe = sorted(path.name for path in entries if path.is_symlink() or not path.is_file())
if unsafe:
    fail(f"release contains unsafe top-level entries: {', '.join(unsafe)}")
regular_files = {path.name for path in entries}
missing = sorted(allowed_top_level - regular_files)
if missing:
    fail(f"release is missing top-level file(s): {', '.join(missing)}")
unexpected = sorted(regular_files - allowed_top_level)
if unexpected:
    fail(f"release contains unexpected top-level file(s): {', '.join(unexpected)}")
for name, source_path in {
    "install.sh": source / "scripts/install.sh",
    "LICENSE": source / "LICENSE",
    "THIRD_PARTY.md": source / "THIRD_PARTY.md",
}.items():
    if name in regular_files and (root / name).read_bytes() != source_path.read_bytes():
        fail(f"release {name} does not match the audited source")

checksum_path = root / "SHA256SUMS"
if checksum_path.is_symlink() or checksum_path.stat().st_size > 64 * 1024:
    fail("SHA256SUMS is unsafe or oversized")
declared = {}
for line in checksum_path.read_text().splitlines():
    match = re.fullmatch(r"([0-9a-f]{64})  ([A-Za-z0-9][A-Za-z0-9._-]*)", line)
    if not match or match.group(2) in declared:
        fail("SHA256SUMS contains an invalid or duplicate row")
    declared[match.group(2)] = match.group(1)
expected_names = regular_files - {"SHA256SUMS"}
if set(declared) != expected_names:
    fail("SHA256SUMS does not cover the exact release file set")
for name, digest in declared.items():
    if hashlib.sha256((root / name).read_bytes()).hexdigest() != digest:
        fail(f"checksum mismatch: {name}")
PY

printf 'Pika release verified: %s\n' "$pika_release"
