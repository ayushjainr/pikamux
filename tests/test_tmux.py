from __future__ import annotations

import os
import subprocess
import unittest
from unittest.mock import patch

from pikamux.tmux import Tmux, TmuxError, WINDOWS_TERMINAL_DA2_RESPONSE


class TmuxTests(unittest.TestCase):
    def test_escaped_tmux_inventory_preserves_exact_tags_and_backslashes(self) -> None:
        fields = ["home", "%7", "123", r"/tmp/back\slash", "bash",
                  "1", "1", "1", "0", "", "10", "5", "codex", "uuid", "name", "token"]
        for separator in ("\x1f", r"\037"):
            with self.subTest(separator=repr(separator)):
                output = separator.join(fields) + "\n"
                result = subprocess.CompletedProcess([], 0, output, "")
                with patch.object(Tmux, "run", return_value=result):
                    panes = Tmux("test").list_panes()
                self.assertEqual(len(panes), 1)
                self.assertEqual(panes[0].pane_id, "%7")
                self.assertEqual(panes[0].cwd, r"/tmp/back\slash")
                self.assertEqual(panes[0].pika_session_id, "uuid")
                self.assertEqual(panes[0].pika_launch_token, "token")

    def test_ambiguous_escaped_inventory_cannot_assign_an_exact_identity(self) -> None:
        fields = ["home", "%7", "123", "/tmp", "bash", "0", "1", "1",
                  "0", "", "10", "5", "codex", "uuid", r"name\037extra", "token"]
        result = subprocess.CompletedProcess([], 0, r"\037".join(fields) + "\n", "")
        with patch.object(Tmux, "run", return_value=result):
            self.assertEqual(Tmux("test").list_panes(), [])

    def test_modern_tmux_client_inventory_routes_receipt_to_exact_client(self) -> None:
        def run(_self, *args, **_kwargs):
            output = r"client-one\037123" + "\n" if args[0] == "list-clients" else ""
            return subprocess.CompletedProcess([], 0, output, "")
        with patch.object(Tmux, "run", autospec=True, side_effect=run) as called:
            Tmux("test")._display_to_client(123, "exact receipt")
        self.assertTrue(any(call.args[1:4] == ("display-message", "-c", "client-one")
                            for call in called.call_args_list))

    def test_all_provider_homes_are_recognized_as_pika_sessions(self) -> None:
        self.assertTrue(Tmux.is_pika_session("pika-c-codex"))
        self.assertTrue(Tmux.is_pika_session("pika-a-claude"))
        self.assertTrue(Tmux.is_pika_session("pika-o-opencode"))
        self.assertFalse(Tmux.is_pika_session("my-pika-o-opencode"))

    def test_pane_inventory_distinguishes_no_server_from_query_failure(self) -> None:
        tmux = Tmux("test")
        no_server = subprocess.CompletedProcess(
            ["tmux"], 1, "", "no server running on /tmp/tmux-test"
        )
        with patch.object(Tmux, "run", return_value=no_server):
            self.assertEqual(tmux.list_panes(), [])

        failed = subprocess.CompletedProcess(
            ["tmux"], 1, "", "permission denied"
        )
        with (
            patch.object(Tmux, "run", return_value=failed),
            self.assertRaisesRegex(TmuxError, "permission denied"),
        ):
            tmux.list_panes()

    def test_agent_wrapper_restores_interactive_tui_environment(self) -> None:
        with patch.dict(
            os.environ,
            {
                "PATH": "/caller/bin",
                "TERM": "dumb",
                "NO_COLOR": "1",
                "CODEX_THREAD_ID": "unrelated-parent",
            },
            clear=True,
        ):
            wrapper = Tmux("test")._agent_wrapper(
                "codex",
                ["codex", "resume", "uuid"],
                {"PIKA_SESSION_ID": "uuid"},
                "uuid",
                None,
            )
        self.assertIn("env -u NO_COLOR", wrapper)
        self.assertIn("-u CODEX_THREAD_ID", wrapper)
        self.assertNotIn("CODEX_THREAD_ID=unrelated-parent", wrapper)
        self.assertIn("PATH=/caller/bin", wrapper)
        self.assertIn("TERM=tmux-direct", wrapper)
        self.assertIn("codex resume uuid", wrapper)

    def test_rgb_capability_is_added_once(self) -> None:
        calls: list[tuple[str, ...]] = []

        def run(_self, *args, **_kwargs):
            calls.append(args)
            existing = args[:4] == (
                "show-options",
                "-s",
                "-v",
                "terminal-features",
            )
            stdout = "xterm*:focus:title\n" if existing else ""
            return subprocess.CompletedProcess(["tmux"], 0, stdout, "")

        with patch.object(Tmux, "run", new=run):
            Tmux("test").ensure_pika_rgb()
        self.assertIn(
            ("set-option", "-as", "terminal-features", "xterm*:RGB"), calls
        )

        calls.clear()

        def already_rgb(_self, *args, **_kwargs):
            calls.append(args)
            return subprocess.CompletedProcess(
                ["tmux"], 0, "xterm*:focus:title\nxterm*:RGB\n", ""
            )

        with patch.object(Tmux, "run", new=already_rgb):
            Tmux("test").ensure_pika_rgb()
        self.assertFalse(any(call and call[0] == "set-option" for call in calls))

    def test_terminal_reply_guard_uses_only_an_unowned_user_key(self) -> None:
        calls: list[tuple[str, ...]] = []

        def run(_self, *args, **_kwargs):
            calls.append(args)
            stdout = ""
            if args == ("show-options", "-s", "user-keys"):
                stdout = "user-keys[199] custom-sequence\n"
            elif args == ("list-keys", "-T", "root"):
                stdout = "bind-key -T root User198 display-message user-owned\n"
            elif args == (
                "show-options",
                "-s",
                "-v",
                "user-keys[199]",
            ):
                stdout = "custom-sequence\n"
            return subprocess.CompletedProcess(["tmux"], 0, stdout, "")

        with patch.object(Tmux, "run", new=run):
            self.assertTrue(Tmux("test").ensure_pika_terminal_reply_guard())

        self.assertIn(
            (
                "set-option",
                "-s",
                "user-keys[197]",
                WINDOWS_TERMINAL_DA2_RESPONSE,
            ),
            calls,
        )
        self.assertIn(
            ("set-option", "-s", "@pika_terminal_reply_key", "197"), calls
        )
        binding = next(call for call in calls if call[:3] == ("bind-key", "-T", "root"))
        self.assertEqual(binding[3:8], ("User197", "if-shell", "-F", "#{@pika_provider}", ""))
        self.assertIn(WINDOWS_TERMINAL_DA2_RESPONSE, binding[8])
        self.assertFalse(
            any(
                call[:3] == ("set-option", "-s", "user-keys[199]")
                or call[:3] == ("set-option", "-s", "user-keys[198]")
                for call in calls
            )
        )

    def test_terminal_reply_guard_reuses_its_claim_idempotently(self) -> None:
        calls: list[tuple[str, ...]] = []

        def run(_self, *args, **_kwargs):
            calls.append(args)
            stdout = ""
            if args == (
                "show-options",
                "-s",
                "-v",
                "@pika_terminal_reply_key",
            ):
                stdout = "199\n"
            elif args == ("show-options", "-s", "user-keys"):
                stdout = "user-keys[199] existing-pika-value\n"
            elif args == (
                "show-options",
                "-s",
                "-v",
                "user-keys[199]",
            ):
                stdout = reply + "\n"
            return subprocess.CompletedProcess(["tmux"], 0, stdout, "")

        for reply in (WINDOWS_TERMINAL_DA2_RESPONSE, r"\033[>0;10;1c"):
            calls.clear()
            with self.subTest(reply=repr(reply)), patch.object(Tmux, "run", new=run):
                self.assertTrue(Tmux("test").ensure_pika_terminal_reply_guard())
            self.assertFalse(any(call and call[0] == "set-option" for call in calls))
            self.assertTrue(any(call and call[0] == "bind-key" for call in calls))

    def test_agent_wrapper_preserves_explicit_interactive_no_color(self) -> None:
        with patch.dict(
            os.environ,
            {"PATH": "/caller/bin", "TERM": "xterm-256color", "NO_COLOR": "1"},
            clear=True,
        ):
            wrapper = Tmux("test")._agent_wrapper(
                "claude", ["claude"], {}, None, "token"
            )
        self.assertNotIn("-u NO_COLOR", wrapper)
        self.assertIn("NO_COLOR=1", wrapper)

    def test_agent_wrapper_reports_exact_owner_token_on_exit(self) -> None:
        with patch.dict(os.environ, {"PATH": "/caller/bin"}, clear=True):
            wrapper = Tmux("test")._agent_wrapper(
                "codex",
                ["codex", "resume", "uuid"],
                {"PIKA_OWNER_TOKEN": "owner-123"},
                "uuid",
                None,
            )
        self.assertIn("PIKA_OWNER_TOKEN=owner-123", wrapper)
        self.assertIn("--owner-token owner-123", wrapper)

    def test_codex_uses_palette_bridge_when_outer_colors_are_known(self) -> None:
        with patch.dict(
            os.environ,
            {
                "PATH": "/caller/bin",
                "TERM": "xterm-256color",
                "PIKA_TERMINAL_FOREGROUND": "221,204,187",
                "PIKA_TERMINAL_BACKGROUND": "34,33,51",
            },
            clear=True,
        ):
            wrapper = Tmux("test")._agent_wrapper(
                "codex", ["codex", "resume", "uuid"], {}, "uuid", None
            )
        self.assertIn("pikamux.terminal_bridge", wrapper)
        self.assertIn("--foreground 221,204,187", wrapper)
        self.assertIn("--background 34,33,51", wrapper)
        self.assertIn("-- codex resume uuid", wrapper)
        self.assertIn(
            "exec env PIKA_TERMINAL_FOREGROUND=221,204,187 "
            "PIKA_TERMINAL_BACKGROUND=34,33,51",
            wrapper,
        )

    def test_claude_does_not_need_the_codex_palette_bridge(self) -> None:
        with patch.dict(
            os.environ,
            {
                "PATH": "/caller/bin",
                "TERM": "xterm-256color",
                "PIKA_TERMINAL_FOREGROUND": "221,204,187",
                "PIKA_TERMINAL_BACKGROUND": "34,33,51",
            },
            clear=True,
        ):
            wrapper = Tmux("test")._agent_wrapper(
                "claude", ["claude"], {}, None, "token"
            )
        self.assertNotIn("pikamux.terminal_bridge", wrapper)
        self.assertIn(" claude; pika_rc=$?", wrapper)

    def test_attached_means_the_exact_pika_pane_is_visible(self) -> None:
        separator = "\x1f"
        common = [
            "home",
            "%1",
            "123",
            "/tmp",
            "bash",
        ]
        tail = ["0", "", "1", "1", "codex", "uuid", "named", ""]
        hidden = separator.join(common + ["1", "0", "1"] + tail)
        visible = separator.join(
            ["home", "%2", "456", "/tmp", "bash", "1", "1", "1"] + tail
        )
        result = subprocess.CompletedProcess(
            ["tmux"], returncode=0, stdout=hidden + "\n" + visible + "\n", stderr=""
        )
        tmux = Tmux("test")
        with patch.object(Tmux, "run", return_value=result):
            panes = tmux.list_panes()
        self.assertEqual(len(panes), 2)
        self.assertFalse(panes[0].attached)
        self.assertTrue(panes[1].attached)

    def test_inside_tmux_attach_displays_literal_threshold_receipt(self) -> None:
        calls: list[tuple[str, ...]] = []

        def run(_self, *args, **_kwargs):
            calls.append(args)
            stdout = "client-1\n" if args[:2] == ("display-message", "-p") else ""
            return subprocess.CompletedProcess(["tmux"], 0, stdout, "")

        attached: list[bool] = []
        with (
            patch.object(Tmux, "run", new=run),
            patch.dict(os.environ, {"TMUX": "socket,1,0"}, clear=False),
        ):
            result = Tmux("test").attach(
                "pika-c-home",
                target_pane="%7",
                on_attached=lambda: attached.append(True),
                receipt="Pika → exact #thread · ATTACHED LIVE",
            )
        self.assertEqual(result, 0)
        self.assertEqual(attached, [True])
        self.assertIn(
            ("set-option", "-t", "pika-c-home", "status", "off"), calls
        )
        self.assertIn(
            ("set-option", "-t", "pika-c-home", "mouse", "on"), calls
        )
        self.assertIn(
            (
                "set-option",
                "-w",
                "-t",
                "%7",
                "history-limit",
                "100000",
            ),
            calls,
        )
        self.assertIn(("select-window", "-t", "%7"), calls)
        self.assertIn(("select-pane", "-t", "%7"), calls)
        self.assertIn(
            (
                "display-message",
                "-c",
                "client-1",
                "-d",
                "3000",
                "-l",
                "Pika → exact #thread · ATTACHED LIVE",
            ),
            calls,
        )

    def test_attach_evaluates_receipt_after_successful_attached_callback(self) -> None:
        calls: list[tuple[str, ...]] = []
        receipt = ["before"]

        def run(_self, *args, **_kwargs):
            calls.append(args)
            stdout = "client-1\n" if args[:2] == ("display-message", "-p") else ""
            return subprocess.CompletedProcess(["tmux"], 0, stdout, "")

        with (
            patch.object(Tmux, "run", new=run),
            patch.dict(os.environ, {"TMUX": "socket,1,0"}, clear=False),
        ):
            result = Tmux("test").attach(
                "pika-c-home",
                target_pane="%7",
                on_attached=lambda: receipt.__setitem__(0, "after"),
                receipt=lambda: receipt[0],
            )
        self.assertEqual(result, 0)
        self.assertTrue(
            any(call[-2:] == ("-l", "after") for call in calls)
        )

    def test_adopted_user_session_keeps_its_status_configuration(self) -> None:
        calls: list[tuple[str, ...]] = []

        def run(_self, *args, **_kwargs):
            calls.append(args)
            return subprocess.CompletedProcess(["tmux"], 0, "", "")

        with (
            patch.object(Tmux, "run", new=run),
            patch.object(Tmux, "ensure_pika_terminal_reply_guard", return_value=True),
            patch.dict(os.environ, {"TMUX": "socket,1,0"}, clear=False),
        ):
            self.assertEqual(Tmux("test").attach("my-existing-session"), 0)
        self.assertFalse(any(call and call[0] == "set-option" for call in calls))


if __name__ == "__main__":
    unittest.main()
