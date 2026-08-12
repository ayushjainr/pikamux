from __future__ import annotations

import os
import shlex
import shutil
import signal
import tempfile
import time
import unittest
import uuid
from pathlib import Path
from unittest.mock import patch

from pikamux.core import Pika
from pikamux.hooks import handle_hook
from pikamux.models import Candidate, Session
from pikamux.processes import find_processes_with_session_id, provider_process
from pikamux.store import Store
from pikamux.tmux import Tmux


class FakeProvider:
    def __init__(self, name: str, candidates: list[Candidate] | None = None):
        self.name = name
        self.candidates = candidates or []

    def discover(self) -> list[Candidate]:
        return self.candidates

    def import_candidates(self) -> list[Candidate]:
        return self.candidates

    def active_pids(self, _session_id: str) -> list[int]:
        return find_processes_with_session_id(_session_id, self.name)

    def installed(self) -> bool:
        return True

    def is_resumable(self, _session_id: str) -> bool:
        return True

    def new_argv(self, _name: str, _session_id: str | None = None) -> list[str]:
        identity = _session_id or "pending"
        return [
            "bash",
            "-lc",
            (
                f"exec -a {shlex.quote(self.name)} python3 -c "
                f"{shlex.quote('import time; time.sleep(30)')} "
                f"{shlex.quote(identity)}"
            ),
        ]

    def resume_argv(self, _session_id: str) -> list[str]:
        return self.new_argv("resume", _session_id)

    def usage(self, _session, _store):
        return None


