from __future__ import annotations

import argparse
import errno
import fcntl
import os
import pty
import select
import signal
import sys
import termios
import tty
from collections.abc import Sequence

from .terminal_palette import Color
from .terminal_palette import decode_color


def _osc_response(slot: int, color: Color) -> bytes:
    components = "/".join(f"{value * 257:04x}" for value in color)
    return f"\x1b]{slot};rgb:{components}\x1b\\".encode()


class ColorQueryFilter:
    """Remove Codex color queries and return the replies for its private PTY."""

    def __init__(self, foreground: Color, background: Color) -> None:
        self._pending = bytearray()
        self._patterns = {
            b"\x1b]10;?\x1b\\": _osc_response(10, foreground),
            b"\x1b]10;?\x07": _osc_response(10, foreground),
            b"\x1b]11;?\x1b\\": _osc_response(11, background),
            b"\x1b]11;?\x07": _osc_response(11, background),
        }

    def feed(self, data: bytes) -> tuple[bytes, list[bytes]]:
        self._pending.extend(data)
        visible = bytearray()
        replies: list[bytes] = []
        while self._pending:
            found: tuple[int, bytes] | None = None
            for pattern in self._patterns:
                position = self._pending.find(pattern)
                if position >= 0 and (found is None or position < found[0]):
                    found = (position, pattern)
            if found is not None:
                position, pattern = found
                visible.extend(self._pending[:position])
                del self._pending[: position + len(pattern)]
                replies.append(self._patterns[pattern])
                continue

            keep = 0
            for pattern in self._patterns:
                limit = min(len(pattern) - 1, len(self._pending))
                for size in range(limit, 0, -1):
                    if self._pending[-size:] == pattern[:size]:
                        keep = max(keep, size)
                        break
            emit = len(self._pending) - keep
            visible.extend(self._pending[:emit])
            del self._pending[:emit]
            break
        return bytes(visible), replies

    def finish(self) -> bytes:
        remaining = bytes(self._pending)
        self._pending.clear()
        return remaining


def _write_all(fd: int, data: bytes) -> None:
    while data:
        written = os.write(fd, data)
        data = data[written:]


def _copy_terminal_size(source_fd: int, target_fd: int) -> None:
    try:
        size = fcntl.ioctl(source_fd, termios.TIOCGWINSZ, b"\0" * 8)
        fcntl.ioctl(target_fd, termios.TIOCSWINSZ, size)
    except OSError:
        pass


def _forward_signal(pid: int, signum: int) -> None:
    try:
        os.killpg(pid, signum)
    except (ProcessLookupError, PermissionError):
        try:
            os.kill(pid, signum)
        except ProcessLookupError:
            pass


def run_bridge(argv: Sequence[str], foreground: Color, background: Color) -> int:
    if not argv:
        raise ValueError("missing command")
    if not sys.stdin.isatty() or not sys.stdout.isatty():
        return os.spawnvpe(os.P_WAIT, argv[0], list(argv), os.environ)

    pid, master_fd = pty.fork()
    if pid == 0:
        try:
            os.execvpe(argv[0], list(argv), os.environ)
        except OSError as exc:
            os.write(2, f"pika: cannot start {argv[0]}: {exc}\n".encode())
            os._exit(127)

    input_fd = sys.stdin.fileno()
    output_fd = sys.stdout.fileno()
    previous_mode = termios.tcgetattr(input_fd)
    query_filter = ColorQueryFilter(foreground, background)
    resized = True
    input_open = True
    master_open = True
    status: int | None = None

    def on_resize(_signum: int, _frame: object) -> None:
        nonlocal resized
        resized = True

    forwarded = (signal.SIGHUP, signal.SIGINT, signal.SIGQUIT, signal.SIGTERM)
    previous_handlers = {signum: signal.getsignal(signum) for signum in forwarded}
    previous_resize = signal.getsignal(signal.SIGWINCH)
    signal.signal(signal.SIGWINCH, on_resize)
    for signum in forwarded:
        signal.signal(signum, lambda received, _frame: _forward_signal(pid, received))

    try:
        tty.setraw(input_fd, when=termios.TCSANOW)
        while master_open:
            if resized:
                _copy_terminal_size(input_fd, master_fd)
                _forward_signal(pid, signal.SIGWINCH)
                resized = False

            readers = [master_fd]
            if input_open:
                readers.append(input_fd)
            try:
                readable, _, _ = select.select(readers, [], [], 0.1)
            except InterruptedError:
                readable = []

            if input_fd in readable:
                data = os.read(input_fd, 4096)
                if data:
                    _write_all(master_fd, data)
                else:
                    input_open = False

            if master_fd in readable:
                try:
                    data = os.read(master_fd, 65536)
                except OSError as exc:
                    if exc.errno != errno.EIO:
                        raise
                    data = b""
                if not data:
                    master_open = False
                else:
                    visible, replies = query_filter.feed(data)
                    if visible:
                        _write_all(output_fd, visible)
                    for reply in replies:
                        _write_all(master_fd, reply)

            if status is None:
                waited_pid, candidate = os.waitpid(pid, os.WNOHANG)
                if waited_pid == pid:
                    status = candidate

        trailing = query_filter.finish()
        if trailing:
            _write_all(output_fd, trailing)
        if status is None:
            _, status = os.waitpid(pid, 0)
    finally:
        for signum, handler in previous_handlers.items():
            signal.signal(signum, handler)
        signal.signal(signal.SIGWINCH, previous_resize)
        termios.tcsetattr(input_fd, termios.TCSADRAIN, previous_mode)
        os.close(master_fd)

    return os.waitstatus_to_exitcode(status)


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(add_help=False)
    parser.add_argument("--foreground", required=True)
    parser.add_argument("--background", required=True)
    parser.add_argument("command", nargs=argparse.REMAINDER)
    return parser


def main() -> None:
    args = _parser().parse_args()
    foreground = decode_color(args.foreground)
    background = decode_color(args.background)
    command = args.command[1:] if args.command[:1] == ["--"] else args.command
    if foreground is None or background is None or not command:
        raise SystemExit(2)
    raise SystemExit(run_bridge(command, foreground, background))


if __name__ == "__main__":
    main()
