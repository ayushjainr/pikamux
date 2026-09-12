"""Side-effect-free probes and fail-closed first-use native activation."""

from __future__ import annotations

import json
import hashlib
import os
from pathlib import Path
import platform
import re
import select
import signal
import subprocess
import sys

from . import __version__

ROOT_MARKER = "pikamux-installer-v1\n"
MAX_JSON_BYTES = 64 * 1024
MAX_NATIVE_ARCHIVE_BYTES = 20 * 1024 * 1024
CHECKSUM_CHUNK_BYTES = 1024 * 1024
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


def _unique_object(pairs: list[tuple[str, object]]) -> dict[str, object]:
    value: dict[str, object] = {}
    for key, item in pairs:
        if key in value:
            raise ValueError(f"duplicate JSON key: {key}")
        value[key] = item
    return value


def _strict_json(path: Path, *, limit: int = MAX_JSON_BYTES) -> object:
    if path.is_symlink() or not path.is_file():
        raise ValueError(f"not a regular JSON file: {path.name}")
    with path.open("rb") as stream:
        payload = stream.read(limit + 1)
    if len(payload) > limit:
        raise ValueError(f"JSON file exceeds {limit} bytes: {path.name}")
    return json.loads(payload, object_pairs_hook=_unique_object)


def _bounded_text(path: Path, *, limit: int) -> str:
    with path.open("rb") as stream:
        payload = stream.read(limit + 1)
    if len(payload) > limit:
        raise ValueError(f"file exceeds {limit} bytes: {path.name}")
    return payload.decode("utf-8")


def _sha256_bounded(path: Path, *, limit: int) -> tuple[int, str]:
    checksum = hashlib.sha256()
    size = 0
    with path.open("rb") as stream:
        while chunk := stream.read(CHECKSUM_CHUNK_BYTES):
            size += len(chunk)
            if size > limit:
                raise ValueError(f"file exceeds {limit} bytes: {path.name}")
            checksum.update(chunk)
    return size, checksum.hexdigest()


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
        value = _strict_json(receipt_path)
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
        manifest = _strict_json(manifest_path)
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
        if (
            isinstance(row["bytes"], bool)
            or not isinstance(row["bytes"], int)
            or not 0 < row["bytes"] <= MAX_NATIVE_ARCHIVE_BYTES
        ):
            raise ValueError("invalid selected native artifact size")
        if not isinstance(row["sha256"], str) or not re.fullmatch(r"[0-9a-f]{64}", row["sha256"]):
            raise ValueError("invalid selected native artifact checksum")
        for path in (bundle / artifact, bundle / f"{artifact}.sha256", bundle / "install.sh"):
            if not path.is_file() or path.is_symlink():
                raise ValueError(f"missing native bridge asset: {path.name}")
        archive = bundle / artifact
        archive_size, checksum = _sha256_bounded(
            archive, limit=MAX_NATIVE_ARCHIVE_BYTES
        )
        if archive_size != row["bytes"]:
            raise ValueError("selected native artifact size differs")
        if checksum != row["sha256"] or _bounded_text(
            bundle / f"{artifact}.sha256", limit=128
        ) != checksum + "\n":
            raise ValueError("selected native artifact checksum differs")
    except (KeyError, OSError, TypeError, ValueError, json.JSONDecodeError) as exc:
        raise BridgeError("The verified native transition payload is incomplete; nothing changed.") from exc
    return bundle


def _activation_outcome(root: Path, bridge_prefix: Path) -> str:
    current = root / "current"
    try:
        if not current.is_symlink():
            raise OSError("current is not a symbolic link")
        active = current.resolve(strict=True)
        bridge = bridge_prefix.resolve(strict=True)
        releases = (root / "releases").resolve(strict=True)
        active.relative_to(releases)
    except (OSError, ValueError):
        return "the managed current release cannot be verified"
    if active == bridge:
        return "the Python bridge remains current"
    return f"managed release {active.name!r} is now current"


_ACTIVATION_SUPERVISOR = r"""
import os
import signal
import subprocess
import sys

receipt = int(sys.argv[1])
try:
    installer = subprocess.Popen(sys.argv[2:])
    code = installer.wait()
except BaseException:
    code = 125
try:
    os.write(receipt, (str(code) + "\n").encode("ascii"))
finally:
    os.close(receipt)
# Keep the session leader alive so its PID continues to pin the exact process
# group until the bridge has cleaned every same-group descendant.
while True:
    signal.pause()
"""


