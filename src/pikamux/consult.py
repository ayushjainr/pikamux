from __future__ import annotations

import base64
import json
import os
import re
import secrets
import select
import selectors
import socket
import sqlite3
import subprocess
import time
import urllib.error
import urllib.parse
import urllib.request
from abc import ABC, abstractmethod
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable

from . import __version__
from .executables import (
    configured_executable,
    executable_available,
    executable_version,
    provider_compatibility_error,
)
from .models import Session
from .paths import opencode_data_home


class ConsultationError(RuntimeError):
    pass


DEFAULT_CODEX_MODEL = "gpt-5.6-sol"
DEFAULT_CODEX_EFFORT = "medium"
FAST_CODEX_MODEL = "gpt-5.6-luna"
FAST_CODEX_EFFORT = "medium"


@dataclass(frozen=True, slots=True)
class ConsultationPolicy:
    mode: str
    model: str | None
    effort: str | None

    @property
    def label(self) -> str:
        if self.model and self.effort:
            return f"{self.model} · {self.effort}"
        return "provider native"

    def receipt(self) -> dict[str, str | None]:
        return {
            "consultation_mode": self.mode,
            "model": self.model,
            "effort": self.effort,
        }


def consultation_policy(session: Session, *, fast: bool = False) -> ConsultationPolicy:
    if session.provider == "codex":
        if fast:
            return ConsultationPolicy("fast", FAST_CODEX_MODEL, FAST_CODEX_EFFORT)
        return ConsultationPolicy(
            "default", DEFAULT_CODEX_MODEL, DEFAULT_CODEX_EFFORT
        )
    if session.provider == "claude":
        if fast:
            raise ConsultationError(
                "Fast consultations are not benchmarked for Claude; "
                "omit --fast to use its provider-native model"
            )
        return ConsultationPolicy("provider-native", None, None)
    if session.provider == "opencode":
        if fast:
            raise ConsultationError(
                "Fast consultations are not benchmarked for OpenCode; "
                "omit --fast to use the session's provider-native model"
            )
        return ConsultationPolicy("provider-native", None, None)
    raise ConsultationError(f"Unsupported provider: {session.provider}")


def _validated_policy(
    session: Session,
    policy: ConsultationPolicy | None = None,
    *,
    fast: bool = False,
) -> ConsultationPolicy:
    if fast and policy is not None:
        raise ConsultationError("Choose either fast mode or an explicit policy, not both")
    candidate = policy or consultation_policy(session, fast=fast)
    expected = consultation_policy(session, fast=candidate.mode == "fast")
    if candidate != expected:
        raise ConsultationError(
            f"Invalid {session.provider} consultation policy: {candidate.label}"
        )
    return candidate


class Consultation(ABC):
    """A provider-native, non-persistent side conversation."""

    def __init__(self, session: Session, policy: ConsultationPolicy):
        self.session = session
        self.policy = policy
        self._progress_callback: Callable[[str, str], None] | None = None

    def set_progress_callback(self, callback: Callable[[str, str], None]) -> None:
        self._progress_callback = callback

    def _progress(self, stage: str, delivery: str) -> None:
        if self._progress_callback:
            self._progress_callback(stage, delivery)

    def _cleanup_failed_start(self, error: BaseException) -> None:
        try:
            self.close()
            error.cleanup = "complete"
        except (Exception, KeyboardInterrupt) as cleanup_error:
            error.cleanup = "failed"
            error.cleanup_error = str(cleanup_error)

    @abstractmethod
    def ask(self, question: str) -> str:
        raise NotImplementedError

    @abstractmethod
    def close(self) -> None:
        raise NotImplementedError

    def __enter__(self) -> Consultation:
        return self

    def __exit__(self, exc_type, exc, traceback) -> None:
        try:
            self.close()
        except (Exception, KeyboardInterrupt) as cleanup_error:
            if exc is None:
                raise
            exc.cleanup = "failed"
            exc.cleanup_error = str(cleanup_error)


def _ephemeral_environment() -> dict[str, str]:
    environment = os.environ.copy()
    # Pika's own hooks must not inventory the provider's transient fork.
    environment["PIKA_EPHEMERAL"] = "1"
    return environment


