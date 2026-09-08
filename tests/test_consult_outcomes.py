from __future__ import annotations

import io
import json
import subprocess
import sys
import time
from contextlib import redirect_stdout
from dataclasses import replace
from unittest.mock import Mock, patch

import pytest

from pikamux.cli import _ask
from pikamux.consult import (
    Consultation, ConsultationError, ConsultationPolicy, CodexConsultation, ClaudeConsultation,
    OpenCodeConsultation, DEFAULT_CODEX_MODEL, DEFAULT_CODEX_EFFORT,
    _read_json_line, _terminate,
)
from pikamux.consult_reporting import ConsultationRun
from pikamux.models import Session


SESSION = Session("codex", "parent-id", name="parent", transcript_path="/tmp/parent.jsonl")
POLICY = ConsultationPolicy("default", DEFAULT_CODEX_MODEL, DEFAULT_CODEX_EFFORT)


class Side(Consultation):
    def __init__(self, *, delivery="confirmed", failure=None, cleanup_failure=None):
        super().__init__(SESSION, POLICY)
        self.delivery = delivery
        self.failure = failure
        self.cleanup_failure = cleanup_failure
        self.questions = []
        self.close_calls = 0

    def ask(self, question):
        self.questions.append(question)
        self._progress("turn", self.delivery)
        if self.failure:
            raise self.failure
        return "answer to " + question

    def close(self):
        self.close_calls += 1
        if self.cleanup_failure:
            raise self.cleanup_failure


def cli(side=None, *, initial=None, requests="", opening_error=None):
    pika = Mock()
    pika.resolve.return_value = SESSION
    with (
        patch("pikamux.cli.consultation_for", return_value=side, side_effect=opening_error),
        patch("pikamux.cli.sys.stdin", io.StringIO(requests)),
        redirect_stdout(io.StringIO()) as output,
    ):
        result = _ask(pika, "parent", initial or [], jsonl=True)
    return result, [json.loads(line) for line in output.getvalue().splitlines()]


def test_answer_is_emitted_before_cleanup_and_discard_is_after_verification():
    side = Side()
    original_close = side.close

    def close():
        # Observe stdout at the cleanup boundary, not merely final event order.
        messages = [json.loads(line) for line in output.getvalue().splitlines()]
        assert any(event["type"] == "answer" for event in messages)
        assert not any(event.get("discarded") for event in messages)
        original_close()

    side.close = close
    pika = Mock()
    pika.resolve.return_value = SESSION
    with (
        patch("pikamux.cli.consultation_for", return_value=side),
        patch("pikamux.cli.sys.stdin", io.StringIO("")),
        redirect_stdout(io.StringIO()) as output,
    ):
        assert _ask(pika, "parent", ["question"], jsonl=True) == 0
    terminal = json.loads(output.getvalue().splitlines()[-1])
    assert terminal["discarded"] is True
    assert terminal["cleanup"] == "complete"
    assert terminal["receipt_version"] == 2
    assert terminal["parent_transcript_unchanged"] is None
    assert terminal["parent_transcript_verification"] == "not_performed"
    assert side.close_calls == 1


def test_answer_survives_cleanup_failure_without_false_discard():
    side = Side(cleanup_failure=ConsultationError("side ses_child still exists"))
    result, events = cli(side, initial=["question"])
    assert result == 1
    assert next(e for e in events if e["type"] == "answer")["text"] == "answer to question"
    error = next(e for e in events if e["type"] == "error")
    assert error["stage"] == "cleanup"
    assert error["answers_received"] == 1
    assert error["delivery"] == "confirmed"
    assert error["retry_safe"] is False
    assert events[-1]["discarded"] is False
    assert events[-1]["cleanup"] == "failed"
    assert events[-1]["parent_transcript_unchanged"] is None
    assert events[-1]["parent_transcript_verification"] == "not_performed"
    assert side.questions == ["question"]


def test_cleanup_does_not_certify_parent_bytes_when_the_parent_changes(tmp_path):
    parent = tmp_path / "parent.jsonl"
    parent.write_text("before\n")
    side = Side()
    original_ask = side.ask

    def ask(question):
        # This could be the original agent continuing its own work, or an
        # isolation defect. Teardown alone cannot tell which, or prove equality.
        parent.write_text("before\nnew parent turn\n")
        return original_ask(question)

    side.ask = ask
    with patch.object(SESSION, "transcript_path", str(parent)):
        result, events = cli(side, initial=["question"])
    assert result == 0
    assert parent.read_text() == "before\nnew parent turn\n"
    assert events[-1]["discarded"] is True
    assert events[-1]["parent_transcript_unchanged"] is None
    assert events[-1]["parent_transcript_verification"] == "not_performed"


