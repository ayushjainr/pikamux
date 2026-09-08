"""User-local, staged releases. Never opens Pika/provider state or stops agents."""
from __future__ import annotations

import argparse
import contextlib
import email.parser
import hashlib
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import subprocess
import sys
import tempfile
import time
import urllib.request
import uuid
import zipfile

from . import __version__

RELEASE_API = "https://api.github.com/repos/ayushjainr/pikamux/releases?per_page=100"
UPDATE_CHECK_SECONDS = 6 * 60 * 60
ROOT_MARKER = "pikamux-installer-v1\n"
VERSION = re.compile(r"[0-9]+\.[0-9]+\.[0-9]+(?:(?:a|b|rc)[0-9]+)?\Z")


class InstallError(RuntimeError):
    pass


def default_root() -> Path:
    return Path.home() / ".local" / "share" / "pikamux"


def digest(path: Path) -> str:
    result = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            result.update(block)
    return result.hexdigest()


def read_manifest(path: Path) -> dict:
    try:
        if path.stat().st_size > 65536:
            raise ValueError("oversized manifest")
        value = json.loads(path.read_text())
        version, wheel, checksum = value["version"], value["wheel"], value["sha256"]
        if value["schema"] != 1 or not isinstance(version, str) or not VERSION.fullmatch(version):
            raise ValueError("unsupported manifest/version")
        if wheel != f"pikamux-{version}-py3-none-any.whl":
            raise ValueError("unexpected wheel name")
        if not isinstance(checksum, str) or not re.fullmatch(r"[a-f0-9]{64}", checksum):
            raise ValueError("invalid SHA256")
        return value
    except (KeyError, TypeError, ValueError, OSError) as exc:
        raise InstallError(f"Invalid release manifest: {exc}") from exc


def wheel_version(wheel: Path, checksum: str) -> str:
    if not re.fullmatch(r"[a-f0-9]{64}", checksum) or digest(wheel) != checksum:
        raise InstallError("Package checksum mismatch. Nothing activated.")
    try:
        with zipfile.ZipFile(wheel) as archive:
            names = [name for name in archive.namelist() if name.endswith('.dist-info/METADATA')]
            if len(names) != 1 or archive.getinfo(names[0]).file_size > 1024 * 1024:
                raise ValueError("invalid wheel metadata")
            metadata = email.parser.Parser().parsestr(archive.read(names[0]).decode())
            version = metadata['Version']
            if metadata['Name'] != 'pikamux' or not version or not VERSION.fullmatch(version):
                raise ValueError("not a supported Pika wheel")
            if wheel.name != f"pikamux-{version}-py3-none-any.whl":
                raise ValueError("filename/version mismatch")
            return version
    except (OSError, ValueError, zipfile.BadZipFile, KeyError) as exc:
        raise InstallError(f"Invalid Pika wheel: {exc}") from exc


def _environment() -> dict[str, str]:
    # A project .venv, uv.toml, index override or PYTHONPATH must not redirect
    # installation into a user's development environment.
    return {
        key: value for key, value in os.environ.items()
        if not key.startswith(("UV_", "PIP_", "PYTHON"))
        and key not in {"VIRTUAL_ENV", "CONDA_PREFIX"}
    }


def _run(command: list[str]) -> str:
    try:
        result = subprocess.run(command, env=_environment(), capture_output=True, text=True, timeout=600)
    except (OSError, subprocess.TimeoutExpired) as exc:
        raise InstallError(f"Installation step failed: {exc}. Previous release is unchanged.") from exc
    if result.returncode:
        detail = (result.stdout + "\n" + result.stderr).strip()[-2000:]
        raise InstallError(f"Installation step failed: {detail}. Previous release is unchanged.")
    return result.stdout.strip()


def _atomic_link(target: Path, link: Path) -> None:
    temporary = link.with_name(f".{link.name}.{uuid.uuid4().hex}")
    try:
        temporary.symlink_to(target)
        os.replace(temporary, link)
    finally:
        temporary.unlink(missing_ok=True)


def _validate_root(root: Path) -> None:
    if root in {Path('/'), Path.home().resolve(), Path.cwd().resolve()}:
        raise InstallError("Choose a dedicated Pika installation directory, not a home or workspace root.")
    marker = root / '.pika-install-root'
    for entry in (marker, root / 'tools', root / 'releases', root / '.install.lock', root / '.update-check.lock'):
        if entry.is_symlink():
            raise InstallError(f'Unexpected symlink in managed installation: {entry}. Nothing overwritten.')
    if root.exists() and any(root.iterdir()):
        if not marker.is_file() or marker.read_text() != ROOT_MARKER:
            raise InstallError(f"{root} is not a Pika-managed installation. Nothing overwritten.")