@unittest.skipUnless(shutil.which("tmux"), "tmux is required")
class TmuxIntegrationTests(unittest.TestCase):
    def setUp(self) -> None:
        self.socket = "pika-test-" + uuid.uuid4().hex[:10]
        self.tmux = Tmux(self.socket)
        self.temp = tempfile.TemporaryDirectory(dir="/mnt/ebs1/ajain")
        self.store = Store(Path(self.temp.name) / "pika.db")

    def tearDown(self) -> None:
        self.tmux.run("kill-server", check=False)
        self.temp.cleanup()

    @staticmethod
    def wait_for_provider(pane_pid: int, provider: str) -> int | None:
        deadline = time.time() + 3
        while time.time() < deadline:
            pid = provider_process(pane_pid, provider)
            if pid:
                return pid
            time.sleep(0.05)
        return None

    def test_create_tag_capture_and_detect_process(self) -> None:
        pane = self.tmux.create_agent_session(
            tmux_name="pika-c-test",
            cwd="/tmp",
            provider="codex",
            agent_argv=[
                "bash",
                "-lc",
                "printf 'pika-ready\\n'; exec -a codex sleep 30",
            ],
            environment={},
            session_id="uuid-test",
            display_name="integration",
            launch_token=None,
        )
        deadline = time.time() + 3
        while (
            time.time() < deadline and provider_process(pane.pane_pid, "codex") is None
        ):
            time.sleep(0.05)
        panes = self.tmux.list_panes()
        self.assertEqual(len(panes), 1)
        status = self.tmux.run(
            "show-options", "-v", "-t", pane.session_name, "status"
        )
        self.assertEqual(status.stdout.strip(), "off")
        self.assertEqual(panes[0].pika_session_id, "uuid-test")
        self.assertEqual(panes[0].pika_name, "integration")
        self.assertIsNotNone(provider_process(panes[0].pane_pid, "codex"))
        self.assertIn("pika-ready", self.tmux.capture(panes[0].pane_id, 20))

    def test_agent_gets_rich_tui_environment_not_stale_server_values(self) -> None:
        self.tmux.run("new-session", "-d", "-s", "seed", "sleep", "30")
        self.tmux.run("set-environment", "-g", "NO_COLOR", "1")
        self.tmux.run("set-environment", "-g", "PATH", "/stale/tmux/path")
        pane = self.tmux.create_agent_session(
            tmux_name="pika-c-tui-environment",
            cwd="/tmp",
            provider="codex",
            agent_argv=[
                "bash",
                "-c",
                (
                    "printf 'TERM=%s NO_COLOR=%s PATH=%s\\n' "
                    '"$TERM" "${NO_COLOR-unset}" "$PATH"; sleep 30'
                ),
            ],
            environment={},
            session_id="uuid-tui",
            display_name="tui-environment",
            launch_token=None,
        )
        deadline = time.time() + 3
        output = ""
        while time.time() < deadline:
            output = self.tmux.capture(pane.pane_id, 20)
            if "TERM=" in output:
                break
            time.sleep(0.05)
        self.assertIn("TERM=tmux-256color", output)
        self.assertIn("NO_COLOR=unset", output)
        self.assertIn(f"PATH={os.environ['PATH']}", output)
        self.assertNotIn("/stale/tmux/path", output)

    def test_exact_pane_target_selects_its_window_in_multi_window_home(self) -> None:
        exact = self.tmux.create_agent_session(
            tmux_name="pika-c-multi-window",
            cwd="/tmp",
            provider="codex",
            agent_argv=["bash", "-lc", "exec -a codex sleep 30"],
            environment={},
            session_id="99999999-9999-4999-8999-999999999999",
            display_name="exact-window",
            launch_token=None,
        )
        self.tmux.run(
            "new-window",
            "-d",
            "-t",
            exact.session_name,
            "-n",
            "other",
            "sleep 30",
        )
        self.tmux.run("select-window", "-t", f"{exact.session_name}:other")
        before = self.tmux.run(
            "display-message",
            "-p",
            "-t",
            exact.session_name,
            "#{pane_id}",
        ).stdout.strip()
        self.assertNotEqual(before, exact.pane_id)
        self.tmux.run("select-window", "-t", exact.pane_id)
        self.tmux.run("select-pane", "-t", exact.pane_id)
        after = self.tmux.run(
            "display-message",
            "-p",
            "-t",
            exact.session_name,
            "#{pane_id}",
        ).stdout.strip()
        self.assertEqual(after, exact.pane_id)

    def test_open_reuses_then_resurrects_exact_tagged_home(self) -> None:
        session_id = "11111111-1111-4111-8111-111111111111"
        pane = self.tmux.create_agent_session(
            tmux_name="pika-c-resurrect",
            cwd="/tmp",
            provider="codex",
            agent_argv=["bash", "-lc", "exec -a codex sleep 30"],
            environment={},
            session_id=session_id,
            display_name="resurrect",
            launch_token=None,
        )
        self.store.upsert_session(
            Session(
                "codex",
                session_id,
                name="resurrect",
                cwd="/tmp",
                tmux_session=pane.session_name,
                tmux_pane=pane.pane_id,
            )
        )
        initial_pid = self.wait_for_provider(pane.pane_pid, "codex")
        self.assertIsNotNone(initial_pid)
        assert initial_pid is not None
        # Simulate the lifecycle hook's independent UUID-to-PID proof.
        self.store.set_live_owner("codex", session_id, initial_pid)
        pika = Pika(self.store, self.tmux, {"codex": FakeProvider("codex")})
        self.assertEqual(
            pika.open(self.store.get_session("codex", session_id), attach=False),
            0,
        )
        self.assertEqual(len(self.tmux.list_panes()), 1)

        live_pid = provider_process(pane.pane_pid, "codex")
        self.assertIsNotNone(live_pid)
        os.kill(live_pid, signal.SIGTERM)
        deadline = time.time() + 3
        while time.time() < deadline and provider_process(pane.pane_pid, "codex"):
            time.sleep(0.05)
        self.assertIsNone(provider_process(pane.pane_pid, "codex"))

        deadline = time.time() + 6
        current = None
        while time.time() < deadline:
            current = self.tmux.get_pane(pane.pane_id)
            if current and pika._pane_is_idle(current):
                break
            time.sleep(0.05)
        self.assertTrue(current and pika._pane_is_idle(current))

        self.assertEqual(
            pika.open(self.store.get_session("codex", session_id), attach=False), 0
        )
        deadline = time.time() + 3
        while time.time() < deadline and not provider_process(pane.pane_pid, "codex"):
            time.sleep(0.05)
        panes = self.tmux.list_panes()
        self.assertEqual(len(panes), 1)
        self.assertEqual(panes[0].pika_session_id, session_id)
        self.assertIsNotNone(provider_process(panes[0].pane_pid, "codex"))

    def test_open_preserves_busy_saved_pane_and_creates_fresh_home(self) -> None:
        session_id = "33333333-3333-4333-8333-333333333333"
        old = self.tmux.create_agent_session(
            tmux_name="pika-c-busy-original",
            cwd="/tmp",
            provider="codex",
            agent_argv=["bash", "-lc", "exec -a codex sleep 30"],
            environment={},
            session_id=session_id,
            display_name="busy-home",
            launch_token=None,
        )
        self.store.upsert_session(
            Session(
                "codex",
                session_id,
                name="busy-home",
                cwd="/tmp",
                tmux_session=old.session_name,
                tmux_pane=old.pane_id,
            )
        )
        live_pid = self.wait_for_provider(old.pane_pid, "codex")
        self.assertIsNotNone(live_pid)
        assert live_pid is not None
        os.kill(live_pid, signal.SIGTERM)
        pika = Pika(self.store, self.tmux, {"codex": FakeProvider("codex")})
        deadline = time.time() + 3
        while time.time() < deadline:
            current = self.tmux.get_pane(old.pane_id)
            if current and pika._pane_is_idle(current):
                break
            time.sleep(0.05)
        self.tmux.run("send-keys", "-t", old.pane_id, "sleep 30", "Enter")
        deadline = time.time() + 3
        while time.time() < deadline:
            current = self.tmux.get_pane(old.pane_id)
            if current and current.current_command == "sleep":
                break
            time.sleep(0.05)
        current = self.tmux.get_pane(old.pane_id)
        self.assertEqual(current.current_command if current else None, "sleep")

        self.assertEqual(
            pika.open(self.store.get_session("codex", session_id), attach=False), 0
        )
        panes = self.tmux.list_panes()
        self.assertEqual(len(panes), 2)
        preserved = next(item for item in panes if item.pane_id == old.pane_id)
        self.assertEqual(preserved.current_command, "sleep")
        self.assertIsNone(preserved.pika_provider)
        self.assertIsNone(preserved.pika_session_id)
        replacement = next(item for item in panes if item.pane_id != old.pane_id)
        self.assertEqual(replacement.pika_session_id, session_id)
        self.assertIsNotNone(self.wait_for_provider(replacement.pane_pid, "codex"))

    def test_new_binds_provider_uuid_and_adopt_finds_exact_identity(self) -> None:
        provider = FakeProvider("claude")
        pika = Pika(self.store, self.tmux, {"claude": provider})
        with patch("pikamux.core.hooks_installed", return_value=True):
            self.assertEqual(pika.new("new-thread", "claude", "/tmp", attach=False), 0)
        with self.store.connect() as db:
            pending = dict(db.execute("SELECT * FROM pending_launches").fetchone())
        pane = self.tmux.get_pane(str(pending["tmux_pane"]))
        self.assertIsNotNone(pane)
        assert pane is not None
        reserved_id = str(pane.pika_session_id)
        with patch.dict(
            os.environ,
            {
                "PIKA_TMUX_SOCKET": self.socket,
                "PIKA_LAUNCH_TOKEN": str(pending["launch_token"]),
                "TMUX_PANE": pane.pane_id,
            },
            clear=False,
        ):
            handle_hook(
                "claude",
                {
                    "session_id": reserved_id,
                    "cwd": "/tmp",
                    "hook_event_name": "SessionStart",
                },
                self.store,
            )
        bound = self.store.get_session("claude", reserved_id)
        self.assertEqual(bound.name if bound else None, "new-thread")
        self.assertIsNone(self.store.get_pending(str(pending["launch_token"])))
        self.assertEqual(self.tmux.get_pane(pane.pane_id).pika_session_id, reserved_id)

        manual = self.tmux.create_agent_session(
            tmux_name="manual-adopt",
            cwd="/tmp",
            provider="claude",
            agent_argv=["bash", "-lc", "exec -a claude sleep 30"],
            environment={},
            session_id=None,
            display_name="manual",
            launch_token=None,
        )
        manual_pid = self.wait_for_provider(manual.pane_pid, "claude")
        self.assertIsNotNone(manual_pid)
        adopted_id = "22222222-2222-4222-8222-222222222222"
        provider.candidates = [
            Candidate(
                "claude",
                adopted_id,
                name="native-adopt",
                cwd="/tmp",
                live=True,
                pid=manual_pid,
            )
        ]
        adopted = pika.adopt(manual.pane_id)
        self.assertEqual(adopted.session_id, adopted_id)
        self.assertEqual(self.tmux.get_pane(manual.pane_id).pika_session_id, adopted_id)


if __name__ == "__main__":
    unittest.main()
