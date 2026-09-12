from __future__ import annotations

import os
import re
import select
import sys
import termios
import time
import tty
from dataclasses import dataclass
from typing import Mapping


FOREGROUND_ENV = "PIKA_TERMINAL_FOREGROUND"
BACKGROUND_ENV = "PIKA_TERMINAL_BACKGROUND"

_OSC_DEFAULT_COLOR_QUERY = b"\x1b]10;?\x1b\\\x1b]11;?\x1b\\"
_OSC_COLOR_RESPONSE = re.compile(
    rb"\x1b\](10|11);(?:rgb|rgba):"
    rb"([0-9a-fA-F]{2}|[0-9a-fA-F]{4})/"
    rb"([0-9a-fA-F]{2}|[0-9a-fA-F]{4})/"
    rb"([0-9a-fA-F]{2}|[0-9a-fA-F]{4})"
    rb"(?:/[0-9a-fA-F]{2,4})?(?:\x07|\x1b\\)"
)


Color = tuple[int, int, int]


@dataclass(frozen=True, slots=True)
class TerminalPalette:
    foreground: Color
    background: Color


def encode_color(color: Color) -> str:
    return ",".join(str(component) for component in color)


def decode_color(value: str | None) -> Color | None:
    if not value:
        return None
    try:
        parts = tuple(int(component) for component in value.split(","))
    except ValueError:
        return None
    if len(parts) != 3 or any(component < 0 or component > 255 for component in parts):
        return None
    return parts


def palette_from_environment(
    environment: Mapping[str, str] | None = None,
) -> TerminalPalette | None:
    if environment is None:
        environment = os.environ
    foreground = decode_color(environment.get(FOREGROUND_ENV))
    background = decode_color(environment.get(BACKGROUND_ENV))
    if foreground is None or background is None:
        return None
    return TerminalPalette(foreground, background)


def _component(value: bytes) -> int:
    parsed = int(value, 16)
    return parsed if len(value) == 2 else parsed // 257


def parse_palette_response(data: bytes) -> TerminalPalette | None:
    colors: dict[int, Color] = {}
    for match in _OSC_COLOR_RESPONSE.finditer(data):
        colors[int(match.group(1))] = (
            _component(match.group(2)),
            _component(match.group(3)),
            _component(match.group(4)),
        )
    if 10 not in colors or 11 not in colors:
        return None
    return TerminalPalette(colors[10], colors[11])


def probe_terminal_palette(timeout: float = 0.12) -> TerminalPalette | None:
    """Ask the directly attached terminal for its default foreground/background."""
    try:
        input_fd = sys.stdin.fileno()
        output_fd = sys.stdout.fileno()
    except (AttributeError, OSError, ValueError):
        return None
    if not os.isatty(input_fd) or not os.isatty(output_fd):
        return None

    try:
        previous = termios.tcgetattr(input_fd)
    except (OSError, termios.error):
        return None

    buffer = bytearray()
    deadline = time.monotonic() + timeout
    try:
        tty.setraw(input_fd, when=termios.TCSANOW)
        os.write(output_fd, _OSC_DEFAULT_COLOR_QUERY)
        while time.monotonic() < deadline:
            remaining = max(0.0, deadline - time.monotonic())
            readable, _, _ = select.select([input_fd], [], [], remaining)
            if not readable:
                break
            chunk = os.read(input_fd, 256)
            if not chunk:
                break
            buffer.extend(chunk)
            palette = parse_palette_response(bytes(buffer))
            if palette is not None:
                return palette
    except (OSError, ValueError, termios.error):
        return None
    finally:
        try:
            termios.tcsetattr(input_fd, termios.TCSADRAIN, previous)
        except (OSError, termios.error):
            pass
    return None


def terminal_palette_environment() -> dict[str, str]:
    """Return inherited or freshly probed palette values for a Pika child."""
    palette = palette_from_environment()
    if palette is None:
        palette = probe_terminal_palette()
    if palette is None:
        return {}
    return {
        FOREGROUND_ENV: encode_color(palette.foreground),
        BACKGROUND_ENV: encode_color(palette.background),
    }
