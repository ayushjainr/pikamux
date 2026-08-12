from __future__ import annotations

import unittest
from unittest.mock import patch

from pikamux.terminal_palette import BACKGROUND_ENV
from pikamux.terminal_palette import FOREGROUND_ENV
from pikamux.terminal_palette import TerminalPalette
from pikamux.terminal_palette import decode_color
from pikamux.terminal_palette import palette_from_environment
from pikamux.terminal_palette import parse_palette_response
from pikamux.terminal_palette import terminal_palette_environment


class TerminalPaletteTests(unittest.TestCase):
    def test_parses_eight_and_sixteen_bit_osc_responses_in_any_order(self) -> None:
        response = (
            b"noise\x1b]11;rgb:2222/2121/3333\x1b\\"
            b"\x1b]10;rgb:dd/cc/bb\x07"
        )
        self.assertEqual(
            parse_palette_response(response),
            TerminalPalette((221, 204, 187), (34, 33, 51)),
        )

    def test_rejects_missing_or_invalid_environment_palette(self) -> None:
        self.assertIsNone(decode_color("1,2"))
        self.assertIsNone(decode_color("1,2,999"))
        self.assertIsNone(
            palette_from_environment(
                {FOREGROUND_ENV: "221,204,187", BACKGROUND_ENV: "invalid"}
            )
        )

    def test_inherited_palette_avoids_a_probe_inside_tmux(self) -> None:
        environment = {
            FOREGROUND_ENV: "221,204,187",
            BACKGROUND_ENV: "34,33,51",
        }
        with (
            patch.dict("os.environ", environment, clear=True),
            patch(
                "pikamux.terminal_palette.probe_terminal_palette"
            ) as probe,
        ):
            self.assertEqual(terminal_palette_environment(), environment)
        probe.assert_not_called()
