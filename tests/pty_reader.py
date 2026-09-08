"""Consume PTY output like a terminal, including on Darwin's small buffers."""
import os
import select
import threading
import time


class PtyReader:
    def __init__(self, fd):
        self.fd = fd
        self.data = bytearray()
        self.condition = threading.Condition()
        self.stop = threading.Event()
        self.thread = threading.Thread(target=self._drain, daemon=True)
        self.thread.start()

    def _drain(self):
        while not self.stop.is_set():
            try:
                if not select.select([self.fd], [], [], .02)[0]:
                    continue
                chunk = os.read(self.fd, 65536)
                if not chunk:
                    break
                with self.condition:
                    self.data.extend(chunk)
                    self.condition.notify_all()
            except OSError:
                break

    def until(self, needle, timeout=1):
        needles = (needle,) if isinstance(needle, bytes) else needle
        deadline = time.monotonic() + timeout
        with self.condition:
            while not all(value in self.data for value in needles):
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise AssertionError(f"Missing {needle!r} in {self.data[-1000:]!r}")
                self.condition.wait(remaining)
            end = max(self.data.index(value) + len(value) for value in needles)
            result = bytes(self.data[:end])
            del self.data[:end]
            return result

    def close(self):
        self.stop.set()
        self.thread.join(1)
        assert not self.thread.is_alive()