def _opencode_ephemeral_environment() -> dict[str, str]:
    environment = _ephemeral_environment()
    # Inline config has runtime precedence over user/project agent settings.
    # The catch-all deny also covers custom and MCP tools; only transcript-free
    # local reads are enabled for Pika's private inherited consultation.
    environment["OPENCODE_CONFIG_CONTENT"] = json.dumps(
        {
            "agent": {
                "pika-readonly": {
                    "description": "Pika read-only inherited consultation",
                    "mode": "primary",
                    "permission": {
                        "*": "deny",
                        "read": "allow",
                        "glob": "allow",
                        "grep": "allow",
                        "list": "allow",
                    },
                }
            }
        },
        separators=(",", ":"),
    )
    return environment


def _terminate(process: subprocess.Popen[str] | None) -> None:
    if process is None or process.poll() is not None:
        return
    process.terminate()
    try:
        process.wait(timeout=2)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait(timeout=2)


def _read_json_line(
    process: subprocess.Popen[str], *, deadline: float
) -> dict[str, Any]:
    """Read provider frames without TextIOWrapper read-ahead hiding ready data."""
    assert process.stdout is not None
    try:
        stdout_fd = process.stdout.fileno()
    except (AttributeError, OSError, ValueError):
        # In-memory streams are useful provider fixtures; actual child pipes
        # always take the byte-buffer path below.
        return _read_json_line_text(process, deadline=deadline)

    buffered = getattr(process, "_pika_stdout_buffer", None)
    if not isinstance(buffered, bytearray):
        buffered = bytearray()
        process._pika_stdout_buffer = buffered
    stderr_buffer = getattr(process, "_pika_stderr_buffer", None)
    if not isinstance(stderr_buffer, bytearray):
        stderr_buffer = bytearray()
        process._pika_stderr_buffer = stderr_buffer
    selector = selectors.DefaultSelector()
    selector.register(stdout_fd, selectors.EVENT_READ, "stdout")
    if process.stderr is not None:
        selector.register(process.stderr.fileno(), selectors.EVENT_READ, "stderr")
    eof = False
    try:
        while time.monotonic() < deadline:
            newline = buffered.find(b"\n")
            if newline >= 0 or (eof and buffered):
                size = newline if newline >= 0 else len(buffered)
                if size > 16 * 1024 * 1024:
                    raise ConsultationError("Provider side response exceeded the 16 MiB frame limit")
                line = bytes(buffered[:size])
                del buffered[:size + (1 if newline >= 0 else 0)]
                try:
                    message = json.loads(line)
                except (UnicodeDecodeError, ValueError):
                    continue
                if isinstance(message, dict):
                    return message
                continue
            if len(buffered) > 16 * 1024 * 1024:
                raise ConsultationError("Provider side response exceeded the 16 MiB frame limit")
            if eof:
                break
            ready = selector.select(max(0.0, deadline - time.monotonic()))
            if not ready:
                break
            for key, _mask in ready:
                try:
                    chunk = os.read(key.fd, 65536)
                except BlockingIOError:
                    continue
                if not chunk:
                    selector.unregister(key.fd)
                    if key.data == "stdout":
                        eof = True
                    continue
                if key.data == "stdout":
                    buffered.extend(chunk)
                else:
                    stderr_buffer.extend(chunk)
                    if len(stderr_buffer) > 65536:
                        del stderr_buffer[:-65536]
    finally:
        selector.close()
    if not eof and process.poll() is None:
        raise ConsultationError("provider side consultation timed out")
    detail = bytes(stderr_buffer).decode("utf-8", errors="replace").strip()
    suffix = f": {detail[-500:]}" if detail else ""
    raise ConsultationError(
        f"provider side consultation exited with status {process.poll()}{suffix}"
    )


def _read_json_line_text(
    process: subprocess.Popen[str], *, deadline: float
) -> dict[str, Any]:
    assert process.stdout is not None
    while time.monotonic() < deadline:
        ready, _, _ = select.select(
            [process.stdout], [], [], min(1.0, max(0.0, deadline - time.monotonic()))
        )
        if not ready:
            if process.poll() is not None:
                break
            continue
        line = process.stdout.readline()
        if not line:
            break
        try:
            message = json.loads(line)
        except ValueError:
            continue
        if isinstance(message, dict):
            return message
    if process.poll() is None:
        raise ConsultationError("provider side consultation timed out")
    detail = ""
    if process.stderr is not None:
        try:
            detail = process.stderr.read().strip()
        except OSError:
            pass
    suffix = f": {detail[-500:]}" if detail else ""
    raise ConsultationError(
        f"provider side consultation exited with status {process.returncode}{suffix}"
    )


