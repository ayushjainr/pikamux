from __future__ import annotations

import io
import json
import os
import time
import unittest
from contextlib import redirect_stdout
from unittest.mock import Mock, patch

from pikamux.models import Session, Status
from pikamux.ui import (
    SelectionCancelled,
    choose_session,
    format_tokens,
    print_sessions,
)


class UiTests(unittest.TestCase):
    def setUp(self) -> None:
        now = time.time()
        self.sessions = [
            Session(
                "codex",
                "11111111-1111-4111-8111-111111111111",
                name="shared",
                cwd="/tmp/alpha",
                status=Status.NEEDS_YOU.value,
                unread=True,
                attention_reason="permission",
                last_activity_at=now - 60,
            ),
            Session(
                "claude",
                "22222222-2222-4222-8222-222222222222",
                name="shared",
                cwd="/tmp/beta",
                status=Status.ERROR.value,
                unread=True,
                error="claude exited with status 7",
                attention_reason="exited",
                last_activity_at=now - 120,
            ),
        ]

    def test_human_list_is_a_briefing_with_reasons_and_exceptions(self) -> None:
        output = io.StringIO()
        with (
            patch(
                "pikamux.ui.shutil.get_terminal_size",
                return_value=os.terminal_size((160, 24)),
            ),
            redirect_stdout(output),
        ):
            print_sessions(self.sessions)
        rendered = output.getvalue()
        self.assertIn("Pika briefing · 1 waiting on you · 1 failed", rendered)
        self.assertIn("NEW", rendered)
        self.assertIn("WHY", rendered)
        self.assertIn("VIEW", rendered)
        self.assertIn("permission", rendered)
        self.assertIn("Exceptions:", rendered)
        self.assertIn("claude exited with status 7", rendered)

    def test_large_token_counts_use_a_legible_billions_unit(self) -> None:
        self.assertEqual(format_tokens(9_417_690_000), "9.42b")

    def test_json_list_stays_pure_machine_output(self) -> None:
        output = io.StringIO()
        with redirect_stdout(output):
            print_sessions(self.sessions, as_json=True)
        payload = json.loads(output.getvalue())
        self.assertEqual(payload[0]["attention_reason"], "permission")
        self.assertNotIn("home_state", payload[0])
        self.assertNotIn("Pika briefing", output.getvalue())

    def test_human_surfaces_neutralize_provider_control_characters(self) -> None:
        dangerous = Session(
            "codex",
            "99999999-9999-4999-8999-999999999999",
            name="bad\x1b[2J\nname",
            status=Status.ERROR.value,
            unread=True,
            error="failure\rrewritten",
        )
        output = io.StringIO()
        with redirect_stdout(output):
            print_sessions([dangerous])
        rendered = output.getvalue()
        self.assertNotIn("\x1b", rendered)
        self.assertNotIn("failure\rrewritten", rendered)
        self.assertIn("bad�[2J�name", rendered)

    def test_cross_provider_collision_shows_fingerprints(self) -> None:
        output = io.StringIO()
        fake_stdin = Mock()
        fake_stdin.isatty.return_value = True
        with (
            patch("pikamux.ui.sys.stdin", fake_stdin),
            patch("builtins.input", return_value="2"),
            redirect_stdout(output),
        ):
            chosen = choose_session(self.sessions)
        self.assertEqual(chosen.provider, "claude")
        self.assertIn('Both Codex and Claude have "shared"', output.getvalue())
        self.assertIn("id 11111111", output.getvalue())
        self.assertIn("id 22222222", output.getvalue())

    def test_collision_sanitizes_branch(self) -> None:
        self.sessions[0].branch = "feature\x1b[2J\nrewritten"
        output = io.StringIO()
        fake_stdin = Mock()
        fake_stdin.isatty.return_value = True
        with (
            patch("pikamux.ui.sys.stdin", fake_stdin),
            patch("builtins.input", return_value="1"),
            redirect_stdout(output),
        ):
            choose_session(self.sessions)
        rendered = output.getvalue()
        self.assertNotIn("\x1b", rendered)
        self.assertIn("feature�[2J�rewritten", rendered)

    def test_collision_can_be_cancelled_or_closed(self) -> None:
        fake_stdin = Mock()
        fake_stdin.isatty.return_value = True
        for answer in ("q", "\x1b"):
            with (
                self.subTest(answer=answer),
                patch("pikamux.ui.sys.stdin", fake_stdin),
                patch("builtins.input", return_value=answer),
                redirect_stdout(io.StringIO()),
                self.assertRaisesRegex(SelectionCancelled, "nothing was opened"),
            ):
                choose_session(self.sessions)
        with (
            patch("pikamux.ui.sys.stdin", fake_stdin),
            patch("builtins.input", side_effect=EOFError),
            redirect_stdout(io.StringIO()),
            self.assertRaisesRegex(SelectionCancelled, "nothing was opened"),
        ):
            choose_session(self.sessions)


if __name__ == "__main__":
    unittest.main()
