from __future__ import annotations

import argparse
import unittest
from unittest.mock import Mock, patch

from pikamux.cli import _peek_popup
from pikamux.core import Pika, PikaError
from pikamux.models import Pane, Session


class CaptureIdentityTests(unittest.TestCase):
    def setUp(self) -> None:
        self.session = Session("codex", "exact-uuid", tmux_pane="%1")
        self.pane = Pane(
            "home", "%1", 123, "/tmp", "codex", False, False, None, 1, 1,
            pika_provider="codex", pika_session_id="exact-uuid",
        )
        self.pika = Pika.__new__(Pika)
        self.pika.tmux = Mock()
        self.pika.tmux.get_pane.return_value = self.pane
        self.pika.tmux.capture.return_value = "exact output"
        self.pika.exact_pane_pid = Mock(return_value=123)

    def test_exact_owner_is_revalidated_before_capture(self) -> None:
        self.assertEqual(self.pika.capture(self.session, 12), "exact output")
        self.pika.exact_pane_pid.assert_called_once_with(self.session, self.pane)
        self.pika.tmux.capture.assert_called_once_with("%1", 12)

    def test_reused_pane_with_foreign_tag_never_reads_output(self) -> None:
        self.pane.pika_session_id = "foreign-uuid"
        with self.assertRaisesRegex(PikaError, "No output was read"):
            self.pika.capture(self.session, 12)
        self.pika.tmux.capture.assert_not_called()

    def test_missing_identity_or_reused_pid_never_reads_output(self) -> None:
        self.pika.exact_pane_pid.return_value = None
        with self.assertRaisesRegex(PikaError, "No output was read"):
            self.pika.capture(self.session, 12)
        self.pika.tmux.capture.assert_not_called()

    def test_popup_rejects_changed_pane_before_capture(self) -> None:
        self.pika.store = Mock()
        self.pika.store.get_session.return_value = self.session
        args = argparse.Namespace(provider="codex", session_id="exact-uuid", target="%2", lines=12)
        with patch("pikamux.cli.Pika", return_value=self.pika):
            with self.assertRaisesRegex(PikaError, "pane changed"):
                _peek_popup(args)
        self.pika.tmux.capture.assert_not_called()

    def test_popup_revalidates_exact_identity_before_printing(self) -> None:
        self.pika.store = Mock()
        self.pika.store.get_session.return_value = self.session
        self.pika.exact_pane_pid.return_value = None
        args = argparse.Namespace(provider="codex", session_id="exact-uuid", target="%1", lines=12)
        with patch("pikamux.cli.Pika", return_value=self.pika), patch("builtins.print") as output:
            with self.assertRaises(PikaError):
                _peek_popup(args)
        output.assert_not_called()
        self.pika.tmux.capture.assert_not_called()
