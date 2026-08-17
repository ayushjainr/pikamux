from __future__ import annotations

import argparse
import io
import time
import unittest
from contextlib import redirect_stderr, redirect_stdout
from pathlib import Path
from unittest.mock import Mock, patch

from pikamux.cli import (
    _bare,
    _confirm_shared_lease,
    _normalize_argv,
    _parser,
    _peek,
    _peek_popup,
    _setup,
    _wait,
    run,
)
from pikamux.core import PikaError, SharedLeaseConflict
from pikamux.models import Candidate, Pane, Session, Status
from pikamux.setup_hooks import hook_spec_fingerprint
from pikamux.tmux import TmuxError


class WaitStore:
    def __init__(self, session: Session):
        self.session = session

    def get_session(self, *_key: str) -> Session:
        return self.session


class WaitPika:
    def __init__(self) -> None:
        self.working = Session(
            "codex",
            "11111111-1111-4111-8111-111111111111",
            name="waited",
            status=Status.WORKING.value,
        )
        self.ready = Session(
            "codex",
            self.working.session_id,
            name="waited",
            status=Status.READY.value,
            unread=True,
        )
        self.store = WaitStore(self.working)
        self.refresh_calls = 0

    def resolve(self, _name: str, _sessions=None) -> Session:
        return self.working

    def refresh(self, *, usage: bool = False) -> list[Session]:
        self.refresh_calls += 1
        return [self.ready]


class PeekTmux:
    def __init__(self, *, fail: bool):
        self.fail = fail

    def get_pane(self, _target: str) -> Pane:
        return Pane("home", "%1", 1, "/tmp", "codex", False, False, None, 1, 1)

    def capture(self, _target: str, _lines: int) -> str:
        if self.fail:
            raise TmuxError("capture failed")
        return "pane output"


class PeekPika:
    def __init__(self, *, fail: bool):
        self.session = Session(
            "codex",
            "11111111-1111-4111-8111-111111111111",
            name="peeked",
            tmux_pane="%1",
            status=Status.READY.value,
            unread=True,
        )
        self.tmux = PeekTmux(fail=fail)
        self.acknowledged = False

    def resolve(self, _name: str, _sessions=None) -> Session:
        return self.session

    def acknowledge(self, _session: Session) -> None:
        self.acknowledged = True


