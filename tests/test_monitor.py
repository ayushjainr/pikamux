from __future__ import annotations

import fcntl
import os
import pty
import struct
import termios
import threading
import time
import unittest
from dataclasses import replace
from unittest.mock import Mock, patch

from pikamux.consult import FAST_CODEX_EFFORT, FAST_CODEX_MODEL
from pikamux.client_bridge import ClientLaunchReceipt
from pikamux.experts import ExpertCardState
from pikamux.models import ExpertProfile, Session, Status
from pikamux.monitor import (
    AskMessage,
    HandoffSummary,
    MonitorState,
    _InlineAskWorker,
    _briefing_lines,
    _capture_preview,
    _handle_key,
    _identity_text,
    _morning_handoff,
    build_handoff,
    decode_keys,
    playbook_tip,
    render_monitor,
    run_monitor,
    semantic_age,
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
                last_event_at=now - 40,
                last_activity_at=now - 40,
                live=True,
                home_state="exact-live",
                cpu_percent=2.4,
                rss_kb=220_000,
            ),
            Session(
                "claude",
                "22222222-2222-4222-8222-222222222222",
                name="long-running-analysis",
                cwd="/tmp/project-b",
                status=Status.WORKING.value,
                last_event_at=now - 5,
                last_activity_at=now - 5,
                live=True,
                home_state="exact-live",
            ),
            Session(
                "codex",
                "33333333-3333-4333-8333-333333333333",
                name="finished-result",
                cwd="/tmp/project-c",
                status=Status.READY.value,
                unread=True,
                attention_reason="completed",
                last_event_at=now - 300,
                last_activity_at=now - 300,
            ),
        ]

    def test_wide_frame_has_attention_table_detail_and_controls(self) -> None:
        state = MonitorState(sessions=self.sessions, last_update=time.time() - 1)
        frame = render_monitor(
            state, width=140, height=30, now=time.time(), refreshing=True, color=True
        )
        self.assertIn("PIKA // LIVE OPERATIONS", frame.plain)
        self.assertIn("1 need you", frame.plain)
        self.assertIn("1 results", frame.plain)
        self.assertIn("NEEDS YOU", frame.plain)
        self.assertIn("WORKING", frame.plain)
        self.assertIn("permission", frame.plain)
        self.assertIn("needs-permission", frame.plain)
        self.assertNotIn("TOKENS", frame.plain)
        self.assertIn("EXACT HOME", frame.plain)
        self.assertIn("LIVE PANE TAIL", frame.plain)
        self.assertIn("Enter open", frame.plain)
        self.assertIn("a ask here", frame.plain)
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
        self.assertIn("pika ", frame.plain)
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
        self.assertIn("pika NAME", empty.plain)

        minimum = render_monitor(
            MonitorState(sessions=self.sessions, last_update=time.time()),
            width=58,
            height=15,
            color=False,
        )
        self.assertIn("NEED 1", minimum.plain)
        self.assertIn("? keys", minimum.plain)
        self.assertIn("q quit", minimum.plain)

        initial = render_monitor(
            MonitorState(), width=90, height=20, refreshing=True, color=False
        )
        self.assertIn("INITIAL HANDOFF", initial.plain)
        self.assertNotIn("YOU'RE CLEAR", initial.plain)

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
        buffer = bytearray(b"jua\x1b[A\x1b[<65;10;5M\r?")
        self.assertEqual(
            decode_keys(buffer),
            ["down", "usage", "ask", "up", "down", "enter", "help"],
        )
        self.assertEqual(buffer, bytearray())
        self.assertEqual(decode_keys(bytearray(b"A")), ["ask-fast"])

        text_buffer = bytearray("a quick?\nπ".encode() + b"\x7f")
        self.assertEqual(
            decode_keys(text_buffer, text_mode=True),
            ["a", " ", "q", "u", "i", "c", "k", "?", "newline", "π", "backspace"],
        )

    def test_fragmented_and_alternate_arrow_sequences_are_decoded(self) -> None:
        fragmented = bytearray(b"\x1b")
        self.assertEqual(decode_keys(fragmented, flush_escape=False), [])
        self.assertEqual(fragmented, bytearray(b"\x1b"))
        fragmented.extend(b"[")
        self.assertEqual(decode_keys(fragmented, flush_escape=False), [])
        fragmented.extend(b"A")
        self.assertEqual(decode_keys(fragmented, flush_escape=False), ["up"])
        self.assertEqual(fragmented, bytearray())

        self.assertEqual(decode_keys(bytearray(b"\x1bOB")), ["down"])
        self.assertEqual(decode_keys(bytearray(b"\x1b[1;2A")), ["up"])
        self.assertEqual(decode_keys(bytearray(b"\x1b[C")), ["right"])

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
        self.assertEqual(action, "enter")
        self.assertEqual(selected, unbound)

        outside = replace(
            unbound,
            name="sample plugin",
            root_pid=2997494,
            home_state="outside-live",
        )
        state = MonitorState(sessions=[outside])
        action, selected = _handle_key("enter", Mock(), state)
        self.assertEqual((action, selected), ("enter", outside))
        self.assertIn("RECOVERY OPTIONS", _identity_text(outside))
        self.assertIn("TERMINAL FOR PID 2997494, /exit", _identity_text(outside))

        identity_error = Session(
            "codex",
            "identity-error",
            name="blocked",
            status=Status.ERROR.value,
            home_state="identity-error",
        )
        state = MonitorState(sessions=[identity_error])
        action, selected = _handle_key("enter", Mock(), state)
        self.assertEqual(action, "open")
        self.assertEqual(selected, identity_error)
        # Core open revalidates identity; a stale board error must not prevent
        # recovery, nor can the board itself bypass the authoritative check.

    def test_terminal_sequence_stripping_protects_peek_surface(self) -> None:
        value = "safe\x1b[2Jrewritten\x1b]0;title\x07"
        self.assertEqual(strip_terminal_sequences(value), "saferewritten")

    def test_playbook_rotates_exactly_every_five_minutes(self) -> None:
        first = playbook_tip(0.0, self.sessions)
        same = playbook_tip(299.9, self.sessions)
        following = playbook_tip(300.0, self.sessions)
        self.assertEqual(first, same)
        self.assertNotEqual(following, first)

    def test_briefing_buckets_are_disjoint_and_quiet_is_earned(self) -> None:
        headline, next_line = _briefing_lines(
            self.sessions,
            width=120,
            handoff=None,
            initialized=True,
            refresh_error=None,
        )
        self.assertIn("1 PLACE", headline)
        self.assertIn("1 RESULT", headline)
        self.assertNotIn("RESULTs", headline)
        self.assertIn("needs-permission", next_line)

        quiet = [
            Session(
                "codex", "a", status=Status.WORKING.value, home_state="exact-live"
            ),
            Session("claude", "b", status=Status.PARKED.value),
        ]
        headline, _ = _briefing_lines(
            quiet,
            width=120,
            handoff=None,
            initialized=True,
            refresh_error=None,
        )
        self.assertIn("YOU'RE CLEAR", headline)
        self.assertIn("1 exact live", headline)

        quiet.append(
            Session("codex", "c", status=Status.ERROR.value, unread=True)
        )
        headline, _ = _briefing_lines(
            quiet,
            width=120,
            handoff=None,
            initialized=True,
            refresh_error=None,
        )
        self.assertNotIn("YOU'RE CLEAR", headline)

        quiet[-1].unread = False
        headline, _ = _briefing_lines(
            quiet,
            width=120,
            handoff=None,
            initialized=True,
            refresh_error=None,
        )
        self.assertIn("YOU'RE CLEAR", headline)

    def test_identity_copy_never_infers_exactness_from_live_or_tags(self) -> None:
        live = Session(
            "codex",
            "11111111-rest",
            status=Status.WORKING.value,
            live=True,
            tmux_pane="%1",
            home_state="outside-live",
        )
        self.assertIn("NOT PROTECTED", _identity_text(live))
        self.assertNotIn("EXACT", _identity_text(live))
        live.home_state = "exact-live"
        self.assertIn("EXACT HOME", _identity_text(live))

        error = Session(
            "codex",
            "22222222-rest",
            status=Status.ERROR.value,
            unread=True,
            attention_reason="identity",
            tmux_pane="%2",
            home_state="identity-error",
        )
        self.assertIn("IDENTITY UNVERIFIED", _identity_text(error))
        self.assertIn("PROTECTED PAUSE", _identity_text(error))
        protected = render_monitor(
            MonitorState(sessions=[error], last_update=time.time()),
            width=100,
            height=20,
            color=False,
        )
        self.assertIn("PROTECTED", protected.plain)
        self.assertNotIn(" ERROR ", protected.plain)

        states = {
            "unbound": "UNBOUND PROCESS",
            "saved-idle": "SAVED HOME",
            "no-live-home": "NO LIVE HOME",
            "unknown": "HOME STATE UNKNOWN",
        }
        for home_state, expected in states.items():
            with self.subTest(home_state=home_state):
                item = Session("codex", "33333333-rest", home_state=home_state)
                self.assertIn(expected, _identity_text(item))

    def test_playbook_prioritizes_selected_exception_and_compact_action(self) -> None:
        unbound = Session(
            "claude", "unbound:%9", status=Status.UNBOUND.value, live=True
        )
        _index, _total, tip = playbook_tip(
            0.0, [*self.sessions, unbound], selected=unbound, compact=True
        )
        self.assertIn("pika NAME", tip)
        self.assertLessEqual(len(f"PIKA TIP 1/2 // {tip}"), 58)

        failed = Session("codex", "failed", status=Status.ERROR.value)
        _index, _total, tip = playbook_tip(
            0.0, [failed], selected=failed, compact=True
        )
        self.assertIn("pika doctor", tip)

    def test_calm_refresh_and_partial_states_are_truthful(self) -> None:
        now = time.time()
        state = MonitorState(
            sessions=self.sessions,
            last_update=now - 1,
            refresh_started_at=now - 0.1,
        )
        fast = render_monitor(
            state, width=100, height=24, now=now, refreshing=True, color=False
        )
        self.assertIn("UPDATED 1s AGO", fast.plain.splitlines()[0])
        self.assertNotIn("SYNCING", fast.plain.splitlines()[0])

        state.emphasize_refresh = True
        manual = render_monitor(
            state, width=100, height=24, now=now, refreshing=True, color=False
        )
        self.assertIn("SYNCING", manual.plain.splitlines()[0])

        state.emphasize_refresh = False
        state.refresh_started_at = now - 2
        slow = render_monitor(
            state, width=100, height=24, now=now, refreshing=True, color=False
        )
        self.assertIn("SYNCING", slow.plain.splitlines()[0])

        state.refresh_started_at = now - 0.1
        state.refresh_warning = "claude unavailable"
        partial = render_monitor(
            state, width=100, height=24, now=now, color=False
        )
        self.assertIn("PARTIAL", partial.plain.splitlines()[0])
        self.assertNotIn("SYNCED", partial.plain.splitlines()[0])

    def test_semantic_time_uses_event_for_attention_and_activity_elsewhere(
        self,
    ) -> None:
        now = 10_000.0
        waiting = Session(
            "codex",
            "wait",
            status=Status.NEEDS_YOU.value,
            last_event_at=now - 300,
            last_activity_at=now - 1,
        )
        working = Session(
            "codex",
            "work",
            status=Status.WORKING.value,
            last_event_at=now - 300,
            last_activity_at=now - 10,
        )
        self.assertEqual(semantic_age(waiting, now), "WAIT 5m")
        self.assertEqual(semantic_age(working, now), "ACTIVE 10s")

    def test_usage_view_is_explicit_and_operation_view_hides_usage(self) -> None:
        state = MonitorState(sessions=self.sessions, last_update=time.time())
        normal = render_monitor(state, width=140, height=24, color=False)
        self.assertNotIn("API-EQUIV", normal.plain)
        action, _ = _handle_key("usage", Mock(), state)
        self.assertEqual(action, "usage")
        self.assertTrue(state.show_usage)
        usage = render_monitor(state, width=140, height=24, color=False)
        self.assertIn("LIVE OPERATIONS · USAGE", usage.plain)
        self.assertIn("PROVIDER COUNTERS", usage.plain)
        self.assertIn("API-EQUIV", usage.plain)
        self.assertIn("2026-08-12", usage.plain)
        self.assertTrue(
            all(len(line) == 140 for line in usage.plain.splitlines())
        )

        state.mode = "help"
        minimum = render_monitor(state, width=58, height=15, color=False)
        self.assertIn("u usage · r refresh", minimum.plain)
        self.assertIn("q/Esc close", minimum.plain)
        self.assertIn("q close", minimum.plain.splitlines()[-1])

    def test_stop_watching_confirmation_is_explicit_and_recoverable(self) -> None:
        state = MonitorState(sessions=self.sessions, last_update=time.time())
        selected = state.selected()
        action, target = _handle_key("untrack", Mock(), state)
        self.assertEqual((action, target), ("continue", None))
        self.assertEqual(state.mode, "untrack")

        wide = render_monitor(state, width=140, height=30, color=False)
        self.assertIn("STOP WATCHING", wide.plain)
        self.assertIn("running agent stays running", wide.plain)
        self.assertIn("expert lookup", wide.plain)
        self.assertIn("not archived", wide.plain)
        self.assertIn("x / Enter", wide.plain)

        action, target = _handle_key("escape", Mock(), state)
        self.assertEqual((action, target), ("continue", None))
        self.assertEqual(state.mode, "sessions")

        _handle_key("untrack", Mock(), state)
        narrow = render_monitor(state, width=72, height=20, color=False)
        self.assertIn("STOP WATCHING", narrow.plain)
        action, target = _handle_key("enter", Mock(), state)
        self.assertEqual(action, "untrack")
        self.assertEqual(
            target.key if target else None,
            selected.key if selected else None,
        )
        self.assertEqual(state.mode, "sessions")

    def test_x_decodes_to_stop_watching_action(self) -> None:
        self.assertEqual(decode_keys(bytearray(b"x")), ["untrack"])

    def test_fast_side_ask_keeps_the_selected_policy_in_monitor_state(self) -> None:
        session = replace(self.sessions[0], transcript_path="/tmp/codex.jsonl")
        state = MonitorState(sessions=[session])
        action, target = _handle_key("ask-fast", Mock(), state)
        self.assertEqual(action, "ask-open")
        self.assertIsNotNone(target)
        self.assertIsNotNone(state.ask_policy)
        self.assertEqual(state.ask_policy.model, FAST_CODEX_MODEL)
        self.assertEqual(state.ask_policy.effort, FAST_CODEX_EFFORT)
        opening = render_monitor(state, width=140, height=30, color=False)
        self.assertIn("SIDE REQUEST", opening.plain)
        self.assertIn("awaiting confirmation", opening.plain)
        self.assertNotIn("transcript unchanged", opening.plain)
        state.ask_confirmed = True
        state.ask_status = "ready"
        confirmed = render_monitor(state, width=140, height=30, color=False)
        self.assertIn("SIDE RECEIPT // EPHEMERAL", confirmed.plain)
        self.assertIn(FAST_CODEX_MODEL, confirmed.plain)
        self.assertNotIn("gpt-5.6-sol", confirmed.plain)

    def test_fast_side_ask_fails_closed_for_claude(self) -> None:
        session = Session(
            "claude",
            "22222222-2222-4222-8222-222222222222",
            name="claude-expert",
            transcript_path="/tmp/claude.jsonl",
        )
        state = MonitorState(sessions=[session])
        action, target = _handle_key("ask-fast", Mock(), state)
        self.assertEqual((action, target), ("continue", None))
        self.assertEqual(state.mode, "sessions")
        self.assertIn("not benchmarked for Claude", state.toast or "")

    def test_placeholder_cannot_offer_weak_stop_watching_guarantee(self) -> None:
        placeholder = Session(
            "codex", "unbound:%9", name="unknown", status=Status.UNBOUND.value
        )
        state = MonitorState(sessions=[placeholder])
        action, target = _handle_key("untrack", Mock(), state)
        self.assertEqual((action, target), ("continue", None))
        self.assertEqual(state.mode, "sessions")
        self.assertIn("exact UUID", state.toast or "")

    def test_morning_handoff_has_first_and_long_gap_semantics(self) -> None:
        now = time.time()
        first = HandoffSummary(now, 2, 1, 0, first=True)
        frame = render_monitor(
            MonitorState(
                sessions=self.sessions,
                last_update=now,
                handoff=first,
                handoff_until=time.monotonic() + 10,
            ),
            width=120,
            height=24,
            now=now,
            color=False,
        )
        self.assertIn("FIRST HANDOFF // CURRENT STATE", frame.plain)
        self.assertTrue(self.sessions[0].unread)

        self.assertIsNone(build_handoff(self.sessions, since=now - 60, now=now))
        old = build_handoff(self.sessions, since=now - 8 * 3600, now=now)
        self.assertIsNotNone(old)

    def test_morning_handoff_uses_atomic_visit_cutoff_and_event_ledger(self) -> None:
        now = time.time()
        store = Mock()
        store.claim_monitor_handoff.return_value = (
            now - 8 * 3600,
            {
                Status.READY.value: 3,
                Status.NEEDS_YOU.value: 2,
                Status.ERROR.value: 1,
            },
        )
        pika = Mock(store=store)
        summary = _morning_handoff(pika, self.sessions, now=now)
        self.assertEqual(
            (summary.finished, summary.decisions, summary.errors),
            (3, 2, 1),
        )
        store.claim_monitor_handoff.assert_called_once_with(now)

        store.claim_monitor_handoff.return_value = (now - 60, {})
        self.assertIsNone(_morning_handoff(pika, self.sessions, now=now))

    def test_runtime_collects_usage_only_while_usage_view_is_enabled(self) -> None:
        usage_seen = threading.Event()
        usage_release = threading.Event()

        class FakeStore:
            def __init__(self):
                self.meta = {}

            def claim_monitor_visit(self, timestamp):
                previous = self.meta.get("monitor:last_seen_at")
                self.meta["monitor:last_seen_at"] = str(timestamp)
                return float(previous) if previous else None

            def claim_monitor_handoff(self, timestamp):
                return self.claim_monitor_visit(timestamp), {}

            def set_meta(self, key, value):
                self.meta[key] = value

            def attention_event_counts(self, **_kwargs):
                return {}

        class FakePika:
            def __init__(inner_self):
                inner_self.store = FakeStore()
                inner_self.tmux = Mock()
                inner_self.discovery_errors = []
                inner_self.usage_errors = []
                inner_self.refresh_usage_flags = []
                inner_self.usage_calls = 0

            def refresh(inner_self, *, usage=False):
                inner_self.refresh_usage_flags.append(usage)
                return [Session("codex", "runtime", status=Status.WORKING.value)]

            def hydrate_usage(inner_self, sessions):
                inner_self.usage_calls += 1
                usage_seen.set()
                usage_release.wait(1.0)
                sessions[0].total_tokens = 10
                return sessions

            def next_attention(inner_self, _sessions=None):
                return None

            def acknowledge(inner_self, _session, *, attaching=False):
                return False

            def open(inner_self, _session, *, attach=True):
                return 0

        pika = FakePika()
        master, slave = pty.openpty()
        fcntl.ioctl(
            slave,
            termios.TIOCSWINSZ,
            struct.pack("HHHH", 24, 100, 0, 0),
        )
        result = []
        thread = threading.Thread(
            target=lambda: result.append(
                run_monitor(
                    pika,
                    input_fd=slave,
                    output_fd=slave,
                    refresh_seconds=0.02,
                )
            )
        )
        calls_after_hiding = 0
        try:
            with patch("pikamux.monitor.USAGE_REFRESH_SECONDS", 0.03):
                thread.start()
                time.sleep(0.08)
                os.write(master, b"u")
                self.assertTrue(usage_seen.wait(1.0))
                operational_calls = len(pika.refresh_usage_flags)
                time.sleep(0.08)
                self.assertGreater(len(pika.refresh_usage_flags), operational_calls)
                os.write(master, b"u")
                calls_after_hiding = pika.usage_calls
                time.sleep(0.12)
                self.assertEqual(pika.usage_calls, calls_after_hiding)
                os.write(master, b"q")
                thread.join(0.5)
                self.assertFalse(thread.is_alive())
        finally:
            usage_release.set()
            if thread.is_alive():
                os.write(master, b"q")
                thread.join(2.0)
            os.close(master)
            os.close(slave)
        self.assertFalse(thread.is_alive())
        self.assertEqual(result, [0])
        self.assertTrue(pika.refresh_usage_flags)
        self.assertTrue(all(flag is False for flag in pika.refresh_usage_flags))
        self.assertEqual(pika.usage_calls, calls_after_hiding)

    def test_runtime_keeps_multi_turn_ephemeral_ask_inside_monitor(self) -> None:
        session = Session(
            "codex",
            "ask-parent",
            name="expert",
            transcript_path="/tmp/expert.jsonl",
            status=Status.PARKED.value,
        )

        class FakeStore:
            def claim_monitor_handoff(self, _timestamp):
                return None, {}

        class FakePika:
            store = FakeStore()
            discovery_errors: list[str] = []

            def refresh(self, *, usage=False):
                return [session]

            def next_attention(self, _sessions=None):
                return None

            def open(self, _session, *, attach=True):
                return 0

        questions: list[str] = []
        first_seen = threading.Event()
        second_seen = threading.Event()

        class FakeConsultation:
            def __init__(self):
                self.closed = threading.Event()

            def ask(self, question):
                questions.append(question)
                (first_seen if len(questions) == 1 else second_seen).set()
                return f"answer to {question}"

            def close(self):
                self.closed.set()

        consultation = FakeConsultation()
        master, slave = pty.openpty()
        os.set_blocking(master, False)
        fcntl.ioctl(
            slave,
            termios.TIOCSWINSZ,
            struct.pack("HHHH", 24, 120, 0, 0),
        )
        stop_drain = threading.Event()

        def drain_output():
            while not stop_drain.is_set():
                try:
                    os.read(master, 65_536)
                except BlockingIOError:
                    time.sleep(0.01)
                except OSError:
                    return

        drainer = threading.Thread(target=drain_output, daemon=True)
        drainer.start()
        result: list[int] = []
        thread = threading.Thread(
            target=lambda: result.append(
                run_monitor(
                    FakePika(),
                    input_fd=slave,
                    output_fd=slave,
                    refresh_seconds=0.02,
                )
            )
        )
        try:
            with patch(
                "pikamux.monitor.consultation_for", return_value=consultation
            ):
                thread.start()
                time.sleep(0.08)
                os.write(master, b"a")
                time.sleep(0.08)
                self.assertTrue(thread.is_alive())
                os.write(master, b"first\r")
                self.assertTrue(first_seen.wait(1.0))
                time.sleep(0.08)
                os.write(master, b"second\r")
                self.assertTrue(second_seen.wait(1.0))
                time.sleep(0.08)
                self.assertTrue(thread.is_alive())
                os.write(master, b"\x1b")
                time.sleep(0.08)
                self.assertTrue(thread.is_alive())
                os.write(master, b"q")
                thread.join(1.0)
        finally:
            if thread.is_alive():
                os.write(master, b"\x1bq")
                thread.join(1.0)
            stop_drain.set()
            drainer.join(0.2)
            os.close(master)
            os.close(slave)
        self.assertFalse(thread.is_alive())
        self.assertEqual(result, [0])
        self.assertEqual(questions, ["first", "second"])
        self.assertTrue(consultation.closed.wait(1.0))

    def test_runtime_does_not_quit_on_fragmented_arrow_key(self) -> None:
        sessions = [
            Session("codex", "first", name="first", status=Status.WORKING.value),
            Session("codex", "second", name="second", status=Status.PARKED.value),
        ]

        class FakeStore:
            def claim_monitor_handoff(self, _timestamp):
                return None, {}

        class FakePika:
            store = FakeStore()
            discovery_errors: list[str] = []

            def refresh(self, *, usage=False):
                return sessions

            def next_attention(self, _sessions=None):
                return None

            def open(self, _session, *, attach=True):
                return 0

        master, slave = pty.openpty()
        fcntl.ioctl(
            slave,
            termios.TIOCSWINSZ,
            struct.pack("HHHH", 24, 100, 0, 0),
        )
        result: list[int] = []
        thread = threading.Thread(
            target=lambda: result.append(
                run_monitor(
                    FakePika(),
                    input_fd=slave,
                    output_fd=slave,
                    refresh_seconds=0.02,
                )
            )
        )
        try:
            with patch("pikamux.monitor.KEY_SEQUENCE_TIMEOUT_SECONDS", 0.2):
                thread.start()
                time.sleep(0.08)
                os.write(master, b"\x1b")
                time.sleep(0.05)
                self.assertTrue(thread.is_alive())
                os.write(master, b"[B")
                time.sleep(0.08)
                self.assertTrue(thread.is_alive())
                os.write(master, b"q")
                thread.join(1.0)
        finally:
            if thread.is_alive():
                os.write(master, b"q")
                thread.join(1.0)
            os.close(master)
            os.close(slave)
        self.assertFalse(thread.is_alive())
        self.assertEqual(result, [0])

    def test_runtime_client_window_receipt_keeps_monitor_open(self) -> None:
        session_id = "99999999-9999-4999-8999-999999999999"
        session = Session(
            "codex",
            session_id,
            name="remote-window",
            status=Status.WORKING.value,
            home_state="exact-live",
        )
        launched = threading.Event()

        class FakeStore:
            def claim_monitor_handoff(self, _timestamp):
                return None, {}

        class FakePika:
            store = FakeStore()
            discovery_errors: list[str] = []

            def refresh(self, *, usage=False):
                return [session]

            def next_attention(self, _sessions=None):
                return None

            def open_on_client(self, selected):
                self.assert_identity = selected.session_id
                launched.set()
                return ClientLaunchReceipt(
                    "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
                    "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
                    "codex",
                    session_id,
                    "WINDOW LAUNCHED · devbox · id 99999999",
                )

            def open(self, _session, *, attach=True):
                raise AssertionError("current-terminal fallback must not run")

        pika = FakePika()
        master, slave = pty.openpty()
        fcntl.ioctl(
            slave,
            termios.TIOCSWINSZ,
            struct.pack("HHHH", 24, 100, 0, 0),
        )
        result: list[int] = []
        thread = threading.Thread(
            target=lambda: result.append(
                run_monitor(
                    pika,
                    input_fd=slave,
                    output_fd=slave,
                    refresh_seconds=0.02,
                )
            )
        )
        try:
            thread.start()
            time.sleep(0.08)
            os.write(master, b"\r")
            self.assertTrue(launched.wait(1.0))
            time.sleep(0.08)
            self.assertTrue(thread.is_alive())
            os.write(master, b"q")
            thread.join(1.0)
        finally:
            if thread.is_alive():
                os.write(master, b"q")
                thread.join(1.0)
            os.close(master)
            os.close(slave)
        self.assertEqual(result, [0])
        self.assertEqual(pika.assert_identity, session_id)

    def test_tiny_viewport_never_writes_past_real_dimensions(self) -> None:
        frame = render_monitor(
            MonitorState(), width=10, height=6, now=1.0, color=False
        )
        lines = frame.plain.splitlines()
        self.assertEqual(len(lines), 6)
        self.assertTrue(all(len(line) == 10 for line in lines))
        self.assertNotIn("20x6", frame.plain)

    def test_split_detail_surfaces_card_freshness_and_ephemeral_ask(self) -> None:
        session = self.sessions[0]
        session.transcript_path = "/tmp/provider-thread.jsonl"
        profile = ExpertProfile(
            session.provider,
            session.session_id,
            "Verified the production returns reconciliation workflow.",
            ("daily returns", "trade reconciliation", "PostgreSQL"),
            ("ops/returns.md",),
            100.0,
            "interview",
            current_state="Reconciling three late source identifiers.",
        )
        card = ExpertCardState(session, profile, "STALE", "conversation changed")
        state = MonitorState(
            sessions=self.sessions,
            last_update=time.time(),
            expert_cards={session.key: card},
            expert_cards_updated_at=time.time(),
        )
        frame = render_monitor(state, width=140, height=30, color=False)
        self.assertIn("1 expert thread", frame.plain.splitlines()[0])
        self.assertIn("THREAD EXPERTISE", frame.plain)
        self.assertIn("INTERVIEWED", frame.plain)
        self.assertIn("PROFILE STALE", frame.plain)
        self.assertIn("trade reconciliation", frame.plain)
        self.assertIn("worked on", frame.plain)
        self.assertIn("Reconciling three late source", frame.plain)
        self.assertIn("[a] ask here", frame.plain)

        legacy = replace(profile, current_state="")
        state.expert_cards[session.key] = ExpertCardState(
            session,
            legacy,
            "STALE",
            "legacy card lacks a current-state snapshot",
        )
        legacy_frame = render_monitor(state, width=140, height=30, color=False)
        self.assertIn("NEEDS REFRESH", legacy_frame.plain)
        self.assertIn("now      not captured", legacy_frame.plain)
        state.expert_cards[session.key] = card

        action, selected = _handle_key("ask", Mock(), state)
        self.assertEqual(action, "ask-open")
        self.assertIs(selected, session)
        self.assertEqual(state.mode, "ask")

        state.ask_status = "ready"
        state.ask_confirmed = True
        state.ask_messages = [
            AskMessage("user", "Why did three trades mismatch?"),
            AskMessage("expert", "Their source identifiers arrived late."),
        ]
        state.ask_input = "Were the returns affected?"
        side = render_monitor(state, width=140, height=30, color=False)
        self.assertIn("LIVE OPERATIONS · SIDE", side.plain)
        self.assertIn("SIDE RECEIPT // EPHEMERAL", side.plain)
        self.assertIn("gpt-5.6-sol · medium", side.plain)
        self.assertIn("Why did three trades mismatch?", side.plain)
        self.assertIn("Were the returns affected?█", side.plain)
        self.assertIn("Esc close side", side.plain)

        compact = render_monitor(state, width=72, height=20, color=False)
        self.assertIn("SIDE RECEIPT // EPHEMERAL", compact.plain)
        self.assertIn("Enter send", compact.plain)
        self.assertTrue(all(len(line) == 72 for line in compact.plain.splitlines()))

    def test_live_tail_is_read_only_and_preserves_unread(self) -> None:
        session = self.sessions[0]
        pika = Mock()
        pika.capture.return_value = "old\n\x1b[31mnew result\x1b[0m"
        key, lines, error = _capture_preview(pika, session)
        self.assertEqual(key, session.key)
        self.assertEqual(lines[-1], "new result")
        self.assertIsNone(error)
        self.assertTrue(session.unread)
        pika.acknowledge.assert_not_called()
        pika.capture.assert_called_once_with(session, 12)
        pika.tmux.capture.assert_not_called()

    def test_inline_ask_editor_preserves_input_and_has_explicit_recovery(self) -> None:
        session = self.sessions[0]
        session.transcript_path = "/tmp/provider-thread.jsonl"
        state = MonitorState(sessions=[session])
        action, _ = _handle_key("ask", Mock(), state)
        self.assertEqual(action, "ask-open")

        for key in ["w", "h", "y", "newline", "n", "o", "w", "backspace", "?"]:
            _handle_key(key, Mock(), state)
        self.assertEqual(state.ask_input, "why\nno?")
        action, target = _handle_key("enter", Mock(), state)
        self.assertEqual(action, "ask-send")
        self.assertEqual(target.key, session.key)
        self.assertEqual(state.ask_outbox, "why\nno?")
        self.assertTrue(state.ask_pending)
        self.assertEqual(state.ask_messages, [AskMessage("user", "why\nno?")])

        _handle_key("x", Mock(), state)
        self.assertEqual(state.ask_input, "")
        action, _ = _handle_key("escape", Mock(), state)
        self.assertEqual(action, "ask-close")
        self.assertEqual(state.mode, "sessions")

    def test_inline_side_cleanup_failure_is_reported_not_lost_in_thread(self) -> None:
        consultation = Mock()
        consultation.close.side_effect = RuntimeError("provider delete failed")
        worker = _InlineAskWorker(self.sessions[0], Mock())
        worker._consultation = consultation
        worker._close_consultation()
        self.assertEqual(
            worker.poll(),
            [("error", "side cleanup failed: provider delete failed")],
        )


if __name__ == "__main__":
    unittest.main()
