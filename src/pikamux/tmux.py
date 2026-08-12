from __future__ import annotations

import os
import shlex
import subprocess
import sys
from collections.abc import Callable, Iterable
from dataclasses import dataclass

from .models import Pane
from .terminal_palette import BACKGROUND_ENV
from .terminal_palette import FOREGROUND_ENV
from .terminal_palette import palette_from_environment
from .terminal_palette import terminal_palette_environment


class TmuxError(RuntimeError):
    pass


PIKA_HISTORY_LIMIT = 100_000


@dataclass(slots=True)
class Tmux:
    socket_name: str | None = None

    def __post_init__(self) -> None:
        if self.socket_name is None:
            self.socket_name = os.environ.get("PIKA_TMUX_SOCKET")

    def command(self, *args: str) -> list[str]:
        command = ["tmux"]
        if self.socket_name:
            command.extend(["-L", self.socket_name])
        command.extend(args)
        return command

    def run(
        self,
        *args: str,
        check: bool = True,
        capture_output: bool = True,
        timeout: float = 5,
    ) -> subprocess.CompletedProcess[str]:
        try:
            proc = subprocess.run(
                self.command(*args),
                text=True,
                capture_output=capture_output,
                check=False,
                timeout=timeout,
            )
        except (OSError, subprocess.TimeoutExpired) as exc:
            raise TmuxError(str(exc)) from exc
        if check and proc.returncode:
            message = (
                proc.stderr.strip() or proc.stdout.strip() or "tmux command failed"
            )
            raise TmuxError(message)
        return proc

    def available(self) -> bool:
        try:
            return self.run("-V", check=False).returncode == 0
        except TmuxError:
            return False

    def server_running(self) -> bool:
        try:
            return self.run("list-sessions", check=False).returncode == 0
        except TmuxError:
            return False

    def list_panes(self) -> list[Pane]:
        separator = "\x1f"
        fmt = separator.join(
            [
                "#{session_name}",
                "#{pane_id}",
                "#{pane_pid}",
                "#{pane_current_path}",
                "#{pane_current_command}",
                "#{session_attached}",
                "#{window_active}",
                "#{pane_active}",
                "#{pane_dead}",
                "#{pane_dead_status}",
                "#{session_activity}",
                "#{session_created}",
                "#{@pika_provider}",
                "#{@pika_session_id}",
                "#{@pika_name}",
                "#{@pika_launch_token}",
            ]
        )
        proc = self.run("list-panes", "-a", "-F", fmt, check=False)
        if proc.returncode:
            message = proc.stderr.strip() or proc.stdout.strip()
            no_server = (
                "no server running" in message
                or "failed to connect to server" in message
                or (
                    "error connecting to " in message
                    and "No such file or directory" in message
                )
            )
            if no_server:
                return []
            raise TmuxError(message or "tmux pane inventory failed")
        panes: list[Pane] = []
        for line in proc.stdout.splitlines():
            parts = line.split(separator)
            if len(parts) != 16:
                continue
            try:
                panes.append(
                    Pane(
                        session_name=parts[0],
                        pane_id=parts[1],
                        pane_pid=int(parts[2]),
                        cwd=parts[3],
                        current_command=parts[4],
                        # An attached session may have this pane hidden behind
                        # another window or split. Only its selected pane counts.
                        attached=(
                            parts[5] != "0"
                            and parts[6] != "0"
                            and parts[7] != "0"
                        ),
                        dead=parts[8] != "0",
                        dead_status=int(parts[9]) if parts[9] else None,
                        activity=float(parts[10] or 0),
                        created=float(parts[11] or 0),
                        pika_provider=parts[12] or None,
                        pika_session_id=parts[13] or None,
                        pika_name=parts[14] or None,
                        pika_launch_token=parts[15] or None,
                    )
                )
            except ValueError:
                continue
        return panes

    def get_pane(self, target: str) -> Pane | None:
        for pane in self.list_panes():
            if target in {
                pane.pane_id,
                pane.session_name,
                f"{pane.session_name}:{pane.pane_id}",
            }:
                return pane
        return None

    def tag_pane(
        self,
        target: str,
        *,
        provider: str | None = None,
        session_id: str | None = None,
        name: str | None = None,
        launch_token: str | None = None,
    ) -> None:
        values = {
            "@pika_provider": provider,
            "@pika_session_id": session_id,
            "@pika_name": name,
            "@pika_launch_token": launch_token,
        }
        for option, value in values.items():
            if value is not None:
                self.run("set-option", "-p", "-t", target, option, value)

    def clear_pika_tags(self, target: str) -> None:
        for option in (
            "@pika_provider",
            "@pika_session_id",
            "@pika_name",
            "@pika_launch_token",
        ):
            self.run("set-option", "-p", "-u", "-t", target, option, check=False)

    @staticmethod
    def is_pika_session(name: str) -> bool:
        """Identify tmux homes created by Pika, not user-owned adopted sessions."""
        return name.startswith(("pika-c-", "pika-a-"))

    def hide_pika_status(self, session_name: str) -> None:
        """Keep Pika's tmux transport visually transparent to the agent UI."""
        if self.is_pika_session(session_name):
            # `status` is a per-session option here; the user's global tmux
            # theme and explicitly adopted tmux sessions remain untouched.
            self.run("set-option", "-t", session_name, "status", "off")

    def configure_pika_scrollback(
        self, session_name: str, *, target: str | None = None
    ) -> None:
        """Give Pika-owned homes wheel scrolling and deep pane-local history."""
        if not self.is_pika_session(session_name):
            return
        # `mouse` is a session option. With Codex not requesting application
        # mouse events, tmux's default WheelUpPane binding enters copy mode and
        # makes the wheel/trackpad operate on the truthful pane history.
        self.run("set-option", "-t", session_name, "mouse", "on")
        # tmux allocates a pane's history limit when the pane screen is created.
        # Callers configure the window before create/respawn and pass the exact
        # pane when repairing an existing Pika home.
        self.run(
            "set-option",
            "-w",
            "-t",
            target or session_name,
            "history-limit",
            str(PIKA_HISTORY_LIMIT),
        )

    def configure_pika_session(
        self, session_name: str, *, target: str | None = None
    ) -> None:
        self.hide_pika_status(session_name)
        self.configure_pika_scrollback(session_name, target=target)

    def ensure_pika_rgb(self) -> None:
        """Teach the shared tmux server that xterm clients accept 24-bit colour."""
        current = self.run(
            "show-options",
            "-s",
            "-v",
            "terminal-features",
            check=False,
        )
        if current.returncode == 0 and any(
            feature.startswith("xterm*") and "RGB" in feature.split(":")[1:]
            for feature in current.stdout.splitlines()
        ):
            return
        self.run(
            "set-option",
            "-as",
            "terminal-features",
            "xterm*:RGB",
        )

    def create_agent_session(
        self,
        *,
        tmux_name: str,
        cwd: str,
        provider: str,
        agent_argv: list[str],
        environment: dict[str, str],
        session_id: str | None,
        display_name: str,
        launch_token: str | None,
    ) -> Pane:
        wrapper = self._agent_wrapper(
            provider, agent_argv, environment, session_id, launch_token
        )
        # Create a short-lived holding pane first. tmux fixes the history buffer
        # limit when a pane screen is created, so applying the Pika window option
        # before respawn is what gives the real agent pane its deeper history.
        self.run("new-session", "-d", "-s", tmux_name, "-c", cwd, "sleep 30")
        self.configure_pika_session(tmux_name)
        holding = self.get_pane(tmux_name)
        if holding is None:
            raise TmuxError(f"tmux created {tmux_name!r} but its pane was not found")
        self.run("respawn-pane", "-k", "-t", holding.pane_id, "-c", cwd, wrapper)
        self.ensure_pika_rgb()
        pane = self.get_pane(tmux_name)
        if pane is None:
            raise TmuxError(f"tmux created {tmux_name!r} but its pane was not found")
        self.tag_pane(
            pane.pane_id,
            provider=provider,
            session_id=session_id,
            name=display_name,
            launch_token=launch_token,
        )
        return self.get_pane(pane.pane_id) or pane

    def respawn_agent(
        self,
        pane_id: str,
        *,
        cwd: str,
        provider: str,
        agent_argv: list[str],
        environment: dict[str, str],
        session_id: str,
        display_name: str,
    ) -> Pane:
        wrapper = self._agent_wrapper(
            provider, agent_argv, environment, session_id, None
        )
        pane = self.get_pane(pane_id)
        if pane is None:
            raise TmuxError("pane disappeared before it could be respawned")
        self.configure_pika_session(pane.session_name, target=pane_id)
        self.run("respawn-pane", "-k", "-t", pane_id, "-c", cwd, wrapper)
        self.tag_pane(
            pane_id,
            provider=provider,
            session_id=session_id,
            name=display_name,
            launch_token="",
        )
        pane = self.get_pane(pane_id)
        if pane is None:
            raise TmuxError("respawned pane disappeared")
        return pane

    def _agent_wrapper(
        self,
        provider: str,
        agent_argv: list[str],
        environment: dict[str, str],
        session_id: str | None,
        launch_token: str | None,
    ) -> str:
        # A long-lived tmux server can carry stale PATH/NO_COLOR values from
        # whichever process originally started it. Launch the interactive TUI
        # with the caller's executable path and a real 256-colour tmux terminal
        # contract, rather than the server's historical automation environment.
        launch_environment = dict(environment)
        launch_environment["PATH"] = os.environ.get("PATH") or os.defpath
        # `tmux-direct` advertises 24-bit RGB to Codex/Claude. This preserves
        # subtle message-background distinctions that collapse under the
        # 256-colour `tmux-256color` entry.
        launch_environment["TERM"] = "tmux-direct"
        if os.environ.get("COLORTERM"):
            launch_environment["COLORTERM"] = os.environ["COLORTERM"]
        # Codex derives its user-message and composer backgrounds from OSC 10/11
        # replies. tmux consumes those plain queries without answering them, so
        # capture the real outer palette before tmux starts and let Pika's
        # private PTY bridge answer only those two probes. Inherited values make
        # switching between Pika homes work without probing through tmux again.
        launch_environment.update(terminal_palette_environment())
        caller_no_color = os.environ.get("NO_COLOR")
        preserve_no_color = caller_no_color is not None and os.environ.get(
            "TERM"
        ) not in {None, "", "dumb"}
        env_argv = ["env"]
        if preserve_no_color:
            launch_environment["NO_COLOR"] = caller_no_color
        else:
            # Explicitly remove a stale tmux-server value. Omitting the key is
            # insufficient because `env` otherwise inherits the server state.
            env_argv.extend(["-u", "NO_COLOR"])
        env_argv.extend(
            f"{key}={value}" for key, value in launch_environment.items()
        )
        palette = palette_from_environment(launch_environment)
        if provider == "codex" and palette is not None:
            env_argv.extend(
                [
                    sys.executable,
                    "-m",
                    "pikamux.terminal_bridge",
                    "--foreground",
                    launch_environment[FOREGROUND_ENV],
                    "--background",
                    launch_environment[BACKGROUND_ENV],
                    "--",
                    *agent_argv,
                ]
            )
        else:
            env_argv.extend(agent_argv)
        exit_argv = [
            sys.executable,
            "-m",
            "pikamux",
            "_process-exit",
            "--provider",
            provider,
        ]
        if session_id:
            exit_argv.extend(["--session-id", session_id])
        if launch_token:
            exit_argv.extend(["--launch-token", launch_token])
        shell = os.environ.get("SHELL") or "/bin/bash"
        shell_argv = [shell, "-l"]
        if palette is not None:
            # Keep the outer palette available in the protected fallback shell.
            # A later `pika NAME` can then switch or respawn from inside tmux
            # without attempting another OSC query through the multiplexer.
            shell_argv = [
                "env",
                f"{FOREGROUND_ENV}={launch_environment[FOREGROUND_ENV]}",
                f"{BACKGROUND_ENV}={launch_environment[BACKGROUND_ENV]}",
                shell,
                "-l",
            ]
        return (
            f"{shlex.join(env_argv)}; pika_rc=$?; "
            f'{shlex.join(exit_argv)} --code "$pika_rc"; '
            f"exec {shlex.join(shell_argv)}"
        )

    def attach(
        self,
        target_session: str,
        *,
        target_pane: str | None = None,
        on_attached: Callable[[], None] | None = None,
        receipt: str | Callable[[], str] | None = None,
    ) -> int:
        # Also repairs Pika sessions created by older releases whose inherited
        # global status/mouse options exposed transport details or blocked
        # wheel scrolling. A live pane keeps its original history allocation;
        # the deeper limit takes effect on its next exact respawn.
        if self.is_pika_session(target_session):
            self.ensure_pika_rgb()
        self.configure_pika_session(target_session, target=target_pane)
        if os.environ.get("TMUX"):
            client_name: str | None = None
            if receipt:
                current = self.run(
                    "display-message", "-p", "#{client_name}", check=False
                )
                client_name = current.stdout.strip() or None
            result = self.run(
                "switch-client", "-t", target_session, capture_output=False
            ).returncode
            if result == 0 and target_pane:
                result = self.run(
                    "select-window", "-t", target_pane, capture_output=False
                ).returncode
            if result == 0 and target_pane:
                result = self.run(
                    "select-pane", "-t", target_pane, capture_output=False
                ).returncode
            if result == 0 and on_attached:
                on_attached()
            if result == 0 and receipt:
                message = receipt() if callable(receipt) else receipt
                args = ["display-message"]
                if client_name:
                    args.extend(["-c", client_name])
                args.extend(["-d", "3000", "-l", message])
                self.run(*args, check=False)
            return result
        try:
            process = subprocess.Popen(
                self.command(
                    "attach-session", "-t", target_pane or target_session
                )
            )
            try:
                result = process.wait(timeout=0.15)
            except subprocess.TimeoutExpired:
                if on_attached:
                    on_attached()
                if receipt:
                    message = receipt() if callable(receipt) else receipt
                    self._display_to_client(process.pid, message)
                return process.wait()
            if result == 0 and on_attached:
                on_attached()
            return result
        except OSError as exc:
            raise TmuxError(str(exc)) from exc

    def _display_to_client(self, client_pid: int, message: str) -> None:
        separator = "\x1f"
        proc = self.run(
            "list-clients",
            "-F",
            f"#{{client_name}}{separator}#{{client_pid}}",
            check=False,
        )
        for line in proc.stdout.splitlines():
            parts = line.split(separator, 1)
            if len(parts) != 2 or parts[1] != str(client_pid):
                continue
            self.run(
                "display-message",
                "-c",
                parts[0],
                "-d",
                "3000",
                "-l",
                message,
                check=False,
            )
            return

    def capture(self, target: str, lines: int = 200) -> str:
        proc = self.run(
            "capture-pane", "-p", "-e", "-J", "-S", f"-{max(1, lines)}", "-t", target
        )
        return proc.stdout.rstrip()

    def popup(
        self,
        target: str,
        lines: int,
        name: str,
        provider: str,
        session_id: str,
    ) -> int:
        command = shlex.join(
            [
                sys.executable,
                "-m",
                "pikamux",
                "_peek-popup",
                "--target",
                target,
                "--lines",
                str(lines),
                "--name",
                name,
                "--provider",
                provider,
                "--session-id",
                session_id,
            ]
        )
        return self.run(
            "display-popup",
            "-E",
            "-w",
            "90%",
            "-h",
            "80%",
            "-d",
            "#{pane_current_path}",
            command,
            capture_output=False,
        ).returncode

    def display_alert(self, message: str) -> None:
        if not self.server_running():
            return
        separator = "\x1f"
        proc = self.run(
            "list-clients",
            "-F",
            f"#{{client_name}}{separator}#{{client_tty}}",
            check=False,
        )
        for value in proc.stdout.splitlines():
            parts = value.split(separator, 1)
            if len(parts) != 2:
                continue
            client, tty = parts
            self.run("display-message", "-c", client, message, check=False)
            tty = tty.strip()
            if not tty.startswith("/dev/"):
                continue
            try:
                with open(tty, "wb", buffering=0) as stream:
                    stream.write(b"\a")
            except OSError:
                continue

    @staticmethod
    def internal_name(
        provider: str, session_id: str | None = None, token: str | None = None
    ) -> str:
        suffix = (session_id or token or "session").replace("-", "")[:10]
        prefix = "c" if provider == "codex" else "a"
        return f"pika-{prefix}-{suffix}"


def shell_join(values: Iterable[str]) -> str:
    return shlex.join(list(values))