@contextlib.contextmanager
def _lock(root: Path, *, name: str = '.install.lock'):
    import fcntl
    with (root / name).open('a') as stream:
        try:
            fcntl.flock(stream, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError as exc:
            raise InstallError("Another Pika installation/update is running. Try again after it finishes.") from exc
        yield


def install(wheel: Path, checksum: str, uv: Path, *, root: Path | None = None,
            bin_dir: Path | None = None) -> Path:
    """Activate only after validation; retain old environments for running clients."""
    if sys.platform not in {'darwin', 'linux'}:
        raise InstallError("The native installer currently supports macOS and Linux only.")
    version = wheel_version(wheel, checksum)
    raw_root = (root or default_root()).expanduser().absolute()
    if raw_root.is_symlink():
        raise InstallError('The Pika installation root must not be a symlink.')
    root = raw_root.resolve()
    bin_dir = (bin_dir or Path.home() / '.local' / 'bin').expanduser().resolve()
    _validate_root(root)
    launcher = bin_dir / 'pika'
    expected = root / 'current' / 'bin' / 'pika'
    if os.path.lexists(launcher) and (not launcher.is_symlink() or Path(os.readlink(launcher)) != expected):
        raise InstallError(f"{launcher} belongs to another installation. Nothing overwritten. "
                           "Keep it, or explicitly remove it using its original installer before retrying.")
    if not uv.is_file() or not os.access(uv, os.X_OK):
        raise InstallError("The verified installation runtime is missing or not executable.")
    root.mkdir(parents=True, exist_ok=True, mode=0o700)
    marker = root / '.pika-install-root'
    if not marker.exists():
        marker.write_text(ROOT_MARKER)
    with _lock(root):
        current = root / 'current'
        if os.path.lexists(current) and not current.is_symlink():
            raise InstallError("Pika's activation path is not a symlink; refusing to overwrite it.")
        if current.is_symlink():
            if current.resolve().parent != root / 'releases':
                raise InstallError("Pika's current release points outside its managed releases.")
            previous_path = current / '.pika-install.json'
            if previous_path.is_symlink():
                raise InstallError('Unexpected symlink for the installation receipt.')
            previous = json.loads(previous_path.read_text())
            if _version_key(version) < _version_key(previous['version']):
                raise InstallError('Refusing a release downgrade. Nothing activated.')
            if version == previous['version']:
                if checksum != previous['sha256']:
                    raise InstallError('Same version, different bytes. Publish a new version; nothing activated.')
                if not os.path.lexists(launcher):
                    bin_dir.mkdir(parents=True, exist_ok=True)
                    launcher.symlink_to(expected)
                print(f'Pika {version} is already installed.')
                return launcher
        tools = root / 'tools'
        tools.mkdir(exist_ok=True)
        durable_uv = tools / 'uv'
        if uv.resolve() != durable_uv.resolve():
            temp_uv = tools / f'.uv-{uuid.uuid4().hex}'
            try:
                shutil.copyfile(uv, temp_uv)
                temp_uv.chmod(0o700)
                os.replace(temp_uv, durable_uv)
            finally:
                temp_uv.unlink(missing_ok=True)
        releases = root / 'releases'
        releases.mkdir(exist_ok=True)
        stage = Path(tempfile.mkdtemp(prefix=f'{version}-{checksum[:12]}-', dir=releases))
        activated = False
        try:
            print(f"Preparing Pika {version} in its own environment…", flush=True)
            _run([str(durable_uv), '--no-config', 'venv', '--python', sys.executable, str(stage)])
            _run([str(durable_uv), '--no-config', 'pip', 'install', '--python', str(stage / 'bin/python'),
                  '--only-binary', ':all:', '--index-url', 'https://pypi.org/simple', str(wheel.resolve())])
            if _run([str(stage / 'bin/pika'), '--version']) != f'pikamux {version}':
                raise InstallError('Installed version does not match the verified wheel. Nothing activated.')
            _run([str(stage / 'bin/pika'), '--help'])
            _run([str(stage / 'bin/pika'), 'skill', 'show'])
            receipt = {'schema': 1, 'root': str(root), 'bin_dir': str(bin_dir),
                       'version': version, 'sha256': checksum}
            (stage / '.pika-install.json').write_text(json.dumps(receipt, indent=2) + '\n')
            # Preserve the verified artifact for authorized private SSH installation.
            artifacts = stage / 'bundle'
            artifacts.mkdir()
            shutil.copyfile(wheel, artifacts / wheel.name)
            (artifacts / 'pika-release.json').write_text(json.dumps({
                'schema': 1, 'version': version, 'wheel': wheel.name, 'sha256': checksum,
            }) + '\n')
            bin_dir.mkdir(parents=True, exist_ok=True)
            if not os.path.lexists(launcher):
                launcher.symlink_to(expected)
            _atomic_link(stage, current)
            activated = True
            print(f"Installed Pika {version}. Running agents, settings and conversations were not changed.")
            return launcher
        finally:
            if not activated:
                # This exact path was created above; never prune earlier releases.
                shutil.rmtree(stage)


def managed_receipt() -> dict:
    path = Path(sys.prefix) / '.pika-install.json'
    try:
        value = json.loads(path.read_text())
        root = Path(value['root'])
        if value['schema'] != 1 or not root.is_absolute() or root / 'releases' != Path(sys.prefix).parent:
            raise ValueError('invalid installation owner')
        if (root / 'current').resolve() != Path(sys.prefix).resolve():
            raise ValueError('this is not the currently active installation')
        _validate_root(root)
        return value
    except (OSError, ValueError, KeyError, TypeError) as exc:
        raise InstallError("This copy is not an active installer-managed Pika. "
                           "Update it with its original installation method; editable checkouts are left untouched.") from exc


def _download(url: str, target: Path, *, limit: int, timeout: float = 30) -> None:
    try:
        with urllib.request.urlopen(url, timeout=timeout) as response, target.open('wb') as output:
            if not response.url.startswith('https://'):
                raise InstallError('Release download redirected away from HTTPS.')
            size = 0
            while block := response.read(1024 * 1024):
                size += len(block)
                if size > limit:
                    raise InstallError('Release asset exceeds the size limit.')
                output.write(block)
    except OSError as exc:
        raise InstallError("Cannot download the Pika release. It may still be private/unpublished, "
                           "or the network is unavailable. Nothing activated.") from exc


def _version_key(version: str) -> tuple:
    match = re.fullmatch(r'(\d+)\.(\d+)\.(\d+)(?:(a|b|rc)(\d+))?', version)
    if not match:
        raise InstallError('Unsupported release version.')
    major, minor, patch, phase, number = match.groups()
    return (int(major), int(minor), int(patch), {'a': 0, 'b': 1, 'rc': 2, None: 3}[phase], int(number or 0))


def latest_release(current: str) -> str | None:
    """Bounded public metadata lookup; never send local inventory or credentials."""
    preview = _version_key(current)[3] != 3
    with tempfile.TemporaryDirectory(prefix='pika-release-check-') as directory:
        path = Path(directory) / 'releases.json'
        _download(RELEASE_API, path, limit=2 * 1024 * 1024, timeout=5)
        try:
            rows = json.loads(path.read_text())
            if not isinstance(rows, list):
                raise ValueError('expected release list')
        except (OSError, ValueError) as exc:
            raise InstallError('Cannot read the release listing. Nothing changed.') from exc
    versions = []
    for row in rows:
        if not isinstance(row, dict) or row.get('draft') is not False:
            continue
        tag = row.get('tag_name')
        if not isinstance(tag, str) or not tag.startswith('v') or not VERSION.fullmatch(tag[1:]):
            continue
        version = tag[1:]
        if not preview and (row.get('prerelease') is not False or _version_key(version)[3] != 3):
            continue
        assets = row.get('assets')
        if not isinstance(assets, list):
            continue
        names = {asset.get('name') for asset in assets if isinstance(asset, dict)
                 and isinstance(asset.get('name'), str)}
        if {'pika-release.json', f'pikamux-{version}-py3-none-any.whl'} <= names:
            versions.append(version)
    return max(versions, key=_version_key) if versions else None


def update_notice() -> str | None:
    """Best-effort, cached board check. Unmanaged installs never use the network."""
    if os.environ.get('PIKA_UPDATE_CHECK', '').lower() in {'0', 'false', 'off'}:
        return None
    try:
        receipt = managed_receipt()
        root, current = Path(receipt['root']), receipt['version']
        cache = root / '.update-check.json'
        # Concurrent boards share a check lock without blocking real installers.
        with _lock(root, name='.update-check.lock'):
            now = time.time()
            saved = {}
            try:
                if not cache.is_symlink() and cache.stat().st_size <= 4096:
                    saved = json.loads(cache.read_text())
                if not isinstance(saved, dict):
                    saved = {}
                age = now - float(saved.get('checked_at', 0))
                valid = saved.get('current') == current and 0 <= age < (
                    3600 if saved.get('failed') else UPDATE_CHECK_SECONDS
                )
            except (OSError, ValueError, TypeError):
                valid = False
            if not valid:
                try:
                    version = latest_release(current)
                    saved = {'current': current, 'checked_at': now, 'latest': version}
                except (InstallError, OSError):
                    saved = {'current': current, 'checked_at': now, 'latest': None, 'failed': True}
                fd, temporary = tempfile.mkstemp(prefix='.update-check-', dir=root)
                try:
                    with os.fdopen(fd, 'w') as stream:
                        json.dump(saved, stream)
                    os.replace(temporary, cache)
                finally:
                    Path(temporary).unlink(missing_ok=True)
            version = saved.get('latest')
            if isinstance(version, str) and VERSION.fullmatch(version):
                if _version_key(version) > _version_key(current) and (
                    _version_key(current)[3] != 3 or _version_key(version)[3] == 3
                ):
                    return version
    except (InstallError, OSError, ValueError, KeyError, TypeError):
        pass
    return None


def update(*, bundle: Path | None = None, check: bool = False, release: str | None = None) -> None:
    receipt = managed_receipt()
    if release is not None and (bundle is not None or not VERSION.fullmatch(release)):
        raise InstallError('Use a valid --release VERSION or --bundle, not both.')
    if release is not None and _version_key(release) < _version_key(receipt['version']):
        raise InstallError('Refusing a release downgrade. Nothing changed.')
    with tempfile.TemporaryDirectory(prefix='pika-update-') as directory:
        local = bundle.resolve() if bundle else Path(directory)
        if not bundle:
            release = release or latest_release(receipt['version'])
            if release is None or _version_key(release) < _version_key(receipt['version']):
                print(f"Pika {receipt['version']} is already current for its release channel.")
                return
            base = f'https://github.com/ayushjainr/pikamux/releases/download/v{release}'
            _download(base + '/pika-release.json', local / 'pika-release.json', limit=65536)
        manifest = read_manifest(local / 'pika-release.json')
        if release is not None and manifest['version'] != release:
            raise InstallError('Release tag and manifest version differ. Nothing activated.')
        if _version_key(manifest['version']) < _version_key(receipt['version']):
            raise InstallError('Refusing a release downgrade. Nothing changed.')
        if manifest['version'] == receipt['version']:
            if manifest['sha256'] != receipt['sha256']:
                raise InstallError('The same version has different package bytes. Publish a new version; nothing changed.')
            print(f"Pika {receipt['version']} is already current.")
            return
        print(f"Pika {receipt['version']} → {manifest['version']}")
        if check:
            command = ['pika', 'update'] + (['--bundle', str(bundle.resolve())] if bundle else ['--release', release])
            print(f'Update available. Run `{shlex.join(command)}` to install it.')
            return
        wheel = local / manifest['wheel']
        if not bundle:
            _download(base + '/' + manifest['wheel'], wheel, limit=64 * 1024 * 1024)
        install(wheel, manifest['sha256'], Path(receipt['root']) / 'tools/uv',
                root=Path(receipt['root']), bin_dir=Path(receipt['bin_dir']))
        print('Reopen the Pika board when convenient. Existing agent processes were not restarted. '
              'Hook/skill configuration is unchanged; `pika setup` previews any integration changes.')


def board_update(release: str) -> str:
    """Run only after explicit board approval; never redirect global TUI stdout."""
    managed_receipt()
    if not VERSION.fullmatch(release):
        raise InstallError('Invalid release version.')
    # File-backed output lets the child finish safely even if the board exits.
    with tempfile.TemporaryFile() as output:
        result = subprocess.run(
            [sys.executable, '-m', 'pikamux', 'update', '--release', release],
            env=_environment(), stdin=subprocess.DEVNULL, stdout=output,
            stderr=subprocess.STDOUT, timeout=3600, start_new_session=True,
        )
        output.seek(max(0, output.tell() - 4000))
        detail = output.read().decode(errors='replace').strip()
    if result.returncode:
        raise InstallError(detail or 'Update failed. Run `pika update` for diagnostics.')
    return f'Updated to {release} · reopen pika when convenient; agents left running. Run pika setup to review skill changes.'


def onboarding(launcher: Path) -> None:
    if not shutil.which('tmux'):
        print('tmux is missing. Pika has been installed, but hosting agents needs tmux.')
        command = ('brew install tmux' if sys.platform == 'darwin' else
                   'sudo apt-get install tmux (Debian/Ubuntu); use your OS package manager on other Linux systems')
        print(f'Install the prerequisite: {command}')
        print(f'Then run: {shlex.join([str(launcher), "setup"])}')
        return
    # curl | bash consumes stdin. Prompt via the controlling terminal instead.
    try:
        with open('/dev/tty', 'r+') as terminal:
            terminal.write('Start Pika setup now? [y/N] ')
            terminal.flush()
            if terminal.readline().strip().lower() not in {'y', 'yes'}:
                return
            subprocess.run([str(launcher), 'setup'], stdin=terminal, check=False)
    except OSError:
        print(f'Next: {shlex.join([str(launcher), "setup"])}')


def configure_path(bin_dir: Path, *, interactive: bool) -> None:
    """Offer an explicit, backed-up shell change; never source user startup files."""
    if str(bin_dir) in os.environ.get('PATH', '').split(os.pathsep):
        return
    line = f'export PATH={shlex.quote(str(bin_dir))}:"$PATH"'
    print(f'For this terminal: {line}')
    shell = Path(os.environ.get('SHELL', '')).name
    login_profiles = [Path.home() / name for name in ('.bash_profile', '.bash_login', '.profile')]
    login_profile = next((path for path in login_profiles if os.path.lexists(path)), login_profiles[0])
    profiles = ([Path.home() / '.zshrc'] if shell == 'zsh' else
                [Path.home() / '.bashrc', login_profile] if shell == 'bash' else [])
    if not interactive or not profiles:
        print('Shell startup files were not changed. Use the full Pika path or add the line above to your shell profile.')
        return
    try:
        with open('/dev/tty', 'r+') as terminal:
            terminal.write('Make pika available in future terminals by updating ' +
                           ', '.join(str(path) for path in profiles) + '? [y/N] ')
            terminal.flush()
            if terminal.readline().strip().lower() not in {'y', 'yes'}:
                return
        for profile in profiles:
            if profile.is_symlink():
                print(f'Left symlinked profile unchanged: {profile}. Add the PATH line through your dotfile manager.')
                continue
            before = profile.read_text() if profile.exists() else ''
            if line in before:
                continue
            if profile.exists():
                fd, backup_name = tempfile.mkstemp(prefix=profile.name + '.pika-backup-', dir=profile.parent)
                os.close(fd)
                shutil.copy2(profile, backup_name)
                print(f'Profile backup: {backup_name}')
            fd, temporary = tempfile.mkstemp(prefix=profile.name + '.pika-', dir=profile.parent)
            try:
                with os.fdopen(fd, 'w') as stream:
                    stream.write(before + '\n# Pika user-local command\n' + line + '\n')
                os.chmod(temporary, profile.stat().st_mode & 0o777 if profile.exists() else 0o600)
                os.replace(temporary, profile)
            finally:
                Path(temporary).unlink(missing_ok=True)
        print('Open a new terminal to use `pika`, or use the full path in this terminal.')
    except OSError as exc:
        print(f'Could not configure shell PATH: {exc}. Pika remains installed; use its full path.')


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest='command', required=True)
    command = sub.add_parser('install')
    command.add_argument('--wheel', required=True, type=Path)
    command.add_argument('--sha256', required=True)
    command.add_argument('--uv', required=True, type=Path)
    command.add_argument('--root', type=Path)
    command.add_argument('--bin-dir', type=Path)
    command.add_argument('--no-setup', action='store_true')
    args = parser.parse_args(argv)
    try:
        launcher = install(args.wheel, args.sha256, args.uv, root=args.root, bin_dir=args.bin_dir)
        print(f'Pika command: {shlex.quote(str(launcher))}')
        configure_path(launcher.parent, interactive=not args.no_setup)
        if not args.no_setup:
            onboarding(launcher)
        return 0
    except (InstallError, OSError) as exc:
        print(f'pika install: {exc}', file=sys.stderr)
        return 1


if __name__ == '__main__':
    raise SystemExit(main())
