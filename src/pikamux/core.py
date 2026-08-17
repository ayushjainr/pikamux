from __future__ import annotations

import difflib
import json
import os
import shlex
import subprocess
import sys
import time
import uuid
from collections.abc import Iterable
from pathlib import Path

from .client_bridge import (
    ClientBridgeError,
    ClientBridgeUnavailable,
    ClientLaunchReceipt,
    make_launch_request,
    request_client_launch,
)
from .consult import ConsultationPolicy, consultation_for, consultation_policy
from .experts import (
    ExpertCardState,
    ExpertMatch,
    card_state,
    interview_profile,
    make_profile,
    rank_experts,
    transcript_fingerprint,
)
from .fleet import REMOTE_STALE_SECONDS, FleetManager
from .models import (
    Candidate,
    ExpertProfile,
    ExpertRefreshAttempt,
    ExpertRefreshResult,
    FleetSession,
    Pane,
    PendingLaunch,
    Session,
    Status,
)
from .processes import (
    find_processes_with_session_id,
    process_alive,
    process_environment,
    process_start_time,
    process_stats,
    process_tree,
    provider_process,
    shared_provider_process,
)
from .providers import Provider, codex_session_metadata, providers
from .quota import read_provider_quota
from .setup_hooks import hooks_installed
from .store import LIVE_OWNER_LEASE_SECONDS, Store, load_config
from .tmux import Tmux, TmuxError
from .ui import choose_session, sorted_attention_sessions, terminal_text

DUPLICATE_TMUX_ERROR = "duplicate Pika tmux homes"
UNVERIFIED_PANE_ERROR = "tagged provider PID lacks exact-UUID evidence"
OPEN_TWICE_ERROR = "exact provider UUID is open in multiple process trees"
CONTINUATION_CONFLICT_ERROR = "multiple active Codex continuation threads"
CONTINUATION_FRESH_SECONDS = 30 * 60
EXPERT_REFRESH_WINDOW_SECONDS = 6 * 60 * 60
EXPERT_QUOTA_RESERVE_PERCENT = 10.0
PENDING_LAUNCH_WINDOW_SECONDS = 60.0
PENDING_LAUNCH_GRACE_SECONDS = 90.0
PENDING_CANDIDATE_STABILITY_SECONDS = 2.0
# Do not bind a missed-hook Codex launch until the entire candidate creation
# window has elapsed. This lets late provider state arrive before uniqueness is
# evaluated instead of allowing an earlier unrelated same-cwd row to win.
PENDING_CANDIDATE_SETTLE_SECONDS = PENDING_LAUNCH_WINDOW_SECONDS


class PikaError(RuntimeError):
    pass


