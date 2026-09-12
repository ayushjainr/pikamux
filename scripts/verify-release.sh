#!/usr/bin/env bash
# Offline publication gate for an already assembled native or transition release.
set -euo pipefail

fail() { printf 'Pika release verification: %s\n' "$*" >&2; exit 1; }
[ "$#" -eq 1 ] || fail 'usage: verify-release.sh RELEASE_DIRECTORY'
pika_release=$1
[ -d "$pika_release" ] && [ ! -L "$pika_release" ] || fail 'release must be a real directory'

for pika_command in python3 find wc cut; do
    command -v "$pika_command" >/dev/null 2>&1 || fail "required command missing: $pika_command"
done
if command -v sha256sum >/dev/null 2>&1; then
    (cd "$pika_release" && sha256sum --check SHA256SUMS)
elif command -v shasum >/dev/null 2>&1; then
    while read -r pika_sha pika_file; do
        [ "$(shasum -a 256 "$pika_release/$pika_file" | cut -d ' ' -f 1)" = "$pika_sha" ] || \
            fail "checksum mismatch: $pika_file"
    done < "$pika_release/SHA256SUMS"
else
    fail 'a SHA256 verifier is required'
fi
[ -z "$(find "$pika_release" -mindepth 1 -maxdepth 1 -type l -print -quit)" ] || \
    fail 'release contains a top-level symlink'

python3 - "$pika_release" <<'PY'
import base64
import csv
import hashlib
import io
import json
from pathlib import Path
import re
import sys
import zipfile

root = Path(sys.argv[1])
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

def check_native(manifest_path, asset_root):
    try:
        raw = manifest_path.read_bytes()
        if len(raw) > 65536:
            fail("native manifest exceeds 64 KiB")
        value = json.loads(raw)
        if set(value) != {"schema", "package", "version", "channel", "artifacts"}:
            fail("native manifest fields are not exact")
        if value["schema"] != 2 or value["package"] != "pikamux":
            fail("invalid native manifest identity")
        if set(value["artifacts"]) != native_targets:
            fail("native manifest must contain all supported release targets")
        for target, artifact in value["artifacts"].items():
            extension = "zip" if target == "x86_64-pc-windows-msvc" else "tar.gz"
            expected = f"pikamux-{value['version']}-{target}.{extension}"
            if set(artifact) != {"file", "sha256", "bytes"} or artifact["file"] != expected:
                fail(f"invalid native artifact row: {target}")
            path = asset_root / expected
            data = path.read_bytes()
            if path.is_symlink() or len(data) != artifact["bytes"]:
                fail(f"native artifact size mismatch: {target}")
            checksum = hashlib.sha256(data).hexdigest()
            if checksum != artifact["sha256"]:
                fail(f"native artifact checksum mismatch: {target}")
            if (asset_root / f"{expected}.sha256").read_text() != checksum + "\n":
                fail(f"native sidecar mismatch: {target}")
        return value
    except (KeyError, OSError, TypeError, ValueError, json.JSONDecodeError) as exc:
        fail(f"invalid native release: {exc}")

native = root / "pika-native-release.json"
bridge = root / "pika-release.json"
if native.is_file():
    native_manifest = check_native(native, root)
    allowed_top_level.update({"install.sh", native.name})
    for artifact in native_manifest["artifacts"].values():
        allowed_top_level.update({artifact["file"], f'{artifact["file"]}.sha256'})
if bridge.is_file():
    try:
        value = json.loads(bridge.read_text())
        if set(value) != {"schema", "version", "wheel", "sha256"} or value["schema"] != 1:
            fail("invalid schema-1 bridge manifest")
        version = value["version"]
        if not re.fullmatch(r"\d+\.\d+\.\d+(?:(?:a|b|rc)\d+)?", version):
            fail("bridge version is not accepted by the frozen updater")
        if value["wheel"] != f"pikamux-{version}-py3-none-any.whl":
            fail("bridge wheel/version mismatch")
        allowed_top_level.update({bridge.name, value["wheel"]})
        wheel_path = root / value["wheel"]
        data = wheel_path.read_bytes()
        if wheel_path.is_symlink() or len(data) > 64 * 1024 * 1024:
            fail("bridge wheel is missing or oversized")
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
                payload = json.load(stream)
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
                archive = wheel.read(archive_name)
                if (
                    not isinstance(artifact["bytes"], int)
                    or len(archive) != artifact["bytes"]
                    or len(archive) > 20 * 1024 * 1024
                ):
                    fail(f"embedded native artifact size mismatch: {target}")
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
regular_files = {
    path.name for path in root.iterdir() if path.is_file() and not path.is_symlink()
}
missing = sorted(allowed_top_level - regular_files)
if missing:
    fail(f"release is missing top-level file(s): {', '.join(missing)}")
unexpected = sorted(regular_files - allowed_top_level)
if unexpected:
    fail(f"release contains unexpected top-level file(s): {', '.join(unexpected)}")
PY

printf 'Pika release verified: %s\n' "$pika_release"
