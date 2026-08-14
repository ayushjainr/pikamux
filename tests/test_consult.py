from __future__ import annotations

import io
import json
import unittest
from contextlib import redirect_stdout
from unittest.mock import Mock, patch

from pikamux.cli import _ask, _normalize_argv
from pikamux.consult import (
    DEFAULT_CODEX_EFFORT,
    DEFAULT_CODEX_MODEL,
    FAST_CODEX_EFFORT,
    FAST_CODEX_MODEL,
    ClaudeConsultation,
    CodexConsultation,
    ConsultationError,
    ConsultationPolicy,
    _version_tuple,
    consultation_for,
    consultation_policy,
)
from pikamux.models import Session


class FakeProcess:
    def __init__(self, messages: list[dict], *, returncode: int | None = None):
        self.stdin = io.StringIO()
        self.stdout = io.StringIO("".join(json.dumps(item) + "\n" for item in messages))
        self.stderr = io.StringIO()
        self.returncode = returncode
        self.terminated = False

    def poll(self):
        return self.returncode

    def terminate(self):
        self.terminated = True
        self.returncode = 0

    def kill(self):
        self.returncode = -9

    def wait(self, timeout=None):
        return self.returncode


class ConsultationTests(unittest.TestCase):
    def test_version_parser(self) -> None:
        self.assertEqual(_version_tuple("2.1.228 (Claude Code)"), (2, 1, 228))
        self.assertEqual(_version_tuple("unknown"), ())

    def test_codex_uses_one_ephemeral_fork_for_multiple_turns(self) -> None:
        messages = [
            {"id": 1, "result": {"userAgent": "codex"}},
            {
                "id": 2,
                "result": {
                    "thread": {"id": "side-id", "ephemeral": True},
                    "model": DEFAULT_CODEX_MODEL,
                    "reasoningEffort": DEFAULT_CODEX_EFFORT,
                },
            },
            {"id": 3, "result": {"turn": {"id": "turn-1"}}},
            {
                "method": "item/completed",
                "params": {
                    "threadId": "side-id",
                    "turnId": "turn-1",
                    "item": {"type": "agentMessage", "text": "first"},
                },
            },
            {
                "method": "thread/status/changed",
                "params": {
                    "threadId": "side-id",
                    "status": {"type": "idle"},
                },
            },
            {"id": 4, "result": {"turn": {"id": "turn-2"}}},
            {
                "method": "item/completed",
                "params": {
                    "threadId": "side-id",
                    "turnId": "turn-2",
                    "item": {"type": "agentMessage", "text": "second"},
                },
            },
            {
                "method": "turn/completed",
                "params": {
                    "threadId": "side-id",
                    "turn": {"id": "turn-2", "status": "completed"},
                },
            },
        ]
        process = FakeProcess(messages)
        session = Session("codex", "parent-id", cwd="/tmp")
        with (
            patch(
                "pikamux.consult.configured_executable",
                return_value="/usr/bin/codex",
            ),
            patch("pikamux.consult.executable_available", return_value=True),
            patch("pikamux.consult.subprocess.Popen", return_value=process),
            patch(
                "pikamux.consult.select.select",
                side_effect=lambda readable, _w, _x, _timeout: (readable, [], []),
            ),
        ):
            consultation = CodexConsultation(session)
            self.assertEqual(consultation.ask("one"), "first")
            self.assertEqual(consultation.ask("two"), "second")
            consultation.close()
        sent = [json.loads(line) for line in process.stdin.getvalue().splitlines()]
        forks = [item for item in sent if item.get("method") == "thread/fork"]
        self.assertEqual(len(forks), 1)
        self.assertTrue(forks[0]["params"]["ephemeral"])
        self.assertEqual(forks[0]["params"]["model"], DEFAULT_CODEX_MODEL)
        self.assertEqual(
            forks[0]["params"]["config"]["model_reasoning_effort"],
            DEFAULT_CODEX_EFFORT,
        )
        self.assertEqual(
            [
                item["params"]["threadId"]
                for item in sent
                if item.get("method") == "turn/start"
            ],
            ["side-id", "side-id"],
        )
        starts = [item for item in sent if item.get("method") == "turn/start"]
        self.assertEqual(
            [(item["params"]["model"], item["params"]["effort"]) for item in starts],
            [(DEFAULT_CODEX_MODEL, DEFAULT_CODEX_EFFORT)] * 2,
        )
        self.assertTrue(process.terminated)

    def test_codex_fast_policy_is_explicit_on_every_turn(self) -> None:
        messages = [
            {"id": 1, "result": {}},
            {
                "id": 2,
                "result": {
                    "thread": {"id": "side", "ephemeral": True},
                    "model": FAST_CODEX_MODEL,
                    "reasoningEffort": FAST_CODEX_EFFORT,
                },
            },
            {"id": 3, "result": {"turn": {"id": "turn"}}},
            {
                "method": "item/completed",
                "params": {
                    "threadId": "side",
                    "turnId": "turn",
                    "item": {"type": "agentMessage", "text": "fast"},
                },
            },
        ]
        process = FakeProcess(messages)
        policy = ConsultationPolicy("fast", FAST_CODEX_MODEL, FAST_CODEX_EFFORT)
        with (
            patch("pikamux.consult.configured_executable", return_value="codex"),
            patch("pikamux.consult.executable_available", return_value=True),
            patch("pikamux.consult.subprocess.Popen", return_value=process),
            patch(
                "pikamux.consult.select.select",
                side_effect=lambda readable, _w, _x, _timeout: (readable, [], []),
            ),
        ):
            with CodexConsultation(
                Session("codex", "parent", cwd="/tmp"), policy=policy
            ) as consultation:
                self.assertEqual(consultation.ask("question"), "fast")
        sent = [json.loads(line) for line in process.stdin.getvalue().splitlines()]
        fork = next(item for item in sent if item.get("method") == "thread/fork")
        self.assertEqual(fork["params"]["model"], FAST_CODEX_MODEL)
        self.assertEqual(
            fork["params"]["config"]["model_reasoning_effort"],
            FAST_CODEX_EFFORT,
        )
        turn = next(item for item in sent if item.get("method") == "turn/start")
        self.assertEqual(turn["params"]["model"], FAST_CODEX_MODEL)
        self.assertEqual(turn["params"]["effort"], FAST_CODEX_EFFORT)

    def test_claude_fast_mode_fails_closed(self) -> None:
        session = Session("claude", "parent")
        with self.assertRaisesRegex(ConsultationError, "not benchmarked for Claude"):
            consultation_policy(session, fast=True)

    def test_explicit_policy_cannot_bypass_provider_invariants(self) -> None:
        with self.assertRaisesRegex(ConsultationError, "Invalid codex"):
            consultation_for(
                Session("codex", "parent"),
                policy=ConsultationPolicy("default", FAST_CODEX_MODEL, "medium"),
            )
        with self.assertRaisesRegex(ConsultationError, "not benchmarked for Claude"):
            consultation_for(
                Session("claude", "parent"),
                policy=ConsultationPolicy("fast", FAST_CODEX_MODEL, "medium"),
            )

    def test_concrete_consultations_reject_the_wrong_provider(self) -> None:
        with self.assertRaisesRegex(ConsultationError, "cannot open a claude"):
            CodexConsultation(Session("claude", "parent"))
        with self.assertRaisesRegex(ConsultationError, "cannot open a codex"):
            ClaudeConsultation(Session("codex", "parent"))

    def test_codex_fails_closed_when_provider_does_not_confirm_policy(self) -> None:
        process = FakeProcess(
            [
                {"id": 1, "result": {}},
                {
                    "id": 2,
                    "result": {
                        "thread": {"id": "side", "ephemeral": True},
                        "model": DEFAULT_CODEX_MODEL,
                        "reasoningEffort": "high",
                    },
                },
            ]
        )
        with (
            patch("pikamux.consult.configured_executable", return_value="codex"),
            patch("pikamux.consult.executable_available", return_value=True),
            patch("pikamux.consult.subprocess.Popen", return_value=process),
            patch(
                "pikamux.consult.select.select",
                side_effect=lambda readable, _w, _x, _timeout: (readable, [], []),
            ),
            self.assertRaisesRegex(ConsultationError, "observed.*high"),
        ):
            CodexConsultation(Session("codex", "parent", cwd="/tmp"))
        self.assertTrue(process.terminated)

    def test_codex_fails_closed_without_ephemeral_confirmation(self) -> None:
        process = FakeProcess(
            [
                {"id": 1, "result": {}},
                {"id": 2, "result": {"thread": {"id": "persisted"}}},
            ]
        )
        with (
            patch("pikamux.consult.configured_executable", return_value="codex"),
            patch("pikamux.consult.executable_available", return_value=True),
            patch("pikamux.consult.subprocess.Popen", return_value=process),
            patch(
                "pikamux.consult.select.select",
                side_effect=lambda readable, _w, _x, _timeout: (readable, [], []),
            ),
            self.assertRaisesRegex(ConsultationError, "did not confirm"),
        ):
            CodexConsultation(Session("codex", "parent", cwd="/tmp"))
        self.assertTrue(process.terminated)

    def test_claude_uses_one_nonpersistent_no_tools_process_for_multiple_turns(
        self,
    ) -> None:
        process = FakeProcess(
            [
                {
                    "type": "assistant",
                    "message": {"content": [{"type": "text", "text": "first answer"}]},
                },
                {
                    "type": "result",
                    "subtype": "success",
                    "is_error": False,
                    "result": "first answer",
                },
                {
                    "type": "assistant",
                    "message": {"content": [{"type": "text", "text": "second answer"}]},
                },
                {
                    "type": "result",
                    "subtype": "success",
                    "is_error": False,
                    "result": "second answer",
                },
            ]
        )
        version = Mock(stdout="2.1.228", stderr="")
        popen = Mock(return_value=process)
        with (
            patch("pikamux.consult.configured_executable", return_value="claude"),
            patch("pikamux.consult.executable_available", return_value=True),
            patch("pikamux.consult.subprocess.run", return_value=version),
            patch("pikamux.consult.subprocess.Popen", popen),
            patch(
                "pikamux.consult.select.select",
                side_effect=lambda readable, _w, _x, _timeout: (readable, [], []),
            ),
        ):
            consultation = ClaudeConsultation(
                Session("claude", "parent-id", cwd="/tmp")
            )
            self.assertEqual(consultation.ask("one"), "first answer")
            self.assertEqual(consultation.ask("two"), "second answer")
            consultation.close()
        payloads = [json.loads(line) for line in process.stdin.getvalue().splitlines()]
        self.assertEqual(
            [item["message"]["content"][0]["text"] for item in payloads],
            ["one", "two"],
        )
        argv = popen.call_args.args[0]
        self.assertIn("--no-session-persistence", argv)
        self.assertEqual(argv[argv.index("--tools") + 1], "")
        self.assertEqual(popen.call_count, 1)
        self.assertTrue(process.terminated)

    def test_ask_cli_closes_ephemeral_consultation(self) -> None:
        session = Session(
            "codex", "parent-id", name="parent", transcript_path="/tmp/parent.jsonl"
        )
        pika = Mock()
        pika.resolve.return_value = session
        consultation = Mock()
        consultation.__enter__ = Mock(return_value=consultation)
        consultation.__exit__ = Mock(return_value=None)
        consultation.ask.return_value = "answer"
        consultation.policy = ConsultationPolicy(
            "default", DEFAULT_CODEX_MODEL, DEFAULT_CODEX_EFFORT
        )
        with (
            patch("pikamux.cli.consultation_for", return_value=consultation),
            patch("pikamux.cli.sys.stdin", io.StringIO("")),
            redirect_stdout(io.StringIO()) as output,
        ):
            self.assertEqual(_ask(pika, "parent", ["question"]), 0)
        self.assertIn("EPHEMERAL", output.getvalue())
        self.assertIn(DEFAULT_CODEX_MODEL, output.getvalue())
        self.assertIn(DEFAULT_CODEX_EFFORT, output.getvalue())
        self.assertIn("parent transcript unchanged", output.getvalue())

    def test_jsonl_ask_keeps_one_consultation_for_multiple_turns(self) -> None:
        session = Session(
            "codex", "parent-id", name="parent", transcript_path="/tmp/parent.jsonl"
        )
        pika = Mock()
        pika.resolve.return_value = session
        consultation = Mock()
        consultation.__enter__ = Mock(return_value=consultation)
        consultation.__exit__ = Mock(return_value=None)
        consultation.ask.side_effect = ["first answer", "second answer"]
        consultation.policy = ConsultationPolicy(
            "fast", FAST_CODEX_MODEL, FAST_CODEX_EFFORT
        )
        requests = io.StringIO(
            '{"question":"first"}\n{"question":"second"}\n{"close":true}\n'
        )
        with (
            patch(
                "pikamux.cli.consultation_for", return_value=consultation
            ) as factory,
            patch("pikamux.cli.sys.stdin", requests),
            redirect_stdout(io.StringIO()) as output,
        ):
            self.assertEqual(_ask(pika, "parent", [], jsonl=True, fast=True), 0)
        messages = [json.loads(line) for line in output.getvalue().splitlines()]
        self.assertEqual(
            [item["type"] for item in messages],
            ["opened", "answer", "answer", "closed"],
        )
        self.assertEqual(
            [call.args[0] for call in consultation.ask.call_args_list],
            ["first", "second"],
        )
        self.assertEqual(messages[0]["consultation_mode"], "fast")
        self.assertEqual(messages[0]["model"], FAST_CODEX_MODEL)
        self.assertEqual(messages[0]["effort"], FAST_CODEX_EFFORT)
        self.assertEqual(messages[1]["model"], FAST_CODEX_MODEL)
        self.assertEqual(messages[1]["effort"], FAST_CODEX_EFFORT)
        self.assertEqual(messages[-1]["model"], FAST_CODEX_MODEL)
        self.assertEqual(messages[-1]["effort"], FAST_CODEX_EFFORT)
        factory.assert_called_once_with(session, fast=True)
        self.assertTrue(messages[-1]["parent_transcript_unchanged"])

    def test_ask_is_a_public_command(self) -> None:
        self.assertEqual(_normalize_argv(["ask", "name", "why"])[0], "ask")


if __name__ == "__main__":
    unittest.main()
