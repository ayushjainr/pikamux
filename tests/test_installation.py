"""Installer tests use isolated roots and never touch provider or Pika state."""
from __future__ import annotations

import base64
import csv
import hashlib
import io
import json
import os
from pathlib import Path
import re
import select
import shutil
import subprocess
import zipfile

import pytest

from pikamux import installation as installer


@pytest.fixture(autouse=True)
def isolated_provider_environment(monkeypatch):
    for key in list(os.environ):
        if key.startswith(("PIKA_", "XDG_")) and key != "PIKA_INSTALL_BUNDLE":
            monkeypatch.delenv(key)
    for key in ["CODEX_HOME", "CLAUDE_CONFIG_DIR", "OPENCODE_DATA_HOME", "OPENCODE_CONFIG_DIR", "ZDOTDIR"]:
        monkeypatch.delenv(key, raising=False)


def make_bundle(directory: Path, version="1.2.3", *, package="pikamux", payload="first"):
    directory.mkdir(parents=True, exist_ok=True)
    wheel = directory / f"pikamux-{version}-py3-none-any.whl"
    with zipfile.ZipFile(wheel, "w") as archive:
        archive.writestr(f"pikamux-{version}.dist-info/METADATA",
                         f"Metadata-Version: 2.1\nName: {package}\nVersion: {version}\n")
        archive.writestr("pikamux/example.py", payload)
    manifest = {"schema": 1, "version": version, "wheel": wheel.name,
                "sha256": installer.digest(wheel)}
    (directory / "pika-release.json").write_text(json.dumps(manifest))
    return wheel, manifest


@pytest.fixture
def installation(tmp_path, monkeypatch):
    home = tmp_path / "home"
    home.mkdir()
    monkeypatch.setenv("HOME", str(home))
    root = home / ".local/share/pikamux"
    bin_dir = home / ".local/bin"
    uv = tmp_path / "uv"
    uv.write_text("#!/bin/sh\nexit 0\n")
    uv.chmod(0o700)
    calls = []
    active_version = ["1.2.3"]

    def run(command):
        calls.append(command)
        if "venv" in command:
            (Path(command[-1]) / "bin").mkdir()
        if "install" in command:
            active_version[0] = Path(command[-1]).name.removeprefix("pikamux-").removesuffix("-py3-none-any.whl")
        return f"pikamux {active_version[0]}" if command[-1] == "--version" else ""

    monkeypatch.setattr(installer, "_run", run)
    wheel, manifest = make_bundle(tmp_path / "bundle")

    def install(wheel=wheel, checksum=None):
        return installer.install(wheel, checksum or installer.digest(wheel), uv,
                                 root=root, bin_dir=bin_dir)

    return home, root, bin_dir, uv, calls, install, wheel, manifest


def test_install_stages_checks_then_activates_and_saves_private_bundle(installation):
    _, root, bin_dir, _, calls, install, wheel, manifest = installation
    launcher = install()
    assert launcher == bin_dir / "pika"
    assert launcher.is_symlink()
    current = (root / "current").resolve()
    assert current.parent == root / "releases"
    assert [command[-1] for command in calls[-3:]] == ["--version", "--help", "show"]
    assert (current / "bundle" / wheel.name).read_bytes() == wheel.read_bytes()
    assert json.loads((current / "bundle/pika-release.json").read_text()) == manifest
    assert json.loads((current / ".pika-install.json").read_text())["version"] == "1.2.3"


@pytest.mark.parametrize("stage", ["venv", "install", "--version", "--help", "show"])
def test_failed_upgrade_preserves_previous_release(installation, tmp_path, monkeypatch, stage):
    _, root, _, _, _, install, _, _ = installation
    launcher = install()
    previous = (root / "current").resolve()
    original_run = installer._run
    wheel, _ = make_bundle(tmp_path / "next", "1.2.4")

    def failure(command):
        if stage in command:
            raise installer.InstallError("simulated failure")
        return original_run(command)

    monkeypatch.setattr(installer, "_run", failure)
    with pytest.raises(installer.InstallError, match="simulated failure"):
        install(wheel)
    assert (root / "current").resolve() == previous
    assert launcher.is_symlink()
    assert list((root / "releases").iterdir()) == [previous]