@pytest.mark.parametrize("delivery", ["not_sent", "unknown", "confirmed"])
def test_turn_failure_uses_provider_delivery_evidence_and_never_retries(delivery):
    side = Side(delivery=delivery, failure=ConsultationError("timed out"))
    result, events = cli(side, initial=["question"])
    assert result == 1
    error = next(e for e in events if e["type"] == "error")
    assert error["delivery"] == delivery
    assert error["retry_safe"] is (delivery == "not_sent")
    assert side.questions == ["question"]
    assert events[-1]["cleanup"] == "complete"


def test_open_failure_reports_no_delivery_and_actual_cleanup_evidence():
    error = ConsultationError("fork failed")
    error.cleanup = "complete"
    result, events = cli(opening_error=error)
    assert result == 1
    assert events[0]["stage"] == "prepare"
    assert events[-1]["stage"] == "prepare"
    assert events[-1]["delivery"] == "not_sent"
    assert events[-1]["cleanup"] == "complete"
    assert not any(e.get("discarded") for e in events)


def test_multi_turn_resets_delivery_and_preserves_first_answer_on_second_error():
    side = Side()
    ask = side.ask

    def second_fails(question):
        if question == "two":
            side.delivery = "not_sent"
            side.failure = ConsultationError("closed before send")
        return ask(question)

    side.ask = second_fails
    result, events = cli(side, requests='{"question":"one"}\n{"question":"two"}\n')
    assert result == 1
    answer = next(e for e in events if e["type"] == "answer")
    error = next(e for e in events if e["type"] == "error")
    assert answer["turn"] == 1
    assert error["turn"] == 2
    assert error["delivery"] == "not_sent"
    assert events[-1]["answers_received"] == 1
    assert side.close_calls == 1


def test_interrupt_closes_side_and_reports_delivery_unknown():
    side = Side(delivery="unknown", failure=KeyboardInterrupt())
    result, events = cli(side, initial=["question"])
    assert result == 1
    assert side.close_calls == 1
    assert next(e for e in events if e["type"] == "error")["delivery"] == "unknown"
    assert events[-1]["cleanup"] == "complete"


def test_invalid_json_does_not_submit_a_question_and_still_closes():
    side = Side()
    result, events = cli(side, requests="invalid\n")
    assert result == 1
    assert side.questions == []
    error = next(e for e in events if e["type"] == "error")
    assert error["stage"] == "input"
    assert error["delivery"] == "not_sent"
    assert side.close_calls == 1


def test_unknown_adapter_does_not_claim_a_failed_question_was_sent():
    adapter = Mock(policy=POLICY)
    adapter.ask.side_effect = OSError("socket gone")
    run = ConsultationRun.open(lambda: adapter)
    with pytest.raises(OSError) as failure:
        run.ask("question")
    assert failure.value.receipt["delivery"] == "unknown"
    assert failure.value.receipt["retry_safe"] is False
    run.close()


def test_original_failure_retained_when_cleanup_also_fails():
    primary = ConsultationError("turn lost")
    side = Side(failure=primary, cleanup_failure=ConsultationError("delete failed"))
    with pytest.raises(ConsultationError, match="turn lost") as caught:
        with ConsultationRun.open(lambda: side) as run:
            run.ask("question")
    assert caught.value is primary
    assert caught.value.receipt["cleanup"] == "failed"
    assert caught.value.receipt["cleanup_error"] == "delete failed"


def test_elapsed_tracks_preparation_turn_and_cleanup_without_model_calls():
    clock = [10.0]
    side = Side()
    with patch("pikamux.consult_reporting.time.monotonic", side_effect=lambda: clock[0]):
        def opener():
            clock[0] += 30
            return side
        events = []
        run = ConsultationRun.open(opener, on_event=events.append)
        assert events[-1]["elapsed_seconds"] == 30
        run.ask("question")
        clock[0] += 2
        run.close()
        assert events[-1]["elapsed_seconds"] == 32
    assert side.questions == ["question"]


def test_opencode_failed_deletion_preserves_exact_child_for_manual_recovery():
    side = OpenCodeConsultation(Session("opencode", "ses_parent123", cwd="/tmp"))
    side.thread_id = "ses_child123"
    with (
        patch("pikamux.consult.configured_executable", return_value="opencode"),
        patch("pikamux.consult.executable_available", return_value=True),
        patch("pikamux.consult.subprocess.run", return_value=Mock(returncode=0)),
        patch.object(side, "_session_exists", return_value=True),
        pytest.raises(ConsultationError, match="ses_child123 still exists"),
    ):
        side.close()
    assert side.thread_id == "ses_child123"
    assert not side.closed


