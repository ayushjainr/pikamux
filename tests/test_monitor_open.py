"""Board attach failures and presentation, without provider calls or live state."""
import fcntl
import os
import pty
import shutil
import struct
import subprocess
import termios
import threading
from unittest.mock import Mock

import pytest

from pikamux.models import FleetSession, Session, Status
from pikamux.monitor import (
    MonitorState, _handle_key, _open_in_terminal, _public_status, _split_groups,
    render_monitor, run_monitor,
)
from pty_reader import PtyReader


@pytest.mark.parametrize("code", [1, 7, 130, 255, -15])
@pytest.mark.parametrize("remote", [False, True])
def test_failed_attach_preserves_exact_recovery_route(code, remote):
    session = Session("codex", "exact-id", name="same name")
    if remote:
        session = FleetSession("node-id", "devbox", session)
    terminal = Mock()
    terminal.run_external.side_effect = lambda callback: callback()
    with pytest.raises(RuntimeError) as error:
        _open_in_terminal(terminal, lambda: code, session)
    assert f"exit code {code}" in str(error.value)
    assert f"pika exact-id{'@devbox' if remote else ''}" in str(error.value)


def test_normal_detach_is_success_and_provider_exception_is_preserved():
    session = Session("codex", "exact-id")
    terminal = Mock()
    terminal.run_external.side_effect = lambda callback: callback()
    assert _open_in_terminal(terminal, lambda: 0, session) is None
    def failure():
        raise RuntimeError("Exact identity rejected")
    with pytest.raises(RuntimeError, match="Exact identity rejected"):
        _open_in_terminal(terminal, failure, session)


def test_four_sections_keep_every_identity_and_raw_status():
    sessions = [Session("codex", status.value, name="same", status=status.value)
                for status in Status]
    groups = dict(_split_groups(sessions))
    assert list(groups) == ["NEEDS YOU", "WORKING", "READY", "PARKED"]
    assert {s.status for s in groups["NEEDS YOU"]} == {
        "NEEDS YOU", "ERROR", "OPEN TWICE", "UNBOUND"}
    assert {s.status for s in groups["WORKING"]} == {"WORKING", "STARTING"}
    assert sorted(s.key for group in groups.values() for s in group) == sorted(s.key for s in sessions)
    assert [s.status for s in sessions] == [s.value for s in Status]
    assert _split_groups([]) == []
    assert list(dict(_split_groups([sessions[0]]))) == ["WORKING"]


def test_stale_remote_stays_marked_and_cannot_be_opened():
    session = FleetSession("remote-node", "devbox",
                           Session("codex", "exact-id", status="WORKING"), stale=True)
    assert list(dict(_split_groups([session]))) == ["PARKED"]
    assert _public_status(session) == "CACHED"
    state = MonitorState(sessions=[session], selected_key=session.key)
    pika = Mock()
    assert _handle_key("enter", pika, state) == ("continue", None)
    pika.open.assert_not_called()
    frame = render_monitor(state, width=140, height=30, color=False)
    assert "cached" in frame.plain.lower()


@pytest.mark.parametrize("outside", [False, True])
@pytest.mark.parametrize("backend", ["shell", "tmux"])
def test_real_terminal_retains_failed_open_then_recovers(outside, backend, tmp_path):
    if backend == "tmux" and not shutil.which("tmux"):
        pytest.skip("tmux not installed")
    failed_code = 1 if backend == "tmux" else 7
    session = Session("codex", "test-exact-id", name="disposable",
                      status="UNBOUND" if outside else "PARKED",
                      live=outside, home_state="outside-live" if outside else "no-live-home")
    attempts = []
    opened = threading.Event()
    class FakeStore:
        def list_sessions(self):
            return [session]
        def claim_monitor_handoff(self, now):
            return None, {}
    class FakePika:
        store = FakeStore()
        discovery_errors = []
        usage_errors = []
        def refresh(self, **kwargs):
            return [session]
        def next_attention(self, *args):
            return None
        def open(self, target):
            assert target.key == session.key
            attempts.append("open")
            opened.set()
            if backend == "tmux" and len(attempts) == 1:
                # A private nonexistent socket: cannot contact a user's server.
                return subprocess.run(["tmux", "-S", str(tmp_path / "absent.sock"),
                                       "attach-session", "-t", "missing"],
                                      stdin=slave, stdout=slave, stderr=slave).returncode
            return subprocess.run(["/bin/sh", "-c", "exit 7" if len(attempts) == 1 else "exit 0"]).returncode
        def enter(self, query):
            assert query == session.display_name
            return self.open(session)
    pika = FakePika()
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 140, 0, 0))
    previous = termios.tcgetattr(slave)
    results, errors = [], []
    def run():
        try:
            results.append(run_monitor(pika, input_fd=slave, output_fd=slave, refresh_seconds=.02))
        except BaseException as error:
            errors.append(error)
    thread = threading.Thread(target=run, daemon=True)
    reader = PtyReader(master)
    try:
        thread.start()
        reader.until(b"PIKA // LIVE OPERATIONS", timeout=3)
        os.write(master, b"\r")
        assert opened.wait(3), errors
        reader.until((b"OPENING PAUSED", f"exit code {failed_code}".encode()), timeout=3)
        assert attempts == ["open"]
        # No automatic retry; the user explicitly asks to recheck the same ID.
        opened.clear()
        os.write(master, b"\r")
        assert opened.wait(3), errors
        reader.until(b"PIKA // LIVE OPERATIONS", timeout=3)
        assert attempts == ["open", "open"]
        os.write(master, b"q")
        thread.join(3)
        assert not thread.is_alive()
        assert not errors
        assert results == [0]
        restored = termios.tcgetattr(slave)
        restored[3] &= ~getattr(termios, "PENDIN", 0)
        previous[3] &= ~getattr(termios, "PENDIN", 0)
        assert restored == previous
    finally:
        if thread.is_alive():
            os.write(master, b"\x1b")
            os.write(master, b"q")
            thread.join(3)
        reader.close()
        os.close(master)
        os.close(slave)


def test_retry_does_not_follow_changed_provider_identity():
    old = Session("codex", "home-id", active_thread_id="original-id")
    state = MonitorState(sessions=[old])
    state.fail_action("attach failed", old)
    state.sessions = [Session("codex", "home-id", active_thread_id="different-id")]
    assert _handle_key("enter", Mock(), state) == ("continue", None)
    assert "identity changed" in state.action_error


@pytest.mark.parametrize("width,height", [(72,20), (104,20), (140,30), (180,45)])
@pytest.mark.parametrize("vanished", [False, True])
def test_error_panel_is_visible_at_every_board_layout(width, height, vanished):
    target = Session("codex", "exact-id", name="disposable")
    state = MonitorState(sessions=[target])
    state.fail_action("Open failed. Run exactly: pika exact-id", target)
    if vanished:
        state.sessions = []
    frame = render_monitor(state, width=width, height=height, color=False)
    assert "OPENING PAUSED" in frame.plain
    assert "Run exactly: pika exact-id" in frame.plain
    assert state.mode == "action-error"


def test_wide_header_counts_match_the_four_sections():
    sessions = [Session("codex", status.value, status=status.value) for status in Status]
    state = MonitorState(sessions=sessions)
    header = render_monitor(state, width=180, height=45, color=False).plain.splitlines()[0]
    for label, members in _split_groups(sessions):
        caption = "need you" if label == "NEEDS YOU" else label.lower()
        assert f"{len(members)} {caption}" in header
    assert "exceptions" not in header
    assert "results" not in header
