from __future__ import annotations

import os


def main() -> None:
    # Keep the Windows coordinator/bridge import tree free of POSIX-only
    # modules such as fcntl, termios, /proc readers, and tmux integration.
    if os.name == "nt":
        from .client_cli import main as selected
    else:
        from .cli import main as selected

    selected()