class CodexConsultation(Consultation):
    FORK_TIMEOUT_SECONDS = 180.0
    TURN_START_TIMEOUT_SECONDS = 60.0

    def __init__(
        self,
        session: Session,
        *,
        timeout: float = 900.0,
        policy: ConsultationPolicy | None = None,
    ):
        if session.provider != "codex":
            raise ConsultationError(
                f"Codex consultation cannot open a {session.provider} session"
            )
        super().__init__(session, _validated_policy(session, policy))
        self.timeout = timeout
        self.process: subprocess.Popen[str] | None = None
        self.thread_id: str | None = None
        self._request_id = 0
        self._notifications: list[dict[str, Any]] = []
        try:
            self._start()
        except (Exception, KeyboardInterrupt) as exc:
            self._cleanup_failed_start(exc)
            raise

    def _start(self) -> None:
        executable = configured_executable("codex")
        if not executable_available(executable):
            raise ConsultationError("Configured Codex executable is unavailable")
        self.process = subprocess.Popen(
            [str(executable), "app-server", "--stdio"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            bufsize=1,
            cwd=self.session.cwd if _valid_cwd(self.session.cwd) else None,
            env=_ephemeral_environment(),
        )
        self._request(
            "initialize",
            {
                "clientInfo": {
                    "name": "pikamux",
                    "title": "Pika side consultation",
                    "version": __version__,
                },
                "capabilities": {"experimentalApi": True},
            },
            timeout=15.0,
        )
        self._send({"method": "initialized"})
        fork_params: dict[str, Any] = {
            "threadId": self.session.provider_thread_id,
            "ephemeral": True,
            "approvalPolicy": "never",
            "sandbox": "read-only",
            "developerInstructions": (
                "This is an ephemeral side consultation. Answer from the "
                "inherited conversation context without modifying files or "
                "external state. If tools would be required, explain what "
                "needs checking instead. Keep dated or named historical "
                "work separate from later current state; state the chronology "
                "when both are relevant."
            ),
        }
        if self.policy.model:
            fork_params["model"] = self.policy.model
        if self.policy.effort:
            fork_params["config"] = {
                "model_reasoning_effort": self.policy.effort
            }
        result = self._request(
            "thread/fork",
            fork_params,
            timeout=self.FORK_TIMEOUT_SECONDS,
        )
        thread = result.get("thread") if isinstance(result, dict) else None
        thread_id = thread.get("id") if isinstance(thread, dict) else None
        if not thread_id or not thread.get("ephemeral"):
            raise ConsultationError(
                "Codex did not confirm an ephemeral fork; refusing to continue"
            )
        observed_model = result.get("model")
        observed_effort = result.get("reasoningEffort")
        if (
            observed_model != self.policy.model
            or observed_effort != self.policy.effort
        ):
            raise ConsultationError(
                "Codex did not confirm the requested consultation profile; "
                f"requested {self.policy.label}, observed "
                f"{observed_model or 'unknown'} · {observed_effort or 'unknown'}"
            )
        self.thread_id = str(thread_id)
        self._notifications.clear()

    def _send(self, payload: dict[str, Any]) -> None:
        if self.process is None or self.process.stdin is None:
            raise ConsultationError("Codex side consultation is closed")
        self.process.stdin.write(json.dumps(payload, separators=(",", ":")) + "\n")
        self.process.stdin.flush()

    def _request(
        self, method: str, params: dict[str, Any], *, timeout: float
    ) -> dict[str, Any]:
        self._request_id += 1
        request_id = self._request_id
        self._send({"method": method, "id": request_id, "params": params})
        assert self.process is not None
        deadline = time.monotonic() + timeout
        while True:
            message = _read_json_line(self.process, deadline=deadline)
            if message.get("id") == request_id and "method" not in message:
                error = message.get("error")
                if error:
                    detail = (
                        error.get("message") if isinstance(error, dict) else str(error)
                    )
                    raise ConsultationError(f"Codex {method} failed: {detail}")
                result = message.get("result")
                return result if isinstance(result, dict) else {}
            if "id" in message and "method" in message:
                self._send(
                    {
                        "id": message["id"],
                        "error": {
                            "code": -32000,
                            "message": "Pika side consultations are non-interactive",
                        },
                    }
                )
            elif message.get("method"):
                self._notifications.append(message)

    def ask(self, question: str) -> str:
        self._progress("turn", "not_sent")
        question = question.strip()
        if not question:
            raise ConsultationError("Question cannot be empty")
        if not self.thread_id or self.process is None:
            raise ConsultationError("Codex side consultation is closed")
        self._notifications.clear()
        params: dict[str, Any] = {
            "threadId": self.thread_id,
            "input": [{"type": "text", "text": question}],
        }
        if self.policy.model:
            params["model"] = self.policy.model
        if self.policy.effort:
            params["effort"] = self.policy.effort
        self._progress("turn", "unknown")
        result = self._request(
            "turn/start", params, timeout=self.TURN_START_TIMEOUT_SECONDS
        )
        turn = result.get("turn") if isinstance(result, dict) else None
        turn_id = turn.get("id") if isinstance(turn, dict) else None
        if not turn_id:
            raise ConsultationError("Codex did not start the side turn")
        self._progress("turn", "confirmed")
        final_text = ""
        deltas: list[str] = []
        deadline = time.monotonic() + self.timeout
        while True:
            message = (
                self._notifications.pop(0)
                if self._notifications
                else _read_json_line(self.process, deadline=deadline)
            )
            if "id" in message and "method" in message:
                self._send(
                    {
                        "id": message["id"],
                        "error": {
                            "code": -32000,
                            "message": "Pika side consultations are non-interactive",
                        },
                    }
                )
                continue
            method = message.get("method")
            params = message.get("params")
            if not isinstance(params, dict):
                continue
            if params.get("threadId") != self.thread_id:
                continue
            if method == "item/agentMessage/delta" and params.get("turnId") == turn_id:
                deltas.append(str(params.get("delta") or ""))
            elif method == "item/completed" and params.get("turnId") == turn_id:
                item = params.get("item")
                if isinstance(item, dict) and item.get("type") == "agentMessage":
                    final_text = str(item.get("text") or final_text)
                    if final_text and item.get("phase") in {None, "final_answer"}:
                        # The final item is authoritative. Do not wait for Stop
                        # hooks: unrelated provider/plugin hooks may be slow,
                        # and this in-memory app-server will be discarded now.
                        return final_text.strip()
            elif method == "error" and params.get("turnId") == turn_id:
                error = params.get("error")
                detail = error.get("message") if isinstance(error, dict) else error
                raise ConsultationError(f"Codex side turn failed: {detail}")
            elif method == "turn/completed":
                completed = params.get("turn")
                if not isinstance(completed, dict) or completed.get("id") != turn_id:
                    continue
                status = completed.get("status")
                if status != "completed":
                    raise ConsultationError(
                        f"Codex side turn ended with status {status or 'unknown'}"
                    )
                answer = (final_text or "".join(deltas)).strip()
                if not answer:
                    raise ConsultationError("Codex side turn returned no answer")
                return answer
            elif method == "thread/status/changed" and final_text:
                status = params.get("status")
                if isinstance(status, dict) and status.get("type") == "idle":
                    return final_text.strip()

    def close(self) -> None:
        _terminate(self.process)
        self.process = None
        self.thread_id = None
        self._notifications.clear()


class ClaudeConsultation(Consultation):
    MIN_SIDE_QUESTION_VERSION = (2, 1, 228)

    def __init__(
        self,
        session: Session,
        *,
        timeout: float = 900.0,
        policy: ConsultationPolicy | None = None,
    ):
        if session.provider != "claude":
            raise ConsultationError(
                f"Claude consultation cannot open a {session.provider} session"
            )
        super().__init__(session, _validated_policy(session, policy))
        self.timeout = timeout
        self.process: subprocess.Popen[str] | None = None
        try:
            self._check_capability()
            self._start()
        except (Exception, KeyboardInterrupt) as exc:
            self._cleanup_failed_start(exc)
            raise

    def _check_capability(self) -> None:
        executable = configured_executable("claude")
        if not executable_available(executable):
            raise ConsultationError("Configured Claude executable is unavailable")
        try:
            result = subprocess.run(
                [str(executable), "--version"],
                capture_output=True,
                text=True,
                timeout=5,
                check=False,
            )
        except (OSError, subprocess.TimeoutExpired) as exc:
            raise ConsultationError(
                f"Could not verify Claude side support: {exc}"
            ) from exc
        version = _version_tuple(result.stdout or result.stderr)
        if version < self.MIN_SIDE_QUESTION_VERSION:
            needed = ".".join(map(str, self.MIN_SIDE_QUESTION_VERSION))
            found = ".".join(map(str, version)) if version else "unknown"
            raise ConsultationError(
                f"Claude side consultations require tested capability {needed}+; "
                f"found {found}"
            )

    def _start(self) -> None:
        executable = configured_executable("claude")
        if not executable_available(executable):
            raise ConsultationError("Configured Claude executable is unavailable")
        try:
            self.process = subprocess.Popen(
                [
                    str(executable),
                    "-p",
                    "--resume",
                    self.session.provider_thread_id,
                    "--fork-session",
                    "--no-session-persistence",
                    "--tools",
                    "",
                    "--input-format",
                    "stream-json",
                    "--output-format",
                    "stream-json",
                    "--verbose",
                ],
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
                bufsize=1,
                cwd=self.session.cwd if _valid_cwd(self.session.cwd) else None,
                env=_ephemeral_environment(),
            )
        except OSError as exc:
            raise ConsultationError(
                f"Could not start Claude side consultation: {exc}"
            ) from exc

    def ask(self, question: str) -> str:
        self._progress("turn", "not_sent")
        question = question.strip()
        if not question:
            raise ConsultationError("Question cannot be empty")
        payload = {
            "type": "user",
            "message": {
                "role": "user",
                "content": [{"type": "text", "text": question}],
            },
        }
        if self.process is None or self.process.stdin is None:
            raise ConsultationError("Claude side consultation is closed")
        self._progress("turn", "unknown")
        try:
            self.process.stdin.write(json.dumps(payload, separators=(",", ":")) + "\n")
            self.process.stdin.flush()
        except (BrokenPipeError, OSError) as exc:
            raise ConsultationError(f"Claude side consultation closed: {exc}") from exc
        deadline = time.monotonic() + self.timeout
        latest_answer = ""
        while True:
            message = _read_json_line(self.process, deadline=deadline)
            if message.get("type") == "assistant":
                self._progress("turn", "confirmed")
                body = message.get("message")
                content = body.get("content") if isinstance(body, dict) else None
                if isinstance(content, list):
                    text_parts = [
                        str(item.get("text") or "")
                        for item in content
                        if isinstance(item, dict) and item.get("type") == "text"
                    ]
                    latest_answer = "".join(text_parts).strip() or latest_answer
                continue
            if message.get("type") != "result":
                continue
            self._progress("turn", "confirmed")
            if message.get("subtype") != "success" or message.get("is_error"):
                raise ConsultationError(
                    f"Claude side turn failed: {message.get('result') or message}"
                )
            answer = str(message.get("result") or latest_answer).strip()
            if not answer:
                raise ConsultationError("Claude side turn returned no answer")
            return answer

    def close(self) -> None:
        _terminate(self.process)
        self.process = None


class OpenCodeConsultation(Consultation):
    """A disposable OpenCode fork reused for a private multi-turn dialogue."""

    def __init__(
        self,
        session: Session,
        *,
        timeout: float = 900.0,
        policy: ConsultationPolicy | None = None,
    ):
        if session.provider != "opencode":
            raise ConsultationError(
                f"OpenCode consultation cannot open a {session.provider} session"
            )
        super().__init__(session, _validated_policy(session, policy))
        self.timeout = timeout
        self.thread_id: str | None = None
        self.closed = False
        self.process: subprocess.Popen[str] | None = None
        self.database = opencode_data_home() / "opencode.db"
        self._fork_uncertain = False
        self._fork_server: subprocess.Popen[str] | None = None

    @staticmethod
    def _free_loopback_port() -> int:
        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as listener:
            listener.bind(("127.0.0.1", 0))
            return int(listener.getsockname()[1])

    @staticmethod
    def _api_json(
        url: str,
        *,
        username: str,
        password: str,
        method: str = "GET",
        payload: dict[str, Any] | None = None,
        timeout: float = 2.0,
    ) -> dict[str, Any]:
        token = base64.b64encode(
            f"{username}:{password}".encode("utf-8")
        ).decode("ascii")
        body = None
        headers = {"Authorization": f"Basic {token}"}
        if payload is not None:
            body = json.dumps(payload, separators=(",", ":")).encode("utf-8")
            headers["Content-Type"] = "application/json"
        request = urllib.request.Request(
            url, data=body, headers=headers, method=method
        )
        try:
            with urllib.request.urlopen(request, timeout=timeout) as response:
                raw = response.read().decode("utf-8")
        except urllib.error.HTTPError as exc:
            try:
                detail = exc.read().decode("utf-8", errors="replace").strip()
            except OSError:
                detail = ""
            raise ConsultationError(
                f"OpenCode API returned HTTP {exc.code}"
                + (f": {detail[-500:]}" if detail else "")
            ) from exc
        except (OSError, urllib.error.URLError) as exc:
            raise ConsultationError(f"OpenCode API request failed: {exc}") from exc
        try:
            result = json.loads(raw)
        except ValueError as exc:
            raise ConsultationError("OpenCode API returned invalid JSON") from exc
        if not isinstance(result, dict):
            raise ConsultationError("OpenCode API returned an invalid response")
        return result

    def _fork_session(self, executable: Path | str) -> str:
        """Create one exact provider-issued fork before any side prompt runs."""
        port = self._free_loopback_port()
        username = "opencode"
        password = secrets.token_urlsafe(32)
        base_url = f"http://127.0.0.1:{port}"
        environment = _opencode_ephemeral_environment()
        environment["OPENCODE_SERVER_USERNAME"] = username
        environment["OPENCODE_SERVER_PASSWORD"] = password
        server: subprocess.Popen[str] | None = None
        try:
            server = subprocess.Popen(
                [
                    str(executable),
                    "--pure",
                    "serve",
                    "--hostname",
                    "127.0.0.1",
                    "--port",
                    str(port),
                ],
                cwd=self.session.cwd if _valid_cwd(self.session.cwd) else None,
                env=environment,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                text=True,
            )
            self._fork_server = server
            deadline = time.monotonic() + 15.0
            while True:
                if server.poll() is not None:
                    detail = server.stdout.read().strip() if server.stdout else ""
                    raise ConsultationError(
                        "OpenCode side-session server exited before becoming ready"
                        + (f": {detail[-500:]}" if detail else "")
                    )
                try:
                    self._api_json(
                        f"{base_url}/global/health",
                        username=username,
                        password=password,
                        timeout=0.5,
                    )
                    break
                except ConsultationError:
                    if time.monotonic() >= deadline:
                        raise ConsultationError(
                            "OpenCode side-session server did not become ready"
                        )
                    time.sleep(0.05)
            query: dict[str, str] = {}
            if _valid_cwd(self.session.cwd):
                query["directory"] = str(self.session.cwd)
            suffix = f"?{urllib.parse.urlencode(query)}" if query else ""
            parent = urllib.parse.quote(self.session.provider_thread_id, safe="")
            self._fork_uncertain = True
            result = self._api_json(
                f"{base_url}/session/{parent}/fork{suffix}",
                username=username,
                password=password,
                method="POST",
                payload={},
                timeout=30.0,
            )
            # Retain the provider-issued child before server teardown: a failed
            # terminate must not lose the exact identity needed for cleanup.
            session_id = str(result.get("id") or "")
            if self._valid_session_id(session_id) and session_id not in {
                self.session.session_id, self.session.provider_thread_id,
            }:
                self.thread_id = session_id
                self._fork_uncertain = False
        except OSError as exc:
            raise ConsultationError(
                f"OpenCode side-session server failed: {exc}"
            ) from exc
        finally:
            _terminate(server)
            self._fork_server = None

        session_id = str(result.get("id") or "")
        if not self._valid_session_id(session_id):
            raise ConsultationError(
                "OpenCode did not return a valid provider-issued fork identity"
            )
        if session_id in {self.session.session_id, self.session.provider_thread_id}:
            raise ConsultationError("OpenCode did not fork the parent consultation")
        self.thread_id = session_id
        observed_cwd = str(result.get("directory") or "")
        if _valid_cwd(self.session.cwd) and observed_cwd != str(self.session.cwd):
            raise ConsultationError(
                "OpenCode forked the parent in an unexpected working directory"
            )
        return session_id

    def _model_argv(self) -> list[str]:
        value = str(self.session.model or "").strip()
        if not value:
            return []
        variant: str | None = None
        if value.endswith("]") and "[" in value:
            value, variant = value[:-1].rsplit("[", 1)
        result = ["--model", value]
        if variant:
            result.extend(("--variant", variant))
        return result

    def ask(self, question: str) -> str:
        self._progress("prepare" if self.thread_id is None else "turn", "not_sent")
        question = question.strip()
        if not question:
            raise ConsultationError("Question cannot be empty")
        if self.closed:
            raise ConsultationError("OpenCode side consultation is closed")
        executable = configured_executable("opencode")
        if not executable_available(executable):
            raise ConsultationError("Configured OpenCode executable is unavailable")
        compatibility_error = provider_compatibility_error(
            "opencode", executable_version(executable)
        )
        if compatibility_error:
            raise ConsultationError(compatibility_error)
        if self.thread_id is None:
            self.thread_id = self._fork_session(executable)
        assert self.thread_id is not None
        target = self.thread_id
        argv = [
            str(executable),
            "--pure",
            "run",
            "--session",
            target,
        ]
        argv.extend(("--format", "json", "--agent", "pika-readonly"))
        argv.extend(self._model_argv())
        prompt = (
            "This is an ephemeral, read-only Pika side consultation inherited "
            "from the parent conversation. Do not modify files or external "
            "state. Answer the question from context; if a mutating tool would "
            "be required, explain what needs checking instead.\n\n" + question
        )
        argv.append(prompt)
        checkpoint = self._latest_message_time(self.thread_id)
        try:
            self.process = subprocess.Popen(
                argv,
                cwd=self.session.cwd if _valid_cwd(self.session.cwd) else None,
                env=_opencode_ephemeral_environment(),
                stdin=subprocess.DEVNULL,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.PIPE,
                text=True,
                bufsize=1,
            )
        except OSError as exc:
            raise ConsultationError(f"OpenCode side turn failed: {exc}") from exc
        self._progress("turn", "unknown")
        answer: str | None = None
        try:
            deadline = time.monotonic() + self.timeout
            while time.monotonic() < deadline:
                answer = self._completed_answer(self.thread_id, checkpoint)
                if answer:
                    return answer
                assert self.process is not None
                if self.process.poll() is not None:
                    answer = self._completed_answer(self.thread_id, checkpoint)
                    if answer:
                        return answer
                    detail = ""
                    if self.process.stderr is not None:
                        detail = self.process.stderr.read().strip()
                    suffix = f": {detail[-500:]}" if detail else ""
                    raise ConsultationError(
                        "OpenCode side turn exited before a completed answer"
                        + suffix
                    )
                time.sleep(0.1)
            raise ConsultationError("OpenCode side consultation timed out")
        finally:
            try:
                _terminate(self.process)
            except (OSError, subprocess.TimeoutExpired):
                # The completed answer remains usable. close() must retry
                # termination and certify deletion, or report cleanup failure.
                if not answer:
                    raise
            else:
                self.process = None

    def _connect(self) -> sqlite3.Connection:
        db = sqlite3.connect(f"file:{self.database}?mode=ro", uri=True, timeout=1)
        db.row_factory = sqlite3.Row
        return db

    @staticmethod
    def _valid_session_id(value: str) -> bool:
        return (
            value.startswith("ses_")
            and 8 <= len(value) <= 128
            and value[4:].isalnum()
        )

    def _latest_message_time(self, session_id: str) -> int:
        try:
            with self._connect() as db:
                row = db.execute(
                    "SELECT COALESCE(MAX(time_created),0) AS value FROM message "
                    "WHERE session_id=?",
                    (session_id,),
                ).fetchone()
        except (OSError, sqlite3.Error):
            return 0
        return int(row["value"] or 0) if row else 0

    def _completed_answer(self, session_id: str, after: int) -> str | None:
        try:
            with self._connect() as db:
                user = db.execute(
                    "SELECT id FROM message WHERE session_id=? AND time_created>? "
                    "AND json_extract(data,'$.role')='user' "
                    "ORDER BY time_created DESC, id DESC LIMIT 1",
                    (session_id, after),
                ).fetchone()
                if user is None:
                    return None
                self._progress("turn", "confirmed")
                assistant = db.execute(
                    "SELECT id FROM message WHERE session_id=? "
                    "AND json_extract(data,'$.role')='assistant' "
                    "AND json_extract(data,'$.parentID')=? "
                    "AND json_extract(data,'$.time.completed') IS NOT NULL "
                    "AND json_extract(data,'$.finish')='stop' "
                    "ORDER BY time_created DESC, id DESC LIMIT 1",
                    (session_id, str(user["id"])),
                ).fetchone()
                if assistant is None:
                    return None
                parts = db.execute(
                    "SELECT json_extract(data,'$.text') AS text FROM part "
                    "WHERE session_id=? AND message_id=? "
                    "AND json_extract(data,'$.type')='text' "
                    "ORDER BY time_created, id",
                    (session_id, str(assistant["id"])),
                ).fetchall()
        except (OSError, sqlite3.Error):
            return None
        text = "".join(str(row["text"] or "") for row in parts).strip()
        return text or None

    def close(self) -> None:
        if self.closed:
            return
        _terminate(self._fork_server)
        self._fork_server = None
        _terminate(self.process)
        self.process = None
        if self.thread_id is None:
            if self._fork_uncertain:
                raise ConsultationError(
                    "OpenCode fork response was lost; temporary session identity is unknown. "
                    "Inspect OpenCode sessions before retrying; Pika will not guess which session to delete."
                )
            self.closed = True
            return
        executable = configured_executable("opencode")
        if not executable_available(executable):
            raise ConsultationError(
                f"OpenCode side {self.thread_id} was not deleted: executable unavailable"
            )
        try:
            result = subprocess.run(
                [str(executable), "--pure", "session", "delete", self.thread_id],
                cwd=self.session.cwd if _valid_cwd(self.session.cwd) else None,
                env=_opencode_ephemeral_environment(),
                capture_output=True,
                text=True,
                timeout=15,
                check=False,
            )
        except (OSError, subprocess.TimeoutExpired) as exc:
            raise ConsultationError(
                f"OpenCode side {self.thread_id} cleanup failed: {exc}"
            ) from exc
        if result.returncode:
            detail = (result.stderr or result.stdout).strip()
            raise ConsultationError(
                f"OpenCode side {self.thread_id} cleanup failed"
                + (f": {detail[-500:]}" if detail else "")
            )
        if self._session_exists(self.thread_id):
            raise ConsultationError(
                f"OpenCode side {self.thread_id} still exists after cleanup"
            )
        self.thread_id = None
        self.closed = True

    def _session_exists(self, session_id: str) -> bool:
        try:
            with self._connect() as db:
                row = db.execute(
                    "SELECT 1 FROM session WHERE id=?", (session_id,)
                ).fetchone()
        except (OSError, sqlite3.Error):
            # Cleanup cannot be certified when the provider store is unreadable.
            return True
        return row is not None


def consultation_for(
    session: Session,
    *,
    fast: bool = False,
    policy: ConsultationPolicy | None = None,
) -> Consultation:
    policy = _validated_policy(session, policy, fast=fast)
    if session.provider == "codex":
        return CodexConsultation(session, policy=policy)
    if session.provider == "claude":
        return ClaudeConsultation(session, policy=policy)
    if session.provider == "opencode":
        return OpenCodeConsultation(session, policy=policy)
    raise ConsultationError(f"Unsupported provider: {session.provider}")


def _valid_cwd(value: str | None) -> bool:
    return bool(value and Path(value).is_dir())


def _version_tuple(value: str) -> tuple[int, ...]:
    match = re.search(r"\b(\d+)\.(\d+)\.(\d+)\b", value)
    return tuple(map(int, match.groups())) if match else ()