def test_wrong_postinstall_version_never_activates(installation, monkeypatch):
    _, root, _, _, _, install, _, _ = installation
    monkeypatch.setattr(installer, "_run", lambda command: "pikamux 9.9.9")
    with pytest.raises(installer.InstallError, match="version does not match"):
        install()
    assert not os.path.lexists(root / "current")
    assert list((root / "releases").iterdir()) == []


def test_checksum_failure_precedes_all_installation_writes(installation):
    _, root, _, _, calls, install, _, _ = installation
    with pytest.raises(installer.InstallError, match="checksum mismatch"):
        install(checksum="0" * 64)
    assert not root.exists()
    assert calls == []


def test_wrong_package_refused_before_installation(installation, tmp_path):
    _, root, _, _, _, install, _, _ = installation
    wheel, _ = make_bundle(tmp_path / "wrong", package="unrelated")
    with pytest.raises(installer.InstallError, match="supported Pika wheel"):
        install(wheel)
    assert not root.exists()


def test_foreign_root_is_not_overwritten(installation):
    _, root, _, _, _, install, _, _ = installation
    root.mkdir(parents=True)
    sentinel = root / "my-file"
    sentinel.write_text("preserve")
    with pytest.raises(installer.InstallError, match="not a Pika-managed installation"):
        install()
    assert sentinel.read_text() == "preserve"
    assert sorted(path.name for path in root.iterdir()) == ["my-file"]


@pytest.mark.parametrize("kind", ["directory", "outside-link"])
def test_foreign_activation_path_is_not_overwritten(installation, tmp_path, kind):
    _, root, _, _, _, install, _, _ = installation
    root.mkdir(parents=True)
    (root / ".pika-install-root").write_text(installer.ROOT_MARKER)
    current = root / "current"
    if kind == "directory":
        current.mkdir()
        (current / "sentinel").write_text("keep")
    else:
        outside = tmp_path / "outside"
        outside.mkdir()
        current.symlink_to(outside)
    with pytest.raises(installer.InstallError, match="activation path|outside its managed releases"):
        install()
    if kind == "directory":
        assert (current / "sentinel").read_text() == "keep"
    else:
        assert current.is_symlink()
        assert current.resolve() == outside


@pytest.mark.parametrize("symlink", [False, True])
def test_foreign_launcher_is_not_overwritten(installation, symlink):
    _, root, bin_dir, _, _, install, _, _ = installation
    bin_dir.mkdir(parents=True)
    launcher = bin_dir / "pika"
    if symlink:
        launcher.symlink_to(bin_dir / "other-install")
    else:
        launcher.write_text("keep my executable")
    with pytest.raises(installer.InstallError, match="belongs to another installation"):
        install()
    assert not root.exists()
    assert os.readlink(launcher) == str(bin_dir / "other-install") if symlink else launcher.read_text() == "keep my executable"


@pytest.mark.parametrize("root_kind", ["filesystem", "home", "workspace"])
def test_broad_roots_refused(installation, root_kind):
    home, _, bin_dir, uv, calls, _, wheel, manifest = installation
    root = {"filesystem": Path("/"), "home": home, "workspace": Path.cwd()}[root_kind]
    with pytest.raises(installer.InstallError, match="dedicated Pika installation directory"):
        installer.install(wheel, manifest["sha256"], uv, root=root, bin_dir=bin_dir)
    assert calls == []


def test_reinstall_is_idempotent_and_repairs_only_missing_launcher(installation):
    _, root, _, _, calls, install, _, _ = installation
    launcher = install()
    current = (root / "current").resolve()
    count = len(calls)
    install()
    launcher.unlink()
    install()
    assert launcher.is_symlink()
    assert (root / "current").resolve() == current
    assert len(calls) == count
    assert list((root / "releases").iterdir()) == [current]


@pytest.mark.parametrize("version,payload,message", [
    ("1.2.2", "first", "downgrade"),
    ("1.2.3", "different", "Same version, different bytes"),
])
def test_downgrade_and_mutated_version_refused(installation, tmp_path, version, payload, message):
    _, root, _, _, _, install, _, _ = installation
    install()
    previous = (root / "current").resolve()
    wheel, _ = make_bundle(tmp_path / "replacement", version, payload=payload)
    with pytest.raises(installer.InstallError, match=message):
        install(wheel)
    assert (root / "current").resolve() == previous


