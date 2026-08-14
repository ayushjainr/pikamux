from __future__ import annotations

import json
import os
import re
import select
import shutil
import subprocess
import time
from abc import ABC, abstractmethod
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from . import __version__
from .models import Session


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

    @abstractmethod
    def ask(self, question: str) -> str:
        raise NotImplementedError

    @abstractmethod
    def close(self) -> None:
        raise NotImplementedError

    def __enter__(self) -> Consultation:
        return self

    def __exit__(self, *_args: object) -> None:
        self.close()


def _ephemeral_environment() -> dict[str, str]:
    environment = os.environ.copy()
    # Pika's own hooks must not inventory the provider's transient fork.
    environment["PIKA_EPHEMERAL"] = "1"
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
        self._start()

    def _start(self) -> None:
        if shutil.which("codex") is None:
            raise ConsultationError("Codex is not installed or not on PATH")
        try:
            self.process = subprocess.Popen(
                ["codex", "app-server", "--stdio"],
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
                "threadId": self.session.session_id,
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
        except (OSError, BrokenPipeError, ValueError, ConsultationError):
            self.close()
            raise
        thread = result.get("thread") if isinstance(result, dict) else None
        thread_id = thread.get("id") if isinstance(thread, dict) else None
        if not thread_id or not thread.get("ephemeral"):
            self.close()
            raise ConsultationError(
                "Codex did not confirm an ephemeral fork; refusing to continue"
            )
        observed_model = result.get("model")
        observed_effort = result.get("reasoningEffort")
        if (
            observed_model != self.policy.model
            or observed_effort != self.policy.effort
        ):
            self.close()
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
        result = self._request(
            "turn/start", params, timeout=self.TURN_START_TIMEOUT_SECONDS
        )
        turn = result.get("turn") if isinstance(result, dict) else None
        turn_id = turn.get("id") if isinstance(turn, dict) else None
        if not turn_id:
            raise ConsultationError("Codex did not start the side turn")
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
        self._check_capability()
        self._start()

    def _check_capability(self) -> None:
        if shutil.which("claude") is None:
            raise ConsultationError("Claude is not installed or not on PATH")
        try:
            result = subprocess.run(
                ["claude", "--version"],
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
        try:
            self.process = subprocess.Popen(
                [
                    "claude",
                    "-p",
                    "--resume",
                    self.session.session_id,
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
    raise ConsultationError(f"Unsupported provider: {session.provider}")


def _valid_cwd(value: str | None) -> bool:
    return bool(value and Path(value).is_dir())


def _version_tuple(value: str) -> tuple[int, ...]:
    match = re.search(r"\b(\d+)\.(\d+)\.(\d+)\b", value)
    return tuple(map(int, match.groups())) if match else ()