def test_opencode_unknown_fork_identity_cannot_certify_discard():
    side = OpenCodeConsultation(Session("opencode", "ses_parent123"))
    side._fork_uncertain = True
    with pytest.raises(ConsultationError, match="identity is unknown"):
        side.close()


def test_codex_interrupt_during_fork_terminates_owned_process():
    process = Mock()
    process.poll.side_effect = [None, 0]
    with (
        patch("pikamux.consult.configured_executable", return_value="codex"),
        patch("pikamux.consult.executable_available", return_value=True),
        patch("pikamux.consult.subprocess.Popen", return_value=process),
        patch.object(CodexConsultation, "_request", side_effect=[{}, KeyboardInterrupt()]),
        pytest.raises(KeyboardInterrupt) as error,
    ):
        CodexConsultation(SESSION)
    process.terminate.assert_called_once()
    process.wait.assert_called_once_with(timeout=2)
    assert error.value.cleanup == "complete"


def test_closed_progress_pipe_cannot_prevent_cleanup():
    side = Side()
    events = []

    def observer(event):
        if event["stage"] == "cleanup":
            raise BrokenPipeError("receiver exited")
        events.append(event)

    run = ConsultationRun.open(lambda: side, on_event=observer)
    assert run.ask("question") == "answer to question"
    run.close()
    assert side.close_calls == 1
    assert run.cleanup == "complete"


def test_closed_progress_pipe_before_prepare_does_not_open_provider():
    opener = Mock()
    observer = Mock(side_effect=BrokenPipeError("receiver exited"))
    with pytest.raises(ConsultationError, match="no question was sent"):
        ConsultationRun.open(opener, on_event=observer)
    opener.assert_not_called()


@pytest.mark.parametrize("failed_type", ["opened", "answer", "closed"])
def test_disconnected_jsonl_consumer_always_closes_provider(failed_type):
    side = Side()
    pika = Mock()
    pika.resolve.return_value = SESSION

    class Disconnected(io.StringIO):
        failed = False

        def write(self, value):
            if value.startswith("{") and json.loads(value).get("type") == failed_type:
                self.failed = True
            if self.failed:
                raise BrokenPipeError("consumer disconnected")
            return super().write(value)

    with (
        patch("pikamux.cli.consultation_for", return_value=side),
        patch("pikamux.cli.sys.stdin", io.StringIO("")),
        redirect_stdout(Disconnected()),
        pytest.raises(BrokenPipeError),
    ):
        _ask(pika, "parent", ["question"], jsonl=True)
    assert side.close_calls == 1
    assert side.questions == ([] if failed_type == "opened" else ["question"])


def test_opencode_available_answer_survives_initial_termination_failure():
    side = OpenCodeConsultation(Session("opencode", "ses_parent123"))
    process = Mock()
    process.poll.return_value = None
    process.wait.side_effect = subprocess.TimeoutExpired("opencode", 2)
    with (
        patch("pikamux.consult.configured_executable", return_value="opencode"),
        patch("pikamux.consult.executable_available", return_value=True),
        patch("pikamux.consult.executable_version", return_value="1.18.21"),
        patch.object(side, "_fork_session", return_value="ses_child123"),
        patch.object(side, "_latest_message_time", return_value=10),
        patch.object(side, "_completed_answer", return_value="completed answer"),
        patch("pikamux.consult.subprocess.Popen", return_value=process),
    ):
        assert side.ask("question") == "completed answer"
    assert side.process is process
    with pytest.raises(subprocess.TimeoutExpired):
        side.close()
    assert side.thread_id == "ses_child123"


def test_opencode_child_identity_survives_fork_server_termination_failure():
    side = OpenCodeConsultation(Session("opencode", "ses_parent123", cwd="/tmp"))
    server = Mock()
    server.poll.return_value = None
    server.wait.side_effect = subprocess.TimeoutExpired("opencode serve", 2)
    with (
        patch.object(side, "_free_loopback_port", return_value=43210),
        patch("pikamux.consult.subprocess.Popen", return_value=server),
        patch.object(side, "_api_json", side_effect=[
            {"healthy": True}, {"id": "ses_child123", "directory": "/tmp"},
        ]),
        pytest.raises(subprocess.TimeoutExpired),
    ):
        side._fork_session("opencode")
    assert side.thread_id == "ses_child123"
    assert side._fork_server is server
    assert side._fork_uncertain is False


def test_codex_forks_current_provider_leaf_instead_of_stable_workstream():
    session = replace(SESSION, active_thread_id="active-leaf")
    process = Mock()
    with (
        patch("pikamux.consult.configured_executable", return_value="codex"),
        patch("pikamux.consult.executable_available", return_value=True),
        patch("pikamux.consult.subprocess.Popen", return_value=process),
        patch.object(CodexConsultation, "_request", side_effect=[{}, {
            "thread": {"id": "ephemeral-child", "ephemeral": True},
            "model": POLICY.model, "reasoningEffort": POLICY.effort,
        }]) as request,
        patch("pikamux.consult._terminate"),
    ):
        side = CodexConsultation(session)
        assert request.call_args_list[1].args[1]["threadId"] == "active-leaf"
        side.close()


