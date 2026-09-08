"""Release checks are metadata-only; board installation needs explicit approval."""
import json
import subprocess
import fcntl
import os
import pty
import struct
import termios
import threading
from types import SimpleNamespace
from unittest.mock import Mock

import pytest

from pikamux import installation as installer
from pikamux import monitor
from pty_reader import PtyReader


def release(version, *, draft=False, prerelease=None):
    return {'tag_name': 'v' + version, 'draft': draft,
            'prerelease': prerelease if prerelease is not None else installer._version_key(version)[3] != 3,
            'assets': [{'name': 'pika-release.json'}, {'name': f'pikamux-{version}-py3-none-any.whl'}]}


@pytest.mark.parametrize('current,expected', [('1.0.0a1', '1.1.0a10'), ('1.0.0', '1.0.1')])
def test_channel_selection_is_numeric_and_skips_drafts_and_incomplete_releases(monkeypatch, current, expected):
    rows = [release('1.1.0a2'), release('1.0.1'), release('1.1.0a10'),
            release('9.0.0', draft=True), {**release('8.0.0'), 'assets': []},
            {'tag_name': 'v../../other'}, None]
    def download(url, target, **kwargs):
        assert url == installer.RELEASE_API
        assert kwargs == {'limit': 2 * 1024 * 1024, 'timeout': 5}
        target.write_text(json.dumps(rows))
    monkeypatch.setattr(installer, '_download', download)
    assert installer.latest_release(current) == expected


def test_alpha_can_graduate_to_stable(monkeypatch):
    monkeypatch.setattr(installer, '_download', lambda _, path, **kw: path.write_text(json.dumps([
        release('1.0.0rc1'), release('1.0.0'), release('0.9.0'),
    ])))
    assert installer.latest_release('1.0.0a1') == '1.0.0'


@pytest.fixture
def managed(tmp_path, monkeypatch):
    root = tmp_path / 'managed'
    root.mkdir()
    receipt = {'root': str(root), 'bin_dir': str(tmp_path / 'bin'), 'version': '1.0.0a1', 'sha256': 'a' * 64}
    monkeypatch.setattr(installer, 'managed_receipt', lambda: receipt)
    monkeypatch.delenv('PIKA_UPDATE_CHECK', raising=False)
    return root, receipt


def test_cache_throttles_success_and_revalidates_after_clock_rollback(managed, monkeypatch):
    clock = [100000.0]
    monkeypatch.setattr(installer.time, 'time', lambda: clock[0])
    lookup = Mock(return_value='1.0.0a2')
    monkeypatch.setattr(installer, 'latest_release', lookup)
    assert installer.update_notice() == '1.0.0a2'
    assert installer.update_notice() == '1.0.0a2'
    assert lookup.call_count == 1
    clock[0] += installer.UPDATE_CHECK_SECONDS
    assert installer.update_notice() == '1.0.0a2'
    clock[0] -= 10
    assert installer.update_notice() == '1.0.0a2'
    assert lookup.call_count == 3


def test_offline_check_is_quiet_and_backed_off(managed, monkeypatch):
    lookup = Mock(side_effect=installer.InstallError('offline'))
    monkeypatch.setattr(installer, 'latest_release', lookup)
    assert installer.update_notice() is None
    assert installer.update_notice() is None
    assert lookup.call_count == 1
    assert json.loads((managed[0] / '.update-check.json').read_text())['failed']


def test_unmanaged_optout_and_concurrent_check_never_check_network(managed, monkeypatch):
    lookup = Mock(side_effect=AssertionError('must not connect'))
    monkeypatch.setattr(installer, 'latest_release', lookup)
    with installer._lock(managed[0], name='.update-check.lock'):
        assert installer.update_notice() is None
    monkeypatch.setenv('PIKA_UPDATE_CHECK', '0')
    assert installer.update_notice() is None
    monkeypatch.delenv('PIKA_UPDATE_CHECK')
    monkeypatch.setattr(installer, 'managed_receipt', Mock(side_effect=installer.InstallError('editable')))
    assert installer.update_notice() is None
    lookup.assert_not_called()