class CliTests(unittest.TestCase):
    def test_ask_fast_selects_the_benchmarked_fast_profile(self) -> None:
        args = _parser().parse_args(["ask", "expert", "why", "--fast"])
        self.assertTrue(args.fast)
        self.assertEqual(args.question, ["why"])
        leading = _parser().parse_args(
            _normalize_argv(["ask", "expert", "--fast", "why"])
        )
        self.assertTrue(leading.fast)
        self.assertEqual(leading.question, ["why"])

    def test_manual_expert_card_requires_scope_and_current_state(self) -> None:
        args = _parser().parse_args(
            [
                "expert",
                "publish",
                "--scope",
                "Owns exact recovery.",
                "--now",
                "Validating PID lease expiry.",
                "--topic",
                "identity",
            ]
        )
        self.assertEqual(args.summary, "Owns exact recovery.")
        self.assertEqual(args.current_state, "Validating PID lease expiry.")

    def test_bare_name_normalizes_to_universal_entry_without_shadowing_commands(
        self,
    ) -> None:
        self.assertEqual(_normalize_argv(["research"]), ["_enter", "research"])
        self.assertEqual(_normalize_argv(["list"]), ["list"])
        self.assertEqual(_normalize_argv(["open", "list"]), ["open", "list"])

    def test_shared_lease_confirmation_recovers_without_a_new_command(self) -> None:
        session = Session("codex", "uuid", name="master pika")
        conflict = SharedLeaseConflict("shared lease", session)
        pika = Mock()
        pika.recover_after_closed_confirmation.return_value = 17
        fake_stdin = Mock()
        fake_stdin.isatty.return_value = True
        with (
            patch("pikamux.cli.sys.stdin", fake_stdin),
            patch("builtins.input", return_value="yes"),
            redirect_stderr(io.StringIO()) as error,
        ):
            self.assertEqual(_confirm_shared_lease(pika, conflict), 17)
        pika.recover_after_closed_confirmation.assert_called_once_with(session)
        self.assertIn("may still be open in another Codex client", error.getvalue())
        self.assertNotIn("run exactly", error.getvalue())

    def test_shared_lease_decline_changes_nothing_and_repeats_same_command(self) -> None:
        session = Session("codex", "uuid", name="master pika")
        conflict = SharedLeaseConflict("shared lease", session)
        pika = Mock()
        fake_stdin = Mock()
        fake_stdin.isatty.return_value = True
        with (
            patch("pikamux.cli.sys.stdin", fake_stdin),
            patch("builtins.input", return_value=""),
            redirect_stderr(io.StringIO()) as error,
        ):
            self.assertEqual(_confirm_shared_lease(pika, conflict), 1)
        pika.recover_after_closed_confirmation.assert_not_called()
        self.assertIn("`pika 'master pika'`", error.getvalue())

    def test_shared_lease_noninteractive_call_stays_fail_closed(self) -> None:
        session = Session("codex", "uuid", name="master_pika")
        conflict = SharedLeaseConflict("shared lease", session)
        fake_stdin = Mock()
        fake_stdin.isatty.return_value = False
        with (
            patch("pikamux.cli.sys.stdin", fake_stdin),
            self.assertRaisesRegex(PikaError, "Interactive confirmation is required"),
        ):
            _confirm_shared_lease(Mock(), conflict)

    def test_one_name_entry_routes_ambiguous_lease_to_confirmation(self) -> None:
        session = Session("codex", "uuid", name="master_pika")
        conflict = SharedLeaseConflict("shared lease", session)
        pika = Mock()
        pika.enter.side_effect = conflict
        with (
            patch("pikamux.cli.Pika", return_value=pika),
            patch("pikamux.cli._confirm_shared_lease", return_value=23) as confirm,
        ):
            self.assertEqual(run(["master_pika"]), 23)
        pika.enter.assert_called_once_with("master_pika")
        confirm.assert_called_once_with(pika, conflict)

    def test_remote_exact_open_routes_ambiguous_lease_to_confirmation(self) -> None:
        session = Session("codex", "uuid", name="master_remote")
        conflict = SharedLeaseConflict("shared lease", session)
        pika = Mock()
        pika.store.local_node_id.return_value = "node-1"
        pika.store.get_session.return_value = session
        pika.refresh.return_value = [session]
        pika.open.side_effect = conflict
        with (
            patch("pikamux.cli.Pika", return_value=pika),
            patch("pikamux.cli._confirm_shared_lease", return_value=29) as confirm,
        ):
            self.assertEqual(
                run(
                    [
                        "_fleet-open",
                        "--expected-node-id",
                        "node-1",
                        "--provider",
                        "codex",
                        "--session-id",
                        "uuid",
                    ]
                ),
                29,
            )
        pika.open.assert_called_once_with(session)
        confirm.assert_called_once_with(pika, conflict)

    def test_legacy_recovery_receipt_remains_a_hidden_safe_alias(self) -> None:
        session = Session("codex", "uuid", name="master_pika")
        conflict = SharedLeaseConflict("shared lease", session)
        pika = Mock()
        pika.resolve.return_value = session
        pika.open.side_effect = conflict
        self.assertEqual(
            _normalize_argv(["recover-closed", "master_pika"]),
            ["recover-closed", "master_pika"],
        )
        with (
            patch("pikamux.cli.Pika", return_value=pika),
            patch("pikamux.cli._confirm_shared_lease", return_value=31) as confirm,
        ):
            self.assertEqual(run(["recover-closed", "master_pika"]), 31)
        pika.open.assert_called_once_with(session)
        confirm.assert_called_once_with(pika, conflict)

    def test_primary_help_teaches_one_name_command_not_lifecycle_mechanics(self) -> None:
        rendered = _parser().format_help()
        self.assertIn("pika NAME", rendered)
        self.assertNotIn("recover-closed", rendered)
        self.assertNotIn("open a named conversation", rendered)
        self.assertNotIn("start a new managed conversation", rendered)
        self.assertNotIn("adopt a running agent", rendered)

    def test_bare_interactive_terminal_opens_live_monitor(self) -> None:
        class BarePika:
            pass

        pika = BarePika()
        fake_stdin = Mock()
        fake_stdin.isatty.return_value = True
        fake_stdout = Mock()
        fake_stdout.isatty.return_value = True
        with (
            patch("pikamux.cli.sys.stdin", fake_stdin),
            patch("pikamux.cli.sys.stdout", fake_stdout),
            patch("pikamux.cli.run_monitor", return_value=7) as monitor,
        ):
            self.assertEqual(_bare(pika), 7)
        monitor.assert_called_once_with(pika)

    def test_bare_redirected_output_remains_static(self) -> None:
        pika = Mock()
        pika.refresh.return_value = []
        with redirect_stdout(io.StringIO()) as output:
            self.assertEqual(_bare(pika), 0)
        pika.refresh.assert_called_once_with(usage=True)
        self.assertIn("No Pika sessions yet", output.getvalue())

    def test_wait_reconciles_provider_state_periodically(self) -> None:
        pika = WaitPika()
        args = argparse.Namespace(name="waited", wait_for="any", timeout=10, json=False)
        with (
            patch("pikamux.cli.time.monotonic", side_effect=[0.0, 5.0]),
            redirect_stdout(io.StringIO()) as output,
        ):
            self.assertEqual(_wait(pika, args), 0)
        self.assertEqual(pika.refresh_calls, 1)
        self.assertIn("waited: READY", output.getvalue())

    def test_wait_sanitizes_name_and_reason_for_terminal_output(self) -> None:
        pika = WaitPika()
        pika.ready.name = "waited\x1b[2J\nrenamed"
        pika.ready.attention_reason = "done\rrewritten"
        args = argparse.Namespace(name="waited", wait_for="any", timeout=10, json=False)
        with (
            patch("pikamux.cli.time.monotonic", side_effect=[0.0, 5.0]),
            redirect_stdout(io.StringIO()) as output,
        ):
            self.assertEqual(_wait(pika, args), 0)
        rendered = output.getvalue()
        self.assertNotIn("\x1b", rendered)
        self.assertNotIn("\r", rendered)
        self.assertIn("waited�[2J�renamed", rendered)
        self.assertIn("done�rewritten", rendered)

    def test_peek_popup_sanitizes_name(self) -> None:
        pika = Mock()
        pika.tmux.capture.return_value = "pane output"
        args = argparse.Namespace(
            target="%1",
            lines=20,
            name="peek\x1b[2J\nrenamed",
            provider="codex",
            session_id="uuid",
        )
        with (
            patch("pikamux.cli.Pika", return_value=pika),
            patch("pikamux.cli.sys.stdin", io.StringIO("q")),
            redirect_stdout(io.StringIO()) as output,
        ):
            self.assertEqual(_peek_popup(args), 0)
        rendered = output.getvalue()
        self.assertNotIn("\x1b", rendered)
        self.assertIn("peek�[2J�renamed", rendered)

    def test_peek_popup_enter_routes_ambiguous_lease_to_confirmation(self) -> None:
        session = Session("codex", "uuid", name="peeked")
        conflict = SharedLeaseConflict("shared lease", session)
        pika = Mock()
        pika.tmux.capture.return_value = "pane output"
        pika.store.get_session.return_value = session
        pika.open.side_effect = conflict
        args = argparse.Namespace(
            target="%1",
            lines=20,
            name="peeked",
            provider="codex",
            session_id="uuid",
        )
        with (
            patch("pikamux.cli.Pika", return_value=pika),
            patch("pikamux.cli.sys.stdin", io.StringIO("\n")),
            patch("pikamux.cli._confirm_shared_lease", return_value=37) as confirm,
            redirect_stdout(io.StringIO()),
        ):
            self.assertEqual(_peek_popup(args), 37)
        pika.open.assert_called_once_with(session)
        confirm.assert_called_once_with(pika, conflict)

    def test_failed_peek_does_not_acknowledge_unread_ready(self) -> None:
        pika = PeekPika(fail=True)
        with (
            patch("pikamux.cli.load_config", return_value={"peek_lines": 20}),
            self.assertRaisesRegex(TmuxError, "capture failed"),
        ):
            _peek(pika, "peeked", None)
        self.assertFalse(pika.acknowledged)

    def test_successful_peek_acknowledges_unread_ready(self) -> None:
        pika = PeekPika(fail=False)
        with (
            patch("pikamux.cli.load_config", return_value={"peek_lines": 20}),
            redirect_stdout(io.StringIO()),
        ):
            self.assertEqual(_peek(pika, "peeked", None, ack=True), 0)
        self.assertTrue(pika.acknowledged)

    def test_redirected_peek_preserves_unread_without_explicit_ack(self) -> None:
        pika = PeekPika(fail=False)
        with (
            patch("pikamux.cli.load_config", return_value={"peek_lines": 20}),
            redirect_stdout(io.StringIO()),
        ):
            self.assertEqual(_peek(pika, "peeked", None), 0)
        self.assertFalse(pika.acknowledged)

    def test_setup_frames_configuration_as_a_commissioning_contract(self) -> None:
        class MetaStore:
            @staticmethod
            def list_sessions():
                return []

            @staticmethod
            def untracked_session_keys():
                return set()

            @staticmethod
            def get_meta(_key):
                return None

        pika = Mock(store=MetaStore())
        pika.refresh.return_value = []
        args = argparse.Namespace(
            no_import=True,
            dry_run=False,
            import_all=False,
            yes=True,
            default_provider="codex",
        )
        output = io.StringIO()
        with (
            patch("pikamux.cli.proposed_changes", return_value=[]),
            patch("pikamux.cli.hooks_installed", return_value=True),
            patch("pikamux.cli.load_config", return_value={}),
            redirect_stdout(output),
        ):
            self.assertEqual(_setup(pika, args), 0)
        rendered = output.getvalue()
        self.assertIn("Pika commissioning", rendered)
        self.assertIn("existing settings retained", rendered)
        self.assertIn("Default for new conversations: Codex", rendered)
        self.assertIn("Commissioning status", rendered)
        self.assertIn("Pika not yet commissioned", rendered)
        self.assertIn("Codex observation", rendered)
        self.assertIn("Claude observation", rendered)

    def test_setup_never_claims_commissioned_from_stale_observation(self) -> None:
        class MetaStore:
            @staticmethod
            def list_sessions():
                return []

            @staticmethod
            def untracked_session_keys():
                return set()

            @staticmethod
            def get_meta(key):
                provider = key.rsplit(":", 1)[-1]
                return hook_spec_fingerprint(provider)

        pika = Mock(store=MetaStore())
        pika.refresh.return_value = []
        args = argparse.Namespace(
            no_import=True,
            dry_run=False,
            import_all=False,
            yes=True,
            default_provider="codex",
        )
        output = io.StringIO()
        with (
            patch("pikamux.cli.proposed_changes", return_value=[]),
            patch(
                "pikamux.cli.hooks_installed",
                side_effect=lambda provider: provider == "codex",
            ),
            patch("pikamux.cli.load_config", return_value={}),
            redirect_stdout(output),
        ):
            self.assertEqual(_setup(pika, args), 0)
        rendered = output.getvalue()
        self.assertIn("Pika not yet commissioned", rendered)
        self.assertIn("Claude activation", rendered)
        self.assertNotIn("Pika commissioned ·", rendered)

    def test_setup_never_commissions_with_nondefault_provider_missing(self) -> None:
        class MetaStore:
            @staticmethod
            def list_sessions():
                return []

            @staticmethod
            def untracked_session_keys():
                return set()

            @staticmethod
            def list_pending():
                return []

            @staticmethod
            def get_hook_observation(provider):
                return {
                    "fingerprint": hook_spec_fingerprint(provider),
                    "event_name": "SessionStart",
                    "session_id": f"{provider}-uuid",
                    "observed_at": time.time(),
                }

            @staticmethod
            def get_meta(_key):
                return None

        pika = Mock(store=MetaStore())
        args = argparse.Namespace(
            no_import=True,
            dry_run=False,
            import_all=False,
            yes=True,
            default_provider="codex",
        )
        output = io.StringIO()
        with (
            patch(
                "pikamux.cli.setup_executables",
                return_value={"codex": "/bin/codex", "claude": None},
            ),
            patch(
                "pikamux.cli.executable_available",
                side_effect=lambda value: value == "/bin/codex",
            ),
            patch("pikamux.cli.executable_version", return_value="test"),
            patch("pikamux.cli.proposed_changes", return_value=[]),
            patch("pikamux.cli.hooks_installed", return_value=True),
            patch("pikamux.cli.load_config", return_value={}),
            redirect_stdout(output),
        ):
            self.assertEqual(_setup(pika, args), 0)
        rendered = output.getvalue()
        self.assertIn("Claude executable", rendered)
        self.assertNotIn("Pika commissioned ·", rendered)

    def test_setup_reports_overdue_pending_launch_as_degraded(self) -> None:
        class MetaStore:
            @staticmethod
            def list_sessions():
                return []

            @staticmethod
            def untracked_session_keys():
                return set()

            @staticmethod
            def list_pending():
                return [
                    {
                        "provider": "codex",
                        "name": "qis_dash",
                        "created_at": 0,
                    }
                ]

            @staticmethod
            def get_hook_observation(provider):
                return {
                    "fingerprint": hook_spec_fingerprint(provider),
                    "event_name": "SessionStart",
                    "session_id": f"{provider}-uuid",
                    "observed_at": 0,
                }

            @staticmethod
            def get_meta(_key):
                return None

        pika = Mock(store=MetaStore())
        args = argparse.Namespace(
            no_import=True,
            dry_run=False,
            import_all=False,
            yes=True,
            default_provider="codex",
        )
        output = io.StringIO()
        with (
            patch("pikamux.cli.proposed_changes", return_value=[]),
            patch("pikamux.cli.hooks_installed", return_value=True),
            patch("pikamux.cli.load_config", return_value={}),
            redirect_stdout(output),
        ):
            self.assertEqual(_setup(pika, args), 0)
        rendered = output.getvalue()
        self.assertIn("launches DEGRADED", rendered)
        self.assertIn("Codex launch qis_dash identity pending", rendered)
        self.assertNotIn("Pika commissioned ·", rendered)

    def test_setup_reloads_systemd_for_a_service_only_path_change(self) -> None:
        store = Mock()
        store.list_sessions.return_value = []
        store.untracked_session_keys.return_value = set()
        store.list_pending.return_value = []
        store.get_meta.return_value = None
        pika = Mock(store=store)
        change = Mock(
            changed=True,
            path=Path("/tmp/pika-expert-refresh.service"),
        )
        change.diff.return_value = "service path changed\n"
        args = argparse.Namespace(
            no_import=True,
            dry_run=False,
            import_all=False,
            yes=True,
            default_provider="codex",
        )
        with (
            patch("pikamux.cli.proposed_changes", return_value=[change]),
            patch("pikamux.cli.apply_changes", return_value=[]),
            patch("pikamux.cli.activate_timer", return_value=(True, "reloaded"))
            as activate,
            patch("pikamux.cli.hooks_installed", return_value=True),
            patch("pikamux.cli.load_config", return_value={}),
            redirect_stdout(io.StringIO()),
        ):
            self.assertEqual(_setup(pika, args), 0)
        activate.assert_called_once_with()

    def test_setup_one_proof_copy_requires_claude_fully_commissioned(self) -> None:
        class MetaStore:
            @staticmethod
            def list_sessions():
                return []

            @staticmethod
            def untracked_session_keys():
                return set()

            @staticmethod
            def get_meta(_key):
                return None

        pika = Mock(store=MetaStore())
        pika.refresh.return_value = []
        args = argparse.Namespace(
            no_import=True,
            dry_run=False,
            import_all=False,
            yes=True,
            default_provider="codex",
        )
        output = io.StringIO()
        with (
            patch("pikamux.cli.proposed_changes", return_value=[]),
            patch(
                "pikamux.cli.hooks_installed",
                side_effect=lambda provider: provider == "codex",
            ),
            patch("pikamux.cli.load_config", return_value={}),
            redirect_stdout(output),
        ):
            self.assertEqual(_setup(pika, args), 0)
        rendered = output.getvalue()
        self.assertNotIn("One required proof remains", rendered)
        self.assertIn("Codex observation", rendered)
        self.assertIn("Claude activation", rendered)
        self.assertIn("Claude observation", rendered)

    def test_setup_imports_without_interviewing_tracked_agents(self) -> None:
        candidate = Candidate(
            "codex",
            "11111111-1111-4111-8111-111111111111",
            "newly-adopted",
            transcript_path="/tmp/provider-thread.jsonl",
        )
        store = Mock()
        store.list_sessions.return_value = []
        store.untracked_session_keys.return_value = set()
        store.get_meta.return_value = None
        pika = Mock(store=store)
        pika.discover_import_candidates.return_value = [candidate]
        pika.refresh.return_value = []
        args = argparse.Namespace(
            no_import=False,
            dry_run=False,
            import_all=True,
            yes=True,
            default_provider="codex",
        )
        output = io.StringIO()
        with (
            patch("pikamux.cli.proposed_changes", return_value=[]),
            patch("pikamux.cli.hooks_installed", return_value=True),
            patch("pikamux.cli.load_config", return_value={}),
            redirect_stdout(output),
        ):
            self.assertEqual(_setup(pika, args), 0)

        pika.import_candidate.assert_called_once_with(candidate)
        pika.bootstrap_experts.assert_not_called()
        pika.refresh.assert_called_once_with(usage=False)
        self.assertIn("Added 1 existing conversation", output.getvalue())
        self.assertIn("setup did not interview any agents", output.getvalue())
        self.assertIn("pika expert refresh --all", output.getvalue())

    def test_routine_setup_does_not_refresh_conversation_inventory(self) -> None:
        old = Session("codex", "rename-id", name="before")
        new = Session("codex", "rename-id", name="after")
        store = Mock()
        store.list_sessions.return_value = [old]
        store.untracked_session_keys.return_value = set()
        store.get_meta.return_value = None
        pika = Mock(store=store)
        pika.refresh.return_value = [new]
        args = argparse.Namespace(
            no_import=True,
            dry_run=False,
            import_all=False,
            yes=True,
            default_provider="codex",
        )
        output = io.StringIO()
        with (
            patch("pikamux.cli.proposed_changes", return_value=[]),
            patch("pikamux.cli.hooks_installed", return_value=True),
            patch("pikamux.cli.load_config", return_value={}),
            redirect_stdout(output),
        ):
            self.assertEqual(_setup(pika, args), 0)

        pika.refresh.assert_not_called()
        pika.bootstrap_experts.assert_not_called()
        self.assertNotIn("Refreshed 1 provider rename", output.getvalue())
        self.assertNotIn("before → after", output.getvalue())

    def test_setup_respects_explicitly_untracked_conversations(self) -> None:
        candidate = Candidate("claude", "ignored-id", "qes_plugin")
        store = Mock()
        store.list_sessions.return_value = []
        store.untracked_session_keys.return_value = {
            (candidate.provider, candidate.session_id)
        }
        store.get_meta.return_value = None
        pika = Mock(store=store)
        pika.discover_import_candidates.return_value = [candidate]
        pika.refresh.return_value = []
        args = argparse.Namespace(
            no_import=False,
            dry_run=False,
            import_all=True,
            yes=True,
            default_provider="codex",
        )
        output = io.StringIO()
        with (
            patch("pikamux.cli.proposed_changes", return_value=[]),
            patch("pikamux.cli.hooks_installed", return_value=True),
            patch("pikamux.cli.load_config", return_value={}),
            redirect_stdout(output),
        ):
            self.assertEqual(_setup(pika, args), 0)

        pika.import_candidate.assert_not_called()
        self.assertIn("explicitly untracked", output.getvalue())
        self.assertIn("qes_plugin", output.getvalue())

    def test_setup_does_not_reoffer_active_codex_continuation(self) -> None:
        child_id = "22222222-2222-4222-8222-222222222222"
        tracked = Session(
            "codex",
            "11111111-1111-4111-8111-111111111111",
            name="master_quant",
            active_thread_id=child_id,
        )
        candidate = Candidate(
            "codex",
            child_id,
            "master_quant",
            parent_session_id=tracked.session_id,
        )
        store = Mock()
        store.list_sessions.return_value = [tracked]
        store.untracked_session_keys.return_value = set()
        store.get_meta.return_value = None
        pika = Mock(store=store)
        pika.refresh.return_value = [tracked]
        pika.discover_import_candidates.return_value = [candidate]
        args = argparse.Namespace(
            no_import=False,
            dry_run=False,
            import_all=True,
            yes=True,
            default_provider="codex",
        )
        with (
            patch("pikamux.cli.proposed_changes", return_value=[]),
            patch("pikamux.cli.hooks_installed", return_value=True),
            patch("pikamux.cli.load_config", return_value={}),
            redirect_stdout(io.StringIO()),
        ):
            self.assertEqual(_setup(pika, args), 0)

        pika.import_candidate.assert_not_called()


if __name__ == "__main__":
    unittest.main()
