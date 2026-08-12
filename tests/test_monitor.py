from __future__ import annotations

import time
import unittest
from unittest.mock import Mock

from pikamux.models import Session, Status
from pikamux.monitor import (
    MonitorState,
    _handle_key,
    decode_keys,
    playbook_tip,
    render_monitor,
    strip_terminal_sequences,
)


class MonitorTests(unittest.TestCase):
    def setUp(self) -> None:
        now = time.time()
        self.sessions = [
            Session(
                "codex",
                "11111111-1111-4111-8111-111111111111",
                name="needs-permission",
                cwd="/tmp/project-a",
                branch="main",
                tmux_pane="%1",
                status=Status.NEEDS_YOU.value,
                unread=True,
                attention_reason="permission",
                last_activity_at=now - 40,
                live=True,
                cpu_percent=2.4,
                rss_kb=220_000,
            ),
            Session(
                "claude",
                "22222222-2222-4222-8222-222222222222",
                name="long-running-analysis",
                cwd="/tmp/project-b",
                status=Status.WORKING.value,
                last_activity_at=now - 5,
                live=True,
            ),
            Session(
                "codex",
                "33333333-3333-4333-8333-333333333333",
                name="finished-result",
                cwd="/tmp/project-c",
                status=Status.READY.value,
                unread=True,
                attention_reason="completed",
                last_activity_at=now - 300,
            ),
        ]

    def test_wide_frame_has_attention_table_detail_and_controls(self) -> None:
        state = MonitorState(sessions=self.sessions, last_update=time.time() - 1)
        frame = render_monitor(
            state, width=140, height=30, now=time.time(), refreshing=True, color=True
        )
        self.assertIn("PIKA // LIVE OPERATIONS", frame.plain)
        self.assertIn("NEEDS YOU 1", frame.plain)
        self.assertIn("permission", frame.plain)
        self.assertIn("SELECTED // needs-permission", frame.plain)
        self.assertIn("TOKENS", frame.plain)
        self.assertIn("Enter open", frame.plain)
        self.assertIn("PIKA PLAYBOOK", frame.plain)
        lines = frame.plain.splitlines()
        self.assertEqual(len(lines), 30)
        self.assertTrue(all(len(line) == 140 for line in lines))
        ansi_lines = [
            strip_terminal_sequences(line) for line in frame.ansi.splitlines()
        ]
        self.assertEqual(len(ansi_lines), 30)
        self.assertTrue(all(len(line) == 140 for line in ansi_lines))

    def test_narrow_frame_reflows_without_clipping(self) -> None:
        state = MonitorState(sessions=self.sessions, last_update=time.time())
        frame = render_monitor(
            state, width=72, height=20, now=time.time(), color=True
        )
        self.assertIn("PIKA // LIVE OPERATIONS", frame.plain)
        table_header = next(
            line for line in frame.plain.splitlines() if "AG" in line and "NAME" in line
        )
        self.assertNotIn("TOKENS", table_header)
        self.assertIn("needs-permission", frame.plain)
        self.assertEqual(len(frame.plain.splitlines()), 20)
        self.assertTrue(all(len(line) == 72 for line in frame.plain.splitlines()))
        ansi_lines = [
            strip_terminal_sequences(line) for line in frame.ansi.splitlines()
        ]
        self.assertTrue(all(len(line) == 72 for line in ansi_lines))

    def test_too_small_and_empty_states_remain_actionable(self) -> None:
        small = render_monitor(MonitorState(), width=50, height=10, color=False)
        self.assertIn("Terminal too small", small.plain)
        self.assertIn("q quit", small.plain)

        empty = render_monitor(
            MonitorState(last_update=time.time()), width=90, height=20, color=False
        )
        self.assertIn("No managed homes yet", empty.plain)
        self.assertIn("pika new NAME", empty.plain)

        minimum = render_monitor(
            MonitorState(sessions=self.sessions),
            width=58,
            height=15,
            color=False,
        )
        self.assertIn("UNBOUND 0", minimum.plain)
        self.assertIn("q quit", minimum.plain)

    def test_refresh_error_keeps_last_good_data_visible(self) -> None:
        state = MonitorState(
            sessions=self.sessions,
            last_update=time.time() - 5,
            refresh_error="provider database busy",
        )
        frame = render_monitor(state, width=100, height=24, color=False)
        self.assertIn("needs-permission", frame.plain)
        self.assertIn("REFRESH ERROR // provider database busy", frame.plain)

    def test_keyboard_and_mouse_sequences_are_decoded(self) -> None:
        buffer = bytearray(b"j\x1b[A\x1b[<65;10;5M\r?")
        self.assertEqual(
            decode_keys(buffer),
            ["down", "up", "down", "enter", "help"],
        )
        self.assertEqual(buffer, bytearray())

    def test_selection_is_stable_and_next_uses_attention_order(self) -> None:
        state = MonitorState(sessions=self.sessions)
        pika = Mock()
        pika.next_attention.return_value = self.sessions[0]
        self.assertEqual(state.selected().name, "needs-permission")
        _handle_key("down", pika, state)
        selected_after_move = state.selected_key
        state.sessions = list(reversed(self.sessions))
        self.assertEqual(state.selected_key, selected_after_move)
        action, selected = _handle_key("next", pika, state)
        self.assertEqual(action, "open")
        self.assertEqual(selected, self.sessions[0])

    def test_unbound_live_session_fails_safe_in_monitor(self) -> None:
        unbound = Session(
            "claude",
            "44444444-4444-4444-8444-444444444444",
            name="unbound",
            status=Status.UNBOUND.value,
            live=True,
        )
        state = MonitorState(sessions=[unbound])
        action, selected = _handle_key("enter", Mock(), state)
        self.assertEqual(action, "continue")
        self.assertIsNone(selected)
        self.assertIn("adopt", state.toast)

    def test_terminal_sequence_stripping_protects_peek_surface(self) -> None:
        value = "safe\x1b[2Jrewritten\x1b]0;title\x07"
        self.assertEqual(strip_terminal_sequences(value), "saferewritten")

    def test_playbook_rotates_exactly_every_five_minutes(self) -> None:
        first_index, first_tip = playbook_tip(0.0)
        same_index, same_tip = playbook_tip(299.9)
        next_index, next_tip = playbook_tip(300.0)
        self.assertEqual((first_index, first_tip), (same_index, same_tip))
        self.assertNotEqual((next_index, next_tip), (first_index, first_tip))


if __name__ == "__main__":
    unittest.main()
