from __future__ import annotations

import os
import pty
import shlex
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import unittest
import uuid
from pathlib import Path
from unittest.mock import patch

from pikamux.core import Pika
from pikamux.hooks import handle_hook
from pikamux.models import Candidate, Session
from pikamux.processes import (
    cmdline,
    find_processes_with_session_id,
    process_tree,
    provider_process,
)
from pikamux.store import Store
from pikamux.tmux import Tmux, WINDOWS_TERMINAL_DA2_RESPONSE


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
        self.temp = tempfile.TemporaryDirectory()
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

    def wait_for_stably_idle_pane(
        self, pika: Pika, pane_id: str, *, timeout: float = 15
    ):
        """Ignore transient shell-only gaps while a login profile is running."""
        deadline = time.monotonic() + timeout
        idle_since = None
        current = None
        while time.monotonic() < deadline:
            current = self.tmux.get_pane(pane_id)
            if current and pika._pane_is_idle(current):
                idle_since = idle_since or time.monotonic()
                if time.monotonic() - idle_since >= 0.5:
                    return current
            else:
                idle_since = None
            time.sleep(0.05)
        return current

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
        mouse = self.tmux.run(
            "show-options", "-v", "-t", pane.session_name, "mouse"
        )
        self.assertEqual(mouse.stdout.strip(), "on")
        history_limit = self.tmux.run(
            "show-options", "-w", "-v", "-t", pane.pane_id, "history-limit"
        )
        self.assertEqual(history_limit.stdout.strip(), "100000")
        self.assertEqual(panes[0].pika_session_id, "uuid-test")
        self.assertEqual(panes[0].pika_name, "integration")
        self.assertIsNotNone(provider_process(panes[0].pane_pid, "codex"))
        self.assertIn("pika-ready", self.tmux.capture(panes[0].pane_id, 20))

    def test_agent_pane_gets_deep_history_not_the_server_default(self) -> None:
        self.tmux.run("new-session", "-d", "-s", "seed", "sleep", "30")
        self.tmux.run("set-option", "-g", "history-limit", "5")
        command = "for i in {1..30}; do echo retained-$i; done; sleep 30"
        pane = self.tmux.create_agent_session(
            tmux_name="pika-c-deep-history",
            cwd="/tmp",
            provider="codex",
            agent_argv=["bash", "-lc", command],
            environment={},
            session_id="uuid-history",
            display_name="deep-history",
            launch_token=None,
        )
        deadline = time.time() + 10
        output = ""
        while time.time() < deadline:
            output = self.tmux.capture(pane.pane_id, 100)
            if "retained-30" in output:
                break
            time.sleep(0.05)
        self.assertIn("retained-1", output)
        self.assertIn("retained-30", output)

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
        self.assertIn("TERM=tmux-direct", output)
        self.assertIn("NO_COLOR=unset", output)
        self.assertIn(f"PATH={os.environ['PATH']}", output)
        self.assertNotIn("/stale/tmux/path", output)
        features = self.tmux.run(
            "show-options", "-s", "-v", "terminal-features"
        ).stdout
        self.assertIn("xterm*:RGB", features)

    def test_codex_palette_probe_is_answered_inside_isolated_tmux(self) -> None:
        child = """
import os
import select
import time
import tty

tty.setraw(0)
os.write(1, b"\\x1b]10;?\\x1b\\\\\\x1b]11;?\\x1b\\\\")
deadline = time.monotonic() + 1
data = b""
while time.monotonic() < deadline and b"\\x1b]11;" not in data:
    readable, _, _ = select.select([0], [], [], deadline - time.monotonic())
    if not readable:
        break
    data += os.read(0, 512)
os.write(1, b"PALETTE_REPLIES=" + data.hex().encode() + b"\\n")
"""
        with patch.dict(
            os.environ,
            {
                "PIKA_TERMINAL_FOREGROUND": "221,204,187",
                "PIKA_TERMINAL_BACKGROUND": "34,33,51",
            },
            clear=False,
        ):
            pane = self.tmux.create_agent_session(
                tmux_name="pika-c-palette-probe",
                cwd="/tmp",
                provider="codex",
                agent_argv=[sys.executable, "-c", child],
                environment={
                    "PYTHONPATH": str(Path(__file__).resolve().parents[1] / "src")
                },
                session_id="uuid-palette",
                display_name="palette-probe",
                launch_token=None,
            )

        expected = (
            b"\x1b]10;rgb:dddd/cccc/bbbb\x1b\\"
            b"\x1b]11;rgb:2222/2121/3333\x1b\\"
        ).hex()
        deadline = time.time() + 3
        output = ""
        while time.time() < deadline:
            output = self.tmux.capture(pane.pane_id, 20)
            if "PALETTE_REPLIES=" in output:
                break
            time.sleep(0.05)
        self.assertIn(f"PALETTE_REPLIES={expected}", output)

    def test_windows_terminal_da2_reply_never_becomes_pika_input(self) -> None:
        child = """
import os
import time
import tty

tty.setraw(0)
data = b""
while b"after" not in data:
    data += os.read(0, 1024)
os.write(1, b"GOT=" + data.hex().encode() + b"\\n")
time.sleep(2)
"""
        self.tmux.run("new-session", "-d", "-s", "seed", "sleep 20")

        def observed_input(
            session_name: str, *, pika_tagged: bool, through_pika: bool
        ) -> bytes:
            command = f"{sys.executable} -u -c {shlex.quote(child)}"
            self.tmux.run("new-session", "-d", "-s", session_name, command)
            pane = self.tmux.get_pane(session_name)
            self.assertIsNotNone(pane)
            assert pane is not None
            if pika_tagged:
                self.tmux.tag_pane(
                    pane.pane_id,
                    provider="codex",
                    session_id="terminal-reply-probe",
                    name="terminal-reply-probe",
                )

            master_fd, slave_fd = pty.openpty()
            attach_command = self.tmux.command("attach-session", "-t", session_name)
            if through_pika:
                helper = (
                    "from pikamux.tmux import Tmux; "
                    f"raise SystemExit(Tmux({self.socket!r}).attach({session_name!r}))"
                )
                attach_command = [sys.executable, "-c", helper]
            process = subprocess.Popen(
                attach_command,
                stdin=slave_fd,
                stdout=slave_fd,
                stderr=slave_fd,
                env={**os.environ, "TERM": "xterm-256color"},
                close_fds=True,
            )
            os.close(slave_fd)
            try:
                # Let tmux's own capability-request window expire. The bytes
                # below must therefore travel through the root key table, just
                # like a late Windows OpenSSH response that caused the bug.
                time.sleep(3.5)
                os.write(master_fd, b"before")
                response = WINDOWS_TERMINAL_DA2_RESPONSE.encode()
                os.write(master_fd, response[:3])
                time.sleep(0.02)
                os.write(master_fd, response[3:] + b"after")

                deadline = time.time() + 2
                output = ""
                while time.time() < deadline:
                    output = self.tmux.capture(pane.pane_id, 20)
                    if "GOT=" in output:
                        break
                    time.sleep(0.05)
                got = next(
                    line.removeprefix("GOT=")
                    for line in output.splitlines()
                    if line.startswith("GOT=")
                )
                return bytes.fromhex(got)
            finally:
                if process.poll() is None:
                    process.terminate()
                    try:
                        process.wait(timeout=1)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait(timeout=1)
                os.close(master_fd)

        # Newer tmux handles DA2 replies itself. Preserve the user's native
        # behavior on this version rather than assuming it forwards the reply.
        baseline = observed_input(
            "user-before-guard", pika_tagged=False, through_pika=False
        )
        self.assertIn(baseline, (
            b"beforeafter",
            b"before" + WINDOWS_TERMINAL_DA2_RESPONSE.encode() + b"after",
        ))
        self.assertTrue(self.tmux.ensure_pika_terminal_reply_guard())
        self.assertEqual(
            observed_input(
                "pika-c-terminal-reply", pika_tagged=True, through_pika=True
            ),
            b"beforeafter",
        )
        self.assertEqual(
            observed_input(
                "user-terminal-reply", pika_tagged=False, through_pika=False
            ),
            baseline,
        )

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

        # Login-shell initialization can be slower under the full suite (for
        # example while nvm resolves its Node path). Wait for a truly idle pane
        # instead of weakening Pika's live-job safety classification.
        current = self.wait_for_stably_idle_pane(pika, pane.pane_id)
        tree = process_tree(current.pane_pid) if current else []
        self.assertTrue(
            current and pika._pane_is_idle(current),
            f"pane={current!r}; process_tree={[(pid, cmdline(pid)) for pid in tree]!r}",
        )

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
        current = self.wait_for_stably_idle_pane(pika, old.pane_id)
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
