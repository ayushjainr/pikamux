from __future__ import annotations

import os
import subprocess
import unittest
from unittest.mock import patch

from pikamux.tmux import Tmux


class TmuxTests(unittest.TestCase):
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

    def test_adopted_user_session_keeps_its_status_configuration(self) -> None:
        calls: list[tuple[str, ...]] = []

        def run(_self, *args, **_kwargs):
            calls.append(args)
            return subprocess.CompletedProcess(["tmux"], 0, "", "")

        with (
            patch.object(Tmux, "run", new=run),
            patch.dict(os.environ, {"TMUX": "socket,1,0"}, clear=False),
        ):
            self.assertEqual(Tmux("test").attach("my-existing-session"), 0)
        self.assertFalse(any(call and call[0] == "set-option" for call in calls))


if __name__ == "__main__":
    unittest.main()