def test_install_and_update_leave_provider_and_pika_configuration_untouched(installation, tmp_path, monkeypatch):
    from pikamux.providers import ClaudeProvider, CodexProvider, OpenCodeProvider
    from pikamux.store import Store
    def forbidden(*args, **kwargs):
        pytest.fail("installation must not initialize state or discover provider conversations")
    monkeypatch.setattr(Store, "initialize", forbidden)
    for provider in (CodexProvider, ClaudeProvider, OpenCodeProvider):
        monkeypatch.setattr(provider, "discover", forbidden)
    home, _, _, _, _, install, _, _ = installation
    files = [home / path for path in [".codex/config.toml", ".claude/settings.json", ".config/opencode/opencode.json", ".config/pika/config.json", ".local/state/pika/pika.db"]]
    for path in files:
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(b"untouched user state")
    before = {path: (path.read_bytes(), path.stat().st_mtime_ns) for path in files}
    install()
    wheel, _ = make_bundle(tmp_path / "next", "1.2.4")
    install(wheel)
    assert {path: (path.read_bytes(), path.stat().st_mtime_ns) for path in files} == before


def test_concurrent_installer_is_rejected(installation):
    _, root, _, _, calls, install, _, _ = installation
    install()
    count = len(calls)
    with installer._lock(root):
        with pytest.raises(installer.InstallError, match="Another Pika installation/update"):
            install()
    assert len(calls) == count


@pytest.mark.parametrize("field,value", [
    ("wheel", "../../arbitrary.whl"), ("wheel", "/tmp/arbitrary.whl"),
    ("wheel", "https://evil.invalid/package.whl"), ("version", "../../path"),
    ("sha256", "not-a-digest"), ("schema", 2),
])
def test_manifest_cannot_select_arbitrary_files(tmp_path, field, value):
    _, manifest = make_bundle(tmp_path)
    manifest[field] = value
    path = tmp_path / "pika-release.json"
    path.write_text(json.dumps(manifest))
    with pytest.raises(installer.InstallError, match="Invalid release manifest"):
        installer.read_manifest(path)


def test_unmanaged_editable_copy_cannot_self_update(tmp_path, monkeypatch):
    monkeypatch.setattr(installer.sys, "prefix", str(tmp_path / "project/.venv"))
    with pytest.raises(installer.InstallError, match="editable checkouts are left untouched"):
        installer.update(check=True)


def test_update_check_noop_and_private_bundle(installation, tmp_path, monkeypatch, capsys):
    _, root, _, _, calls, install, wheel, _ = installation
    install()
    old = (root / "current").resolve()
    monkeypatch.setattr(installer.sys, "prefix", str(old))
    monkeypatch.setattr(installer, "_download", lambda *args, **kwargs: pytest.fail("private bundle must not download"))
    count = len(calls)
    installer.update(bundle=wheel.parent)
    assert len(calls) == count
    assert "already current" in capsys.readouterr().out
    next_wheel, _ = make_bundle(tmp_path / "next", "1.2.4")
    installer.update(bundle=next_wheel.parent, check=True)
    assert len(calls) == count
    assert (root / "current").resolve() == old
    assert "Update available" in capsys.readouterr().out
    installer.update(bundle=next_wheel.parent)
    assert (root / "current").resolve() != old
    assert old.is_dir()
    with pytest.raises(installer.InstallError, match="not an active installer-managed"):
        installer.managed_receipt()


def test_venv_and_index_environment_cannot_redirect_install(monkeypatch):
    for key in ["UV_PROJECT", "PIP_INDEX_URL", "PYTHONPATH", "PYTHONHOME", "VIRTUAL_ENV", "CONDA_PREFIX"]:
        monkeypatch.setenv(key, "untrusted-override")
    monkeypatch.setenv("TERM", "xterm-256color")
    environment = installer._environment()
    assert environment["TERM"] == "xterm-256color"
    assert not any(key.startswith(("UV_", "PIP_", "PYTHON")) for key in environment)
    assert "VIRTUAL_ENV" not in environment
    assert "CONDA_PREFIX" not in environment


