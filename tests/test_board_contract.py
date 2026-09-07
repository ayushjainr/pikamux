"""User-visible attention, navigation and first-frame contracts."""
from __future__ import annotations

import fcntl
import os
import pty
import select
import struct
import termios
import threading
import time
from dataclasses import replace

from pikamux.models import FleetNode, Session, Status
from pikamux.monitor import (
    MonitorState, _handle_key, _observation_scope, _split_groups,
    _split_left_pane, render_monitor, run_monitor,
)


def test_action_errors_keep_exact_steps_until_explicitly_dismissed():
    session = Session("codex", "exact-uuid", name="research", status="PARKED")
    state = MonitorState(sessions=[session])
    state.fail_action("\n".join(["Provider unavailable"] * 40 + ["Run exactly: pika exact-uuid"]))
    assert state.mode == "action-error"
    _handle_key("pagedown", None, state)
    state.action_error_offset = 40
    frame = render_monitor(state, width=72, height=20, now=time.time() + 9999, color=False)
    assert "Run exactly: pika exact-uuid" in frame.plain
    assert "OPENING PAUSED" in frame.plain
    action, target = _handle_key("enter", None, state)
    assert (action, target) == ("open", session)
    _handle_key("escape", None, state)
    assert state.mode == "sessions" and state.action_error is None


def test_error_retry_cannot_drift_to_another_thread_after_refresh():
    original = Session("codex", "original", name="same-name", status="PARKED")
    other = Session("claude", "other", name="same-name", status="PARKED")
    state = MonitorState(sessions=[original, other], selected_key=original.key)
    state.fail_action("Run exactly: pika original", original)
    state.sessions = [other]
    assert state.selected().key == other.key
    action, target = _handle_key("enter", None, state)
    assert (action, target) == ("continue", None)
    assert "no longer in current inventory" in state.action_error
    assert state.action_error_target.key == original.key
    # Reappearing under another name does not change the pinned identity.
    state.sessions.append(replace(original, name="renamed"))
    action, target = _handle_key("enter", None, state)
    assert action == "open" and target.key == original.key


def test_error_retry_blocks_a_changed_active_fork():
    original = Session("codex", "logical", name="research", active_thread_id="before")
    state = MonitorState(sessions=[original])
    state.fail_action("Opening failed", original)
    state.sessions = [replace(original, active_thread_id="after")]
    assert _handle_key("enter", None, state) == ("continue", None)
    assert "active provider identity changed" in state.action_error


def test_attention_is_not_an_unread_inbox():
    items = [
        Session("codex", "question", status=Status.NEEDS_YOU.value, unread=True),
        Session("codex", "done", status=Status.READY.value, unread=True, live=True),
        Session("codex", "failed", status=Status.ERROR.value, unread=True),
        Session("codex", "duplicate", status=Status.OPEN_TWICE.value, unread=True),
        Session("codex", "busy", status=Status.WORKING.value, live=True),
    ]
    groups = {label: [item.session_id for item in members] for label, members in _split_groups(items)}
    assert groups["NEEDS YOU"] == ["question"]
    assert groups["RESULTS"] == ["done"]
    assert set(groups["EXCEPTIONS"]) == {"failed", "duplicate"}
    assert items[1].unread  # Classification is not acknowledgement.


def test_status_updates_preserve_navigation_and_real_labels():
    items = [Session("codex", str(i), name=f"agent-{i:02}", status="WORKING") for i in range(30)]
    state = MonitorState(sessions=items)
    original_order = [item.key for item in state.ordered()]
    state.selected_key = items[15].key
    _split_left_pane(state, width=40, height=10, now=time.time(), color=False)
    state.sessions = [replace(item, status="NEEDS YOU" if item.session_id == "1" else "READY") for item in items]
    assert [item.key for item in state.ordered()] == original_order
    assert state.selected().key == items[15].key
    state.move(1)
    assert state.selected().key == items[16].key
    frame = render_monitor(state, width=150, height=35, color=False)
    assert "RESULTS" in frame.plain
    assert "WORKING" not in frame.plain.split("THREAD EXPERTISE")[0]


