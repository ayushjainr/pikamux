from __future__ import annotations

import json
import os
import tempfile
import unittest
from pathlib import Path
from typing import ClassVar
from unittest.mock import patch

from pikamux.hooks import _event_state, handle_hook, handle_process_exit, hook_stdout
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


class FailingTagTmux(FakeTmux):
    def tag_pane(self, target, **values):
        raise OSError("tmux tag failed")


class HookTests(unittest.TestCase):
    def test_opencode_attention_events_map_without_transcript_text(self) -> None:
        self.assertEqual(
            _event_state("opencode", {"hook_event_name": "QuestionRequest"}),
            (Status.NEEDS_YOU.value, True, None, "question"),
        )
        self.assertEqual(
            _event_state("opencode", {"hook_event_name": "PermissionRequest"}),
            (Status.NEEDS_YOU.value, True, None, "permission"),
        )
        self.assertEqual(
            _event_state("opencode", {"hook_event_name": "QuestionReply"}),
            (Status.WORKING.value, False, None, None),
        )

    @patch("pikamux.hooks.OpenCodeProvider")
    @patch("pikamux.hooks.Tmux", FakeTmux)
    def test_opencode_deleted_event_removes_exact_inventory_and_ownership(
        self, provider_class
    ) -> None:
        self.store.delete_pending("launch")
        session_id = "ses_deleted123"
        self.store.upsert_session(
            Session("opencode", session_id, name="deleted", cwd="/tmp")
        )
        with patch("pikamux.store.process_start_time", return_value=10):
            self.assertTrue(self.store.set_live_owner("opencode", session_id, 4321))
        provider_class.return_value.worker_originator.return_value = None
        FakeTmux.pika_provider = "opencode"
        FakeTmux.pika_session_id = session_id
        with patch.dict(
            os.environ, {"PIKA_LAUNCH_TOKEN": "", "TMUX_PANE": "%9"}, clear=False
        ):
            handle_hook(
                "opencode",
                {
                    "session_id": session_id,
                    "cwd": "/tmp",
                    "hook_event_name": "SessionEnd",
                    "deleted": True,
                },
                self.store,
            )
        self.assertIsNone(self.store.get_session("opencode", session_id))
        self.assertEqual(self.store.get_live_owners("opencode", session_id), [])
        self.assertEqual(FakeTmux.cleared, ["%9"])

    @patch("pikamux.hooks.OpenCodeProvider")
    @patch("pikamux.hooks.Tmux", FakeTmux)
    def test_opencode_native_name_requires_verified_readback(self, provider_class) -> None:
        self.store.delete_pending("launch")
        provider_class.return_value.worker_originator.return_value = None
        session_id = "ses_name123"
        key = f"native_name_error:opencode:{session_id}"
        with patch.dict(
            os.environ,
            {"PIKA_NAME": "wanted", "PIKA_LAUNCH_TOKEN": "", "TMUX_PANE": ""},
            clear=False,
        ):
            handle_hook(
                "opencode",
                {
                    "session_id": session_id,
                    "cwd": "/tmp",
                    "hook_event_name": "SessionStart",
                    "session_title": "New session - now",
                    "desired_name": "wanted",
                    "native_name_error": "update failed",
                },
                self.store,
            )
            self.assertEqual(self.store.get_meta(key), "wanted")
            self.assertEqual(
                self.store.get_session("opencode", session_id).name,
                "wanted",
            )
            handle_hook(
                "opencode",
                {
                    "session_id": session_id,
                    "cwd": "/tmp",
                    "hook_event_name": "SessionStart",
                    "session_title": "wanted",
                    "desired_name": "wanted",
                    "native_name_error": None,
                },
                self.store,
            )
        self.assertIsNone(self.store.get_meta(key))

    @patch("pikamux.hooks.provider_ancestor", return_value=4321)
    @patch("pikamux.hooks.OpenCodeProvider")
    @patch("pikamux.hooks.Tmux", FakeTmux)
    def test_unmanaged_opencode_placeholder_keeps_lease_without_inventory(
        self, provider_class, _owner
    ) -> None:
        self.store.delete_pending("launch")
        provider_class.return_value.worker_originator.return_value = None
        session_id = "ses_placeholder123"
        with (
            patch.dict(
                os.environ,
                {"PIKA_LAUNCH_TOKEN": "", "PIKA_NAME": "", "TMUX_PANE": ""},
                clear=False,
            ),
            patch("pikamux.store.process_start_time", return_value=12345),
        ):
            handle_hook(
                "opencode",
                {
                    "session_id": session_id,
                    "cwd": "/tmp",
                    "hook_event_name": "SessionStart",
                    "session_title": "New session - 2026-08-26T00:00:00Z",
                    "desired_name": None,
                },
                self.store,
            )

        self.assertIsNone(self.store.get_session("opencode", session_id))
        self.assertEqual(
            self.store.get_live_owners("opencode", session_id),
            [(4321, 12345)],
        )

    @patch("pikamux.hooks.OpenCodeProvider")
    @patch("pikamux.hooks.Tmux", FakeTmux)
    def test_renamed_external_opencode_root_enters_inventory(
        self, provider_class
    ) -> None:
        self.store.delete_pending("launch")
        provider_class.return_value.worker_originator.return_value = None
        session_id = "ses_renamed123"
        with patch.dict(
            os.environ,
            {"PIKA_LAUNCH_TOKEN": "", "PIKA_NAME": "", "TMUX_PANE": ""},
            clear=False,
        ):
            handle_hook(
                "opencode",
                {
                    "session_id": session_id,
                    "cwd": "/tmp",
                    "hook_event_name": "SessionStart",
                    "session_title": "oc_research",
                    "desired_name": None,
                },
                self.store,
            )

        current = self.store.get_session("opencode", session_id)
        self.assertEqual(current.name if current else None, "oc_research")

    @patch("pikamux.hooks.provider_ancestor", return_value=4321)
    @patch("pikamux.hooks.OpenCodeProvider")
    @patch("pikamux.hooks.Tmux", FakeTmux)
    def test_opencode_heartbeat_renews_only_current_root_without_state_change(
        self, provider_class, _owner
    ) -> None:
        self.store.delete_pending("launch")
        current = "ses_current123"
        previous = "ses_previous123"
        self.store.upsert_session(
            Session(
                "opencode",
                current,
                name="current",
                status=Status.READY.value,
                unread=True,
                updated_at=20,
                last_event_at=10,
                last_activity_at=15,
            )
        )
        with patch("pikamux.store.process_start_time", return_value=10):
            self.store.set_live_owner("opencode", current, 4321)
            self.store.set_live_owner("opencode", previous, 4321)
        with self.store.connect() as db:
            db.execute(
                "UPDATE live_owners SET last_seen=1 WHERE provider='opencode'"
            )
        before = self.store.get_session("opencode", current)
        provider_class.return_value.worker_originator.return_value = None
        with (
            patch.dict(
                os.environ,
                {
                    "PIKA_LAUNCH_TOKEN": "",
                    "PIKA_OWNER_TOKEN": "",
                    "PIKA_NAME": "",
                    "TMUX_PANE": "",
                },
                clear=False,
            ),
            patch("pikamux.store.process_start_time", return_value=10),
        ):
            handle_hook(
                "opencode",
                {
                    "session_id": current,
                    "cwd": "/tmp",
                    "hook_event_name": "SessionHeartbeat",
                },
                self.store,
            )

        after = self.store.get_session("opencode", current)
        self.assertEqual(after.status, Status.READY.value)
        self.assertTrue(after.unread)
        self.assertEqual(after.updated_at, before.updated_at)
        self.assertEqual(after.last_event_at, before.last_event_at)
        self.assertEqual(after.last_activity_at, before.last_activity_at)
        self.assertEqual(self.store.get_live_owners("opencode", previous), [])
        leases = self.store.get_live_owner_leases("opencode", current)
        self.assertEqual([(pid, start) for pid, start, *_ in leases], [(4321, 10)])
        self.assertGreater(leases[0][2], 1)

    @patch("pikamux.hooks.provider_ancestor", return_value=4321)
    @patch("pikamux.hooks.OpenCodeProvider")
    @patch("pikamux.hooks.Tmux", FakeTmux)
    def test_managed_opencode_root_switch_moves_exact_home_atomically(
        self, provider_class, _owner
    ) -> None:
        self.store.delete_pending("launch")
        old = "ses_oldroot123"
        new = "ses_newroot123"
        token = "opencode-launch"
        self.store.upsert_session(
            Session("opencode", old, name="old", managed=True, tmux_pane="%9")
        )
        self.store.upsert_session(Session("opencode", new, name="new"))
        self.assertTrue(self.store.bind_launch(token, "opencode", old))
        self.store.set_recovery_owner("opencode", old, 4321, 10, token)
        with patch("pikamux.store.process_start_time", return_value=10):
            self.store.set_live_owner("opencode", old, 4321)
        provider_class.return_value.worker_originator.return_value = None
        FakeTmux.pika_provider = "opencode"
        FakeTmux.pika_session_id = old
        with (
            patch.dict(
                os.environ,
                {
                    "PIKA_LAUNCH_TOKEN": token,
                    "PIKA_OWNER_TOKEN": "owner",
                    "PIKA_NAME": "old",
                    "TMUX_PANE": "%9",
                },
                clear=False,
            ),
            patch("pikamux.store.process_start_time", return_value=10),
            patch("pikamux.hooks.provider_process", return_value=4321),
        ):
            handle_hook(
                "opencode",
                {
                    "session_id": new,
                    "cwd": "/tmp",
                    "hook_event_name": "UserPromptSubmit",
                    "session_title": "new",
                },
                self.store,
            )

        self.assertEqual(self.store.get_launch_binding(token), ("opencode", new))
        self.assertIsNone(self.store.get_recovery_owner("opencode", old))
        self.assertEqual(
            self.store.get_recovery_owner("opencode", new),
            (4321, 10, token),
        )
        self.assertEqual(self.store.get_live_owners("opencode", old), [])
        self.assertEqual(self.store.get_live_owners("opencode", new), [(4321, 10)])
        self.assertTrue(self.store.get_session("opencode", new).managed)
        self.assertEqual(self.store.get_session("opencode", new).name, "new")
        self.assertEqual(FakeTmux.tags[-1][1]["session_id"], new)

    @patch("pikamux.hooks.OpenCodeProvider")
    @patch("pikamux.hooks.Tmux", FakeTmux)
    def test_opencode_automation_root_never_enters_attention(self, provider_class) -> None:
        self.store.delete_pending("launch")
        provider_class.return_value.worker_originator.return_value = "agentic-fund:"
        with patch.dict(
            os.environ, {"PIKA_LAUNCH_TOKEN": "", "TMUX_PANE": ""}, clear=False
        ):
            handle_hook(
                "opencode",
                {
                    "session_id": "ses_worker123",
                    "cwd": "/runs/opencode-runtime/worker",
                    "hook_event_name": "Stop",
                    "session_title": "agentic-fund:calibration",
                },
                self.store,
            )
        self.assertIsNone(self.store.get_session("opencode", "ses_worker123"))
        self.assertEqual(
            self.store.get_live_owners("opencode", "ses_worker123"), []
        )

    @patch("pikamux.hooks.provider_ancestor", return_value=4321)
    @patch("pikamux.hooks.OpenCodeProvider")
    @patch("pikamux.hooks.Tmux", FakeTmux)
    def test_opencode_root_switch_revokes_same_process_old_root(
        self, provider_class, _owner
    ) -> None:
        self.store.delete_pending("launch")
        first = "ses_first123"
        second = "ses_second123"
        self.store.upsert_session(Session("opencode", first, name="first"))
        self.store.upsert_session(Session("opencode", second, name="second"))
        with patch("pikamux.store.process_start_time", return_value=10):
            self.store.set_live_owner("opencode", first, 4321)
        provider_class.return_value.worker_originator.return_value = None
        with (
            patch.dict(
                os.environ, {"PIKA_LAUNCH_TOKEN": "", "TMUX_PANE": ""}, clear=False
            ),
            patch("pikamux.store.process_start_time", return_value=10),
        ):
            handle_hook(
                "opencode",
                {
                    "session_id": second,
                    "cwd": "/tmp",
                    "hook_event_name": "UserPromptSubmit",
                },
                self.store,
            )
        self.assertEqual(self.store.get_live_owners("opencode", first), [])
        self.assertEqual(self.store.get_live_owners("opencode", second), [(4321, 10)])

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
        # A fast SessionStart may bind before the launcher captures the exact
        # provider PID generation. Keep the pending row until that second proof
        # is available instead of publishing an unverified home.
        self.assertIsNotNone(self.store.get_pending("launch"))
        self.assertEqual(
            self.store.get_launch_binding("launch"), ("codex", "uuid-1")
        )
        observation = self.store.get_hook_observation("codex")
        self.assertEqual(observation["event_name"], "SessionStart")
        self.assertEqual(observation["session_id"], "uuid-1")
        self.assertEqual(observation["managed"], 1)

    @patch("pikamux.hooks.Tmux", FailingTagTmux)
    def test_hook_keeps_pending_launch_when_exact_pane_tag_fails(self) -> None:
        event = {
            "session_id": "uuid-tag-retry",
            "cwd": "/tmp",
            "hook_event_name": "SessionStart",
        }
        handle_hook("codex", event, self.store)
        self.assertEqual(
            self.store.get_launch_binding("launch"),
            ("codex", "uuid-tag-retry"),
        )
        self.assertIsNotNone(self.store.get_session("codex", "uuid-tag-retry"))
        self.assertIsNotNone(self.store.get_pending("launch"))
        self.store.finalize_pending_pane(
            "launch", "pika-c-token", "%9", root_pid=777, root_pid_start=99
        )
        with (
            patch("pikamux.hooks.Tmux", FakeTmux),
            patch("pikamux.hooks.provider_process", return_value=777),
            patch("pikamux.hooks.process_start_time", return_value=99),
            patch(
                "pikamux.hooks.process_environment",
                return_value={
                    "PIKA_PROVIDER": "codex",
                    "PIKA_LAUNCH_TOKEN": "launch",
                },
            ),
        ):
            handle_hook("codex", event, self.store)
        self.assertEqual(
            self.store.get_launch_binding("launch"),
            ("codex", "uuid-tag-retry"),
        )
        self.assertIsNone(self.store.get_pending("launch"))
        self.assertEqual(FakeTmux.tags[-1][1]["session_id"], "uuid-tag-retry")

    @patch("pikamux.hooks.Tmux", FakeTmux)
    def test_wrong_provider_hook_cannot_claim_pending_launch(self) -> None:
        handle_hook(
            "claude",
            {
                "session_id": "claude-wrong-provider",
                "cwd": "/tmp",
                "hook_event_name": "SessionStart",
            },
            self.store,
        )
        self.assertIsNone(self.store.get_launch_binding("launch"))
        self.assertIsNotNone(self.store.get_pending("launch"))
        self.assertIsNone(
            self.store.get_session("claude", "claude-wrong-provider")
        )
        self.assertIn(
            "expected provider codex",
            self.store.get_meta("launch_binding_error:launch") or "",
        )

    @patch("pikamux.hooks.Tmux", FakeTmux)
    def test_wrong_expected_uuid_hook_cannot_claim_pending_launch(self) -> None:
        self.store.delete_pending("launch")
        self.store.add_pending(
            "launch",
            "codex",
            "thread-name",
            "/tmp",
            "pika-c-token",
            "%9",
            expected_session_id="expected-uuid",
        )
        handle_hook(
            "codex",
            {
                "session_id": "wrong-uuid",
                "cwd": "/tmp",
                "hook_event_name": "SessionStart",
            },
            self.store,
        )
        self.assertIsNone(self.store.get_launch_binding("launch"))
        self.assertIsNone(self.store.get_session("codex", "wrong-uuid"))
        self.assertIn(
            "expected UUID expected-uuid",
            self.store.get_meta("launch_binding_error:launch") or "",
        )

    @patch("pikamux.hooks.Tmux", FakeTmux)
    def test_wrong_pane_or_cwd_hook_cannot_claim_pending_launch(self) -> None:
        with patch.dict(os.environ, {"TMUX_PANE": "%other"}, clear=False):
            handle_hook(
                "codex",
                {
                    "session_id": "wrong-pane",
                    "cwd": "/tmp",
                    "hook_event_name": "SessionStart",
                },
                self.store,
            )
        self.assertIsNone(self.store.get_launch_binding("launch"))
        self.store.delete_meta("launch_binding_error:launch")
        handle_hook(
            "codex",
            {
                "session_id": "wrong-cwd",
                "cwd": "/different",
                "hook_event_name": "SessionStart",
            },
            self.store,
        )
        self.assertIsNone(self.store.get_launch_binding("launch"))
        self.assertIsNone(self.store.get_session("codex", "wrong-cwd"))
        self.assertIn(
            "expected cwd /tmp",
            self.store.get_meta("launch_binding_error:launch") or "",
        )

    @patch("pikamux.hooks.Tmux", FakeTmux)
    def test_session_end_mismatch_does_not_publish_launch_conflict(self) -> None:
        handle_hook(
            "claude",
            {
                "session_id": "exiting-claude",
                "cwd": "/tmp",
                "hook_event_name": "SessionEnd",
            },
            self.store,
        )
        self.assertIsNone(self.store.get_launch_binding("launch"))
        self.assertIsNone(self.store.get_meta("launch_binding_error:launch"))
        self.assertIsNotNone(self.store.get_pending("launch"))

    @patch("pikamux.hooks.Tmux", FakeTmux)
    def test_missing_launch_token_cannot_claim_matching_pending_pane(self) -> None:
        with patch.dict(os.environ, {"PIKA_LAUNCH_TOKEN": ""}, clear=False):
            handle_hook(
                "codex",
                {
                    "session_id": "otherwise-matching",
                    "cwd": "/tmp",
                    "hook_event_name": "SessionStart",
                },
                self.store,
            )
        self.assertIsNone(self.store.get_launch_binding("launch"))
        self.assertIsNone(self.store.get_session("codex", "otherwise-matching"))
        self.assertIsNotNone(self.store.get_pending("launch"))
        self.assertEqual(FakeTmux.tags, [])
        self.assertIn(
            "missing or wrong PIKA_LAUNCH_TOKEN",
            self.store.get_meta("launch_binding_error:launch") or "",
        )

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
    def test_claude_hook_records_current_structured_observation(self) -> None:
        self.store.delete_pending("launch")
        with patch.dict(os.environ, {"PIKA_LAUNCH_TOKEN": ""}, clear=False):
            handle_hook(
                "claude",
                {
                    "session_id": "claude-uuid",
                    "cwd": "/tmp",
                    "hook_event_name": "UserPromptSubmit",
                },
                self.store,
            )
        observation = self.store.get_hook_observation("claude")
        self.assertEqual(observation["event_name"], "UserPromptSubmit")
        self.assertEqual(observation["session_id"], "claude-uuid")
        self.assertEqual(observation["managed"], 0)

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
        self.store.delete_pending("launch")
        with patch.dict(os.environ, {"PIKA_LAUNCH_TOKEN": ""}, clear=False):
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
        self.store.delete_pending("launch")
        with patch.dict(os.environ, {"PIKA_LAUNCH_TOKEN": ""}, clear=False):
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
        self.store.delete_pending("launch")
        with patch.dict(
            os.environ,
            {
                "PIKA_NAME": "native-title",
                "PIKA_SESSION_ID": "uuid-c",
                "PIKA_LAUNCH_TOKEN": "",
            },
            clear=False,
        ):
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
    def test_claude_child_uuid_cannot_inherit_parent_pika_identity(self) -> None:
        self.store.delete_pending("launch")
        parent_id = "11111111-1111-4111-8111-111111111111"
        child_id = "22222222-2222-4222-8222-222222222222"
        self.store.upsert_session(
            Session(
                "claude",
                parent_id,
                name="sample_plugin",
                tmux_session="pika-a-parent",
                tmux_pane="%9",
                status=Status.WORKING.value,
            )
        )
        FakeTmux.pika_provider = "claude"
        FakeTmux.pika_session_id = parent_id
        self.assertTrue(self.store.bind_launch("parent-launch", "claude", parent_id))
        with patch("pikamux.store.process_start_time", return_value=10):
            self.assertTrue(
                self.store.set_live_owner(
                    "claude", parent_id, 4321, owner_token="parent-owner"
                )
            )
        with patch.dict(
            os.environ,
            {
                "PIKA_NAME": "sample_plugin",
                "PIKA_SESSION_ID": parent_id,
                "PIKA_LAUNCH_TOKEN": "parent-launch",
                "PIKA_OWNER_TOKEN": "parent-owner",
                "TMUX_PANE": "%9",
            },
            clear=True,
        ):
            result = handle_hook(
                "claude",
                {
                    "session_id": child_id,
                    "cwd": "/tmp/automation",
                    "hook_event_name": "SessionStart",
                },
                self.store,
            )

        self.assertIsNone(result)
        self.assertIsNone(self.store.get_session("claude", child_id))
        parent = self.store.get_session("claude", parent_id)
        self.assertEqual(parent.status if parent else None, Status.WORKING.value)
        self.assertFalse(parent.unread if parent else True)
        self.assertEqual(
            self.store.get_launch_binding("parent-launch"), ("claude", parent_id)
        )
        self.assertIsNone(
            self.store.get_meta("launch_binding_error:parent-launch")
        )
        self.assertEqual(
            self.store.get_live_owners("claude", parent_id), [(4321, 10)]
        )
        self.assertIsNone(self.store.get_hook_observation("claude"))
        self.assertEqual(FakeTmux.tags, [])

    @patch("pikamux.hooks.Tmux", FakeTmux)
    def test_cross_provider_child_cannot_inherit_parent_pika_identity(self) -> None:
        self.store.delete_pending("launch")
        parent_id = "ses_parent123"
        child_id = "33333333-3333-4333-8333-333333333333"
        self.store.upsert_session(
            Session(
                "opencode",
                parent_id,
                name="oc_qes_style",
                tmux_session="pika-o-parent",
                tmux_pane="%9",
                status=Status.WORKING.value,
            )
        )
        FakeTmux.pika_provider = "opencode"
        FakeTmux.pika_session_id = parent_id
        self.assertTrue(
            self.store.bind_launch("parent-launch", "opencode", parent_id)
        )
        with patch("pikamux.store.process_start_time", return_value=10):
            self.assertTrue(
                self.store.set_live_owner(
                    "opencode", parent_id, 4321, owner_token="parent-owner"
                )
            )

        with patch.dict(
            os.environ,
            {
                "PIKA_PROVIDER": "opencode",
                "PIKA_NAME": "oc_qes_style",
                "PIKA_SESSION_ID": parent_id,
                "PIKA_LAUNCH_TOKEN": "parent-launch",
                "PIKA_OWNER_TOKEN": "parent-owner",
                "TMUX_PANE": "%9",
            },
            clear=True,
        ):
            result = handle_hook(
                "codex",
                {
                    "session_id": child_id,
                    "cwd": "/tmp/automation",
                    "hook_event_name": "SessionStart",
                },
                self.store,
            )

        self.assertIsNone(result)
        self.assertIsNone(self.store.get_session("codex", child_id))
        parent = self.store.get_session("opencode", parent_id)
        self.assertEqual(parent.status if parent else None, Status.WORKING.value)
        self.assertFalse(parent.unread if parent else True)
        self.assertEqual(
            self.store.get_launch_binding("parent-launch"),
            ("opencode", parent_id),
        )
        self.assertIsNone(
            self.store.get_meta("launch_binding_error:parent-launch")
        )
        self.assertEqual(
            self.store.get_live_owners("opencode", parent_id), [(4321, 10)]
        )
        self.assertIsNone(self.store.get_hook_observation("codex"))
        self.assertEqual(FakeTmux.tags, [])

    @patch("pikamux.hooks.Tmux", FakeTmux)
    def test_claude_sdk_cli_hook_removes_only_false_inventory_row(self) -> None:
        self.store.delete_pending("launch")
        worker_id = "33333333-3333-4333-8333-333333333333"
        transcript = Path(self.temp.name) / f"{worker_id}.jsonl"
        content = (
            json.dumps({"type": "custom-title", "customTitle": "sample_plugin"})
            + "\n"
            + json.dumps(
                {
                    "type": "user",
                    "sessionId": worker_id,
                    "entrypoint": "sdk-cli",
                    "isSidechain": False,
                }
            )
            + "\n"
        )
        transcript.write_text(content)
        self.store.upsert_session(
            Session(
                "claude",
                worker_id,
                name="sample_plugin",
                transcript_path=str(transcript),
                status=Status.READY.value,
                unread=True,
            )
        )

        with patch.dict(os.environ, {}, clear=True):
            result = handle_hook(
                "claude",
                {
                    "session_id": worker_id,
                    "transcript_path": str(transcript),
                    "hook_event_name": "Stop",
                },
                self.store,
            )

        self.assertIsNone(result)
        self.assertIsNone(self.store.get_session("claude", worker_id))
        self.assertEqual(transcript.read_text(), content)

    @patch("pikamux.hooks.Tmux", FakeTmux)
    def test_codex_exec_hook_removes_only_false_inventory_row(self) -> None:
        self.store.delete_pending("launch")
        worker_id = "44444444-4444-4444-8444-444444444444"
        transcript = Path(self.temp.name) / f"{worker_id}.jsonl"
        content = (
            json.dumps(
                {
                    "type": "session_meta",
                    "payload": {
                        "id": worker_id,
                        "originator": "codex_exec",
                        "source": "exec",
                    },
                }
            )
            + "\n"
        )
        transcript.write_text(content)
        self.store.upsert_session(
            Session(
                "codex",
                worker_id,
                name="oc_qes_style",
                transcript_path=str(transcript),
                status=Status.READY.value,
                unread=True,
            )
        )

        with patch.dict(os.environ, {}, clear=True):
            result = handle_hook(
                "codex",
                {
                    "session_id": worker_id,
                    "transcript_path": str(transcript),
                    "hook_event_name": "Stop",
                },
                self.store,
            )

        self.assertIsNone(result)
        self.assertIsNone(self.store.get_session("codex", worker_id))
        self.assertEqual(transcript.read_text(), content)

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
        self.assertIsNone(self.store.get_hook_observation("codex"))
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
    def test_native_side_thread_stays_subordinate_to_parent(self) -> None:
        parent_id = "11111111-1111-4111-8111-111111111111"
        child_id = "22222222-2222-4222-8222-222222222222"
        transcript = Path(self.temp.name) / "side.jsonl"
        transcript.write_text(
            json.dumps(
                {
                    "type": "session_meta",
                    "payload": {
                        "id": child_id,
                        "forked_from_id": parent_id,
                        "thread_source": "subagent",
                        "source": {"subagent": {"thread_spawn": {}}},
                    },
                }
            )
            + "\n"
        )
        self.store.delete_pending("launch")
        self.store.upsert_session(
            Session(
                "codex",
                parent_id,
                name="research-notes",
                cwd="/tmp",
                tmux_pane="%9",
                status=Status.WORKING.value,
            )
        )
        with patch.dict(os.environ, {"PIKA_LAUNCH_TOKEN": ""}, clear=False):
            handle_hook(
                "codex",
                {
                    "session_id": child_id,
                    "cwd": "/tmp",
                    "hook_event_name": "Stop",
                    "transcript_path": str(transcript),
                },
                self.store,
            )
        parent = self.store.get_session("codex", parent_id)
        self.assertEqual(parent.status if parent else None, Status.WORKING.value)
        self.assertIsNone(self.store.get_session("codex", child_id))
        self.assertEqual(FakeTmux.alerts, [])

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
                name="research-notes",
                cwd="/tmp",
                tmux_session="pika-parent",
                tmux_pane="%9",
                status=Status.READY.value,
            )
        )
        candidate = Candidate(
            "codex",
            child_id,
            "research-notes",
            cwd="/tmp",
            transcript_path=str(transcript),
            parent_session_id=parent_id,
            lifecycle_status=Status.WORKING.value,
        )
        # This is a later continuation of an already bound Pika launch, not
        # the first hook that is allowed to claim the launch token.
        self.store.bind_launch("launch", "codex", parent_id)
        self.store.delete_pending("launch")
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
    def test_renamed_codex_fork_transfers_same_pane_home_and_name(self) -> None:
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
                name="returns_tracker",
                cwd="/tmp",
                tmux_session="pika-parent",
                tmux_pane="%9",
                status=Status.READY.value,
            )
        )
        candidate = Candidate(
            "codex",
            child_id,
            "cf_perf",
            cwd="/tmp",
            transcript_path=str(transcript),
            parent_session_id=parent_id,
            lifecycle_status=Status.WORKING.value,
        )
        self.store.bind_launch("launch", "codex", parent_id)
        self.store.delete_pending("launch")

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
        self.assertEqual(current.active_thread_id if current else None, child_id)
        self.assertEqual(current.name if current else None, "cf_perf")
        self.assertEqual(current.status if current else None, Status.WORKING.value)
        self.assertIsNone(self.store.get_session("codex", child_id))
        self.assertTrue(
            any(values.get("name") == "cf_perf" for _, values in FakeTmux.tags)
        )

    @patch("pikamux.hooks.Tmux", FakeTmux)
    def test_renamed_codex_fork_in_other_pane_stays_independent(self) -> None:
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
        self.store.delete_pending("launch")
        self.store.upsert_session(
            Session(
                "codex",
                parent_id,
                name="returns_tracker",
                cwd="/tmp",
                tmux_pane="%1",
                status=Status.READY.value,
            )
        )
        candidate = Candidate(
            "codex",
            child_id,
            "cf_perf",
            cwd="/tmp",
            transcript_path=str(transcript),
            parent_session_id=parent_id,
            lifecycle_status=Status.WORKING.value,
        )

        with patch.object(CodexProvider, "thread_candidate", return_value=candidate):
            handle_hook(
                "codex",
                {
                    "session_id": child_id,
                    "cwd": "/tmp",
                    "hook_event_name": "UserPromptSubmit",
                    "transcript_path": str(transcript),
                },
                self.store,
            )

        parent = self.store.get_session("codex", parent_id)
        child = self.store.get_session("codex", child_id)
        self.assertIsNone(parent.active_thread_id if parent else None)
        self.assertEqual(child.name if child else None, "cf_perf")
        self.assertEqual(child.tmux_pane if child else None, "%9")

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
                name="research-notes",
                cwd="/tmp",
                tmux_session="pika-parent",
                tmux_pane="%9",
                status=Status.WORKING.value,
            )
        )
        candidate = Candidate(
            "codex",
            child_id,
            "research-notes",
            cwd="/tmp",
            parent_session_id=parent_id,
            lifecycle_status=Status.WORKING.value,
        )
        self.store.bind_launch("launch", "codex", parent_id)
        self.store.delete_pending("launch")
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
        self.store.delete_pending("launch")
        self.assertIsNone(self.store.get_pending("launch"))
        handle_process_exit("codex", 7, launch_token="launch", store=self.store)
        session = self.store.get_session("codex", "uuid-exit")
        self.assertEqual(session.status if session else None, Status.ERROR.value)
        self.assertIn("status 7", session.error if session else "")
        self.assertEqual(session.attention_reason if session else None, "exited")

    def test_opencode_exit_after_root_switch_targets_current_binding(self) -> None:
        for code in (0, 9):
            with self.subTest(code=code):
                old = f"ses_oldexit{code}"
                current = f"ses_currentexit{code}"
                token = f"switch-token-{code}"
                owner_token = f"owner-{code}"
                self.store.upsert_session(
                    Session("opencode", old, name="old", status=Status.WORKING.value)
                )
                self.store.upsert_session(
                    Session(
                        "opencode",
                        current,
                        name="current",
                        status=Status.WORKING.value,
                    )
                )
                self.assertTrue(self.store.bind_launch(token, "opencode", current))
                self.store.set_recovery_owner(
                    "opencode", current, 4321, 10, token
                )
                with patch("pikamux.store.process_start_time", return_value=10):
                    self.assertTrue(
                        self.store.set_live_owner(
                            "opencode",
                            current,
                            4321,
                            owner_token=owner_token,
                        )
                    )

                handle_process_exit(
                    "opencode",
                    code,
                    session_id=old,
                    launch_token=token,
                    owner_token=owner_token,
                    store=self.store,
                )

                old_after = self.store.get_session("opencode", old)
                current_after = self.store.get_session("opencode", current)
                self.assertEqual(old_after.status, Status.WORKING.value)
                self.assertEqual(
                    current_after.status,
                    Status.PARKED.value if code == 0 else Status.ERROR.value,
                )
                self.assertEqual(current_after.unread, code != 0)
                self.assertEqual(
                    current_after.attention_reason,
                    None if code == 0 else "exited",
                )
                self.assertEqual(
                    self.store.get_live_owner_leases("opencode", current), []
                )
                self.assertIsNone(
                    self.store.get_recovery_owner("opencode", current)
                )
                self.assertIsNone(self.store.get_launch_binding(token))

    def test_ctrl_c_exit_revokes_only_its_owner_lease_and_parks(self) -> None:
        self.store.upsert_session(
            Session(
                "codex",
                "uuid-interrupted",
                name="interrupted",
                status=Status.WORKING.value,
            )
        )
        with patch("pikamux.store.process_start_time", return_value=12345):
            self.store.set_live_owner(
                "codex",
                "uuid-interrupted",
                4321,
                owner_token="pika-client",
            )
            self.store.set_live_owner(
                "codex",
                "uuid-interrupted",
                4321,
                owner_token="desktop-client",
            )

        handle_process_exit(
            "codex",
            130,
            session_id="uuid-interrupted",
            owner_token="pika-client",
            store=self.store,
        )

        session = self.store.get_session("codex", "uuid-interrupted")
        self.assertEqual(session.status if session else None, Status.PARKED.value)
        self.assertFalse(session.unread if session else True)
        self.assertIsNone(session.error if session else "missing")
        leases = self.store.get_live_owner_leases("codex", "uuid-interrupted")
        self.assertEqual(
            [token for _pid, _start, _seen, token in leases],
            ["desktop-client"],
        )

    def test_legacy_process_exit_clears_undifferentiated_owner_lease(self) -> None:
        self.store.upsert_session(
            Session("codex", "uuid-legacy-exit", status=Status.WORKING.value)
        )
        with patch("pikamux.store.process_start_time", return_value=12345):
            self.store.set_live_owner("codex", "uuid-legacy-exit", 4321)

        handle_process_exit(
            "codex", 130, session_id="uuid-legacy-exit", store=self.store
        )

        self.assertEqual(
            self.store.get_live_owners("codex", "uuid-legacy-exit"), []
        )

    @patch("pikamux.hooks.Tmux", FakeTmux)
    def test_late_competing_hook_cannot_overwrite_recovered_launch(self) -> None:
        winner = "11111111-1111-4111-8111-111111111111"
        competitor = "22222222-2222-4222-8222-222222222222"
        self.assertTrue(self.store.bind_launch("launch", "codex", winner))
        self.store.delete_pending("launch")
        handle_hook(
            "codex",
            {
                "session_id": competitor,
                "cwd": "/tmp",
                "hook_event_name": "SessionStart",
            },
            self.store,
        )
        self.assertEqual(
            self.store.get_launch_binding("launch"), ("codex", winner)
        )
        self.assertIsNone(self.store.get_session("codex", competitor))
        self.assertIn(
            competitor,
            self.store.get_meta("launch_binding_error:launch") or "",
        )

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