def test_corrupt_cache_and_changed_installed_version_recheck(managed, monkeypatch):
    root, receipt = managed
    cache = root / '.update-check.json'
    cache.write_text('not json')
    lookup = Mock(return_value='1.0.0a2')
    monkeypatch.setattr(installer, 'latest_release', lookup)
    assert installer.update_notice() == '1.0.0a2'
    receipt['version'] = '1.0.0a2'
    assert installer.update_notice() is None
    assert lookup.call_count == 2


def test_cache_symlink_does_not_modify_external_file(managed, tmp_path, monkeypatch):
    external = tmp_path / 'keep'
    external.write_text('keep')
    (managed[0] / '.update-check.json').symlink_to(external)
    monkeypatch.setattr(installer, 'latest_release', lambda _: '1.0.0a2')
    assert installer.update_notice() == '1.0.0a2'
    assert external.read_text() == 'keep'


@pytest.mark.parametrize('check', [False, True])
def test_online_update_pins_tag_not_latest_and_validates_manifest(managed, monkeypatch, check):
    downloads, installs = [], []
    monkeypatch.setattr(installer, 'latest_release', lambda _: '1.0.0a2')
    def download(url, target, **kw):
        downloads.append(url)
        if target.name == 'pika-release.json':
            target.write_text(json.dumps({'schema': 1, 'version': '1.0.0a2',
                'wheel': 'pikamux-1.0.0a2-py3-none-any.whl', 'sha256': 'b' * 64}))
    monkeypatch.setattr(installer, '_download', download)
    monkeypatch.setattr(installer, 'install', lambda *a, **kw: installs.append((a, kw)))
    installer.update(check=check)
    assert all('/releases/download/v1.0.0a2/' in url for url in downloads)
    assert len(downloads) == (1 if check else 2)
    assert len(installs) == (0 if check else 1)


def test_explicit_release_does_not_rediscover_and_mismatched_manifest_fails(managed, monkeypatch):
    monkeypatch.setattr(installer, 'latest_release', Mock(side_effect=AssertionError('must use approved version')))
    monkeypatch.setattr(installer, '_download', lambda _, path, **kw: path.write_text(json.dumps({
        'schema': 1, 'version': '2.0.0', 'wheel': 'pikamux-2.0.0-py3-none-any.whl', 'sha256': 'b' * 64,
    })))
    with pytest.raises(installer.InstallError, match='tag and manifest'):
        installer.update(release='1.0.0a2')
    with pytest.raises(installer.InstallError, match='downgrade'):
        installer.update(release='0.9.0')
    with pytest.raises(installer.InstallError, match='valid --release'):
        installer.update(release='../other')


@pytest.mark.parametrize('width,height', [(140, 30), (72, 24), (10, 6)])
def test_board_confirmation_is_explicit_cancellable_and_dimension_safe(width, height):
    state = monitor.MonitorState(update_version='1.0.0a2')
    assert monitor._handle_key('U', Mock(), state) == ('continue', None)
    assert state.mode == 'update'
    frame = monitor.render_monitor(state, width=width, height=height, color=False)
    assert len(frame.plain.splitlines()) == height
    assert all(len(line) <= width for line in frame.plain.splitlines())
    assert monitor._handle_key('escape', Mock(), state) == ('continue', None)
    assert state.mode == 'sessions'
    monitor._handle_key('U', Mock(), state)
    assert monitor._handle_key('enter', Mock(), state) == ('update', None)
    assert state.update_status == 'installing'
    assert monitor._handle_key('enter', Mock(), state) == ('continue', None)


@pytest.mark.parametrize('mode', ['ask', 'filter'])
def test_update_key_is_text_inside_conversation_or_filter(mode):
    state = monitor.MonitorState(mode=mode, update_version='1.0.0a2')
    monitor._handle_key('U', Mock(), state)
    assert state.mode == mode
    assert (state.ask_input if mode == 'ask' else state.filter_text) == 'U'


