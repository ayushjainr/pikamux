"""Side-effect-free probes and fail-closed first-use native activation."""

from __future__ import annotations

import json
import hashlib
import os
from pathlib import Path
import platform
import re
import signal
import subprocess
import sys

from . import __version__

ROOT_MARKER = "pikamux-installer-v1\n"
SUPPORTED_TARGETS = {
    ("Darwin", "arm64"): "aarch64-apple-darwin",
    ("Darwin", "aarch64"): "aarch64-apple-darwin",
    ("Darwin", "x86_64"): "x86_64-apple-darwin",
    ("Linux", "aarch64"): "aarch64-unknown-linux-musl",
    ("Linux", "arm64"): "aarch64-unknown-linux-musl",
    ("Linux", "x86_64"): "x86_64-unknown-linux-musl",
}


class BridgeError(RuntimeError):
    pass


def native_target(system: str | None = None, machine: str | None = None) -> str:
    key = (system or platform.system(), machine or platform.machine())
    try:
        return SUPPORTED_TARGETS[key]
    except KeyError as exc:
        raise BridgeError(
            f"Native Pika does not support this bridge host: {key[0]}/{key[1]}"
        ) from exc


def _managed_receipt() -> tuple[Path, Path]:
    receipt_path = Path(sys.prefix) / ".pika-install.json"
    try:
        value = json.loads(receipt_path.read_text())
        root = Path(value["root"])
        bin_dir = Path(value["bin_dir"])
        if value["schema"] != 1 or not root.is_absolute() or not bin_dir.is_absolute():
            raise ValueError("invalid owner")
        if root / "releases" != Path(sys.prefix).parent:
            raise ValueError("prefix is outside managed releases")
        if (root / "current").resolve() != Path(sys.prefix).resolve():
            raise ValueError("bridge is not current")
        marker = root / ".pika-install-root"
        if marker.is_symlink() or marker.read_text() != ROOT_MARKER:
            raise ValueError("invalid root marker")
    except (KeyError, OSError, TypeError, ValueError, json.JSONDecodeError) as exc:
        raise BridgeError(
            "This bridge is not the active installer-managed Pika; nothing changed."
        ) from exc
    return root, bin_dir


def _native_bundle() -> Path:
    bundle = Path(__file__).resolve().parent / "native"
    target = native_target()
    manifest_path = bundle / "pika-native-release.json"
    try:
        manifest = json.loads(manifest_path.read_text())
        if set(manifest) != {"schema", "package", "version", "channel", "artifacts"}:
            raise ValueError("unexpected native manifest fields")
        if manifest["schema"] != 2 or manifest["package"] != "pikamux":
            raise ValueError("invalid native manifest")
        version = manifest["version"]
        if not isinstance(version, str) or not re.fullmatch(
            r"\d+\.\d+\.\d+(?:(?:a|b|rc)\d+|-(?:alpha|beta|rc)\.\d+)?", version
        ):
            raise ValueError("invalid native version")
        expected_channel = "stable" if re.fullmatch(r"\d+\.\d+\.\d+", version) else "preview"
        if manifest["channel"] != expected_channel or not isinstance(manifest["artifacts"], dict):
            raise ValueError("invalid native release channel")
        row = manifest["artifacts"][target]
        artifact = f"pikamux-{version}-{target}.tar.gz"
        if set(row) != {"file", "sha256", "bytes"} or row["file"] != artifact:
            raise ValueError("invalid selected native artifact")
        if not isinstance(row["bytes"], int) or not 0 < row["bytes"] <= 100 * 1024 * 1024:
            raise ValueError("invalid selected native artifact size")
        if not isinstance(row["sha256"], str) or not re.fullmatch(r"[0-9a-f]{64}", row["sha256"]):
            raise ValueError("invalid selected native artifact checksum")
        for path in (bundle / artifact, bundle / f"{artifact}.sha256", bundle / "install.sh"):
            if not path.is_file() or path.is_symlink():
                raise ValueError(f"missing native bridge asset: {path.name}")
        archive = bundle / artifact
        if archive.stat().st_size != row["bytes"]:
            raise ValueError("selected native artifact size differs")
        checksum = hashlib.sha256(archive.read_bytes()).hexdigest()
        if checksum != row["sha256"] or (bundle / f"{artifact}.sha256").read_text() != checksum + "\n":
            raise ValueError("selected native artifact checksum differs")
    except (KeyError, OSError, TypeError, ValueError, json.JSONDecodeError) as exc:
        raise BridgeError("The verified native transition payload is incomplete; nothing changed.") from exc
    return bundle


def _activate_and_exec(arguments: list[str]) -> None:
    root, bin_dir = _managed_receipt()
    bundle = _native_bundle()
    command = [
        "/bin/bash",
        str(bundle / "install.sh"),
        "--bundle",
        str(bundle),
        "--root",
        str(root),
        "--bin-dir",
        str(bin_dir),
        "--no-setup",
    ]
    try:
        process = subprocess.Popen(command, start_new_session=True)
        try:
            return_code = process.wait(timeout=600)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGTERM)
            try:
                process.wait(timeout=3)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait()
            raise BridgeError("Native activation timed out; the Python bridge remains current.")
    except OSError as exc:
        raise BridgeError(f"Cannot start the verified native installer: {exc}") from exc
    if return_code:
        raise BridgeError(
            "Native activation failed; the Python bridge remains current and no agent was restarted."
        )
    launcher = bin_dir / "pika"
    try:
        if (root / "current").resolve() == Path(sys.prefix).resolve():
            raise BridgeError("Native activation did not switch the managed release.")
        os.execv(launcher, [str(launcher), *arguments])
    except OSError as exc:
        raise BridgeError(f"Native Pika was activated but could not be started: {exc}") from exc


def _help() -> str:
    return """Pika native transition bridge

The approved update is staged. The first ordinary Pika command activates the
verified native executable for this exact Mac/Linux target, then continues.

Validation commands: --version, --help, skill show
"""


def main(arguments: list[str] | None = None) -> int:
    arguments = list(sys.argv[1:] if arguments is None else arguments)
    if arguments == ["--version"]:
        print(f"pikamux {__version__}")
        return 0
    if not arguments or arguments in (["--help"], ["-h"]):
        if arguments:
            print(_help())
            return 0
        try:
            _activate_and_exec(arguments)
        except BridgeError as exc:
            print(f"pika: {exc}", file=sys.stderr)
            return 1
        return 0
    if arguments == ["skill", "show"]:
        print((Path(__file__).resolve().parent / "agent-convo" / "SKILL.md").read_text(), end="")
        return 0
    try:
        _activate_and_exec(arguments)
    except BridgeError as exc:
        print(f"pika: {exc}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