def _start_activation(command: list[str]) -> tuple[subprocess.Popen[bytes], int]:
    receipt_read, receipt_write = os.pipe()
    try:
        process = subprocess.Popen(
            [sys.executable, "-c", _ACTIVATION_SUPERVISOR, str(receipt_write), *command],
            start_new_session=True,
            pass_fds=(receipt_write,),
        )
    except BaseException:
        os.close(receipt_read)
        raise
    finally:
        os.close(receipt_write)
    return process, receipt_read


def _wait_activation_receipt(process: subprocess.Popen[bytes], receipt: int, timeout: float) -> int:
    """Read one bounded installer status while the supervisor pins the PGID."""
    ready, _, _ = select.select([receipt], [], [], timeout)
    if not ready:
        raise subprocess.TimeoutExpired(process.args, timeout)
    value = os.read(receipt, 32)
    if not re.fullmatch(rb"-?[0-9]+\n", value):
        raise BridgeError("Native activation returned an invalid status receipt.")
    return int(value)


def _stop_activation(process: subprocess.Popen[bytes]) -> str | None:
    """Terminate the owned group while its live supervisor pins the PGID."""
    failures: list[str] = []
    previous_interrupt = None
    try:
        previous_interrupt = signal.signal(signal.SIGINT, signal.SIG_IGN)
    except ValueError:
        # Activation runs on the main thread; retain safe cleanup if an embedder
        # invokes it elsewhere.
        pass
    try:
        # The supervisor deliberately remains alive after its installer exits,
        # pinning this exact session-owned process group. Clean same-group
        # descendants before wait(2) releases that identity; an escaped process
        # is outside our ownership.
        term_sent = False
        for owned_signal in (signal.SIGTERM, signal.SIGKILL):
            try:
                os.killpg(process.pid, owned_signal)
                term_sent = term_sent or owned_signal == signal.SIGTERM
            except ProcessLookupError:
                pass
            except PermissionError:
                # Darwin reports EPERM when a process group contains only the
                # already-signalled zombie leader. If TERM succeeded, any live
                # same-user descendant would still make SIGKILL signalable.
                if not (owned_signal == signal.SIGKILL and term_sent):
                    failures.append(
                        f"{signal.Signals(owned_signal).name} failed: permission denied"
                    )
            except OSError as exc:
                failures.append(f"{signal.Signals(owned_signal).name} failed: {exc}")
        try:
            process.wait(timeout=3)
        except subprocess.TimeoutExpired:
            failures.append("process group leader was not reaped after SIGKILL")
        except OSError as exc:
            failures.append(f"reap failed: {exc}")
    finally:
        if previous_interrupt is not None:
            signal.signal(signal.SIGINT, previous_interrupt)
    return "; ".join(failures) or None


def _activation_error(reason: str, root: Path, bridge_prefix: Path, cleanup: str | None) -> BridgeError:
    detail = f"{reason}; {_activation_outcome(root, bridge_prefix)}."
    if cleanup:
        detail += f" Installer cleanup could not be verified ({cleanup})."
    return BridgeError(detail)


def _activate_and_exec(arguments: list[str]) -> None:
    root, bin_dir = _managed_receipt()
    bundle = _native_bundle()
    bridge_prefix = Path(sys.prefix)
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
        process, receipt = _start_activation(command)
    except OSError as exc:
        raise _activation_error(
            f"Cannot start the verified native installer ({exc})",
            root,
            bridge_prefix,
            None,
        ) from exc
    try:
        try:
            return_code = _wait_activation_receipt(process, receipt, 600)
        finally:
            if receipt >= 0:
                os.close(receipt)
    except subprocess.TimeoutExpired as exc:
        cleanup = _stop_activation(process)
        raise _activation_error(
            "Native activation timed out", root, bridge_prefix, cleanup
        ) from exc
    except KeyboardInterrupt as exc:
        cleanup = _stop_activation(process)
        raise _activation_error(
            "Native activation was interrupted", root, bridge_prefix, cleanup
        ) from exc
    except BaseException as exc:
        cleanup = _stop_activation(process)
        raise _activation_error(
            f"Native activation stopped unexpectedly ({type(exc).__name__})",
            root,
            bridge_prefix,
            cleanup,
        ) from exc
    cleanup = _stop_activation(process)
    if cleanup:
        raise _activation_error(
            "Native activation cleanup was not verified", root, bridge_prefix, cleanup
        )
    if return_code:
        raise _activation_error(
            f"Native activation exited {return_code}", root, bridge_prefix, cleanup
        )
    launcher = bin_dir / "pika"
    try:
        if (root / "current").resolve() == bridge_prefix.resolve():
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