def test_update_subprocess_uses_pinned_version_and_file_not_terminal(managed, monkeypatch):
    def run(argv, **kw):
        assert argv == [installer.sys.executable, '-m', 'pikamux', 'update', '--release', '1.0.0a2']
        assert kw['stdin'] == subprocess.DEVNULL
        assert kw['start_new_session'] is True
        kw['stdout'].write(b'installed')
        return subprocess.CompletedProcess(argv, 0)
    monkeypatch.setattr(installer.subprocess, 'run', run)
    assert 'Updated to 1.0.0a2' in installer.board_update('1.0.0a2')


@pytest.mark.parametrize('fail', [False, True])
def test_board_remains_responsive_during_check_and_install(monkeypatch, fail):
    check_started, release_check = threading.Event(), threading.Event()
    install_started, release_install = threading.Event(), threading.Event()
    calls = []
    def check():
        check_started.set()
        release_check.wait(5)
        return '1.0.0a2'
    def install(version):
        calls.append(version)
        install_started.set()
        release_install.wait(5)
        if fail:
            raise installer.InstallError('simulated offline update')
        return 'Installed test release; reopen pika'
    monkeypatch.setattr(installer, 'update_notice', check)
    monkeypatch.setattr(installer, 'board_update', install)
    pika = SimpleNamespace(store=SimpleNamespace(list_sessions=lambda: []), refresh=lambda **kw: [])
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', 24, 120, 0, 0))
    reader = PtyReader(master)
    results = []
    thread = threading.Thread(target=lambda: results.append(monitor.run_monitor(
        pika, input_fd=slave, output_fd=slave, refresh_seconds=.02)))
    try:
        thread.start()
        reader.until(b'LIVE OPERATIONS', timeout=2)
        assert check_started.wait(1)
        os.write(master, b'/still-responsive')
        reader.until(b'still-responsive', timeout=2)
        os.write(master, b'\x1b')
        release_check.set()
        reader.until(b'Update available: 1.0.0a2', timeout=2)
        os.write(master, b'U')
        reader.until(b'Install this release on this machine only?', timeout=2)
        assert not install_started.is_set()
        os.write(master, b'n')
        reader.until(b'LIVE OPERATIONS', timeout=2)
        assert not install_started.is_set()
        os.write(master, b'U\r\r')
        assert install_started.wait(2)
        reader.until(b'Installing in the background', timeout=2)
        os.write(master, b'\x1b')
        reader.until(b'Updating Pika to 1.0.0a2', timeout=2)
        os.write(master, b'/during-install')
        reader.until(b'during-install', timeout=2)
        os.write(master, b'\x1b')
        release_install.set()
        reader.until(b'Pika update failed' if fail else b'1.0.0a2 installed', timeout=2)
        # First Esc clears the filter; q exits after installation or failure.
        os.write(master, b'q')
        thread.join(2)
        assert not thread.is_alive()
        assert calls == ['1.0.0a2']
        assert results == [0]
    finally:
        release_check.set()
        release_install.set()
        if thread.is_alive():
            os.write(master, b'\x1b')
            os.write(master, b'q')
            thread.join(2)
        reader.close()
        os.close(master)
        os.close(slave)


@pytest.mark.parametrize('width', [72, 140])
def test_notice_preserves_usage_tips_and_does_not_change_attention(width):
    state = monitor.MonitorState(update_version='1.0.0a2')
    frame = monitor.render_monitor(state, width=width, height=24, color=False)
    assert 'Update available: 1.0.0a2' in frame.plain
    assert ('PIKA TIP' if width == 72 else 'PIKA PLAYBOOK') in frame.plain
    assert state.sessions == [] and state.refresh_error is None


def test_long_update_failure_keeps_retry_controls_visible():
    state = monitor.MonitorState(mode='update', update_version='1.0.0a2',
        update_status='error', update_detail='many diagnostic lines ' * 1000)
    frame = monitor.render_monitor(state, width=72, height=15, color=False)
    assert 'Enter / y retry' in frame.plain.splitlines()[-1]
    assert monitor._handle_key('y', Mock(), state) == ('update', None)
    assert monitor._handle_key('y', Mock(), state) == ('continue', None)