def test_scope_never_reassures_over_missing_machine():
    node = FleetNode(node_id="remote", alias="unreachable", ssh_target="host", status="offline")
    state = MonitorState(machines=[node], last_update=time.time())
    scope = _observation_scope(state, time.time())
    assert "UNAVAILABLE" in scope and "unreachable" in scope
    assert "UNAVAILABLE" in render_monitor(state, width=130, height=24, color=False).plain


class _CacheStore:
    def __init__(self, sessions):
        self.sessions = sessions

    def list_sessions(self):
        return self.sessions

    def claim_monitor_handoff(self, now):
        return 0, {}


def _terminal():
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 150, 0, 0))
    return master, slave


def _read_until(fd, needle, timeout=1.0):
    data = b""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if select.select([fd], [], [], max(0, deadline - time.monotonic()))[0]:
            data += os.read(fd, 65536)
            if needle in data:
                return data
    raise AssertionError(f"Missing {needle!r} in terminal output {data[-1000:]!r}")


def test_cached_board_is_usable_while_local_and_remote_scans_are_blocked():
    release = threading.Event()
    opened = threading.Event()
    item = Session("codex", "cached-uuid", name="cached-agent", status="PARKED")

    class Fleet:
        def cached_sessions(self):
            return []

        def nodes(self):
            return [FleetNode(node_id="offline", alias="offline", ssh_target="offline", status="ready")]

    class Pika:
        store = _CacheStore([item])
        fleet = Fleet()

        def refresh(self, usage=False):
            release.wait(2)
            return [item]

        def refresh_remote_node(self, _node):
            release.wait(2)
            return []

        def open(self, selected, attach=True):
            assert selected.session_id == "cached-uuid"
            opened.set()
            return 0

    master, slave = _terminal()
    thread = threading.Thread(target=lambda: run_monitor(Pika(), input_fd=slave, output_fd=slave))
    try:
        started = time.monotonic()
        thread.start()
        output = _read_until(master, b"cached-agent", timeout=.5)
        assert time.monotonic() - started < .5
        assert b"LAST KNOWN" in output
        os.write(master, b"\r")
        assert opened.wait(.5), "Cached row must be actionable before scans finish"
        _read_until(master, b"cached-agent")
        os.write(master, b"q")
        thread.join(.5)
        assert not thread.is_alive(), "Offline node cannot block exit either"
    finally:
        release.set()
        if thread.is_alive():
            os.write(master, b"q")
            thread.join(1)
        os.close(master)
        os.close(slave)


def test_attach_detach_returns_to_filtered_selection_with_native_terminal():
    items = [Session("codex", name, name=name, status="PARKED") for name in ["alpha", "beta"]]
    opened = []
    master, slave = _terminal()
    original = termios.tcgetattr(slave)

    class Pika:
        store = _CacheStore(items)

        def refresh(self, usage=False):
            return list(reversed(items))

        def open(self, selected, attach=True):
            assert termios.tcgetattr(slave) == original
            opened.append(selected.session_id)
            return 0

    thread = threading.Thread(target=lambda: run_monitor(Pika(), input_fd=slave, output_fd=slave))
    try:
        thread.start()
        _read_until(master, b"alpha")
        os.write(master, b"/beta\r\r")
        _read_until(master, b"/ beta")
        assert opened == ["beta"]
        os.write(master, b"\r")
        _read_until(master, b"/ beta")
        assert opened == ["beta", "beta"]
        os.write(master, b"q")
        thread.join(1)
        assert not thread.is_alive()
        assert termios.tcgetattr(slave) == original
    finally:
        if thread.is_alive():
            os.write(master, b"q")
            thread.join(1)
        os.close(master)
        os.close(slave)
