from __future__ import annotations

import os
import shlex
import subprocess
import time
import uuid
from collections.abc import Iterable
from pathlib import Path

from .experts import ExpertMatch, make_profile, rank_experts
from .models import Candidate, ExpertProfile, Pane, Session, Status
from .processes import (
    find_processes_with_session_id,
    process_alive,
    process_start_time,
    process_stats,
    process_tree,
    provider_process,
    shared_provider_process,
)
from .providers import Provider, providers
from .setup_hooks import hook_spec_fingerprint, hooks_installed
from .store import LIVE_OWNER_LEASE_SECONDS, Store, load_config
from .tmux import Tmux, TmuxError
from .ui import choose_session, sorted_attention_sessions, terminal_text

DUPLICATE_TMUX_ERROR = "duplicate Pika tmux homes"
UNVERIFIED_PANE_ERROR = "tagged provider PID lacks exact-UUID evidence"
OPEN_TWICE_ERROR = "exact provider UUID is open in multiple process trees"


class PikaError(RuntimeError):
    pass


class Pika:
    def __init__(
        self,
        store: Store | None = None,
        tmux: Tmux | None = None,
        provider_map: dict[str, Provider] | None = None,
    ):
        self.store = store or Store()
        self.tmux = tmux or Tmux()
        self.providers = provider_map or providers()
        self.discovery_errors: list[str] = []
        self.usage_errors: list[str] = []
        self.store.initialize()

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
        return sorted(by_key.values(), key=lambda item: item.updated_at, reverse=True)

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

    def hidden_session_keys(self) -> set[tuple[str, str]]:
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
        return result

    def import_candidate(
        self, candidate: Candidate, *, managed: bool = False
    ) -> Session:
        now = time.time()
        adopted_pane: Pane | None = None
        if candidate.live and candidate.pid:
            matching_panes = [
                pane
                for pane in self.tmux.list_panes()
                if provider_process(pane.pane_pid, candidate.provider) == candidate.pid
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
            Status.READY.value
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
            root_pid=candidate.pid,
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

    def refresh(self, *, usage: bool = False) -> list[Session]:
        tracked_sessions = self.store.list_sessions()
        candidates = self.discover_candidates(tracked_sessions)
        hidden_keys = self.hidden_session_keys()
        candidates = [
            item
            for item in candidates
            if (item.provider, item.session_id) not in hidden_keys
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
        # Recover tagged panes even if the ledger was lost.
        known_keys = {
            session.key
            for session in self.store.list_sessions()
            if session.key not in hidden_keys
        }
        for key, pane in pane_by_key.items():
            assert key[0] is not None and key[1] is not None
            if key in known_keys or key in hidden_keys:
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
            if session.key not in hidden_keys
        ]
        exact_panes = {(p.pika_provider, p.pika_session_id): p for p in panes}
        for session in sessions:
            candidate = candidate_map.get(session.key)
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
            if candidate:
                if candidate.name:
                    fields["name"] = candidate.name
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
                    if outside_exact and raw_pid in self.uuid_identity_pids(session):
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
                len(duplicate_panes) <= 1
                and session.error
                and (
                    session.error.startswith(DUPLICATE_TMUX_ERROR)
                    or session.error.startswith(UNVERIFIED_PANE_ERROR)
                    or session.error.startswith(OPEN_TWICE_ERROR)
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
            if session.key not in hidden_keys
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

    def hydrate_usage(self, sessions: list[Session]) -> list[Session]:
        """Add provider usage without delaying operational reconciliation."""
        self.usage_errors = []
        for session in sessions:
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

    def resolve(self, query: str, sessions: list[Session] | None = None) -> Session:
        supplied_sessions = sessions is not None
        sessions = sessions or self.refresh()
        discovered: list[Candidate] = []
        query_folded = query.casefold()
        uuid_matches = [item for item in sessions if item.session_id == query]
        if uuid_matches:
            return choose_session(uuid_matches)
        exact = [
            item
            for item in sessions
            if item.name and item.name.casefold() == query_folded
        ]
        if exact:
            return choose_session(exact, "Two continuations share that name")
        prefix = [item for item in sessions if item.session_id.startswith(query)]
        if len(prefix) == 1:
            return prefix[0]
        if not supplied_sessions:
            discovered = self.discover_candidates()
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

    def _outside_processes(self, session: Session, panes: list) -> list[int]:
        owned_pane_pids: set[int] = set()
        for pane in panes:
            if (pane.pika_provider, pane.pika_session_id) == session.key or (
                session.tmux_pane == pane.pane_id
                and not pane.pika_session_id
                and not pane.pika_provider
            ):
                owned_pane_pids.update(process_tree(pane.pane_pid))
        candidates = set(
            find_processes_with_session_id(session.session_id, session.provider)
        )
        provider = self.providers.get(session.provider)
        if provider:
            active_pids = getattr(provider, "active_pids", None)
            if active_pids:
                candidates.update(active_pids(session.session_id))
        candidates.update(self._live_owner_pids(session))
        return sorted(pid for pid in candidates if pid and pid not in owned_pane_pids)

    def uuid_identity_pids(self, session: Session) -> set[int]:
        """Return processes carrying provider-native UUID evidence."""
        exact: set[int] = set()
        provider = self.providers.get(session.provider)
        if provider:
            active_pids = getattr(provider, "active_pids", None)
            if active_pids:
                try:
                    exact.update(active_pids(session.session_id))
                except (OSError, RuntimeError):
                    pass
        return exact

    def _live_owner_pids(self, session: Session) -> set[int]:
        """Return valid hook leases, pruning dead, reused, and expired claims."""
        result: set[int] = set()
        now = time.time()
        for owner, owner_start, last_seen in self.store.get_live_owner_leases(
            *session.key
        ):
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
                self.store.delete_live_owner(*session.key, pid=owner)
                continue
            result.add(live_owner)
        return result

    def identity_pids(self, session: Session) -> set[int]:
        """Return UUID evidence plus currently valid hook-owner leases."""
        return self.uuid_identity_pids(session) | self._live_owner_pids(session)

    def _outside_uuid_pids(self, session: Session, pane: Pane) -> list[int]:
        inside = set(process_tree(pane.pane_pid))
        return sorted(self.uuid_identity_pids(session) - inside)

    def exact_pane_pid(self, session: Session, pane: Pane) -> int | None:
        """Accept one pane only when it contains all live UUID-owned processes."""
        pid = provider_process(pane.pane_pid, session.provider)
        if not pid:
            return None
        identities = self.identity_pids(session)
        if pid not in identities:
            return None
        other_identities = identities - {pid}
        if other_identities and not other_identities.issubset(
            process_tree(pane.pane_pid)
        ):
            return None
        return pid

    def open(self, session: Session, *, attach: bool = True) -> int:
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
            outside_exact = self._outside_uuid_pids(current, pane)
            if outside_exact and raw_pane_pid in self.uuid_identity_pids(current):
                raise PikaError(
                    f"OPEN TWICE: {current.display_name} has exact UUID "
                    f"{current.session_id} in its Pika pane (PID {raw_pane_pid}) "
                    f"and outside it (PID {', '.join(map(str, outside_exact))}). "
                    "Close one copy before attaching."
                )
            raise PikaError(
                f"{current.display_name}'s tagged pane contains a running "
                f"{current.provider} process (PID {raw_pane_pid}) that cannot be tied "
                f"to exact UUID {current.session_id}. Pika refuses to attach or "
                "issue an exact-thread receipt."
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
                raise PikaError(
                    f"{current.display_name} is already running outside Pika tmux (PID {pid_text}). "
                    "Pika refuses to open a duplicate conversation."
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
                    resumable = resumable_check(current.session_id)
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
            argv = provider.resume_argv(current.session_id)
            environment = {
                "PIKA_NAME": current.display_name,
                "PIKA_PROVIDER": current.provider,
                "PIKA_SESSION_ID": current.session_id,
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
                f"{'exact id' if exact else 'id'} {current.session_id[:8]}",
            ]
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
        if provider_name == "codex" and self.store.get_meta(
            "hook_seen:codex"
        ) != hook_spec_fingerprint("codex"):
            raise PikaError(
                "The current Codex hook definition has not run yet. Open `/hooks` "
                "in Codex, trust the Pika hooks, use that Codex session once, then "
                "run `pika new` again."
            )
        cwd = cwd or os.getcwd()
        if not Path(cwd).is_dir():
            raise PikaError(f"Working directory does not exist: {cwd}")
        token = str(uuid.uuid4())
        reserved_id = str(uuid.uuid4()) if provider_name == "claude" else None
        tmux_name = self._free_tmux_name(
            self.tmux.internal_name(provider_name, session_id=reserved_id, token=token),
            self.tmux.list_panes(),
        )
        self.store.add_pending(token, provider_name, name, cwd, tmux_session=tmux_name)
        environment = {
            "PIKA_NAME": name,
            "PIKA_PROVIDER": provider_name,
            "PIKA_LAUNCH_TOKEN": token,
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
        binding = self.store.finalize_pending_pane(
            token, pane.session_name, pane.pane_id
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

    def acknowledge(self, session: Session, *, attaching: bool = False) -> bool:
        return self.store.acknowledge_attention(
            session.provider,
            session.session_id,
            expected_event_at=session.last_event_at,
            attaching=attaching,
        )

    def next_attention(self, sessions: list[Session] | None = None) -> Session | None:
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
        topics: Iterable[str],
        artifacts: Iterable[str] = (),
    ) -> ExpertProfile:
        session = self.current_exact_session()
        profile = make_profile(
            session,
            summary=summary,
            topics=topics,
            artifacts=artifacts,
        )
        return self.store.put_expert_profile(profile)

    def clear_current_expert(self) -> Session:
        session = self.current_exact_session()
        self.store.delete_expert_profile(*session.key)
        return session

    def expert_matches(self, query: str = "") -> list[ExpertMatch]:
        return rank_experts(
            self.store.list_expert_profiles(),
            self.refresh(usage=False),
            query,
        )

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
        matches = [
            item
            for item in self.discover_candidates()
            if item.provider == provider_name and item.live and item.pid == pid
        ]
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
        self.store.upsert_session(session)
        self.tmux.tag_pane(
            pane.pane_id,
            provider=provider_name,
            session_id=session_id,
            name=session.display_name,
        )
        return session