class Pika:
    def __init__(
        self,
        store: Store | None = None,
        tmux: Tmux | None = None,
        provider_map: dict[str, Provider] | None = None,
        fleet: FleetManager | None = None,
    ):
        self.store = store or Store()
        self.tmux = tmux or Tmux()
        self.providers = provider_map or providers()
        self.discovery_errors: list[str] = []
        self.usage_errors: list[str] = []
        self._last_discovered_candidates: list[Candidate] = []
        self.store.initialize()
        self.fleet = fleet or FleetManager(self.store)

    def discover_candidates(
        self, tracked_sessions: list[Session] | None = None
    ) -> list[Candidate]:
        self.discovery_errors = []
        result: list[Candidate] = []
        for provider in self.providers.values():
            try:
                result.extend(provider.discover())
                tracked = getattr(provider, "tracked_candidates", None)
                if tracked and tracked_sessions:
                    result.extend(
                        tracked(
                            item
                            for item in tracked_sessions
                            if item.provider == provider.name
                        )
                    )
            except Exception as exc:  # noqa: BLE001 - one provider must not hide the other
                self.discovery_errors.append(f"{provider.name}: {exc}")
        by_key: dict[tuple[str, str], Candidate] = {}
        for item in result:
            key = (item.provider, item.session_id)
            old = by_key.get(key)
            if old is None:
                by_key[key] = item
                continue
            primary, secondary = (
                (item, old) if item.updated_at >= old.updated_at else (old, item)
            )
            primary.name = primary.name or secondary.name
            primary.cwd = primary.cwd or secondary.cwd
            primary.branch = primary.branch or secondary.branch
            primary.transcript_path = (
                primary.transcript_path or secondary.transcript_path
            )
            primary.model = primary.model or secondary.model
            primary.parent_session_id = (
                primary.parent_session_id or secondary.parent_session_id
            )
            primary.created_at = max(primary.created_at, secondary.created_at)
            primary.lifecycle_status = (
                primary.lifecycle_status or secondary.lifecycle_status
            )
            live_pid = next(
                (
                    candidate.pid
                    for candidate in (item, old)
                    if candidate.live and candidate.pid
                ),
                None,
            )
            primary.live = primary.live or secondary.live
            primary.pid = live_pid or primary.pid or secondary.pid
            primary.updated_at = max(primary.updated_at, secondary.updated_at)
            by_key[key] = primary
        discovered = sorted(
            by_key.values(), key=lambda item: item.updated_at, reverse=True
        )
        self._last_discovered_candidates = discovered
        return discovered

    def discover_import_candidates(self) -> list[Candidate]:
        """Run the slower one-time discovery used only by `pika setup`."""
        self.discovery_errors = []
        result: list[Candidate] = []
        for provider in self.providers.values():
            try:
                for item in provider.import_candidates():
                    if item.live or provider.is_resumable(item.session_id):
                        result.append(item)
            except Exception as exc:  # noqa: BLE001 - imports are best-effort
                self.discovery_errors.append(f"{provider.name}: {exc}")
        by_key = {(item.provider, item.session_id): item for item in result}
        return sorted(by_key.values(), key=lambda item: item.updated_at, reverse=True)

    def discover_exact_candidates(self, query: str) -> list[Candidate]:
        """Look up one daily name/UUID without setup's bulk resumability scan."""
        result: list[Candidate] = []
        for provider in self.providers.values():
            finder = getattr(provider, "find_candidates", None)
            try:
                matches = list(
                    finder(query) if callable(finder) else provider.import_candidates()
                )
                for item in matches:
                    if item.session_id != query and not (
                        item.name and item.name.casefold() == query.casefold()
                    ):
                        continue
                    if item.live or provider.is_resumable(item.session_id):
                        result.append(item)
            except Exception as exc:  # noqa: BLE001 - exact lookup is best-effort
                self.discovery_errors.append(f"{provider.name} exact lookup: {exc}")
        by_key = {(item.provider, item.session_id): item for item in result}
        return sorted(by_key.values(), key=lambda item: item.updated_at, reverse=True)

    @staticmethod
    def _same_path(left: str | None, right: str | None) -> bool:
        if not left or not right:
            return False
        try:
            return Path(left).resolve() == Path(right).resolve()
        except OSError:
            return left == right

    def _pending_candidates(self, row: dict[str, object]) -> list[Candidate]:
        provider_name = str(row["provider"])
        provider = self.providers.get(provider_name)
        if provider is None:
            return []
        discover = getattr(provider, "launch_candidates", None)
        try:
            candidates = list(discover() if callable(discover) else provider.discover())
        except (OSError, RuntimeError):
            return []
        created_at = float(row["created_at"])
        expected_id = str(row.get("expected_session_id") or "")
        if (
            provider_name == "codex"
            and not expected_id
            and row.get("preexisting_session_ids_json") is None
        ):
            # Without a successful pre-launch baseline, time/cwd matching can
            # mistake an older unnamed thread for the newly launched one.
            return []
        try:
            preexisting = set(
                json.loads(str(row.get("preexisting_session_ids_json") or "[]"))
            )
        except (TypeError, ValueError):
            preexisting = set()
        matches: list[Candidate] = []
        for candidate in candidates:
            if candidate.provider != provider_name:
                continue
            if candidate.session_id in preexisting:
                continue
            if expected_id and candidate.session_id != expected_id:
                continue
            if not self._same_path(candidate.cwd, str(row["cwd"])):
                continue
            if not candidate.created_at or not (
                created_at - 2 <= candidate.created_at <= created_at + PENDING_LAUNCH_WINDOW_SECONDS
            ):
                continue
            if provider_name == "codex" and not codex_session_metadata(
                candidate.session_id, candidate.transcript_path
            ):
                continue
            if provider_name == "codex" and candidate.parent_session_id:
                continue
            matches.append(candidate)
        return matches

    def reconcile_pending_launches(self, panes: list[Pane] | None = None) -> int:
        """Recover a Pika-created launch when its provider hook never arrived."""
        panes = panes if panes is not None else self.tmux.list_panes()
        repaired = 0
        for row in self.store.list_pending():
            token = str(row["launch_token"])
            if self.store.get_meta(f"launch_binding_error:{token}"):
                continue
            provider_name = str(row["provider"])
            matching = [
                pane
                for pane in panes
                if (row.get("tmux_pane") and pane.pane_id == row["tmux_pane"])
                or (row.get("tmux_session") and pane.session_name == row["tmux_session"])
                or pane.pika_launch_token == token
            ]
            matching = list({pane.pane_id: pane for pane in matching}.values())
            if len(matching) != 1:
                continue
            pane = matching[0]
            if pane.pika_launch_token != token:
                continue
            provider_pid = provider_process(pane.pane_pid, provider_name)
            other_provider = "claude" if provider_name == "codex" else "codex"
            if not provider_pid or provider_process(pane.pane_pid, other_provider):
                continue
            environment = process_environment(provider_pid)
            if (
                environment.get("PIKA_LAUNCH_TOKEN") != token
                or environment.get("PIKA_PROVIDER") != provider_name
            ):
                continue
            pid_start = process_start_time(provider_pid)
            if pid_start is None:
                continue
            saved_pid = row.get("root_pid")
            saved_start = row.get("root_pid_start")
            if saved_pid is not None and int(saved_pid) != provider_pid:
                continue
            if saved_start is not None and int(saved_start) != pid_start:
                continue
            if saved_pid is None:
                self.store.finalize_pending_pane(
                    token,
                    pane.session_name,
                    pane.pane_id,
                    provider_pid,
                    pid_start,
                )
            binding = self.store.get_launch_binding(token)
            candidates = self._pending_candidates(row)
            if binding:
                candidates = [
                    item
                    for item in candidates
                    if (item.provider, item.session_id) == binding
                ]
            if len(candidates) != 1:
                continue
            candidate = candidates[0]
            expected_id = str(row.get("expected_session_id") or "")
            if not expected_id:
                if (
                    time.time() - float(row["created_at"])
                    < PENDING_CANDIDATE_SETTLE_SECONDS
                ):
                    continue
                first_seen = self.store.observe_pending_candidate(
                    token, candidate.session_id
                )
                if (
                    first_seen is None
                    or time.time() - first_seen
                    < PENDING_CANDIDATE_STABILITY_SECONDS
                ):
                    continue
            provider = self.providers.get(provider_name)
            if provider is None:
                continue
            active_pids = set(provider.active_pids(candidate.session_id))
            pane_tree = set(process_tree(pane.pane_pid))
            if len(active_pids) > 1 or (
                active_pids and not active_pids.issubset(pane_tree)
            ):
                continue
            if not self.store.bind_launch(token, provider_name, candidate.session_id):
                continue
            now = time.time()
            status = candidate.lifecycle_status or Status.WORKING.value
            unread = status == Status.READY.value and not pane.attached
            session = Session(
                provider=provider_name,
                session_id=candidate.session_id,
                name=str(row["name"]),
                cwd=candidate.cwd or str(row["cwd"]),
                branch=candidate.branch,
                transcript_path=candidate.transcript_path,
                tmux_session=pane.session_name,
                tmux_pane=pane.pane_id,
                root_pid=provider_pid,
                status=status,
                unread=unread,
                model=candidate.model,
                source="managed-recovery",
                managed=True,
                attention_reason="completed" if unread else None,
                created_at=candidate.created_at or float(row["created_at"]),
                updated_at=now,
                last_event_at=candidate.updated_at or now,
                last_activity_at=max(candidate.updated_at, pane.activity),
            )
            try:
                self.tmux.tag_pane(
                    pane.pane_id,
                    provider=provider_name,
                    session_id=candidate.session_id,
                    name=str(row["name"]),
                    launch_token="",
                )
            except (AttributeError, OSError, TmuxError):
                continue
            self.store.restore_tracking(provider_name, candidate.session_id)
            self.store.upsert_session(session)
            self.store.set_recovery_owner(
                provider_name,
                candidate.session_id,
                provider_pid,
                pid_start,
                token,
            )
            self.store.delete_pending(token)
            attach_key = f"attached_launch:{token}"
            if self.store.get_meta(attach_key):
                self.store.record_attach(provider_name, candidate.session_id)
                self.store.delete_meta(attach_key)
            if provider_name == "codex" and candidate.name != str(row["name"]):
                try:
                    named = provider.set_native_name(candidate.session_id, str(row["name"]))
                except (AttributeError, OSError, RuntimeError):
                    named = False
                error_key = f"native_name_error:codex:{candidate.session_id}"
                if named:
                    self.store.delete_meta(error_key)
                else:
                    self.store.set_meta(error_key, str(row["name"]))
            repaired += 1
        return repaired

    def pending_launches(self, panes: list[Pane] | None = None) -> list[PendingLaunch]:
        panes = panes if panes is not None else self.tmux.list_panes()
        by_id = {pane.pane_id: pane for pane in panes}
        by_name = {pane.session_name: pane for pane in panes}
        now = time.time()
        result: list[PendingLaunch] = []
        for row in self.store.list_pending():
            pane = by_id.get(str(row.get("tmux_pane") or "")) or by_name.get(
                str(row.get("tmux_session") or "")
            )
            live_pid = (
                provider_process(pane.pane_pid, str(row["provider"])) if pane else None
            )
            age = max(0.0, now - float(row["created_at"]))
            stale = age > PENDING_LAUNCH_GRACE_SECONDS
            conflict = self.store.get_meta(
                f"launch_binding_error:{row['launch_token']}"
            )
            failed = stale or bool(conflict)
            result.append(
                PendingLaunch(
                    launch_token=str(row["launch_token"]),
                    provider=str(row["provider"]),
                    name=str(row["name"]),
                    cwd=str(row["cwd"]),
                    tmux_session=str(row["tmux_session"]) if row["tmux_session"] else None,
                    tmux_pane=str(row["tmux_pane"]) if row["tmux_pane"] else None,
                    created_at=float(row["created_at"]),
                    expected_session_id=(
                        str(row["expected_session_id"])
                        if row.get("expected_session_id")
                        else None
                    ),
                    root_pid=(
                        int(row["root_pid"]) if row.get("root_pid") is not None else None
                    ),
                    root_pid_start=(
                        int(row["root_pid_start"])
                        if row.get("root_pid_start") is not None
                        else None
                    ),
                    status=Status.ERROR.value if failed else Status.STARTING.value,
                    live=bool(live_pid),
                    attached=bool(pane and pane.attached),
                    error=(
                        "IDENTITY CONFLICT: " + conflict
                        if conflict
                        else "IDENTITY PENDING: exact provider UUID not yet proven"
                        if stale
                        else None
                    ),
                    attention_reason="identity" if failed else "identity pending",
                    last_activity_at=pane.activity if pane else float(row["created_at"]),
                    last_event_at=float(row["created_at"]),
                    unread=failed,
                )
            )
        return result

    def hidden_session_keys(
        self, tracked_sessions: Iterable[Session] = ()
    ) -> set[tuple[str, str]]:
        """Return provider-owned records that should not enter the daily surface."""
        result: set[tuple[str, str]] = set()
        for provider in self.providers.values():
            hidden = getattr(provider, "hidden_session_ids", None)
            if hidden is None:
                continue
            try:
                result.update((provider.name, session_id) for session_id in hidden())
            except Exception as exc:  # noqa: BLE001 - discovery remains best-effort
                self.discovery_errors.append(f"{provider.name} hidden sessions: {exc}")
        for session in tracked_sessions:
            provider = self.providers.get(session.provider)
            if provider is None:
                continue
            classifier = getattr(provider, "worker_originator", None)
            if classifier is None:
                continue
            try:
                if classifier(session.session_id, session.transcript_path):
                    result.add(session.key)
            except Exception as exc:  # noqa: BLE001 - provenance is fail-open
                self.discovery_errors.append(
                    f"{session.provider}:{session.session_id} provenance: {exc}"
                )
        return result

    def import_candidate(
        self, candidate: Candidate, *, managed: bool = False
    ) -> Session:
        self.store.restore_tracking(candidate.provider, candidate.session_id)
        now = time.time()
        adopted_pane: Pane | None = None
        identity_pids = self._candidate_identity_pids(candidate)
        if len(identity_pids) == 1:
            identity_pid = next(iter(identity_pids))
            other_provider = "claude" if candidate.provider == "codex" else "codex"
            matching_panes = [
                pane
                for pane in self.tmux.list_panes()
                if (
                    identity_pid in process_tree(pane.pane_pid)
                    or provider_process(pane.pane_pid, candidate.provider)
                    == identity_pid
                )
                and provider_process(pane.pane_pid, candidate.provider)
                and not provider_process(pane.pane_pid, other_provider)
                and (
                    not pane.pika_session_id
                    or pane.pika_session_id.startswith("unbound:")
                    or (pane.pika_provider, pane.pika_session_id)
                    == (candidate.provider, candidate.session_id)
                )
            ]
            if len(matching_panes) == 1:
                proposed = matching_panes[0]
                try:
                    self.tmux.tag_pane(
                        proposed.pane_id,
                        provider=candidate.provider,
                        session_id=candidate.session_id,
                        name=candidate.display_name,
                    )
                except (AttributeError, OSError, TmuxError):
                    pass
                else:
                    adopted_pane = proposed
                    managed = True
        status = (
            candidate.lifecycle_status or Status.WORKING.value
            if adopted_pane
            else Status.UNBOUND.value
            if candidate.live
            else Status.PARKED.value
        )
        session = Session(
            provider=candidate.provider,
            session_id=candidate.session_id,
            name=candidate.name,
            cwd=candidate.cwd,
            branch=candidate.branch,
            transcript_path=candidate.transcript_path,
            tmux_session=adopted_pane.session_name if adopted_pane else None,
            tmux_pane=adopted_pane.pane_id if adopted_pane else None,
            root_pid=(
                provider_process(adopted_pane.pane_pid, candidate.provider)
                if adopted_pane
                else candidate.pid or (next(iter(identity_pids)) if identity_pids else None)
            ),
            status=status,
            unread=False,
            model=candidate.model,
            source=candidate.source,
            managed=managed,
            created_at=candidate.updated_at or now,
            updated_at=now,
            last_event_at=now,
            last_activity_at=candidate.updated_at or now,
        )
        self.store.upsert_session(session)
        return session

    def _candidate_identity_pids(self, candidate: Candidate) -> set[int]:
        result = {candidate.pid} if candidate.pid else set()
        provider = self.providers.get(candidate.provider)
        active_pids = getattr(provider, "active_pids", None) if provider else None
        if active_pids:
            try:
                result.update(active_pids(candidate.session_id))
            except (OSError, RuntimeError):
                pass
        return {int(pid) for pid in result if pid}

    def refresh(self, *, usage: bool = False) -> list[Session]:
        panes = self.tmux.list_panes()
        self.reconcile_pending_launches(panes)
        tracked_sessions = self.store.list_sessions()
        candidates = self.discover_candidates(tracked_sessions)
        hidden_keys = self.hidden_session_keys(tracked_sessions)
        # Hidden provider rows stay provider-owned.  Automation workers are
        # also removed from Pika's operational ledger so stale READY/error
        # events cannot survive after their short-lived transcripts disappear.
        for session in tracked_sessions:
            if session.key not in hidden_keys:
                continue
            provider = self.providers.get(session.provider)
            classifier = getattr(provider, "worker_originator", None)
            if classifier and classifier(session.session_id, session.transcript_path):
                self.store.delete_session(*session.key)
        untracked_keys = self.store.untracked_session_keys()
        excluded_keys = hidden_keys | untracked_keys
        candidates = [
            item
            for item in candidates
            if (item.provider, item.session_id) not in excluded_keys
        ]
        candidate_map = {(item.provider, item.session_id): item for item in candidates}
        panes = self.tmux.list_panes()
        pane_by_id = {pane.pane_id: pane for pane in panes}
        pane_by_key = {
            (pane.pika_provider, pane.pika_session_id): pane
            for pane in panes
            if pane.pika_provider and pane.pika_session_id
        }
        panes_by_key: dict[tuple[str, str], list[Pane]] = {}
        for pane in panes:
            if pane.pika_provider and pane.pika_session_id:
                panes_by_key.setdefault(
                    (pane.pika_provider, pane.pika_session_id), []
                ).append(pane)
        # An untracked live process may keep emitting provider hooks. Its Pika
        # tombstone is authoritative, and stale pane tags must not resurrect it.
        for key in untracked_keys:
            for pane in panes_by_key.get(key, []):
                try:
                    self.tmux.clear_pika_tags(pane.pane_id)
                except (AttributeError, OSError, TmuxError):
                    pass
        # Recover tagged panes even if the ledger was lost.
        known_keys = {
            session.key
            for session in self.store.list_sessions()
            if session.key not in excluded_keys
        }
        for key, pane in pane_by_key.items():
            assert key[0] is not None and key[1] is not None
            if key in known_keys or key in excluded_keys:
                continue
            candidate = candidate_map.get((key[0], key[1]))
            now = time.time()
            self.store.upsert_session(
                Session(
                    provider=key[0],
                    session_id=key[1],
                    name=pane.pika_name or (candidate.name if candidate else None),
                    cwd=(candidate.cwd if candidate else None) or pane.cwd,
                    branch=candidate.branch if candidate else None,
                    transcript_path=candidate.transcript_path if candidate else None,
                    tmux_session=pane.session_name,
                    tmux_pane=pane.pane_id,
                    root_pid=provider_process(pane.pane_pid, key[0]),
                    status=Status.READY.value,
                    unread=False,
                    model=candidate.model if candidate else None,
                    source="tmux-recovery",
                    managed=True,
                    created_at=pane.created,
                    updated_at=now,
                    last_event_at=now,
                    last_activity_at=pane.activity,
                )
            )
        sessions = [
            session
            for session in self.store.list_sessions()
            if session.key not in excluded_keys
        ]
        exact_panes = {(p.pika_provider, p.pika_session_id): p for p in panes}
        for session in sessions:
            pane = exact_panes.get(session.key)
            conflicting_stored_pane = False
            if pane is None and session.tmux_pane:
                pane = pane_by_id.get(session.tmux_pane)
                if (
                    pane
                    and (pane.pika_provider or pane.pika_session_id)
                    and (
                        pane.pika_provider,
                        pane.pika_session_id,
                    )
                    != session.key
                ):
                    conflicting_stored_pane = True
                    pane = None
            fields: dict[str, object] = {}
            candidate, continuation_conflicts = self._continuation_candidate(
                session,
                candidates,
                pane=pane,
            )
            if candidate and candidate.session_id != session.session_id:
                fields["active_thread_id"] = candidate.session_id
                session.active_thread_id = candidate.session_id
            if conflicting_stored_pane:
                fields.update(tmux_session=None, tmux_pane=None, root_pid=None)
            duplicate_panes = panes_by_key.get(session.key, [])
            if len(duplicate_panes) > 1:
                if not (
                    session.status == Status.ERROR.value
                    and session.attention_reason == "identity"
                ):
                    self.store.capture_identity_interruption(*session.key)
                homes = ", ".join(
                    f"{item.session_name}:{item.pane_id}" for item in duplicate_panes
                )
                fields.update(
                    status=Status.ERROR.value,
                    unread=True,
                    error=f"{DUPLICATE_TMUX_ERROR}: {homes}",
                    attention_reason="identity",
                )
            elif continuation_conflicts:
                if not (
                    session.status == Status.OPEN_TWICE.value
                    and session.attention_reason == "identity"
                ):
                    self.store.capture_identity_interruption(*session.key)
                fields.update(
                    status=Status.OPEN_TWICE.value,
                    unread=True,
                    error=(
                        f"{CONTINUATION_CONFLICT_ERROR}: "
                        + ", ".join(
                            item.session_id[:8] for item in continuation_conflicts
                        )
                    ),
                    attention_reason="identity",
                )
                session.status = Status.OPEN_TWICE.value
                session.unread = True
                session.error = str(fields["error"])
                session.attention_reason = "identity"
            if candidate:
                if candidate.name:
                    fields["name"] = candidate.name
                    if pane and pane.pika_name != candidate.name:
                        try:
                            self.tmux.tag_pane(
                                pane.pane_id,
                                name=candidate.name,
                            )
                        except (AttributeError, OSError, TmuxError) as exc:
                            self.discovery_errors.append(
                                f"{session.provider}:{session.session_id} "
                                f"pane name tag: {exc}"
                            )
                    if session.provider == "codex":
                        self.store.delete_meta(
                            f"native_name_error:codex:{session.session_id}"
                        )
                if candidate.cwd:
                    fields["cwd"] = candidate.cwd
                if candidate.branch:
                    fields["branch"] = candidate.branch
                if candidate.transcript_path:
                    fields["transcript_path"] = candidate.transcript_path
                if candidate.model:
                    fields["model"] = candidate.model
                fields["last_activity_at"] = max(
                    session.last_activity_at, candidate.updated_at
                )
                if (
                    candidate.lifecycle_status
                    and candidate.updated_at > session.last_event_at
                    and not continuation_conflicts
                ):
                    fields.update(
                        status=candidate.lifecycle_status,
                        unread=(
                            candidate.lifecycle_status == Status.READY.value
                            and not bool(pane and pane.attached)
                        ),
                        error=None,
                        attention_reason=(
                            "completed"
                            if candidate.lifecycle_status == Status.READY.value
                            else None
                        ),
                        last_event_at=candidate.updated_at,
                    )
            direct_uuid_pids = self.uuid_identity_pids(session)
            live_pid: int | None = None
            raw_pid: int | None = None
            if pane:
                raw_pid = provider_process(pane.pane_pid, session.provider)
                live_pid = self.exact_pane_pid(session, pane)
                fields.update(
                    tmux_session=pane.session_name,
                    tmux_pane=pane.pane_id,
                    root_pid=live_pid,
                    last_activity_at=max(session.last_activity_at, pane.activity),
                )
                if raw_pid and not live_pid:
                    if not (
                        session.status in {Status.ERROR.value, Status.OPEN_TWICE.value}
                        and session.attention_reason == "identity"
                    ):
                        self.store.capture_identity_interruption(*session.key)
                    outside_exact = self._outside_uuid_pids(session, pane)
                    if len(direct_uuid_pids) > 1:
                        fields.update(
                            status=Status.OPEN_TWICE.value,
                            unread=True,
                            error=(
                                f"{OPEN_TWICE_ERROR}: PIDs "
                                + ", ".join(map(str, sorted(direct_uuid_pids)))
                            ),
                            attention_reason="identity",
                        )
                    elif outside_exact and raw_pid in direct_uuid_pids:
                        fields.update(
                            status=Status.OPEN_TWICE.value,
                            unread=True,
                            error=(
                                f"{OPEN_TWICE_ERROR}: pane PID {raw_pid}; outside "
                                f"PID {', '.join(map(str, outside_exact))}"
                            ),
                            attention_reason="identity",
                        )
                    else:
                        fields.update(
                            status=Status.ERROR.value,
                            unread=True,
                            error=(
                                f"{UNVERIFIED_PANE_ERROR}: PID {raw_pid} in "
                                f"{pane.session_name}:{pane.pane_id}"
                            ),
                            attention_reason="identity",
                        )
                if pane.dead and not session.unread:
                    fields.update(
                        status=Status.ERROR.value,
                        unread=True,
                        error=f"tmux pane exited with status {pane.dead_status}",
                        attention_reason="exited",
                    )
            elif candidate and candidate.live and process_alive(candidate.pid):
                live_pid = candidate.pid
                fields["root_pid"] = live_pid
            elif session.root_pid:
                stored_pid = provider_process(session.root_pid, session.provider)
                if stored_pid in self.identity_pids(session):
                    live_pid = stored_pid
            if len(direct_uuid_pids) > 1:
                if not (
                    session.status == Status.OPEN_TWICE.value
                    and session.attention_reason == "identity"
                ):
                    self.store.capture_identity_interruption(*session.key)
                fields.update(
                    status=Status.OPEN_TWICE.value,
                    unread=True,
                    error=(
                        f"{OPEN_TWICE_ERROR}: PIDs "
                        + ", ".join(map(str, sorted(direct_uuid_pids)))
                    ),
                    attention_reason="identity",
                )
                session.status = Status.OPEN_TWICE.value
                session.unread = True
                session.attention_reason = "identity"
            if not live_pid and session.root_pid:
                fields["root_pid"] = None
            if (
                not live_pid
                and not (raw_pid and not live_pid)
                and session.status == Status.WORKING.value
            ):
                fields.update(status=Status.PARKED.value, attention_reason=None)
            if (
                not live_pid
                and not (raw_pid and not live_pid)
                and session.status == Status.UNBOUND.value
            ):
                fields.update(
                    status=Status.PARKED.value,
                    unread=False,
                    attention_reason=None,
                )
            identity_repaired = False
            if (
                not continuation_conflicts
                and len(duplicate_panes) <= 1
                and len(direct_uuid_pids) <= 1
                and session.error
                and (
                    session.error.startswith(DUPLICATE_TMUX_ERROR)
                    or session.error.startswith(UNVERIFIED_PANE_ERROR)
                    or session.error.startswith(OPEN_TWICE_ERROR)
                    or session.error.startswith(CONTINUATION_CONFLICT_ERROR)
                )
                and not (raw_pid and not live_pid)
            ):
                identity_repaired = True
            if fields:
                self.store.update_session(
                    session.provider, session.session_id, **fields
                )
            if identity_repaired:
                self.store.restore_identity_interruption(
                    *session.key, live=bool(live_pid)
                )
            if (
                live_pid
                and len(duplicate_panes) <= 1
                and not (
                    session.status in {Status.ERROR.value, Status.OPEN_TWICE.value}
                    and session.attention_reason == "identity"
                )
            ):
                self.store.discard_healthy_identity_interruption(*session.key)
        # Drop placeholder UNBOUND rows once the same pane is bound exactly.
        bound_panes = {
            p.pane_id
            for p in panes
            if p.pika_session_id and not p.pika_session_id.startswith("unbound:")
        }
        for session in self.store.list_sessions():
            if (
                session.session_id.startswith("unbound:")
                and session.tmux_pane in bound_panes
            ):
                self.store.delete_session(session.provider, session.session_id)
        sessions = [
            session
            for session in self.store.list_sessions()
            if session.key not in excluded_keys
        ]
        for session in sessions:
            pane = pane_by_id.get(session.tmux_pane or "")
            if (
                pane
                and pane.pika_session_id
                and (
                    pane.pika_provider,
                    pane.pika_session_id,
                )
                != session.key
            ):
                pane = None
            live_pid = self.exact_pane_pid(session, pane) if pane else None
            if not pane and not live_pid and session.root_pid:
                stored_pid = provider_process(session.root_pid, session.provider)
                if stored_pid in self.identity_pids(session):
                    live_pid = stored_pid
            session.live = bool(live_pid)
            session.attached = bool(pane and pane.attached)
            exact_home = bool(
                pane and live_pid and len(panes_by_key.get(session.key, [])) == 1
            )
            if exact_home:
                session.home_state = "exact-live"
            elif session.attention_reason == "identity" and session.status in {
                Status.ERROR.value,
                Status.OPEN_TWICE.value,
            }:
                session.home_state = (
                    "open-twice"
                    if session.status == Status.OPEN_TWICE.value
                    else "identity-error"
                )
            elif session.status == Status.UNBOUND.value:
                session.home_state = "unbound"
            elif session.live:
                session.home_state = "outside-live"
            elif session.tmux_pane:
                session.home_state = "saved-idle"
            else:
                session.home_state = "no-live-home"
            if live_pid:
                session.cpu_percent, session.rss_kb = process_stats(
                    pane.pane_pid if pane else live_pid
                )
        return self.hydrate_usage(sessions) if usage else sessions

    @staticmethod
    def _same_continuation_identity(session: Session, candidate: Candidate) -> bool:
        if not session.name or not candidate.name:
            return False
        if session.name.casefold() != candidate.name.casefold():
            return False
        if session.cwd and candidate.cwd:
            try:
                if Path(session.cwd).resolve() != Path(candidate.cwd).resolve():
                    return False
            except OSError:
                if session.cwd != candidate.cwd:
                    return False
        return True

    def _continuation_candidate(
        self,
        session: Session,
        candidates: list[Candidate],
        *,
        pane: Pane | None,
    ) -> tuple[Candidate | None, list[Candidate]]:
        """Select one live Codex continuation or fail closed on concurrency."""
        current_id = session.provider_thread_id
        current = next(
            (
                item
                for item in candidates
                if (item.provider, item.session_id) == (session.provider, current_id)
            ),
            None,
        )
        if session.provider != "codex" or pane is None:
            return current, []
        if not provider_process(pane.pane_pid, session.provider):
            return current, []
        cutoff = time.time() - CONTINUATION_FRESH_SECONDS
        working: list[Candidate] = []
        if (
            current
            and current.lifecycle_status == Status.WORKING.value
            and current.updated_at >= cutoff
        ):
            working.append(current)
        continuation_parents = {session.session_id, current_id}
        working.extend(
            item
            for item in candidates
            if item.provider == session.provider
            and item.parent_session_id in continuation_parents
            and item.lifecycle_status == Status.WORKING.value
            and item.updated_at >= cutoff
            and self._same_continuation_identity(session, item)
        )
        unique = {item.session_id: item for item in working}
        if len(unique) > 1:
            return current, sorted(
                unique.values(), key=lambda item: item.updated_at, reverse=True
            )
        if unique:
            return next(iter(unique.values())), []
        return current, []

    def hydrate_usage(
        self, sessions: list[Session | FleetSession]
    ) -> list[Session | FleetSession]:
        """Add provider usage without delaying operational reconciliation."""
        self.usage_errors = []
        for session in sessions:
            if isinstance(session, FleetSession):
                continue
            provider = self.providers.get(session.provider)
            if not provider:
                continue
            try:
                values = provider.usage(session, self.store)
            except Exception as exc:  # noqa: BLE001 - stats are best-effort
                self.usage_errors.append(f"{provider.name}:{session.session_id}: {exc}")
                continue
            if values:
                session.input_tokens = values.input_tokens
                session.output_tokens = values.output_tokens
                session.cached_input_tokens = values.cached_input_tokens
                session.cache_write_tokens = values.cache_write_tokens
                session.total_tokens = values.total_tokens
                session.estimated_cost_usd = values.estimated_cost_usd
                session.model = values.model or session.model
        return sessions

    def monitor_sessions(
        self, local: list[Session] | None = None
    ) -> list[Session | FleetSession | PendingLaunch]:
        """Combine fresh local truth with cache-only remote views."""
        return [
            *(local if local is not None else self.refresh()),
            *self.pending_launches(),
            *self.fleet.cached_sessions(),
        ]

    def refresh_remote_node(self, node_id: str) -> list[FleetSession]:
        return self.fleet.refresh_node(node_id)

    def resolve_target(self, query: str) -> Session | FleetSession:
        """Resolve a local name or an explicit thread@machine route."""
        if "@" in query:
            _thread, alias = query.rsplit("@", 1)
            if self.store.get_fleet_node(alias) is not None:
                remote = self.fleet.resolve(query, fresh=True)
                if remote is not None:
                    local_exact = [
                        item
                        for item in self.refresh()
                        if item.name and item.name.casefold() == query.casefold()
                    ]
                    if local_exact:
                        return choose_session(
                            [*local_exact, remote],
                            f"{query!r} is both a local name and a remote route",
                        )
                    return remote
        try:
            return self.resolve(query)
        except PikaError as exc:
            remote_matches = [
                item.display_name
                for item in self.fleet.cached_sessions()
                if item.name and item.name.casefold() == query.casefold()
            ]
            if remote_matches:
                raise PikaError(
                    f"{exc} Remote matches: {', '.join(remote_matches)}. "
                    "Use a machine-qualified name."
                ) from exc
            raise

    def consultation(
        self,
        session: Session | FleetSession,
        *,
        fast: bool = False,
        policy: ConsultationPolicy | None = None,
    ):
        base = session.session if isinstance(session, FleetSession) else session
        selected = policy or consultation_policy(base, fast=fast)
        if isinstance(session, FleetSession):
            return self.fleet.consultation(session, selected)
        return consultation_for(session, policy=selected)

    def capture(self, session: Session | FleetSession, lines: int) -> str:
        if isinstance(session, FleetSession):
            return self.fleet.capture(session, lines)
        pane = self.tmux.get_pane(session.tmux_pane or session.tmux_session or "")
        if pane is None:
            raise PikaError(
                f"{session.display_name} has no surviving tmux pane to peek"
            )
        return self.tmux.capture(pane.pane_id, lines)

    def resolve(self, query: str, sessions: list[Session] | None = None) -> Session:
        supplied_sessions = sessions is not None
        sessions = sessions or self.refresh()
        discovered: list[Candidate] = []
        query_folded = query.casefold()
        uuid_matches = [
            item
            for item in sessions
            if item.session_id == query or item.provider_thread_id == query
        ]
        if uuid_matches:
            return choose_session(uuid_matches)
        exact = [
            item
            for item in sessions
            if item.name and item.name.casefold() == query_folded
        ]
        if exact:
            return choose_session(exact, "Two continuations share that name")
        prefix = [
            item
            for item in sessions
            if item.session_id.startswith(query)
            or item.provider_thread_id.startswith(query)
        ]
        if len(prefix) == 1:
            return prefix[0]
        if not supplied_sessions:
            untracked = self.store.list_untracked_sessions()
            hidden_exact = [item for item in untracked if item.session_id == query]
            hidden_named = [
                item
                for item in untracked
                if item.name and item.name.casefold() == query_folded
            ]
            hidden_prefix = [
                item for item in untracked if item.session_id.startswith(query)
            ]
            hidden_matches = hidden_exact or hidden_named
            if not hidden_matches and len(hidden_prefix) == 1:
                hidden_matches = hidden_prefix
            if hidden_matches:
                restored = choose_session(
                    hidden_matches,
                    "Two untracked conversations share that name",
                )
                self.store.restore_tracking(*restored.key)
                return restored
            discovered = self.discover_candidates()
            discovered_uuid = [item for item in discovered if item.session_id == query]
            if discovered_uuid:
                return self.import_candidate(choose_session(discovered_uuid))
            discovered_prefix = [
                item for item in discovered if item.session_id.startswith(query)
            ]
            if len(discovered_prefix) == 1:
                return self.import_candidate(discovered_prefix[0])
            discovered_matches = [
                item
                for item in discovered
                if item.name and item.name.casefold() == query_folded
            ]
            if discovered_matches:
                imported = [self.import_candidate(item) for item in discovered_matches]
                return choose_session(
                    imported, "Two native conversations share that name"
                )
        suggestion_pool = [*sessions, *discovered]
        suggestions = list(
            dict.fromkeys(
                item.display_name
                for item in suggestion_pool
                if query_folded in item.display_name.casefold()
                or item.display_name.casefold() in query_folded
            )
        )[:5]
        suffix = f" Close matches: {', '.join(suggestions)}." if suggestions else ""
        raise PikaError(
            f"No saved conversation named {query!r}.{suffix} Use `pika new {query}` to create one."
        )

    @staticmethod
    def _candidate_session(candidate: Candidate) -> Session:
        now = time.time()
        return Session(
            provider=candidate.provider,
            session_id=candidate.session_id,
            name=candidate.name,
            cwd=candidate.cwd,
            branch=candidate.branch,
            transcript_path=candidate.transcript_path,
            root_pid=candidate.pid,
            status=Status.UNBOUND.value if candidate.live else Status.PARKED.value,
            model=candidate.model,
            source=candidate.source,
            managed=False,
            created_at=candidate.created_at or candidate.updated_at or now,
            updated_at=candidate.updated_at or now,
            last_event_at=candidate.updated_at or now,
            last_activity_at=candidate.updated_at or now,
            live=candidate.live,
            home_state="outside-live" if candidate.live else "no-live-home",
        )

    def open_pending(self, pending: PendingLaunch, *, attach: bool = True) -> int:
        pane = self.tmux.get_pane(pending.tmux_pane or pending.tmux_session or "")
        provider_pid = (
            provider_process(pane.pane_pid, pending.provider) if pane else None
        )
        other_provider = "claude" if pending.provider == "codex" else "codex"
        environment = process_environment(provider_pid) if provider_pid else {}
        pid_start = process_start_time(provider_pid)
        conflict = self.store.get_meta(
            f"launch_binding_error:{pending.launch_token}"
        )
        if conflict:
            raise PikaError(
                f"{pending.display_name} has an identity conflict: {conflict}. "
                "Run `pika doctor --verbose`."
            )
        if (
            pane is None
            or pane.pika_launch_token != pending.launch_token
            or not provider_pid
            or provider_process(pane.pane_pid, other_provider)
            or environment.get("PIKA_LAUNCH_TOKEN") != pending.launch_token
            or environment.get("PIKA_PROVIDER") != pending.provider
            or (pending.root_pid is not None and pending.root_pid != provider_pid)
            or (
                pending.root_pid_start is not None
                and pending.root_pid_start != pid_start
            )
        ):
            raise PikaError(
                f"{pending.display_name} is still identity-pending, but its live "
                "Pika launch pane cannot be verified. Run `pika doctor --verbose`."
            )
        if not attach:
            return 0

        def attached() -> None:
            self.store.set_meta(f"attached_launch:{pending.launch_token}", "1")

        try:
            return self.tmux.attach(
                pane.session_name,
                target_pane=pane.pane_id,
                on_attached=attached,
                receipt=(
                    f"Pika → {terminal_text(pending.display_name)} · "
                    f"{pending.provider.title()} · ATTACHED · IDENTITY PENDING · "
                    f"launch {pending.launch_token[:8]}"
                ),
            )
        except TmuxError as exc:
            raise PikaError(str(exc)) from exc

    def enter(self, query: str, *, attach: bool = True) -> int:
        """Fulfil the user's named-conversation intent without exposing mechanics."""
        query = query.strip()
        if not query:
            raise PikaError("Conversation name cannot be empty")
        if query in {".", "-"}:
            session = self.current_repo() if query == "." else self.previous()
            return self.open(session, attach=attach)
        if "@" in query:
            _name, alias = query.rsplit("@", 1)
            if self.store.get_fleet_node(alias) is not None:
                return self.open(self.resolve_target(query), attach=attach)

        sessions = self.refresh()
        pending = self.pending_launches()
        untracked = self.store.list_untracked_sessions()
        # `refresh` has already populated the provider's normal, current
        # discovery surface.  Use it for the common attach/resume path and
        # defer broad historical scans until the name is otherwise unknown.
        discovered = list(self._last_discovered_candidates)
        query_folded = query.casefold()
        entries: dict[tuple[str, str], tuple[Session, str, object]] = {}
        for session in sessions:
            if session.session_id == query or session.provider_thread_id == query or (
                session.name and session.name.casefold() == query_folded
            ):
                entries[session.key] = (session, "tracked", session)
        for session in untracked:
            if session.session_id == query or (
                session.name and session.name.casefold() == query_folded
            ):
                entries.setdefault(session.key, (session, "untracked", session))
        def add_native_matches(candidates: Iterable[Candidate]) -> None:
            for candidate in candidates:
                if candidate.session_id == query or (
                    candidate.name and candidate.name.casefold() == query_folded
                ):
                    if (
                        (candidate.provider, candidate.session_id) not in entries
                        or self.store.is_untracked(
                            candidate.provider, candidate.session_id
                        )
                    ):
                        view = self._candidate_session(candidate)
                        entries[view.key] = (view, "native", candidate)

        add_native_matches(discovered)
        pending_entries: dict[tuple[str, str], PendingLaunch] = {}
        for item in pending:
            if item.name.casefold() == query_folded or item.launch_token == query:
                pending_entries[item.key] = item
        remote_matches = [
            item
            for item in self.fleet.cached_sessions()
            if item.session_id == query
            or (item.name and item.name.casefold() == query_folded)
        ]
        choices: list[Session | FleetSession | PendingLaunch] = [
            *(value[0] for value in entries.values()),
            *pending_entries.values(),
            *remote_matches,
        ]
        if not choices:
            broad = self.discover_exact_candidates(query)
            known = {(item.provider, item.session_id) for item in discovered}
            discovered.extend(
                item
                for item in broad
                if (item.provider, item.session_id) not in known
            )
            add_native_matches(broad)
            choices = [
                *(value[0] for value in entries.values()),
                *pending_entries.values(),
                *remote_matches,
            ]
        if choices:
            chosen = choose_session(
                choices, "Several conversations share that name"
            )
            if isinstance(chosen, PendingLaunch):
                return self.open_pending(chosen, attach=attach)
            if isinstance(chosen, FleetSession):
                return self.open(chosen, attach=attach)
            _view, source, value = entries[chosen.key]
            if source == "native":
                chosen = self.import_candidate(value)  # type: ignore[arg-type]
            elif source == "untracked":
                self.store.restore_tracking(*chosen.key)
                chosen = self.store.get_session(*chosen.key) or chosen
            return self.open(chosen, attach=attach)

        names = list(
            dict.fromkeys(
                item.display_name
                for item in [
                    *sessions,
                    *untracked,
                    *(self._candidate_session(item) for item in discovered),
                    *pending,
                    *self.fleet.cached_sessions(),
                ]
            )
        )
        close = difflib.get_close_matches(query, names, n=5, cutoff=0.72)
        if close:
            raise PikaError(
                f"No exact conversation named {query!r}. Close matches: "
                f"{', '.join(close)}. Use an exact name, or `pika new {query}` "
                "if creating another conversation is intentional."
            )
        compact = query.replace("-", "")
        if len(compact) >= 8 and all(char in "0123456789abcdefABCDEF" for char in compact):
            raise PikaError(
                f"No provider conversation has UUID {query!r}; Pika will not turn "
                "an identity-shaped value into a new name."
            )
        stale_nodes: list[str] = []
        now = time.time()
        for node in self.store.list_fleet_nodes():
            stored = self.store.get_remote_snapshot(node.node_id)
            if (
                stored is None
                or node.status != "ready"
                or now - stored[1] > REMOTE_STALE_SECONDS
            ):
                stale_nodes.append(node.alias)
        if stale_nodes:
            names = ", ".join(stale_nodes)
            raise PikaError(
                f"Cannot safely create {query!r} while remote inventory is stale "
                f"for: {names}. Run `pika sync MACHINE`, then rerun `pika {query}`."
            )
        if not sys.stdin.isatty():
            raise PikaError(
                f"No exact conversation named {query!r}. Non-interactive Pika "
                f"will not create one implicitly; use `pika new {query}`."
            )
        return self.new(query, attach=attach)

    def _outside_processes(self, session: Session, panes: list) -> list[int]:
        owned_pane_pids: set[int] = set()
        for pane in panes:
            if (pane.pika_provider, pane.pika_session_id) == session.key or (
                session.tmux_pane == pane.pane_id
                and not pane.pika_session_id
                and not pane.pika_provider
            ):
                owned_pane_pids.update(process_tree(pane.pane_pid))
        thread_ids = {session.session_id, session.provider_thread_id}
        candidates: set[int] = set()
        for thread_id in thread_ids:
            candidates.update(
                find_processes_with_session_id(thread_id, session.provider)
            )
        provider = self.providers.get(session.provider)
        if provider:
            active_pids = getattr(provider, "active_pids", None)
            if active_pids:
                for thread_id in thread_ids:
                    candidates.update(active_pids(thread_id))
        candidates.update(self._live_owner_pids(session))
        candidates.update(self._recovery_owner_pids(session))
        return sorted(pid for pid in candidates if pid and pid not in owned_pane_pids)

    def uuid_identity_pids(self, session: Session) -> set[int]:
        """Return processes carrying provider-native UUID evidence."""
        exact: set[int] = set()
        provider = self.providers.get(session.provider)
        if provider:
            active_pids = getattr(provider, "active_pids", None)
            if active_pids:
                for thread_id in {session.session_id, session.provider_thread_id}:
                    try:
                        exact.update(active_pids(thread_id))
                    except (OSError, RuntimeError):
                        pass
        return exact

    def _live_owner_pids(self, session: Session) -> set[int]:
        """Return valid hook leases, pruning dead, reused, and expired claims."""
        result: set[int] = set()
        now = time.time()
        leases = self.store.get_live_owner_leases(*session.key)
        for owner, owner_start, last_seen, owner_token in leases:
            live_owner = (
                provider_process(owner, session.provider)
                if owner_start is not None and process_start_time(owner) == owner_start
                else None
            )
            expired_shared_lease = bool(
                live_owner
                and shared_provider_process(live_owner, session.provider)
                and now - last_seen > LIVE_OWNER_LEASE_SECONDS
            )
            if not live_owner or expired_shared_lease:
                self.store.delete_live_owner(
                    *session.key, pid=owner, owner_token=owner_token
                )
                continue
            result.add(live_owner)
        return result

    def _recovery_owner_pids(self, session: Session) -> set[int]:
        """Revalidate command-recovery proof without treating it as a hook."""
        owner = self.store.get_recovery_owner(*session.key)
        if owner is None:
            return set()
        pid, start_time, launch_token = owner
        environment = process_environment(pid)
        if (
            process_start_time(pid) != start_time
            or not provider_process(pid, session.provider)
            or environment.get("PIKA_LAUNCH_TOKEN") != launch_token
            or environment.get("PIKA_PROVIDER") != session.provider
        ):
            self.store.delete_recovery_owner(*session.key)
            return set()
        return {pid}

    def identity_pids(self, session: Session) -> set[int]:
        """Return direct UUID evidence plus non-conflicting hook-owner leases.

        A shared Codex app-server can emit hooks for several clients and cannot
        compete with a process that carries this exact UUID in argv. It remains
        useful as a short lease only when no direct UUID-bearing client exists.
        """
        uuid_pids = self.uuid_identity_pids(session)
        lease_pids = self._live_owner_pids(session)
        if uuid_pids:
            lease_pids = {
                pid
                for pid in lease_pids
                if not shared_provider_process(pid, session.provider)
            }
        return uuid_pids | lease_pids | self._recovery_owner_pids(session)

    def _outside_uuid_pids(self, session: Session, pane: Pane) -> list[int]:
        inside = set(process_tree(pane.pane_pid))
        pane_provider_pid = provider_process(pane.pane_pid, session.provider)
        if pane_provider_pid:
            inside.add(pane_provider_pid)
        return sorted(self.uuid_identity_pids(session) - inside)

    def exact_pane_pid(self, session: Session, pane: Pane) -> int | None:
        """Accept one pane only when it contains all live UUID-owned processes."""
        pid = provider_process(pane.pane_pid, session.provider)
        if not pid:
            return None
        uuid_pids = self.uuid_identity_pids(session)
        if len(uuid_pids) > 1:
            return None
        pane_tree = set(process_tree(pane.pane_pid))
        # provider_process only returns a descendant of pane.pane_pid. Keep
        # that direct proof even when a concurrently exiting process makes a
        # second /proc tree walk incomplete.
        pane_tree.add(pid)
        identities = self.identity_pids(session)
        if uuid_pids:
            return pid if identities.issubset(pane_tree) else None
        if pid not in identities:
            return None
        other_identities = identities - {pid}
        if other_identities and not other_identities.issubset(pane_tree):
            return None
        return pid

    def open_on_client(
        self, session: Session | FleetSession
    ) -> ClientLaunchReceipt | None:
        """Ask a paired SSH client to launch an exact session in a new window.

        An unavailable reverse tunnel is an ordinary absence and lets the
        monitor retain its current-terminal attach behavior. A reachable bridge
        that rejects identity is different: fail closed and keep the board open.
        """
        if session.session_id.startswith("unbound:"):
            return None
        config = load_config()
        configured = config.get("client_bridges")
        if not isinstance(configured, list) or not configured:
            return None
        # Do not probe a configured client bridge from cron, hooks, or an
        # unrelated local shell. PIKA_CLIENT_BRIDGE=1 is the explicit escape
        # hatch for custom transports that preserve the same contract.
        if not os.environ.get("SSH_CONNECTION") and os.environ.get(
            "PIKA_CLIENT_BRIDGE"
        ) != "1":
            return None
        source_node_id = self.store.local_node_id()
        target_node_id = (
            session.node_id if isinstance(session, FleetSession) else source_node_id
        )
        for raw in configured:
            if not isinstance(raw, dict) or raw.get("enabled", True) is False:
                continue
            try:
                host = str(raw.get("host") or "127.0.0.1")
                port = int(raw.get("port") or 47654)
                timeout = float(raw.get("timeout") or 0.35)
                if host not in {"127.0.0.1", "::1", "localhost"}:
                    raise ClientBridgeError(
                        "Client bridge endpoint must stay on server loopback"
                    )
                if not 1024 <= port <= 65535:
                    raise ClientBridgeError("Client bridge port is invalid")
                request = make_launch_request(
                    client_id=str(raw.get("client_id") or ""),
                    token=str(raw.get("token") or ""),
                    source_node_id=source_node_id,
                    target_node_id=target_node_id,
                    provider=session.provider,
                    session_id=session.session_id,
                )
                return request_client_launch(
                    request,
                    host=host,
                    port=port,
                    timeout=max(0.05, min(timeout, 2.0)),
                )
            except ClientBridgeUnavailable:
                continue
            except (ClientBridgeError, TypeError, ValueError) as exc:
                raise PikaError(f"CLIENT WINDOW BLOCKED · {exc}") from exc
        # No live reverse tunnel: Enter falls through to the proven existing
        # attach path. Keep transport diagnostics out of normal dashboard UX.
        return None

    def open(self, session: Session | FleetSession, *, attach: bool = True) -> int:
        if isinstance(session, FleetSession):
            if not attach:
                raise PikaError(
                    "Remote Pika sessions require an interactive SSH attach"
                )
            return self.fleet.attach(session)
        sessions = self.refresh()
        current = next((item for item in sessions if item.key == session.key), session)
        collecting_result = current.status == Status.READY.value and current.unread
        panes = self.tmux.list_panes()
        matching_panes = [
            item
            for item in panes
            if (item.pika_provider, item.pika_session_id) == current.key
            or (
                current.tmux_pane
                and item.pane_id == current.tmux_pane
                and not item.pika_session_id
                and not item.pika_provider
            )
        ]
        if len(matching_panes) > 1:
            homes = ", ".join(
                f"{item.session_name}:{item.pane_id}" for item in matching_panes
            )
            raise PikaError(
                f"{current.display_name} has duplicate Pika tmux homes ({homes}). "
                "Resolve the duplicate before opening it."
            )
        pane = matching_panes[0] if matching_panes else None
        raw_pane_pid = (
            provider_process(pane.pane_pid, current.provider) if pane else None
        )
        live_pid = self.exact_pane_pid(current, pane) if pane else None
        if raw_pane_pid and not live_pid:
            direct_uuid_pids = self.uuid_identity_pids(current)
            if len(direct_uuid_pids) > 1:
                raise PikaError(
                    f"OPEN TWICE: {current.display_name} has exact UUID "
                    f"{current.provider_thread_id} in multiple provider clients "
                    f"(PID {', '.join(map(str, sorted(direct_uuid_pids)))}). "
                    "Close one copy before attaching."
                )
            outside_exact = self._outside_uuid_pids(current, pane)
            if outside_exact and raw_pane_pid in self.uuid_identity_pids(current):
                raise PikaError(
                    f"OPEN TWICE: {current.display_name} has exact UUID "
                    f"{current.provider_thread_id} in its Pika pane "
                    f"(PID {raw_pane_pid}) "
                    f"and outside it (PID {', '.join(map(str, outside_exact))}). "
                    "Close one copy before attaching."
                )
            raise PikaError(
                f"{current.display_name}'s tagged pane contains a running "
                f"{current.provider} process (PID {raw_pane_pid}) that cannot be tied "
                f"to exact UUID {current.provider_thread_id}. Pika refuses to "
                "attach or issue an exact-thread receipt."
            )
        outcome = "ATTACHED LIVE" if live_pid else "RESUMED EXACT"
        preserved: str | None = None
        if pane and not live_pid:
            other_provider = "claude" if current.provider == "codex" else "codex"
            other_pid = provider_process(pane.pane_pid, other_provider)
            if other_pid:
                raise PikaError(
                    f"{current.display_name}'s saved tmux pane contains a running "
                    f"{other_provider} process (PID {other_pid}). Pika refuses to replace it."
                )
        if not live_pid:
            outside = self._outside_processes(current, panes)
            if outside:
                pid_text = ", ".join(str(pid) for pid in outside)
                direct_uuid_pids = self.uuid_identity_pids(current)
                if len(direct_uuid_pids) > 1:
                    raise PikaError(
                        f"OPEN TWICE: {current.display_name} has exact UUID "
                        f"{current.provider_thread_id} in multiple process trees "
                        f"(PID {pid_text}). Close one copy, then rerun "
                        f"`pika {current.display_name}`."
                    )
                if not direct_uuid_pids and all(
                    shared_provider_process(pid, current.provider) for pid in outside
                ):
                    raise PikaError(
                        f"ACTIVE IN {current.provider.upper()} APP: "
                        f"{current.display_name}'s exact UUID has a fresh app lease "
                        f"(PID {pid_text}). Pika will not open a second client while "
                        "that view may still be live. Close or leave that conversation, "
                        f"then rerun `pika {current.display_name}`; Pika will recreate "
                        "the exact home automatically. No re-adoption or cleanup is needed. "
                        "If the app is already closed, its lease expires no later than "
                        "five minutes after the last activity."
                    )
                raise PikaError(
                    f"{current.display_name} is already running outside its Pika "
                    f"home (PID {pid_text}) and cannot be moved safely while live. "
                    "Exit that copy normally, then rerun "
                    f"`pika {current.display_name}`. Pika will resume the exact UUID."
                )
        if not live_pid:
            provider = self.providers.get(current.provider)
            if provider is None:
                raise PikaError(f"Unknown provider: {current.provider}")
            if not provider.installed():
                raise PikaError(f"{current.provider} is not installed or not on PATH")
            resumable_check = getattr(provider, "is_resumable", None)
            if resumable_check:
                try:
                    resumable = resumable_check(current.provider_thread_id)
                except (OSError, RuntimeError):
                    resumable = False
                if not resumable:
                    raise PikaError(
                        f"{current.display_name} has no durable {current.provider} "
                        "history to resume. Pika refuses to invent or substitute a conversation."
                    )
            cwd = current.cwd or os.getcwd()
            if not Path(cwd).is_dir():
                raise PikaError(f"Saved working directory no longer exists: {cwd}")
            argv = provider.resume_argv(current.provider_thread_id)
            environment = {
                "PIKA_NAME": current.display_name,
                "PIKA_PROVIDER": current.provider,
                "PIKA_SESSION_ID": current.session_id,
                "PIKA_ACTIVE_THREAD_ID": current.provider_thread_id,
                "PIKA_OWNER_TOKEN": str(uuid.uuid4()),
            }
            reservation_token = str(uuid.uuid4())
            if not self.store.reserve_resume(
                current.provider, current.session_id, reservation_token
            ):
                raise PikaError(
                    f"Another Pika command is already opening {current.display_name}."
                )
            try:
                # Re-check after taking the reservation: another invocation may
                # have completed between our first refresh and this lock.
                panes = self.tmux.list_panes()
                outside = self._outside_processes(current, panes)
                if outside:
                    raise PikaError(
                        f"{current.display_name} started elsewhere while Pika was opening it "
                        f"(PID {', '.join(map(str, outside))})."
                    )
                fresh_matches = [
                    item
                    for item in panes
                    if (item.pika_provider, item.pika_session_id) == current.key
                    or (
                        current.tmux_pane
                        and item.pane_id == current.tmux_pane
                        and not item.pika_session_id
                        and not item.pika_provider
                    )
                ]
                if len(fresh_matches) > 1:
                    raise PikaError(
                        f"{current.display_name} acquired duplicate tmux homes while opening."
                    )
                fresh_pane = fresh_matches[0] if fresh_matches else None
                fresh_pid = (
                    self.exact_pane_pid(current, fresh_pane) if fresh_pane else None
                )
                fresh_raw_pid = (
                    provider_process(fresh_pane.pane_pid, current.provider)
                    if fresh_pane
                    else None
                )
                if fresh_raw_pid and not fresh_pid:
                    raise PikaError(
                        f"{current.display_name}'s tagged pane acquired an "
                        f"unverified {current.provider} process (PID {fresh_raw_pid}). "
                        "Pika refuses to attach it to the saved UUID."
                    )
                if fresh_pane and not fresh_pid:
                    other_provider = (
                        "claude" if current.provider == "codex" else "codex"
                    )
                    other_pid = provider_process(fresh_pane.pane_pid, other_provider)
                    if other_pid:
                        raise PikaError(
                            f"{current.display_name}'s saved tmux pane acquired a "
                            f"running {other_provider} process (PID {other_pid}) while "
                            "Pika was opening it. Pika refuses to replace it."
                        )
                try:
                    if fresh_pid:
                        pane = fresh_pane
                        outcome = "ATTACHED LIVE"
                    elif fresh_pane and (
                        idle_pane := self._settle_idle_pane(fresh_pane)
                    ):
                        fresh_pane = idle_pane
                        pane = self.tmux.respawn_agent(
                            fresh_pane.pane_id,
                            cwd=cwd,
                            provider=current.provider,
                            agent_argv=argv,
                            environment=environment,
                            session_id=current.session_id,
                            display_name=current.display_name,
                        )
                        outcome = "RESUMED EXACT"
                    else:
                        name = self._free_tmux_name(
                            self.tmux.internal_name(
                                current.provider, session_id=current.session_id
                            ),
                            panes,
                        )
                        pane = self.tmux.create_agent_session(
                            tmux_name=name,
                            cwd=cwd,
                            provider=current.provider,
                            agent_argv=argv,
                            environment=environment,
                            session_id=current.session_id,
                            display_name=current.display_name,
                            launch_token=None,
                        )
                        if fresh_pane:
                            self.tmux.clear_pika_tags(fresh_pane.pane_id)
                            preserved = (
                                f"preserved {fresh_pane.session_name}:{fresh_pane.pane_id} "
                                f"running {fresh_pane.current_command}"
                            )
                        outcome = "NEW HOME"
                except TmuxError as exc:
                    raise PikaError(str(exc)) from exc
                resumed_fields: dict[str, object] = {
                    "tmux_session": pane.session_name,
                    "tmux_pane": pane.pane_id,
                    "root_pid": provider_process(pane.pane_pid, current.provider),
                }
                # A real unread result remains collectible after resuming its
                # exact conversation. Every other old state becomes WORKING;
                # launching a process must never manufacture a READY event.
                if not (current.status == Status.READY.value and current.unread):
                    resumed_fields.update(
                        status=Status.WORKING.value,
                        unread=False,
                        error=None,
                        attention_reason=None,
                    )
                self.store.update_session(
                    current.provider,
                    current.session_id,
                    **resumed_fields,
                )
            finally:
                self.store.release_resume(
                    current.provider, current.session_id, reservation_token
                )
        if not attach:
            return 0
        assert pane is not None

        def open_outcome(exact: bool) -> str:
            if outcome == "ATTACHED LIVE":
                return "ATTACHED EXACT" if exact else "ATTACHED · IDENTITY UNVERIFIED"
            if outcome == "RESUMED EXACT":
                return "RESUMED EXACT" if exact else "RESUME STARTED · IDENTITY PENDING"
            if outcome == "NEW HOME":
                return "NEW HOME · EXACT" if exact else "NEW HOME · IDENTITY PENDING"
            return outcome

        receipt_box = [""]

        def ordinary_receipt(exact: bool) -> str:
            parts = [
                f"Pika → {terminal_text(current.display_name)}",
                current.provider.title(),
                open_outcome(exact),
                f"{'exact id' if exact else 'id'} {current.provider_thread_id[:8]}",
            ]
            if current.active_thread_id:
                parts.append(f"home {current.session_id[:8]}")
            if preserved:
                parts.append(terminal_text(preserved))
            elif current.attention_reason:
                parts.append(terminal_text(current.attention_reason))
            return " · ".join(parts)

        receipt_box[0] = ordinary_receipt(bool(self.exact_pane_pid(current, pane)))

        def attached() -> None:
            exact = bool(self.exact_pane_pid(current, pane))
            remaining: int | None = None
            if collecting_result and exact:
                remaining = self.store.collect_result(
                    *current.key,
                    expected_event_at=current.last_event_at,
                )
            elif not collecting_result:
                self.acknowledge(current, attaching=True)
            if remaining is not None:
                remainder = (
                    "INBOX CLEAR"
                    if remaining == 0
                    else (
                        "1 unread remains"
                        if remaining == 1
                        else f"{remaining} unread remain"
                    )
                )
                receipt_box[0] = " · ".join(
                    [
                        "RESULT COLLECTED",
                        remainder,
                        {"codex": "C", "claude": "A"}.get(
                            current.provider, current.provider[:1].upper()
                        ),
                        f"EXACT {current.session_id[:8]}",
                        terminal_text(current.display_name),
                    ]
                )
            else:
                receipt_box[0] = ordinary_receipt(exact)
            self.store.record_attach(*current.key)

        def attach_receipt() -> str:
            return receipt_box[0]

        try:
            result = self.tmux.attach(
                pane.session_name,
                target_pane=pane.pane_id,
                on_attached=attached,
                receipt=attach_receipt,
            )
        except TmuxError as exc:
            raise PikaError(str(exc)) from exc
        if result == 0 and not os.environ.get("TMUX"):
            print(
                f"Left {terminal_text(current.display_name)} running in Pika tmux · "
                "return with: "
                f"{shlex.join(['pika', 'open', current.session_id])}"
            )
        return result

    def new(
        self,
        name: str,
        provider_name: str | None = None,
        cwd: str | None = None,
        *,
        attach: bool = True,
    ) -> int:
        if not name.strip():
            raise PikaError("Conversation name cannot be empty")
        config = load_config()
        provider_name = provider_name or str(config.get("default_provider") or "codex")
        provider = self.providers.get(provider_name)
        if provider is None:
            raise PikaError(f"Unknown provider: {provider_name}")
        if not provider.installed():
            raise PikaError(f"{provider_name} is not installed or not on PATH")
        if not hooks_installed(provider_name):
            raise PikaError(
                f"Pika hooks are not installed for {provider_name}. Run `pika setup` first."
            )
        cwd = cwd or os.getcwd()
        if not Path(cwd).is_dir():
            raise PikaError(f"Working directory does not exist: {cwd}")
        preexisting_session_ids: list[str] | None = None
        if provider_name == "codex":
            snapshot_at = time.time()
            snapshotter = getattr(provider, "launch_candidates", None)
            try:
                existing_candidates = list(
                    snapshotter() if callable(snapshotter) else provider.discover()
                )
            except (OSError, RuntimeError):
                existing_candidates = []
            else:
                preexisting_session_ids = [
                    item.session_id
                    for item in existing_candidates
                    if self._same_path(item.cwd, cwd)
                    and (
                        not item.created_at
                        or item.created_at >= snapshot_at - 120
                    )
                ]
        token = str(uuid.uuid4())
        reserved_id = str(uuid.uuid4()) if provider_name == "claude" else None
        tmux_name = self._free_tmux_name(
            self.tmux.internal_name(provider_name, session_id=reserved_id, token=token),
            self.tmux.list_panes(),
        )
        if not self.store.add_pending(
            token,
            provider_name,
            name,
            cwd,
            tmux_session=tmux_name,
            expected_session_id=reserved_id,
            preexisting_session_ids=preexisting_session_ids,
        ):
            raise PikaError(
                f"Pika is already starting {name!r}; run `pika {name}` to open it."
            )
        environment = {
            "PIKA_NAME": name,
            "PIKA_PROVIDER": provider_name,
            "PIKA_LAUNCH_TOKEN": token,
            "PIKA_OWNER_TOKEN": token,
        }
        if reserved_id:
            environment["PIKA_SESSION_ID"] = reserved_id
        try:
            pane = self.tmux.create_agent_session(
                tmux_name=tmux_name,
                cwd=cwd,
                provider=provider_name,
                agent_argv=provider.new_argv(name, reserved_id),
                environment=environment,
                session_id=reserved_id,
                display_name=name,
                launch_token=token,
            )
        except TmuxError as exc:
            self.store.delete_pending(token)
            raise PikaError(str(exc)) from exc
        root_pid = provider_process(pane.pane_pid, provider_name)
        binding = self.store.finalize_pending_pane(
            token,
            pane.session_name,
            pane.pane_id,
            root_pid,
            process_start_time(root_pid),
        )
        if binding:
            try:
                self.tmux.tag_pane(
                    pane.pane_id,
                    provider=binding[0],
                    session_id=binding[1],
                    name=name,
                    launch_token="",
                )
            except TmuxError:
                pass
        if not attach:
            return 0

        def attached() -> None:
            binding = self.store.get_launch_binding(token)
            if binding:
                self.store.record_attach(*binding)
            elif reserved_id:
                self.store.record_attach(provider_name, reserved_id)
            else:
                # Codex chooses its UUID. The SessionStart hook will replace
                # this short-lived token with the exact identity.
                self.store.set_meta(f"attached_launch:{token}", "1")

        if reserved_id:
            receipt = (
                f"Pika → {terminal_text(name)} · {provider_name.title()} · STARTED · "
                f"IDENTITY PENDING · id {reserved_id[:8]}"
            )
        else:
            receipt = (
                f"Pika → {terminal_text(name)} · Codex · STARTED · IDENTITY PENDING"
            )

        try:
            return self.tmux.attach(
                pane.session_name,
                target_pane=pane.pane_id,
                on_attached=attached,
                receipt=receipt,
            )
        except TmuxError as exc:
            raise PikaError(str(exc)) from exc

    @staticmethod
    def _free_tmux_name(base: str, panes: Iterable) -> str:
        names = {pane.session_name for pane in panes}
        if base not in names:
            return base
        index = 2
        while f"{base}-{index}" in names:
            index += 1
        return f"{base}-{index}"

    @staticmethod
    def _pane_is_idle(pane: Pane) -> bool:
        if pane.dead:
            return True
        shells = {"bash", "dash", "fish", "ksh", "sh", "tcsh", "zsh"}
        if Path(pane.current_command).name not in shells:
            return False
        # A login shell with any live descendant may be running a foreground
        # command, build, server, editor, or background job. Preserve it.
        return process_tree(pane.pane_pid) == [pane.pane_pid]

    def _settle_idle_pane(self, pane: Pane, *, timeout: float = 0.5) -> Pane | None:
        """Absorb short shell-exit races without overwriting durable pane work."""
        if self._pane_is_idle(pane):
            return pane
        shells = {"bash", "dash", "fish", "ksh", "sh", "tcsh", "zsh"}
        if Path(pane.current_command).name not in shells:
            return None
        get_pane = getattr(self.tmux, "get_pane", None)
        if not get_pane:
            return None
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            time.sleep(0.05)
            current = get_pane(pane.pane_id)
            if current is None:
                return None
            if self._pane_is_idle(current):
                return current
            if Path(current.current_command).name not in shells:
                return None
        return None

    def acknowledge(
        self, session: Session | FleetSession, *, attaching: bool = False
    ) -> bool:
        if isinstance(session, FleetSession):
            return self.fleet.acknowledge(session)
        return self.store.acknowledge_attention(
            session.provider,
            session.session_id,
            expected_event_at=session.last_event_at,
            attaching=attaching,
        )

    def next_attention(
        self, sessions: list[Session | FleetSession] | None = None
    ) -> Session | FleetSession | None:
        sessions = sessions or self.refresh()
        attention = [item for item in sessions if item.needs_attention]
        if not attention:
            return None
        return sorted_attention_sessions(attention)[0]

    def current_repo(self, sessions: list[Session] | None = None) -> Session:
        if sessions is None:
            sessions = self.refresh()
            tracked = {item.key for item in sessions}
            for candidate in self.discover_candidates():
                if (
                    candidate.name
                    and (candidate.provider, candidate.session_id) not in tracked
                ):
                    sessions.append(self.import_candidate(candidate))
        cwd = Path.cwd().resolve()
        root = self._git_root(cwd)
        candidates: list[Session] = []
        for item in sessions:
            if not item.cwd:
                continue
            try:
                item_path = Path(item.cwd).resolve()
            except OSError:
                continue
            if item_path == cwd or self._git_root(item_path) == root:
                candidates.append(item)
        if not candidates:
            raise PikaError(f"No Pika conversation belongs to {root}")
        candidates.sort(
            key=lambda item: (not item.needs_attention, -item.last_activity_at)
        )
        return choose_session(candidates, "Choose a continuation for this repository")

    def current_exact_session(self) -> Session:
        """Resolve the calling pane to one exact Pika conversation."""
        target = os.environ.get("TMUX_PANE")
        if not target:
            raise PikaError(
                "Expert cards can only be changed from inside their exact Pika pane"
            )
        pane = self.tmux.get_pane(target)
        if not pane or not pane.pika_provider or not pane.pika_session_id:
            raise PikaError(
                "This pane has no exact Pika identity; adopt or open it with Pika first"
            )
        if pane.pika_session_id.startswith("unbound:"):
            raise PikaError("An unbound pane cannot publish an expert card")
        session = next(
            (
                item
                for item in self.refresh()
                if item.key == (pane.pika_provider, pane.pika_session_id)
            ),
            None,
        )
        fresh_pane = self.tmux.get_pane(target)
        if (
            not session
            or not fresh_pane
            or not self.exact_pane_pid(session, fresh_pane)
        ):
            raise PikaError(
                "Pika cannot prove this pane owns that provider UUID; expert card unchanged"
            )
        return session

    def publish_expert(
        self,
        *,
        summary: str,
        current_state: str,
        topics: Iterable[str],
        artifacts: Iterable[str] = (),
    ) -> ExpertProfile:
        session = self.current_exact_session()
        fingerprint = transcript_fingerprint(session)
        profile = make_profile(
            session,
            summary=summary,
            current_state=current_state,
            topics=topics,
            artifacts=artifacts,
            transcript_mtime_ns=fingerprint[0] if fingerprint else None,
            transcript_size=fingerprint[1] if fingerprint else None,
        )
        return self.store.put_expert_profile(profile)

    def clear_current_expert(self) -> Session:
        session = self.current_exact_session()
        self.store.delete_expert_profile(*session.key)
        return session

    def expert_matches(self, query: str = "") -> list[ExpertMatch]:
        local = rank_experts(
            self.store.list_expert_profiles(),
            self.refresh(usage=False),
            query,
        )
        return sorted(
            [*local, *self.fleet.expert_matches(query)],
            key=lambda item: (
                not isinstance(item.session, FleetSession) or not item.session.stale,
                item.score,
                item.profile.updated_at,
            ),
            reverse=True,
        )

    def expert_card_states(
        self, sessions: list[Session | FleetSession] | None = None
    ) -> list[ExpertCardState]:
        visible = (
            sessions
            if sessions is not None
            else self.monitor_sessions(self.refresh(usage=False))
        )
        local_visible = [item for item in visible if isinstance(item, Session)]
        profiles = {item.key: item for item in self.store.list_expert_profiles()}
        local = [
            card_state(session, profiles.get(session.key))
            for session in local_visible
            if not session.session_id.startswith("unbound:")
        ]
        remote_keys = {item.key for item in visible if isinstance(item, FleetSession)}
        remote = [
            item
            for item in self.fleet.expert_card_states()
            if item.session.key in remote_keys
        ]
        return [*local, *remote]

    def refresh_expert(self, session: Session) -> ExpertProfile:
        existing = self.store.get_expert_profile(*session.key)
        profile = interview_profile(session, existing)
        return self.store.put_expert_profile(profile)

    @staticmethod
    def _expert_consultation_receipt(session: Session) -> dict[str, str | None]:
        return consultation_policy(session).receipt()

    def bootstrap_experts(
        self, sessions: Iterable[Session]
    ) -> list[ExpertRefreshResult]:
        results: list[ExpertRefreshResult] = []
        for session in sessions:
            state = card_state(session, self.store.get_expert_profile(*session.key))
            if state.status == "CURRENT":
                continue
            if state.status == "UNKNOWN":
                results.append(
                    ExpertRefreshResult(
                        session.provider,
                        "UNKNOWN",
                        state.detail,
                        session.session_id,
                        session.display_name,
                    )
                )
                continue
            try:
                self.refresh_expert(session)
            except Exception as exc:  # noqa: BLE001 - one card must not block others
                results.append(
                    ExpertRefreshResult(
                        session.provider,
                        "FAILED",
                        str(exc),
                        session.session_id,
                        session.display_name,
                    )
                )
            else:
                results.append(
                    ExpertRefreshResult(
                        session.provider,
                        "REFRESHED",
                        "exact ephemeral interview",
                        session.session_id,
                        session.display_name,
                        **self._expert_consultation_receipt(session),
                    )
                )
        return results

    def refresh_due_experts(
        self,
        *,
        provider_name: str | None = None,
        now: float | None = None,
    ) -> list[ExpertRefreshResult]:
        """Refresh at most one changed card per provider and reset cycle run."""
        now = time.time() if now is None else now
        sessions = self.refresh(usage=False)
        provider_names = [provider_name] if provider_name else sorted(self.providers)
        states = self.expert_card_states(sessions)
        results: list[ExpertRefreshResult] = []
        for provider in provider_names:
            provider_states = [
                item for item in states if item.session.provider == provider
            ]
            pending_states = [
                item for item in provider_states if item.status in {"MISSING", "STALE"}
            ]
            if not pending_states:
                unknown = any(item.status == "UNKNOWN" for item in provider_states)
                results.append(
                    ExpertRefreshResult(
                        provider,
                        "UNKNOWN" if unknown else "CURRENT",
                        (
                            "one or more cards lack a durable transcript"
                            if unknown
                            else "no changed expert cards"
                        ),
                    )
                )
                continue
            quota = read_provider_quota(provider)
            if quota is None:
                results.append(
                    ExpertRefreshResult(
                        provider,
                        "UNKNOWN",
                        "fresh weekly quota telemetry unavailable; no model call made",
                    )
                )
                continue
            common = {
                "remaining_percent": quota.remaining_percent,
                "reset_at": quota.reset_at,
            }
            seconds_left = quota.reset_at - now
            if seconds_left > EXPERT_REFRESH_WINDOW_SECONDS:
                results.append(
                    ExpertRefreshResult(
                        provider,
                        "WAITING",
                        "outside final 6h before weekly reset",
                        **common,
                    )
                )
                continue
            if seconds_left <= 0:
                results.append(
                    ExpertRefreshResult(
                        provider,
                        "UNKNOWN",
                        "weekly quota observation has expired; no model call made",
                    )
                )
                continue
            if quota.remaining_percent <= EXPERT_QUOTA_RESERVE_PERCENT:
                results.append(
                    ExpertRefreshResult(
                        provider,
                        "DEFERRED",
                        "10% weekly reserve protected; skipped this reset cycle",
                        **common,
                    )
                )
                continue
            candidates = sorted(
                (
                    item
                    for item in pending_states
                    if self.store.get_expert_refresh_attempt(
                        provider, item.session.session_id, quota.reset_at
                    )
                    is None
                ),
                key=lambda item: (
                    item.status != "MISSING",
                    not item.session.live,
                    -item.session.last_activity_at,
                ),
            )
            if not candidates:
                results.append(
                    ExpertRefreshResult(
                        provider,
                        "DEFERRED",
                        "changed cards already attempted in this reset cycle",
                        **common,
                    )
                )
                continue
            selected = candidates[0].session
            self.store.put_expert_refresh_attempt(
                ExpertRefreshAttempt(
                    provider,
                    selected.session_id,
                    quota.reset_at,
                    "STARTED",
                    "claimed before provider call",
                )
            )
            try:
                self.refresh_expert(selected)
            except Exception as exc:  # noqa: BLE001 - scheduled work is fail-closed
                detail = str(exc)
                self.store.put_expert_refresh_attempt(
                    ExpertRefreshAttempt(
                        provider,
                        selected.session_id,
                        quota.reset_at,
                        "FAILED",
                        detail,
                    )
                )
                results.append(
                    ExpertRefreshResult(
                        provider,
                        "FAILED",
                        detail,
                        selected.session_id,
                        selected.display_name,
                        **common,
                    )
                )
            else:
                self.store.put_expert_refresh_attempt(
                    ExpertRefreshAttempt(
                        provider,
                        selected.session_id,
                        quota.reset_at,
                        "REFRESHED",
                        "exact ephemeral interview",
                    )
                )
                results.append(
                    ExpertRefreshResult(
                        provider,
                        "REFRESHED",
                        "exact ephemeral interview",
                        selected.session_id,
                        selected.display_name,
                        **common,
                        **self._expert_consultation_receipt(selected),
                    )
                )
        return results

    @staticmethod
    def _git_root(path: Path) -> Path:
        try:
            proc = subprocess.run(
                ["git", "-C", str(path), "rev-parse", "--show-toplevel"],
                capture_output=True,
                text=True,
                timeout=2,
                check=False,
            )
            if proc.returncode == 0 and proc.stdout.strip():
                return Path(proc.stdout.strip()).resolve()
        except (OSError, subprocess.TimeoutExpired):
            pass
        return path

    def previous(self, sessions: list[Session] | None = None) -> Session:
        key = self.store.previous_attached()
        if not key:
            raise PikaError("No previous Pika session has been recorded yet")
        session = self.store.get_session(*key)
        if not session:
            raise PikaError("The previous Pika session is no longer tracked")
        return session

    def adopt(self, target: str | None = None, name: str | None = None) -> Session:
        target = target or os.environ.get("TMUX_PANE")
        if not target:
            raise PikaError(
                "Specify a tmux target, or run `pika adopt` from inside tmux"
            )
        pane = self.tmux.get_pane(target)
        named_session: Session | None = None
        if pane is None:
            try:
                named_session = self.resolve(target)
            except PikaError:
                named_session = None
            if named_session is not None:
                direct_pids = self.uuid_identity_pids(named_session)
                containing = [
                    item
                    for item in self.tmux.list_panes()
                    if direct_pids.intersection(process_tree(item.pane_pid))
                ]
                if len(containing) == 1:
                    pane = containing[0]
                elif len(containing) > 1:
                    homes = ", ".join(
                        f"{item.session_name}:{item.pane_id}" for item in containing
                    )
                    raise PikaError(
                        f"{terminal_text(named_session.display_name)} appears in "
                        f"multiple tmux panes ({homes}); Pika will not guess."
                    )
                elif direct_pids:
                    pid_text = ", ".join(map(str, sorted(direct_pids)))
                    raise PikaError(
                        f"{terminal_text(named_session.display_name)} is running "
                        f"outside tmux (PID {pid_text}). A live process cannot be "
                        "moved safely into tmux. Exit that agent normally, then run "
                        f"`pika {terminal_text(named_session.display_name)}` to resume "
                        "the exact UUID inside Pika."
                    )
                else:
                    raise PikaError(
                        f"{terminal_text(named_session.display_name)} is not running "
                        "inside a tmux pane. Run "
                        f"`pika {terminal_text(named_session.display_name)}` to create "
                        "its exact Pika home."
                    )
        if not pane:
            raise PikaError(f"No tmux pane matches {target!r}")
        running_agents: list[tuple[str, int]] = []
        for candidate in ("codex", "claude"):
            found = provider_process(pane.pane_pid, candidate)
            if found:
                running_agents.append((candidate, found))
        if not running_agents:
            raise PikaError(
                "That pane does not contain a running Codex or Claude process"
            )
        if len(running_agents) > 1:
            raise PikaError(
                "That pane contains both Codex and Claude processes; Pika cannot "
                "assign one exact owner. Adopt a pane containing only one agent."
            )
        provider_name, pid = running_agents[0]
        pane_tree = set(process_tree(pane.pane_pid))
        matches = []
        for item in self.discover_candidates():
            if item.provider != provider_name:
                continue
            identities = self._candidate_identity_pids(item)
            if len(identities) == 1 and identities.issubset(pane_tree):
                matches.append(item)
        if len(matches) == 1:
            candidate = matches[0]
            session_id = candidate.session_id
            display_name = name or candidate.name
            status = Status.READY.value
            transcript = candidate.transcript_path
            model = candidate.model
        else:
            session_id = f"unbound:{pane.pane_id}"
            display_name = (
                name or pane.pika_name or f"{provider_name}-{pane.pane_id.lstrip('%')}"
            )
            status = Status.UNBOUND.value
            transcript = None
            model = None
        now = time.time()
        session = Session(
            provider=provider_name,
            session_id=session_id,
            name=display_name,
            cwd=pane.cwd,
            tmux_session=pane.session_name,
            tmux_pane=pane.pane_id,
            root_pid=pid,
            status=status,
            unread=False,
            transcript_path=transcript,
            model=model,
            source="adopted",
            managed=True,
            created_at=pane.created,
            updated_at=now,
            last_event_at=now,
            last_activity_at=pane.activity,
        )
        self.store.restore_tracking(provider_name, session_id)
        self.store.upsert_session(session)
        self.tmux.tag_pane(
            pane.pane_id,
            provider=provider_name,
            session_id=session_id,
            name=session.display_name,
        )
        return session

    def untrack(self, session: Session | FleetSession) -> int:
        """Stop showing an exact UUID without stopping or archiving its agent."""
        if isinstance(session, FleetSession):
            return self.fleet.untrack(session)
        if session.session_id.startswith("unbound:"):
            raise PikaError(
                "Pika cannot stop watching this placeholder safely because its "
                "provider UUID is not known yet. Wait for it to bind, or exit the "
                "agent normally."
            )
        panes = [
            pane
            for pane in self.tmux.list_panes()
            if (pane.pika_provider, pane.pika_session_id) == session.key
        ]
        # Commit the tombstone before clearing transport metadata so a racing
        # hook cannot persist the conversation again.
        self.store.untrack_session(*session.key)
        cleared = 0
        for pane in panes:
            try:
                self.tmux.clear_pika_tags(pane.pane_id)
            except (AttributeError, OSError, TmuxError):
                continue
            cleared += 1
        return cleared
