from __future__ import annotations

import json
import os
import tempfile
import unittest
from pathlib import Path
from typing import ClassVar
from unittest.mock import patch

from pikamux.hooks import handle_hook, handle_process_exit, hook_stdout
from pikamux.models import Candidate, Pane, Session, Status
from pikamux.providers import CodexProvider
from pikamux.store import Store


class FakeTmux:
    tags: ClassVar[list] = []
    alerts: ClassVar[list] = []
    attached: ClassVar[bool] = False
    pika_provider: ClassVar[str | None] = None
    pika_session_id: ClassVar[str | None] = None
    cleared: ClassVar[list[str]] = []

    def get_pane(self, target):
        if not target:
            return None
        return Pane(
            session_name="pika-c-token",
            pane_id=target,
            pane_pid=os.getpid(),
            cwd="/tmp",
            current_command="codex",
            attached=self.attached,
            dead=False,
            dead_status=None,
            activity=1,
            created=1,
            pika_provider=self.pika_provider,
            pika_session_id=self.pika_session_id,
        )

    def tag_pane(self, target, **values):
        self.tags.append((target, values))

    def display_alert(self, message):
        self.alerts.append(message)

    def clear_pika_tags(self, target):
        self.cleared.append(target)


class HookTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.store = Store(Path(self.temp.name) / "pika.db")
        self.store.add_pending(
            "launch", "codex", "thread-name", "/tmp", "pika-c-token", "%9"
        )
        self.env = patch.dict(
            os.environ,
            {
                "PIKA_LAUNCH_TOKEN": "launch",
                "TMUX_PANE": "%9",
                "PIKA_CONFIG_HOME": self.temp.name,
            },
            clear=False,
        )
        self.env.start()
        FakeTmux.tags = []
        FakeTmux.alerts = []
        FakeTmux.attached = False
        FakeTmux.pika_provider = None
        FakeTmux.pika_session_id = None
        FakeTmux.cleared = []

    def tearDown(self) -> None:
        self.env.stop()
        self.temp.cleanup()

    @patch("pikamux.hooks.Tmux", FakeTmux)
    def test_session_start_binds_pending_name_to_exact_uuid(self) -> None:
        result = handle_hook(
            "codex",
            {
                "session_id": "uuid-1",
                "cwd": "/tmp",
                "hook_event_name": "SessionStart",
                "transcript_path": "/tmp/rollout.jsonl",
                "model": "gpt-5.4",
            },
            self.store,
        )
        self.assertIsNone(result)
        session = self.store.get_session("codex", "uuid-1")
        self.assertEqual(session.name if session else None, "thread-name")
        self.assertIsNone(self.store.get_pending("launch"))

    @patch("pikamux.hooks.Tmux", FakeTmux)
    def test_state_transitions(self) -> None:
        base = {"session_id": "uuid-1", "cwd": "/tmp", "transcript_path": "/tmp/x"}
        handle_hook("codex", {**base, "hook_event_name": "SessionStart"}, self.store)
        handle_hook(
            "codex", {**base, "hook_event_name": "UserPromptSubmit"}, self.store
        )
        self.assertEqual(
            self.store.get_session("codex", "uuid-1").status, Status.WORKING.value
        )
        handle_hook(
            "codex", {**base, "hook_event_name": "PermissionRequest"}, self.store
        )
        self.assertEqual(
            self.store.get_session("codex", "uuid-1").status, Status.NEEDS_YOU.value
        )
        self.assertEqual(
            self.store.get_session("codex", "uuid-1").attention_reason,
            "permission",
        )
        handle_hook("codex", {**base, "hook_event_name": "PostToolUse"}, self.store)
        self.assertEqual(
            self.store.get_session("codex", "uuid-1").status, Status.WORKING.value
        )
        handle_hook("codex", {**base, "hook_event_name": "Stop"}, self.store)
        session = self.store.get_session("codex", "uuid-1")
        self.assertEqual(session.status, Status.READY.value)
        self.assertTrue(session.unread)
        self.assertEqual(session.attention_reason, "completed")
        self.assertIn("thread-name (Codex) — completed", FakeTmux.alerts[-1])

    @patch("pikamux.hooks.Tmux", FakeTmux)
    def test_codex_question_tool_waits_for_user_until_answered(self) -> None:
        base = {
            "session_id": "uuid-question",
            "cwd": "/tmp",
            "transcript_path": "/tmp/question.jsonl",
            "tool_name": "request_user_input",
        }
        handle_hook(
            "codex",
            {**base, "hook_event_name": "PreToolUse"},
            self.store,
        )
        waiting = self.store.get_session("codex", "uuid-question")
        self.assertEqual(
            waiting.status if waiting else None,
            Status.NEEDS_YOU.value,
        )
        self.assertTrue(waiting.unread if waiting else False)
        self.assertEqual(
            waiting.attention_reason if waiting else None,
            "question",
        )

        handle_hook(
            "codex",
            {**base, "hook_event_name": "PostToolUse"},
            self.store,
        )
        answered = self.store.get_session("codex", "uuid-question")
        self.assertEqual(
            answered.status if answered else None,
            Status.WORKING.value,
        )
        self.assertFalse(answered.unread if answered else True)

    @patch("pikamux.hooks.Tmux", FakeTmux)
    def test_claude_question_tool_uses_same_attention_contract(self) -> None:
        handle_hook(
            "claude",
            {
                "session_id": "uuid-claude-question",
                "cwd": "/tmp",
                "hook_event_name": "PreToolUse",
                "tool_name": "AskUserQuestion",
            },
            self.store,
        )
        waiting = self.store.get_session("claude", "uuid-claude-question")
        self.assertEqual(
            waiting.status if waiting else None,
            Status.NEEDS_YOU.value,
        )
        self.assertEqual(
            waiting.attention_reason if waiting else None,
            "question",
        )

    @patch("pikamux.hooks.Tmux", FakeTmux)
    def test_claude_question_reason_is_structured_without_transcript_text(self) -> None:
        handle_hook(
            "claude",
            {
                "session_id": "uuid-question",
                "cwd": "/tmp",
                "hook_event_name": "Notification",
                "notification_type": "agent_needs_input",
            },
            self.store,
        )
        session = self.store.get_session("claude", "uuid-question")
        self.assertEqual(session.attention_reason if session else None, "question")
        self.assertIn("(Claude) — question waiting", FakeTmux.alerts[-1])

    @patch("pikamux.hooks.Tmux", FakeTmux)
    def test_duplicate_actionable_event_does_not_ring_twice(self) -> None:
        event = {
            "session_id": "uuid-deduped",
            "cwd": "/tmp",
            "hook_event_name": "Stop",
        }
        handle_hook("codex", event, self.store)
        handle_hook("codex", event, self.store)
        self.assertEqual(len(FakeTmux.alerts), 1)

    @patch("pikamux.hooks.Tmux", FakeTmux)
    def test_distinct_actionable_transition_still_alerts(self) -> None:
        base = {"session_id": "uuid-transition", "cwd": "/tmp"}
        handle_hook(
            "codex", {**base, "hook_event_name": "PermissionRequest"}, self.store
        )
        handle_hook("codex", {**base, "hook_event_name": "Stop"}, self.store)
        self.assertEqual(len(FakeTmux.alerts), 2)
        self.assertIn("permission requested", FakeTmux.alerts[0])
        self.assertIn("completed", FakeTmux.alerts[1])

    @patch("pikamux.hooks.Tmux", FakeTmux)
    def test_session_end_preserves_unread_result_event_time(self) -> None:
        base = {"session_id": "uuid-result-age", "cwd": "/tmp"}
        handle_hook("codex", {**base, "hook_event_name": "Stop"}, self.store)
        before = self.store.get_session("codex", "uuid-result-age")
        assert before is not None
        handle_hook("codex", {**base, "hook_event_name": "SessionEnd"}, self.store)
        after = self.store.get_session("codex", "uuid-result-age")
        assert after is not None
        self.assertEqual(after.status, Status.READY.value)
        self.assertTrue(after.unread)
        self.assertEqual(after.last_event_at, before.last_event_at)
        counts = self.store.attention_event_counts(
            since=before.last_event_at - 1,
            until=after.updated_at + 1,
        )
        self.assertEqual(counts[Status.READY.value], 1)

    @patch("pikamux.hooks.Tmux", FakeTmux)
    def test_claude_session_start_sets_native_title(self) -> None:
        with patch.dict(os.environ, {"PIKA_NAME": "native-title"}, clear=False):
            result = handle_hook(
                "claude",
                {
                    "session_id": "uuid-c",
                    "cwd": "/tmp",
                    "hook_event_name": "SessionStart",
                },
                self.store,
            )
        self.assertEqual(result["hookSpecificOutput"]["sessionTitle"], "native-title")

    @patch("pikamux.hooks.Tmux", FakeTmux)
    def test_hook_replaces_unbound_adoption_without_losing_name(self) -> None:
        self.store.delete_pending("launch")
        self.store.upsert_session(
            Session(
                provider="codex",
                session_id="unbound:%9",
                name="adopted-name",
                cwd="/tmp",
                tmux_pane="%9",
                status=Status.UNBOUND.value,
            )
        )
        with patch.dict(os.environ, {"PIKA_LAUNCH_TOKEN": ""}, clear=False):
            handle_hook(
                "codex",
                {
                    "session_id": "real-uuid",
                    "cwd": "/tmp",
                    "hook_event_name": "PostToolUse",
                },
                self.store,
            )
        bound = self.store.get_session("codex", "real-uuid")
        self.assertEqual(bound.name if bound else None, "adopted-name")
        self.assertIsNone(self.store.get_session("codex", "unbound:%9"))

    def test_codex_noop_output_is_valid_json(self) -> None:
        self.assertEqual(hook_stdout("codex", None), "{}")

    def test_ephemeral_consultation_hook_is_ignored(self) -> None:
        with patch.dict(os.environ, {"PIKA_EPHEMERAL": "1"}, clear=False):
            result = handle_hook(
                "codex",
                {
                    "session_id": "ephemeral-id",
                    "cwd": "/tmp",
                    "hook_event_name": "SessionStart",
                },
                self.store,
            )
        self.assertIsNone(result)
        self.assertIsNone(self.store.get_session("codex", "ephemeral-id"))
        self.assertEqual(self.store.get_live_owners("codex", "ephemeral-id"), [])

    @patch("pikamux.hooks.provider_ancestor", return_value=4321)
    @patch("pikamux.hooks.Tmux", FakeTmux)
    def test_automation_worker_never_becomes_attention_or_owns_inherited_pane(
        self, _owner
    ) -> None:
        worker_id = "11111111-1111-4111-8111-111111111111"
        transcript = Path(self.temp.name) / "worker.jsonl"
        transcript.write_text(
            json.dumps(
                {
                    "type": "session_meta",
                    "payload": {"id": worker_id, "originator": "agentic_fund"},
                }
            )
            + "\n"
        )
        self.store.delete_pending("launch")
        self.store.upsert_session(
            Session("codex", "parent-id", name="learning-study-v3")
        )
        self.store.upsert_session(
            Session(
                "codex",
                worker_id,
                name="codex-01a00072",
                status=Status.READY.value,
                unread=True,
                attention_reason="completed",
            )
        )
        with patch("pikamux.store.process_start_time", return_value=12345):
            self.store.set_live_owner("codex", worker_id, 9999)
        FakeTmux.pika_provider = "codex"
        FakeTmux.pika_session_id = worker_id
        with patch.dict(
            os.environ, {"PIKA_LAUNCH_TOKEN": "", "TMUX_PANE": "%9"}, clear=False
        ):
            result = handle_hook(
                "codex",
                {
                    "session_id": worker_id,
                    "session_title": "codex-01a00072",
                    "cwd": "/tmp",
                    "hook_event_name": "Stop",
                    "transcript_path": str(transcript),
                },
                self.store,
            )
        self.assertIsNone(result)
        self.assertIsNone(self.store.get_session("codex", worker_id))
        self.assertIsNotNone(self.store.get_session("codex", "parent-id"))
        self.assertEqual(self.store.get_live_owners("codex", worker_id), [])
        self.assertEqual(FakeTmux.tags, [])
        self.assertEqual(FakeTmux.cleared, ["%9"])
        self.assertEqual(FakeTmux.alerts, [])

    @patch("pikamux.hooks.Tmux", FakeTmux)
    def test_automation_worker_restores_unique_parent_pane_tag(self) -> None:
        worker_id = "11111111-1111-4111-8111-111111111111"
        parent_id = "22222222-2222-4222-8222-222222222222"
        self.store.delete_pending("launch")
        self.store.upsert_session(
            Session(
                "codex",
                parent_id,
                name="parent-run",
                tmux_pane="%9",
                tmux_session="pika-parent",
            )
        )
        FakeTmux.pika_provider = "codex"
        FakeTmux.pika_session_id = worker_id
        with patch.dict(
            os.environ, {"PIKA_LAUNCH_TOKEN": "", "TMUX_PANE": "%9"}, clear=False
        ):
            handle_hook(
                "codex",
                {
                    "session_id": worker_id,
                    "originator": "agentic_fund",
                    "hook_event_name": "Stop",
                },
                self.store,
            )
        self.assertEqual(
            FakeTmux.tags,
            [
                (
                    "%9",
                    {
                        "provider": "codex",
                        "session_id": parent_id,
                        "name": "parent-run",
                        "launch_token": "",
                    },
                )
            ],
        )
        self.assertEqual(FakeTmux.cleared, [])

    @patch("pikamux.hooks.Tmux", FakeTmux)
    def test_interactive_same_name_still_gets_tracked(self) -> None:
        session_id = "22222222-2222-4222-8222-222222222222"
        transcript = Path(self.temp.name) / "human.jsonl"
        transcript.write_text(
            json.dumps(
                {
                    "type": "session_meta",
                    "payload": {"id": session_id, "originator": "codex-tui"},
                }
            )
            + "\n"
        )
        handle_hook(
            "codex",
            {
                "session_id": session_id,
                "session_title": "codex-01a00072",
                "cwd": "/tmp",
                "hook_event_name": "Stop",
                "transcript_path": str(transcript),
            },
            self.store,
        )
        session = self.store.get_session("codex", session_id)
        self.assertEqual(session.status if session else None, Status.READY.value)
        self.assertTrue(session.unread if session else False)

    @patch("pikamux.hooks.Tmux", FakeTmux)
    def test_codex_child_hook_updates_stable_parent_conversation(self) -> None:
        parent_id = "11111111-1111-4111-8111-111111111111"
        child_id = "22222222-2222-4222-8222-222222222222"
        transcript = Path(self.temp.name) / f"{child_id}.jsonl"
        transcript.write_text(
            json.dumps(
                {
                    "type": "session_meta",
                    "payload": {"id": child_id, "forked_from_id": parent_id},
                }
            )
            + "\n"
        )
        self.store.upsert_session(
            Session(
                "codex",
                parent_id,
                name="master_quant",
                cwd="/tmp",
                tmux_session="pika-parent",
                tmux_pane="%1",
                status=Status.READY.value,
            )
        )
        candidate = Candidate(
            "codex",
            child_id,
            "master_quant",
            cwd="/tmp",
            transcript_path=str(transcript),
            parent_session_id=parent_id,
            lifecycle_status=Status.WORKING.value,
        )
        with patch.object(CodexProvider, "thread_candidate", return_value=candidate):
            handle_hook(
                "codex",
                {
                    "session_id": parent_id,
                    "cwd": "/tmp",
                    "hook_event_name": "UserPromptSubmit",
                    "transcript_path": str(transcript),
                },
                self.store,
            )

        current = self.store.get_session("codex", parent_id)
        self.assertEqual(current.status if current else None, Status.WORKING.value)
        self.assertEqual(current.active_thread_id if current else None, child_id)
        self.assertIsNone(self.store.get_session("codex", child_id))

    @patch("pikamux.hooks.Tmux", FakeTmux)
    def test_concurrent_codex_child_hook_keeps_one_fail_closed_home(self) -> None:
        parent_id = "11111111-1111-4111-8111-111111111111"
        child_id = "22222222-2222-4222-8222-222222222222"
        transcript = Path(self.temp.name) / f"{child_id}.jsonl"
        transcript.write_text(
            json.dumps(
                {
                    "type": "session_meta",
                    "payload": {"id": child_id, "forked_from_id": parent_id},
                }
            )
            + "\n"
        )
        self.store.upsert_session(
            Session(
                "codex",
                parent_id,
                name="master_quant",
                cwd="/tmp",
                tmux_session="pika-parent",
                tmux_pane="%1",
                status=Status.WORKING.value,
            )
        )
        candidate = Candidate(
            "codex",
            child_id,
            "master_quant",
            cwd="/tmp",
            parent_session_id=parent_id,
            lifecycle_status=Status.WORKING.value,
        )
        with patch.object(CodexProvider, "thread_candidate", return_value=candidate):
            handle_hook(
                "codex",
                {
                    "session_id": parent_id,
                    "cwd": "/tmp",
                    "hook_event_name": "UserPromptSubmit",
                    "transcript_path": str(transcript),
                },
                self.store,
            )

        current = self.store.get_session("codex", parent_id)
        self.assertEqual(current.status if current else None, Status.OPEN_TWICE.value)
        self.assertTrue(current.unread if current else False)
        self.assertIn("multiple active Codex continuation", current.error or "")
        self.assertIsNone(self.store.get_session("codex", child_id))
        self.assertEqual(FakeTmux.tags, [])

    @patch("pikamux.hooks.Tmux", FakeTmux)
    def test_untracked_session_hook_cannot_restore_tracking_or_tags(self) -> None:
        self.store.untrack_session("codex", "ignored-id")
        result = handle_hook(
            "codex",
            {
                "session_id": "ignored-id",
                "cwd": "/tmp",
                "hook_event_name": "Stop",
            },
            self.store,
        )
        self.assertIsNone(result)
        self.assertIsNone(self.store.get_session("codex", "ignored-id"))
        self.assertEqual(self.store.get_live_owners("codex", "ignored-id"), [])
        self.assertEqual(FakeTmux.tags, [])

    def test_untracked_process_exit_does_not_create_hidden_attention(self) -> None:
        self.store.upsert_session(
            Session("codex", "ignored-exit", name="quiet", status=Status.WORKING.value)
        )
        self.store.untrack_session("codex", "ignored-exit")

        handle_process_exit(
            "codex",
            7,
            session_id="ignored-exit",
            store=self.store,
        )

        hidden = self.store.get_session("codex", "ignored-exit")
        self.assertEqual(hidden.status if hidden else None, Status.PARKED.value)
        self.assertFalse(hidden.unread if hidden else True)

    @patch("pikamux.hooks.CodexProvider")
    @patch("pikamux.hooks.Tmux", FakeTmux)
    def test_codex_session_start_sets_native_name(self, provider_class) -> None:
        with patch.dict(os.environ, {"PIKA_NAME": "native-title"}, clear=False):
            handle_hook(
                "codex",
                {
                    "session_id": "uuid-native",
                    "cwd": "/tmp",
                    "hook_event_name": "SessionStart",
                },
                self.store,
            )
        provider_class.return_value.set_native_name.assert_called_once_with(
            "uuid-native", "native-title"
        )

    @patch("pikamux.hooks.Tmux", FakeTmux)
    def test_ready_while_attached_is_not_unread_or_alerted(self) -> None:
        FakeTmux.attached = True
        handle_hook(
            "codex",
            {
                "session_id": "uuid-attached",
                "cwd": "/tmp",
                "hook_event_name": "Stop",
            },
            self.store,
        )
        session = self.store.get_session("codex", "uuid-attached")
        self.assertEqual(session.status if session else None, Status.READY.value)
        self.assertFalse(session.unread if session else True)
        self.assertEqual(FakeTmux.alerts, [])

    @patch("pikamux.hooks.Tmux", FakeTmux)
    def test_visible_permission_stays_unread_but_does_not_ring(self) -> None:
        FakeTmux.attached = True
        handle_hook(
            "codex",
            {
                "session_id": "uuid-visible-permission",
                "cwd": "/tmp",
                "hook_event_name": "PermissionRequest",
            },
            self.store,
        )
        session = self.store.get_session("codex", "uuid-visible-permission")
        self.assertTrue(session.unread if session else False)
        self.assertEqual(FakeTmux.alerts, [])

    @patch("pikamux.hooks.Tmux", FakeTmux)
    def test_first_attach_token_is_replaced_by_exact_identity(self) -> None:
        self.store.set_meta("attached_launch:launch", "1")
        handle_hook(
            "codex",
            {
                "session_id": "uuid-bound",
                "cwd": "/tmp",
                "hook_event_name": "SessionStart",
            },
            self.store,
        )
        self.assertEqual(
            self.store.get_meta("last_attached"), '["codex", "uuid-bound"]'
        )
        self.assertIsNone(self.store.get_meta("attached_launch:launch"))

    @patch("pikamux.hooks.Tmux", FakeTmux)
    def test_process_exit_uses_launch_binding_after_pending_is_deleted(self) -> None:
        handle_hook(
            "codex",
            {
                "session_id": "uuid-exit",
                "cwd": "/tmp",
                "hook_event_name": "SessionStart",
            },
            self.store,
        )
        self.assertIsNone(self.store.get_pending("launch"))
        handle_process_exit("codex", 7, launch_token="launch", store=self.store)
        session = self.store.get_session("codex", "uuid-exit")
        self.assertEqual(session.status if session else None, Status.ERROR.value)
        self.assertIn("status 7", session.error if session else "")
        self.assertEqual(session.attention_reason if session else None, "exited")

    def test_nonzero_exit_overrides_stale_unread_ready_state(self) -> None:
        self.store.upsert_session(
            Session(
                "codex",
                "uuid-crash",
                name="crash",
                status=Status.READY.value,
                unread=True,
            )
        )
        handle_process_exit("codex", 9, session_id="uuid-crash", store=self.store)
        session = self.store.get_session("codex", "uuid-crash")
        self.assertEqual(session.status if session else None, Status.ERROR.value)
        self.assertIn("status 9", session.error if session else "")

    @patch("pikamux.hooks.CodexProvider")
    @patch("pikamux.hooks.Tmux", FakeTmux)
    def test_failed_codex_native_name_is_visible_and_retried(
        self, provider_class
    ) -> None:
        provider_class.return_value.set_native_name.side_effect = [False, True]
        with patch.dict(os.environ, {"PIKA_NAME": "retry-title"}, clear=False):
            handle_hook(
                "codex",
                {
                    "session_id": "uuid-retry",
                    "cwd": "/tmp",
                    "hook_event_name": "SessionStart",
                },
                self.store,
            )
            key = "native_name_error:codex:uuid-retry"
            self.assertEqual(self.store.get_meta(key), "retry-title")
            handle_hook(
                "codex",
                {
                    "session_id": "uuid-retry",
                    "cwd": "/tmp",
                    "hook_event_name": "PostToolUse",
                },
                self.store,
            )
        self.assertIsNone(self.store.get_meta(key))
        self.assertEqual(provider_class.return_value.set_native_name.call_count, 2)

    @patch("pikamux.hooks.provider_ancestor", return_value=4321)
    @patch("pikamux.hooks.Tmux", FakeTmux)
    def test_unnamed_external_hook_still_records_exact_live_owner(self, _owner) -> None:
        self.store.delete_pending("launch")
        with (
            patch.dict(
                os.environ, {"PIKA_LAUNCH_TOKEN": "", "TMUX_PANE": ""}, clear=False
            ),
            patch("pikamux.store.process_start_time", return_value=12345),
        ):
            result = handle_hook(
                "codex",
                {
                    "session_id": "uuid-hidden",
                    "cwd": "/tmp",
                    "hook_event_name": "PostToolUse",
                },
                self.store,
            )
        self.assertIsNone(result)
        self.assertIsNone(self.store.get_session("codex", "uuid-hidden"))
        self.assertEqual(
            self.store.get_live_owners("codex", "uuid-hidden"), [(4321, 12345)]
        )

    @patch("pikamux.hooks.provider_ancestor", return_value=4321)
    @patch("pikamux.hooks.Tmux", FakeTmux)
    def test_session_end_expires_external_live_owner(self, _owner) -> None:
        with patch("pikamux.store.process_start_time", return_value=12345):
            self.store.set_live_owner("codex", "uuid-ended", 4321)
            self.store.set_live_owner("codex", "uuid-ended", 9876)
        with patch.dict(
            os.environ, {"PIKA_LAUNCH_TOKEN": "", "TMUX_PANE": ""}, clear=False
        ):
            handle_hook(
                "codex",
                {
                    "session_id": "uuid-ended",
                    "cwd": "/tmp",
                    "hook_event_name": "SessionEnd",
                },
                self.store,
            )
        self.assertEqual(
            self.store.get_live_owners("codex", "uuid-ended"), [(9876, 12345)]
        )


if __name__ == "__main__":
    unittest.main()