def _next_real_wheel(source: Path, directory: Path, previous: str):
    """Synthetic test-only upgrade of the same package with a new version."""
    major, minor, patch = map(int, re.match(r"(\d+)\.(\d+)\.(\d+)", previous).groups())
    version = f"{major}.{minor}.{patch + 1}"
    directory.mkdir()
    wheel = directory / f"pikamux-{version}-py3-none-any.whl"
    files = {}
    with zipfile.ZipFile(source) as archive:
        for name in archive.namelist():
            name_new = name.replace(f"pikamux-{previous}.dist-info/", f"pikamux-{version}.dist-info/")
            if name.endswith(".dist-info/RECORD"):
                continue
            data = archive.read(name)
            if name.endswith(".dist-info/METADATA"):
                data = data.replace(f"Version: {previous}\n".encode(), f"Version: {version}\n".encode())
            if name == "pikamux/__init__.py":
                data = data.replace(previous.encode(), version.encode())
            files[name_new] = data
    record_name = f"pikamux-{version}.dist-info/RECORD"
    record = io.StringIO()
    writer = csv.writer(record)
    for name, data in files.items():
        hashed = base64.urlsafe_b64encode(hashlib.sha256(data).digest()).rstrip(b"=").decode()
        writer.writerow([name, f"sha256={hashed}", len(data)])
    writer.writerow([record_name, "", ""])
    files[record_name] = record.getvalue().encode()
    with zipfile.ZipFile(wheel, "w", compression=zipfile.ZIP_DEFLATED) as archive:
        for name, data in files.items():
            archive.writestr(name, data)
    (directory / "pika-release.json").write_text(json.dumps({
        "schema": 1, "version": version, "wheel": wheel.name, "sha256": installer.digest(wheel),
    }))
    return wheel, version


@pytest.mark.skipif(not os.environ.get("PIKA_INSTALL_BUNDLE"), reason="Set PIKA_INSTALL_BUNDLE to a built release bundle for real isolated installation")
def test_real_bundle_installs_and_upgrades_without_touching_home(tmp_path, monkeypatch):
    bundle = Path(os.environ["PIKA_INSTALL_BUNDLE"]).resolve()
    manifest = installer.read_manifest(bundle / "pika-release.json")
    uv = shutil.which("uv")
    assert uv, "Real installer test needs uv"
    home = tmp_path / "home"
    home.mkdir()
    monkeypatch.setenv("HOME", str(home))
    root, bin_dir = home / ".local/share/pikamux", home / ".local/bin"
    wheel = bundle / manifest["wheel"]
    launcher = installer.install(wheel, manifest["sha256"], Path(uv), root=root, bin_dir=bin_dir)
    previous = (root / "current").resolve()
    result = subprocess.run([str(launcher), "--version"], capture_output=True, text=True, check=True, timeout=30)
    assert result.stdout.strip() == f"pikamux {manifest['version']}"
    next_wheel, version = _next_real_wheel(wheel, tmp_path / "next", manifest["version"])
    # The running old client imports only after the update, proving its release
    # remains usable for late imports, not merely that cached Python objects live.
    old_client = subprocess.Popen(
        [str(previous / "bin/python"), "-u", "-c",
         "import sys; print('ready', flush=True); sys.stdin.readline(); "
         "import pikamux; print(pikamux.__version__); print(sys.prefix)"],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        text=True, env=installer._environment(),
    )
    try:
        assert select.select([old_client.stdout], [], [], 10)[0], "old client did not start"
        assert old_client.stdout.readline().strip() == "ready"
        subprocess.run([str(launcher), "update", "--bundle", str(next_wheel.parent)], check=True,
                       capture_output=True, text=True, timeout=180)
        assert (root / "current").resolve() != previous
        assert previous.is_dir()
        assert old_client.poll() is None, "update interrupted the existing client"
        output, errors = old_client.communicate("continue\n", timeout=10)
        assert old_client.returncode == 0, errors
        assert output.splitlines() == [manifest["version"], str(previous)]
        result = subprocess.run([str(launcher), "--version"], capture_output=True, text=True, check=True, timeout=30)
        assert result.stdout.strip() == f"pikamux {version}"
    finally:
        if old_client.poll() is None:
            old_client.terminate()
            try:
                old_client.communicate(timeout=5)
            except subprocess.TimeoutExpired:
                old_client.kill()
                old_client.communicate(timeout=5)
    assert not (home / ".codex").exists()
    assert not (home / ".claude").exists()
    assert not (home / ".config/pika").exists()
    assert not (home / ".local/state/pika").exists()