def test_claude_resumes_current_provider_leaf():
    session = Session("claude", "stable-key", active_thread_id="active-leaf")
    with (
        patch.object(ClaudeConsultation, "_check_capability"),
        patch("pikamux.consult.configured_executable", return_value="claude"),
        patch("pikamux.consult.executable_available", return_value=True),
        patch("pikamux.consult.subprocess.Popen", return_value=Mock()) as popen,
        patch("pikamux.consult._terminate"),
    ):
        side = ClaudeConsultation(session)
        argv = popen.call_args.args[0]
        assert argv[argv.index("--resume") + 1] == "active-leaf"
        side.close()


def test_opencode_forks_current_leaf_and_never_deletes_returned_parent():
    session = Session("opencode", "ses_stable123", active_thread_id="ses_active123")
    side = OpenCodeConsultation(session)
    server = Mock()
    server.poll.return_value = None
    with (
        patch("pikamux.consult.subprocess.Popen", return_value=server),
        patch("pikamux.consult._terminate"),
        patch.object(side, "_api_json", side_effect=[
            {"healthy": True}, {"id": "ses_active123"},
        ]) as api,
        pytest.raises(ConsultationError, match="did not fork"),
    ):
        side._fork_session("opencode")
    assert "/session/ses_active123/fork" in api.call_args.args[0]
    assert side.thread_id is None
    with pytest.raises(ConsultationError, match="identity is unknown"):
        side.close()


def test_jsonl_distinguishes_stable_workstream_from_consulted_leaf():
    session = replace(SESSION, active_thread_id="active-leaf")
    side = Side()
    pika = Mock()
    pika.resolve.return_value = session
    with (
        patch("pikamux.cli.consultation_for", return_value=side),
        patch("pikamux.cli.sys.stdin", io.StringIO("")),
        redirect_stdout(io.StringIO()) as output,
    ):
        assert _ask(pika, "parent", ["question"], jsonl=True) == 0
    opened = next(json.loads(line) for line in output.getvalue().splitlines() if json.loads(line)["type"] == "opened")
    assert opened["workstream_id"] == "parent-id"
    assert opened["parent_id"] == "active-leaf"


def test_provider_two_frames_one_flush_does_not_wait_for_next_os_write():
    process = subprocess.Popen(
        [sys.executable, "-c", "import sys,time; sys.stdout.write('{\"id\":1}\\n{\"id\":2}\\n'); sys.stdout.flush(); time.sleep(2)"],
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
    )
    try:
        assert _read_json_line(process, deadline=time.monotonic() + 1) == {"id": 1}
        start = time.monotonic()
        assert _read_json_line(process, deadline=start + 0.2) == {"id": 2}
        assert time.monotonic() - start < 0.2
    finally:
        _terminate(process)
        process.stdout.close()
        process.stderr.close()


def test_provider_stderr_backpressure_is_drained_and_bounded():
    process = subprocess.Popen(
        [sys.executable, "-c", "import sys,time; sys.stderr.write('x'*200000); sys.stderr.flush(); print('{\"id\":1}',flush=True); time.sleep(2)"],
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
    )
    try:
        assert _read_json_line(process, deadline=time.monotonic() + 1) == {"id": 1}
        assert len(process._pika_stderr_buffer) <= 65536
    finally:
        _terminate(process)
        process.stdout.close()
        process.stderr.close()


def test_provider_partial_frame_obeys_deadline_without_blocking_readline():
    process = subprocess.Popen(
        [sys.executable, "-c", "import sys,time; sys.stdout.write('{'); sys.stdout.flush(); time.sleep(2)"],
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
    )
    try:
        start = time.monotonic()
        with pytest.raises(ConsultationError, match="timed out"):
            _read_json_line(process, deadline=start + 0.15)
        assert time.monotonic() - start < 0.6
    finally:
        _terminate(process)
        process.stdout.close()
        process.stderr.close()


def test_receiver_disconnect_at_turn_progress_prevents_question_submission():
    side = Side()

    def observer(event):
        if event["stage"] == "turn":
            raise BrokenPipeError("receiver disconnected")

    run = ConsultationRun.open(lambda: side, on_event=observer)
    with pytest.raises(ConsultationError, match="no question was sent") as error:
        run.ask("question")
    assert side.questions == []
    assert error.value.receipt["delivery"] == "not_sent"
    run.close()
    assert side.close_calls == 1
