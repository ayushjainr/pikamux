from __future__ import annotations

import io
import json
import os
import threading
import time
from unittest.mock import patch

import pytest

from pikamux.consult import ConsultationError, ConsultationPolicy
from pikamux.consult_reporting import ConsultationRun
from pikamux.fleet import RemoteConsultation, SSHTransport
from pikamux.models import FleetNode, FleetSession, Session


POLICY = ConsultationPolicy("default", "gpt-5.6-sol", "medium")
NODE = FleetNode("node-uuid", "atlas", "atlas")
SESSION = FleetSession("node-uuid", "atlas", Session("codex", "parent-id"))
OPENED = {"type": "opened", "provider": "codex", "parent_id": "parent-id", **POLICY.receipt()}
CLOSED = {"type": "closed", "receipt_version": 2, "discarded": True, "cleanup": "complete"}


class WireProcess:
    def __init__(self, events):
        self.stdin = io.BytesIO()
        read, write = os.pipe()
        os.write(write, b"".join((json.dumps(e) + "\n").encode() for e in events))
        os.close(write)
        self.stdout = os.fdopen(read, "rb", buffering=0)
        read, write = os.pipe()
        os.close(write)
        self.stderr = os.fdopen(read, "rb", buffering=0)
        self.returncode = None
        self.terminated = False

    def poll(self):
        return self.returncode

    def wait(self, timeout=None):
        self.returncode = 0
        return 0

    def terminate(self):
        self.terminated = True
        self.returncode = -15

    def kill(self):
        self.returncode = -9

    def release(self):
        self.stdout.close()
        self.stderr.close()
        self.stdin.close()


def remote(process):
    with patch("pikamux.fleet.subprocess.Popen", return_value=process):
        return RemoteConsultation(NODE, SESSION, POLICY, SSHTransport())


def test_progress_during_prepare_and_turn_does_not_break_protocol():
    progress = {"type": "progress", "stage": "prepare", "delivery": "not_sent"}
    process = WireProcess([
        progress, OPENED,
        {**progress, "stage": "turn", "delivery": "unknown"},
        {**progress, "stage": "turn", "delivery": "confirmed"},
        {"type": "answer", "text": "answer"},
        {**progress, "stage": "cleanup", "delivery": "confirmed"}, CLOSED,
    ])
    try:
        events = []
        run = ConsultationRun.open(lambda: remote(process), on_event=events.append)
        assert run.ask("question") == "answer"
        run.close()
        assert run.cleanup == "complete"
        assert any(e["delivery"] == "confirmed" and e["stage"] == "turn" for e in events)
        assert not process.terminated
    finally:
        process.release()


@pytest.mark.parametrize("terminal", [
    [],
    [{"type": "closed", "discarded": True}],
    [{**CLOSED, "discarded": False, "cleanup": "failed"}],
])
def test_remote_answer_retained_but_missing_or_legacy_cleanup_proof_fails(terminal):
    process = WireProcess([OPENED, {"type": "answer", "text": "useful"}, *terminal])
    try:
        run = ConsultationRun.open(lambda: remote(process))
        assert run.ask("question") == "useful"
        with pytest.raises(ConsultationError):
            run.close()
        assert run.answers_received == 1
        expected = "failed" if terminal and terminal[0].get("cleanup") == "failed" else "unknown"
        assert run.cleanup == expected
        assert process.terminated
    finally:
        process.release()


def test_reported_remote_turn_error_preserves_delivery_and_allows_verified_cleanup():
    process = WireProcess([
        OPENED,
        {"type": "error", "message": "turn timed out", "stage": "turn",
         "delivery": "confirmed", "cleanup": "pending"},
        CLOSED,
    ])
    try:
        run = ConsultationRun.open(lambda: remote(process))
        with pytest.raises(ConsultationError) as error:
            run.ask("question")
        assert error.value.receipt["delivery"] == "confirmed"
        assert error.value.receipt["retry_safe"] is False
        run.close()
        assert run.cleanup == "complete"
        assert not process.terminated
    finally:
        process.release()