@pytest.mark.parametrize("entry", [".pika-install-root", "tools", "releases", ".install.lock"])
def test_managed_internal_symlinks_refused(installation, tmp_path, entry):
    _, root, _, _, calls, install, _, _ = installation
    root.mkdir(parents=True)
    marker = root / ".pika-install-root"
    marker.write_text(installer.ROOT_MARKER)
    outside = tmp_path / "outside"
    outside.write_text(installer.ROOT_MARKER if entry == marker.name else "outside state")
    if entry == marker.name:
        marker.unlink()
    (root / entry).symlink_to(outside)
    before = outside.read_bytes()
    with pytest.raises(installer.InstallError, match="Unexpected symlink"):
        install()
    assert outside.read_bytes() == before
    assert calls == []


def test_symlink_installation_root_refused(installation, tmp_path):
    _, root, _, _, calls, install, _, _ = installation
    outside = tmp_path / "outside"
    outside.mkdir()
    root.parent.mkdir(parents=True)
    root.symlink_to(outside)
    with pytest.raises(installer.InstallError, match="root must not be a symlink"):
        install()
    assert list(outside.iterdir()) == []
    assert calls == []


class Terminal(io.StringIO):
    def __init__(self, answer):
        super().__init__()
        self.answer = answer

    def readline(self, *args):
        return self.answer + "\n"

    def __exit__(self, *args):
        return False


def fake_terminal(monkeypatch, answer):
    terminal = Terminal(answer)

    def opened(path, mode):
        assert path == "/dev/tty" and mode == "r+"
        return terminal

    monkeypatch.setattr(installer, "open", opened, raising=False)
    return terminal


@pytest.mark.parametrize("shell,profiles", [("zsh", [".zshrc"]), ("bash", [".bashrc", ".bash_profile"])])
def test_approved_shell_path_is_backed_up_idempotent_and_not_sourced(installation, monkeypatch, shell, profiles):
    home, _, bin_dir, _, _, _, _, _ = installation
    monkeypatch.setenv("SHELL", f"/bin/{shell}")
    monkeypatch.setenv("PATH", "/usr/bin:/bin")
    marker = home / "profile-was-executed"
    before = "# personal config\ntouch " + str(marker) + "\n"
    for name in profiles:
        (home / name).write_text(before)
        (home / name).chmod(0o640)
    terminal = fake_terminal(monkeypatch, "yes")
    installer.configure_path(bin_dir, interactive=True)
    assert "[y/N]" in terminal.getvalue()
    assert not marker.exists()
    for name in profiles:
        profile = home / name
        assert profile.read_text().startswith(before)
        assert str(bin_dir) in profile.read_text()
        assert profile.stat().st_mode & 0o777 == 0o640
        backups = list(home.glob(name + ".pika-backup-*"))
        assert len(backups) == 1
        assert backups[0].read_text() == before
    installer.configure_path(bin_dir, interactive=True)
    for name in profiles:
        assert len(list(home.glob(name + ".pika-backup-*"))) == 1
        assert (home / name).read_text().count("# Pika user-local command") == 1


@pytest.mark.parametrize("answer,interactive", [("n", True), ("", True), ("yes", False)])
def test_decline_and_noninteractive_path_leave_profiles_unchanged(installation, monkeypatch, answer, interactive):
    home, _, bin_dir, _, _, _, _, _ = installation
    monkeypatch.setenv("SHELL", "/bin/bash")
    monkeypatch.setenv("PATH", "/usr/bin:/bin")
    profiles = [home / ".bashrc", home / ".bash_profile"]
    for profile in profiles:
        profile.write_text("# preserve me\n")
    before = {profile: (profile.read_bytes(), profile.stat().st_mtime_ns) for profile in profiles}
    terminal = fake_terminal(monkeypatch, answer)
    installer.configure_path(bin_dir, interactive=interactive)
    assert {profile: (profile.read_bytes(), profile.stat().st_mtime_ns) for profile in profiles} == before
    assert list(home.glob(".*.pika*")) == []
    if not interactive:
        assert terminal.getvalue() == ""


