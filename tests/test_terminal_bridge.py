from __future__ import annotations

import errno
import os
import pty
import select
import subprocess
import sys
import time
import unittest

from pikamux.terminal_bridge import ColorQueryFilter


class TerminalBridgeTests(unittest.TestCase):
    def test_filters_split_color_queries_and_returns_exact_replies(self) -> None:
        bridge = ColorQueryFilter((221, 204, 187), (34, 33, 51))

        visible_1, replies_1 = bridge.feed(b"before\x1b]10;")
        visible_2, replies_2 = bridge.feed(
            b"?\x1b\\middle\x1b]11;?\x1b\\after"
        )

        self.assertEqual(visible_1 + visible_2 + bridge.finish(), b"beforemiddleafter")
        self.assertEqual(replies_1, [])
        self.assertEqual(
            replies_2,
            [
                b"\x1b]10;rgb:dddd/cccc/bbbb\x1b\\",
                b"\x1b]11;rgb:2222/2121/3333\x1b\\",
            ],
        )

    def test_unrelated_osc_sequences_pass_through_unchanged(self) -> None:
        bridge = ColorQueryFilter((255, 255, 255), (0, 0, 0))
        value = b"\x1b]52;c;clipboard\x07"
        visible, replies = bridge.feed(value)
        self.assertEqual(visible + bridge.finish(), value)
        self.assertEqual(replies, [])

    def test_real_pty_child_receives_palette_replies(self) -> None:
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
os.write(1, b"REPLIES=" + data.hex().encode() + b"\\n")
"""
        master_fd, slave_fd = pty.openpty()
        process = subprocess.Popen(
            [
                sys.executable,
                "-m",
                "pikamux.terminal_bridge",
                "--foreground",
                "221,204,187",
                "--background",
                "34,33,51",
                "--",
                sys.executable,
                "-c",
                child,
            ],
            stdin=slave_fd,
            stdout=slave_fd,
            stderr=slave_fd,
            close_fds=True,
        )
        os.close(slave_fd)
        output = bytearray()
        deadline = time.monotonic() + 3
        try:
            while time.monotonic() < deadline:
                readable, _, _ = select.select([master_fd], [], [], 0.1)
                if readable:
                    try:
                        chunk = os.read(master_fd, 4096)
                    except OSError as exc:
                        if exc.errno != errno.EIO:
                            raise
                        break
                    if not chunk:
                        break
                    output.extend(chunk)
                if process.poll() is not None:
                    break
            self.assertEqual(process.wait(timeout=1), 0)
        finally:
            if process.poll() is None:
                process.kill()
                process.wait()
            os.close(master_fd)

        expected = (
            b"\x1b]10;rgb:dddd/cccc/bbbb\x1b\\"
            b"\x1b]11;rgb:2222/2121/3333\x1b\\"
        )
        self.assertIn(b"REPLIES=" + expected.hex().encode(), output)
        self.assertNotIn(b"\x1b]10;?", output)
