"""Metadata-only consultation receipts shared by CLI and board consumers."""
from __future__ import annotations

import time
from typing import Any, Callable

from .consult import ConsultationError


class ConsultationRun:
    """Observe one side conversation; never retry a submitted question.

    Delivery is confirmed only by provider evidence or a completed answer.
    Callers retain answers independently of the subsequent cleanup outcome.
    """

    def __init__(self, on_event: Callable[[dict[str, Any]], None] | None = None):
        self._on_event = on_event
        self._observer_failure: Exception | None = None
        self.started = self.stage_started = time.monotonic()
        self.stage = "prepare"
        self.delivery = "not_sent"
        self.cleanup = "pending"
        self.turn = 0
        self.answers_received = 0
        self.stage_durations: dict[str, float] = {}
        self.consultation: Any = None
        self._close_attempted = False

    @classmethod
    def open(cls, opener: Callable[[], Any], *, on_event=None) -> ConsultationRun:
        run = cls(on_event)
        run._emit()
        if run._observer_failure is not None:
            error = ConsultationError("Consultation receiver closed before preparation; no question was sent")
            error.cleanup = "complete"
            raise error from run._observer_failure
        try:
            run.consultation = opener()
        except (Exception, KeyboardInterrupt) as exc:
            run.cleanup = getattr(exc, "cleanup", "unknown")
            run._failure(exc)
            raise
        # Opt-in on concrete provider/transport implementations only. A generic
        # third-party object cannot establish delivery merely by being called.
        if callable(getattr(type(run.consultation), "set_progress_callback", None)):
            run.consultation.set_progress_callback(run._provider_progress)
        run._emit(outcome="complete")
        if run._observer_failure is not None:
            run.close()
            raise ConsultationError("Consultation receiver closed; no question was sent") from run._observer_failure
        return run

    @property
    def policy(self):
        return self.consultation.policy

    def receipt(self) -> dict[str, Any]:
        now = time.monotonic()
        durations = dict(self.stage_durations)
        durations[self.stage] = durations.get(self.stage, 0.0) + max(0.0, now - self.stage_started)
        return {
            "receipt_version": 2,
            "stage": self.stage,
            "elapsed_seconds": round(max(0.0, now - self.started), 3),
            "stage_elapsed_seconds": round(max(0.0, now - self.stage_started), 3),
            "turn": self.turn,
            "delivery": self.delivery,
            "cleanup": self.cleanup,
            "answers_received": self.answers_received,
            "stage_durations_seconds": {key: round(value, 3) for key, value in durations.items()},
        }

    def _emit(self, **extra: Any) -> None:
        if self._on_event:
            try:
                self._on_event({"type": "progress", **self.receipt(), **extra})
            except Exception as exc:
                # A disconnected pipe or closed panel cannot prevent provider
                # teardown. Disable this observer while preserving the handle.
                self._observer_failure = exc
                self._on_event = None

    def _stage(self, stage: str) -> None:
        if stage != self.stage:
            now = time.monotonic()
            self.stage_durations[self.stage] = self.stage_durations.get(self.stage, 0.0) + max(0.0, now - self.stage_started)
            self.stage, self.stage_started = stage, now
        self._emit()

    def _provider_progress(self, stage: str, delivery: str) -> None:
        if self.stage == stage and self.delivery == delivery:
            return
        self.delivery = delivery
        self._stage(stage)

    def _failure(self, exc: BaseException) -> None:
        metadata = self.receipt()
        # Transport errors may contain stronger evidence from the remote node.
        remote = getattr(exc, "receipt", None)
        if isinstance(remote, dict):
            for key in ("stage", "delivery", "cleanup", "cleanup_error"):
                if key in remote:
                    metadata[key] = remote[key]
        cleanup_error = getattr(exc, "cleanup_error", None)
        if cleanup_error:
            metadata["cleanup_error"] = str(cleanup_error)
        self.delivery = metadata["delivery"]
        self.cleanup = metadata["cleanup"]
        metadata.update(
            outcome="failed", retry_safe=(
                metadata["delivery"] == "not_sent"
                and metadata["stage"] in {"prepare", "turn"}
                and metadata["cleanup"] not in {"unknown", "failed"}
            ),
            message=str(exc) or "Consultation interrupted",
        )
        exc.receipt = metadata  # type: ignore[attr-defined]
        self._emit(**metadata)

    def ask(self, question: str) -> str:
        self.turn += 1
        self.delivery = "not_sent"
        self._stage("turn")
        if self._observer_failure is not None:
            exc = ConsultationError("Consultation receiver closed before submission; no question was sent")
            self._failure(exc)
            raise exc from self._observer_failure
        if self._close_attempted or not question.strip():
            exc = ConsultationError(
                "Side consultation is closed" if self._close_attempted else "Question cannot be empty"
            )
            self._failure(exc)
            raise exc
        # Unknown until the provider can prove otherwise; do not infer delivery
        # from the fact that a method was invoked or stdin was flushed.
        self.delivery = "unknown"
        try:
            answer = self.consultation.ask(question)
        except (Exception, KeyboardInterrupt) as exc:
            self._failure(exc)
            raise
        self.delivery = "confirmed"
        self.answers_received += 1
        self._stage("response")
        self._emit(outcome="complete")
        return answer

    def close(self) -> None:
        if self._close_attempted:
            if self.cleanup != "complete":
                raise ConsultationError("Side cleanup was not confirmed; do not resend the question")
            return
        self._close_attempted = True
        self._stage("cleanup")
        try:
            self.consultation.close()
        except (Exception, KeyboardInterrupt) as exc:
            self.cleanup = "failed"
            self._failure(exc)
            raise
        self.cleanup = "complete"
        self._emit(outcome="complete")

    def __enter__(self):
        return self

    def __exit__(self, exc_type, exc, traceback):
        try:
            self.close()
        except (Exception, KeyboardInterrupt) as cleanup_error:
            if exc is None:
                raise
            # Preserve the original delivery failure while retaining evidence
            # that teardown also failed.
            receipt = dict(getattr(exc, "receipt", self.receipt()))
            receipt.update(cleanup="failed", cleanup_error=str(cleanup_error))
            exc.receipt = receipt
        return False