def test_no_setup_never_prompts_or_changes_profiles(installation, monkeypatch):
    home, root, bin_dir, uv, _, _, wheel, manifest = installation
    monkeypatch.setenv("SHELL", "/bin/zsh")
    monkeypatch.setenv("PATH", "/usr/bin:/bin")
    (home / ".zshrc").write_text("# unchanged\n")
    monkeypatch.setattr(installer, "open", lambda *args: pytest.fail("no-setup must not prompt"), raising=False)
    monkeypatch.setattr(installer, "onboarding", lambda *args: pytest.fail("no-setup must not onboard"))
    assert installer.main(["install", "--wheel", str(wheel), "--sha256", manifest["sha256"],
                           "--uv", str(uv), "--root", str(root), "--bin-dir", str(bin_dir), "--no-setup"]) == 0
    assert (home / ".zshrc").read_text() == "# unchanged\n"


@pytest.mark.parametrize("shell,flags", [("zsh", ["-i", "-c"]), ("bash", ["--noprofile", "-i", "-c"]), ("bash", ["--login", "-c"])])
def test_new_shell_can_find_pika_with_approved_path(installation, monkeypatch, shell, flags):
    home, _, bin_dir, _, _, _, _, _ = installation
    executable = shutil.which(shell)
    if not executable:
        pytest.skip(f"{shell} unavailable")
    monkeypatch.setenv("SHELL", executable)
    monkeypatch.setenv("PATH", "/usr/bin:/bin")
    bin_dir.mkdir(parents=True)
    launcher = bin_dir / "pika"
    launcher.write_text("#!/bin/sh\nexit 0\n")
    launcher.chmod(0o700)
    fake_terminal(monkeypatch, "y")
    installer.configure_path(bin_dir, interactive=True)
    result = subprocess.run([executable, *flags, "command -v pika"], env=os.environ.copy(),
                            capture_output=True, text=True, timeout=10)
    assert result.returncode == 0, result.stderr
    assert result.stdout.strip().splitlines()[-1] == str(launcher)


def test_symlinked_shell_profile_is_not_replaced(installation, tmp_path, monkeypatch):
    home, _, bin_dir, _, _, _, _, _ = installation
    monkeypatch.setenv("SHELL", "/bin/zsh")
    monkeypatch.setenv("PATH", "/usr/bin:/bin")
    managed = tmp_path / "dotfiles-zshrc"
    managed.write_text("# dotfiles manager owns this\n")
    (home / ".zshrc").symlink_to(managed)
    fake_terminal(monkeypatch, "y")
    installer.configure_path(bin_dir, interactive=True)
    assert (home / ".zshrc").is_symlink()
    assert managed.read_text() == "# dotfiles manager owns this\n"


def test_private_update_check_gives_exact_bundle_command(installation, tmp_path, monkeypatch, capsys):
    _, root, _, _, _, install, _, _ = installation
    install()
    monkeypatch.setattr(installer.sys, "prefix", str((root / "current").resolve()))
    wheel, _ = make_bundle(tmp_path / "private bundle", "1.2.4")
    installer.update(bundle=wheel.parent, check=True)
    output = capsys.readouterr().out
    assert "pika update --bundle" in output
    assert str(wheel.parent) in output
    assert "pika update`" not in output


@pytest.mark.parametrize("existing", [".bash_profile", ".bash_login", ".profile"])
def test_bash_keeps_existing_login_startup_precedence(installation, monkeypatch, existing):
    home, _, bin_dir, _, _, _, _, _ = installation
    monkeypatch.setenv("SHELL", "/bin/bash")
    monkeypatch.setenv("PATH", "/usr/bin:/bin")
    login = home / existing
    login.write_text("export PERSONAL_LOGIN_SETTING=preserved\n")
    fake_terminal(monkeypatch, "yes")
    installer.configure_path(bin_dir, interactive=True)
    assert login.read_text().startswith("export PERSONAL_LOGIN_SETTING=preserved\n")
    assert str(bin_dir) in login.read_text()
    assert (home / ".bashrc").is_file()
    if existing != ".bash_profile":
        assert not (home / ".bash_profile").exists()
    result = subprocess.run(["/bin/bash", "--login", "-c", 'printf "%s" "$PERSONAL_LOGIN_SETTING"'],
                            env=os.environ.copy(), capture_output=True, text=True, timeout=10)
    assert result.returncode == 0
    assert result.stdout == "preserved"
