from __future__ import annotations

import argparse
import io
import unittest
from contextlib import redirect_stdout
from unittest.mock import Mock, patch

from pikamux.cli import (
    _bare,
    _normalize_argv,
    _parser,
    _peek,
    _peek_popup,
    _setup,
    _wait,
)
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

    def test_bare_name_normalizes_to_open_without_shadowing_commands(self) -> None:
        self.assertEqual(_normalize_argv(["research"]), ["open", "research"])
        self.assertEqual(_normalize_argv(["list"]), ["list"])
        self.assertEqual(_normalize_argv(["open", "list"]), ["open", "list"])

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
        self.assertIn("Adopted 1 existing conversation", output.getvalue())
        self.assertIn("setup did not interview any agents", output.getvalue())
        self.assertIn("pika expert refresh --all", output.getvalue())

    def test_setup_reports_provider_renames_without_interviewing(self) -> None:
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

        pika.refresh.assert_called_once_with(usage=False)
        pika.bootstrap_experts.assert_not_called()
        self.assertIn("Refreshed 1 provider rename", output.getvalue())
        self.assertIn("before → after", output.getvalue())

    def test_setup_respects_explicitly_untracked_conversations(self) -> None:
        candidate = Candidate("codex", "ignored-id", "do-not-watch")
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
