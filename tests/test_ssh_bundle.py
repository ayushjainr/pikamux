"""Private release transport does not need GitHub credentials on the target."""
import hashlib
import io
import json
import os
from pathlib import Path
import subprocess
import tarfile
from unittest.mock import Mock
import zipfile
import argparse
import pytest

from pikamux import __version__
from pikamux.fleet import SSHTransport, remote_pika_argv, FleetManager, FleetError, PROTOCOL_NAME, PROTOCOL_VERSION
from pikamux.models import FleetNode
from pikamux.store import Store


def make_bundle(tmp_path):
    wheel = tmp_path / f'pikamux-{__version__}-py3-none-any.whl'
    with zipfile.ZipFile(wheel, 'w') as archive:
        archive.writestr(f'pikamux-{__version__}.dist-info/METADATA', f'Name: pikamux\nVersion: {__version__}\n')
        archive.writestr('pikamux/install.sh', '#!/bin/bash\nexit 0\n')
    manifest = {'schema': 1, 'version': __version__, 'wheel': wheel.name,
                'sha256': hashlib.sha256(wheel.read_bytes()).hexdigest()}
    (tmp_path / 'pika-release.json').write_text(json.dumps(manifest))
    return wheel


def test_ssh_transfers_only_verified_bundle_and_packaged_installer(tmp_path, monkeypatch):
    wheel = make_bundle(tmp_path)
    # An unrelated untrusted script in the directory must never be executed remotely.
    (tmp_path / 'install.sh').write_text('untrusted local extra')
    run = Mock(return_value=subprocess.CompletedProcess([], 0, b'installed', b''))
    monkeypatch.setattr('pikamux.fleet.subprocess.run', run)
    assert SSHTransport().install('trusted-node', bundle=tmp_path) == (0, 'installed')
    args, kwargs = run.call_args
    assert args[0][-3:-1] == ['sh', '-c']
    assert '--no-setup' in args[0][-1]
    with tarfile.open(fileobj=io.BytesIO(kwargs['input'])) as archive:
        assert set(archive.getnames()) == {wheel.name, 'pika-release.json', 'install.sh'}
        assert archive.extractfile('install.sh').read() == b'#!/bin/bash\nexit 0\n'
        assert archive.extractfile(wheel.name).read() == wheel.read_bytes()


def test_bad_bundle_never_contacts_remote(tmp_path, monkeypatch):
    wheel = make_bundle(tmp_path)
    wheel.write_bytes(b'tampered')
    run = Mock()
    monkeypatch.setattr('pikamux.fleet.subprocess.run', run)
    code, detail = SSHTransport().install('trusted-node', bundle=tmp_path)
    assert code == 1 and 'checksum' in detail
    run.assert_not_called()


def test_remote_install_timeout_reports_unknown_outcome(tmp_path, monkeypatch):
    make_bundle(tmp_path)
    run = Mock(side_effect=subprocess.TimeoutExpired('ssh', 900))
    monkeypatch.setattr('pikamux.fleet.subprocess.run', run)
    code, detail = SSHTransport().install('trusted-node', bundle=tmp_path)
    assert code == 1 and 'outcome unknown' in detail


def test_installer_managed_coordinator_uses_its_bundle_by_default(tmp_path, monkeypatch):
    bundle = tmp_path / 'bundle'
    bundle.mkdir()
    make_bundle(bundle)
    (tmp_path / '.pika-install.json').write_text('{}')
    monkeypatch.setattr('pikamux.fleet.sys.prefix', str(tmp_path))
    transport = SSHTransport()
    install = Mock(return_value=(0, 'transferred'))
    monkeypatch.setattr(transport, '_install_bundle', install)
    assert transport.install('trusted-node') == (0, 'transferred')
    install.assert_called_once_with('trusted-node', bundle)


def test_ssh_can_find_user_local_pika_without_profile_and_quotes_arguments(tmp_path):
    bin_dir = tmp_path / '.local' / 'bin'
    bin_dir.mkdir(parents=True)
    launcher = bin_dir / 'pika'
    launcher.write_text('#!/bin/sh\nprintf "%s\\n" "$@"\n')
    launcher.chmod(0o700)
    injected = str(tmp_path / 'must-not-exist')
    argument = f'spaces; touch {injected}'
    env = dict(os.environ, HOME=str(tmp_path), PATH='/usr/bin:/bin')
    result = subprocess.run(['sh', '-c', ' '.join(remote_pika_argv('_fleet', argument))],
                            env=env, capture_output=True, text=True, check=True)
    assert result.stdout.splitlines() == ['_fleet', argument]
    assert not Path(injected).exists()


def test_upgrade_refuses_repointed_alias_before_install(tmp_path):
    from pikamux.cli import _machines
    node = FleetNode('11111111-1111-4111-8111-111111111111', 'server', 'server')
    pika = Mock()
    pika.store.get_fleet_node.return_value = node
    transport = Mock()
    transport.request.return_value = {'type': 'hello', 'protocol': PROTOCOL_NAME,
        'version': PROTOCOL_VERSION, 'node_id': '22222222-2222-4222-8222-222222222222'}
    pika.fleet = FleetManager(Store(tmp_path / 'pika.db'), transport)
    args = argparse.Namespace(machines_command='upgrade', machine='server', yes=True, bundle=None)
    with pytest.raises(FleetError, match='IDENTITY CHANGED'):
        _machines(pika, args)
    transport.install.assert_not_called()


def test_upgrade_identity_is_checked_before_and_after_install(tmp_path, monkeypatch):
    from pikamux.cli import _machines
    node = FleetNode('11111111-1111-4111-8111-111111111111', 'server', 'server')
    pika = Mock()
    pika.store.get_fleet_node.return_value = node
    transport = Mock()
    events = []
    # An older node with no newly added optional naming capability can upgrade.
    transport.request.side_effect = lambda *a, **k: events.append('hello') or {
        'type': 'hello', 'protocol': PROTOCOL_NAME, 'version': PROTOCOL_VERSION,
        'node_id': node.node_id, 'capabilities': []}
    transport.install.side_effect = lambda *a, **k: events.append('install') or (0, 'done')
    pika.fleet = FleetManager(Store(tmp_path / 'pika.db'), transport)
    monkeypatch.setattr(pika.fleet, 'add', lambda *a, **k: node)
    args = argparse.Namespace(machines_command='upgrade', machine='server', yes=True, bundle=None)
    assert _machines(pika, args) == 0
    assert events == ['hello', 'install', 'hello']