def test_cleanup_error_event_is_drained_before_terminal_failed_receipt():
    process = WireProcess([
        OPENED, {"type": "answer", "text": "useful"},
        {"type": "error", "message": "delete failed", "stage": "cleanup", "cleanup": "failed"},
        {**CLOSED, "discarded": False, "cleanup": "failed"},
    ])
    try:
        side = remote(process)
        assert side.ask("question") == "useful"
        with pytest.raises(ConsultationError, match="not verified"):
            side.close()
        assert not side._cleanup_confirmed
    finally:
        process.release()


def test_progress_stream_cannot_extend_the_absolute_deadline():
    side = object.__new__(RemoteConsultation)
    side._progress_callback = None
    clock = [0.0]

    def trickle(timeout):
        clock[0] += 0.4
        return {"type": "progress", "stage": "prepare", "delivery": "not_sent"}

    with (
        patch("pikamux.fleet.time.monotonic", side_effect=lambda: clock[0]),
        patch.object(side, "_read_event_bounded", side_effect=trickle) as reader,
        patch.object(side, "_abort") as abort,
        pytest.raises(ConsultationError, match="timed out"),
    ):
        side._read_event(1.0)
    assert reader.call_count == 3
    abort.assert_called_once()


def test_broken_transport_never_implies_remote_cleanup():
    process = WireProcess([OPENED])
    try:
        side = remote(process)
        with pytest.raises(ConsultationError):
            side.ask("question")
        with pytest.raises(ConsultationError, match="cleanup is unconfirmed"):
            side.close()
        assert side._transport_aborted
    finally:
        process.release()


def test_board_cancel_does_not_start_a_second_remote_reader():
    process = WireProcess([OPENED])
    reader_entered = threading.Event()
    release_reader = threading.Event()
    failures = []
    try:
        side = remote(process)

        def waiting(timeout):
            reader_entered.set()
            assert release_reader.wait(timeout=2)
            raise ConsultationError("connection closed")

        def ask():
            try:
                side.ask("question")
            except ConsultationError as exc:
                failures.append(exc)

        with patch.object(side, "_read_event_serial", side_effect=waiting) as read:
            thread = threading.Thread(target=ask)
            thread.start()
            assert reader_entered.wait(timeout=2)
            try:
                with pytest.raises(ConsultationError, match="cleanup is unconfirmed") as error:
                    side.close()
                assert error.value.receipt["cleanup"] == "unknown"
                assert read.call_count == 1
            finally:
                release_reader.set()
                thread.join(timeout=2)
        assert not thread.is_alive()
        assert process.terminated
        assert len(failures) == 1
    finally:
        process.release()


def test_remote_routes_stable_identity_but_verifies_actual_leaf():
    session = FleetSession("node-uuid", "atlas", Session(
        "codex", "parent-id", active_thread_id="active-leaf",
    ))
    process = WireProcess([
        {**OPENED, "workstream_id": "parent-id", "parent_id": "active-leaf"}, CLOSED,
    ])
    try:
        with patch("pikamux.fleet.subprocess.Popen", return_value=process) as popen:
            side = RemoteConsultation(NODE, session, POLICY, SSHTransport())
        command = popen.call_args.args[0]
        assert command[command.index("--session-id") + 1] == "parent-id"
        side.close()
    finally:
        process.release()


def test_remote_refuses_old_parent_when_current_leaf_is_expected():
    session = FleetSession("node-uuid", "atlas", Session(
        "codex", "parent-id", active_thread_id="active-leaf",
    ))
    process = WireProcess([OPENED])
    try:
        with (
            patch("pikamux.fleet.subprocess.Popen", return_value=process),
            pytest.raises(ConsultationError, match="no question was sent"),
        ):
            RemoteConsultation(NODE, session, POLICY, SSHTransport())
        assert process.stdin.getvalue() == b""
        assert process.terminated
    finally:
        process.release()
