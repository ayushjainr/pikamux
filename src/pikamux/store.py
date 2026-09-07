from __future__ import annotations

import fcntl
import json
import os
import sqlite3
import time
import uuid
from dataclasses import replace
from collections.abc import Iterator
from contextlib import contextmanager
from pathlib import Path
from typing import Any

from .models import (
    ActivityEvent,
    ExpertProfile,
    ExpertRefreshAttempt,
    FleetNode,
    Session,
    Status,
    Usage,
)
from .paths import config_path, database_path
from .processes import process_start_time
from .status_projection import StatusObservation

DEFAULT_CONFIG: dict[str, Any] = {
    "version": 1,
    "default_provider": "codex",
    "alerts": "tmux",
    "peek_lines": 200,
    "provider_executables": {},
    # Paired laptop launchers are explicit, authenticated reverse-SSH routes.
    # Each entry is written only by the hidden exact-node pairing handshake.
    "client_bridges": [],
    # Codex app-server clients used as automation harnesses create real UUIDs,
    # but their short-lived workers are not user-facing conversations.  Keep
    # the known harness origins out of Pika while allowing installations to
    # extend the list for their own runners.
    "codex_worker_originators": ["agentic_fund", "quant_agent_autonomy"],
    # OpenCode does not persist an originator field. Suppress a root automation
    # run only when a configured title prefix and an isolated opencode-runtime
    # directory agree; either signal alone remains visible.
    "opencode_worker_title_prefixes": ["agentic-fund:", "quant-agent:"],
}

# Hook ownership is corroborating evidence, not durable conversation identity.
# Five minutes comfortably spans normal hook delivery while ensuring a shared
# Codex app-server cannot claim an exited client forever.
LIVE_OWNER_LEASE_SECONDS = 300.0


class Store:
    def __init__(self, path: Path | None = None):
        self.path = path or database_path()
        self._initialized = False

    def initialize(self) -> None:
        if self._initialized and self.path.exists():
            return
        self.path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
        lock_path = self.path.with_name(f".{self.path.name}.initialize.lock")
        lock_fd = os.open(lock_path, os.O_CREAT | os.O_RDWR, 0o600)
        try:
            os.chmod(lock_path, 0o600)
            fcntl.flock(lock_fd, fcntl.LOCK_EX)
            if self._initialized and self.path.exists():
                return
            self._initialize_schema()
        finally:
            fcntl.flock(lock_fd, fcntl.LOCK_UN)
            os.close(lock_fd)

    def _initialize_schema(self) -> None:
        try:
            os.chmod(self.path.parent, 0o700)
        except OSError:
            pass
        with self.connect() as db:
            db.executescript(
                """
                CREATE TABLE IF NOT EXISTS sessions (
                    provider TEXT NOT NULL,
                    session_id TEXT NOT NULL,
                    active_thread_id TEXT,
                    name TEXT,
                    cwd TEXT,
                    branch TEXT,
                    transcript_path TEXT,
                    tmux_session TEXT,
                    tmux_pane TEXT,
                    root_pid INTEGER,
                    status TEXT NOT NULL DEFAULT 'PARKED',
                    unread INTEGER NOT NULL DEFAULT 0,
                    model TEXT,
                    source TEXT NOT NULL DEFAULT 'managed',
                    managed INTEGER NOT NULL DEFAULT 1,
                    error TEXT,
                    attention_reason TEXT,
                    created_at REAL NOT NULL,
                    updated_at REAL NOT NULL,
                    last_event_at REAL NOT NULL,
                    last_activity_at REAL NOT NULL,
                    PRIMARY KEY (provider, session_id)
                );
                CREATE INDEX IF NOT EXISTS sessions_name_idx ON sessions(name COLLATE NOCASE);
                CREATE INDEX IF NOT EXISTS sessions_status_idx ON sessions(status, unread);
                CREATE TABLE IF NOT EXISTS pending_launches (
                    launch_token TEXT PRIMARY KEY,
                    provider TEXT NOT NULL,
                    name TEXT NOT NULL,
                    cwd TEXT NOT NULL,
                    tmux_session TEXT,
                    tmux_pane TEXT,
                    expected_session_id TEXT,
                    root_pid INTEGER,
                    root_pid_start INTEGER,
                    preexisting_session_ids_json TEXT,
                    candidate_session_id TEXT,
                    candidate_observed_at REAL,
                    created_at REAL NOT NULL
                );
                CREATE TABLE IF NOT EXISTS launch_reservations (
                    provider TEXT NOT NULL,
                    session_id TEXT NOT NULL,
                    token TEXT NOT NULL,
                    owner_pid INTEGER,
                    owner_start_time INTEGER,
                    created_at REAL NOT NULL,
                    PRIMARY KEY (provider, session_id)
                );
                CREATE TABLE IF NOT EXISTS launch_bindings (
                    launch_token TEXT PRIMARY KEY,
                    provider TEXT NOT NULL,
                    session_id TEXT NOT NULL,
                    created_at REAL NOT NULL
                );
                CREATE TABLE IF NOT EXISTS live_owners (
                    provider TEXT NOT NULL,
                    session_id TEXT NOT NULL,
                    pid INTEGER NOT NULL,
                    start_time INTEGER,
                    owner_token TEXT NOT NULL DEFAULT '',
                    last_seen REAL NOT NULL,
                    PRIMARY KEY (provider, session_id, pid, owner_token)
                );
                CREATE TABLE IF NOT EXISTS recovery_owners (
                    provider TEXT NOT NULL,
                    session_id TEXT NOT NULL,
                    pid INTEGER NOT NULL,
                    start_time INTEGER NOT NULL,
                    launch_token TEXT NOT NULL,
                    created_at REAL NOT NULL,
                    PRIMARY KEY (provider, session_id)
                );
                CREATE TABLE IF NOT EXISTS untracked_sessions (
                    provider TEXT NOT NULL,
                    session_id TEXT NOT NULL,
                    untracked_at REAL NOT NULL,
                    PRIMARY KEY (provider, session_id)
                );
                CREATE TABLE IF NOT EXISTS usage_cache (
                    provider TEXT NOT NULL,
                    session_id TEXT NOT NULL,
                    source_path TEXT NOT NULL,
                    source_mtime_ns INTEGER NOT NULL,
                    source_size INTEGER NOT NULL,
                    model TEXT,
                    input_tokens INTEGER NOT NULL,
                    output_tokens INTEGER NOT NULL,
                    cached_input_tokens INTEGER NOT NULL,
                    cache_write_tokens INTEGER NOT NULL,
                    total_tokens INTEGER NOT NULL,
                    estimated_cost_usd REAL,
                    updated_at REAL NOT NULL,
                    PRIMARY KEY (provider, session_id)
                );
                CREATE TABLE IF NOT EXISTS meta (
                    key TEXT PRIMARY KEY,
                    value TEXT NOT NULL
                );
                CREATE TABLE IF NOT EXISTS hook_observations (
                    provider TEXT PRIMARY KEY,
                    fingerprint TEXT NOT NULL,
                    event_name TEXT NOT NULL,
                    session_id TEXT NOT NULL,
                    observed_at REAL NOT NULL,
                    source TEXT,
                    managed INTEGER NOT NULL DEFAULT 0
                );
                CREATE TABLE IF NOT EXISTS session_events (
                    event_id INTEGER PRIMARY KEY AUTOINCREMENT,
                    provider TEXT NOT NULL,
                    session_id TEXT NOT NULL,
                    event_at REAL NOT NULL,
                    status TEXT NOT NULL,
                    attention_reason TEXT,
                    error TEXT,
                    UNIQUE (provider, session_id, event_at, status)
                );
                CREATE INDEX IF NOT EXISTS session_events_time_idx
                ON session_events(event_at);
                CREATE TABLE IF NOT EXISTS session_status_observations (
                    provider TEXT NOT NULL,
                    session_id TEXT NOT NULL,
                    kind TEXT NOT NULL,
                    status TEXT NOT NULL,
                    unread INTEGER NOT NULL DEFAULT 0,
                    attention_reason TEXT,
                    error TEXT,
                    observed_at REAL NOT NULL,
                    source TEXT NOT NULL,
                    PRIMARY KEY (provider, session_id, kind)
                );
                CREATE INDEX IF NOT EXISTS session_status_observations_time_idx
                ON session_status_observations(observed_at);
                CREATE TABLE IF NOT EXISTS identity_interruptions (
                    provider TEXT NOT NULL,
                    session_id TEXT NOT NULL,
                    status TEXT NOT NULL,
                    unread INTEGER NOT NULL,
                    attention_reason TEXT,
                    error TEXT,
                    last_event_at REAL NOT NULL,
                    PRIMARY KEY (provider, session_id)
                );
                CREATE TABLE IF NOT EXISTS expert_profiles (
                    provider TEXT NOT NULL,
                    session_id TEXT NOT NULL,
                    summary TEXT NOT NULL,
                    current_state TEXT NOT NULL DEFAULT '',
                    topics_json TEXT NOT NULL,
                    artifacts_json TEXT NOT NULL,
                    source TEXT NOT NULL DEFAULT 'self',
                    transcript_mtime_ns INTEGER,
                    transcript_size INTEGER,
                    updated_at REAL NOT NULL,
                    PRIMARY KEY (provider, session_id)
                );
                CREATE INDEX IF NOT EXISTS expert_profiles_updated_idx
                ON expert_profiles(updated_at DESC);
                CREATE TABLE IF NOT EXISTS expert_refresh_attempts (
                    provider TEXT NOT NULL,
                    session_id TEXT NOT NULL,
                    reset_at INTEGER NOT NULL,
                    status TEXT NOT NULL,
                    detail TEXT,
                    attempted_at REAL NOT NULL,
                    PRIMARY KEY (provider, session_id, reset_at)
                );
                CREATE TABLE IF NOT EXISTS fleet_nodes (
                    node_id TEXT PRIMARY KEY,
                    alias TEXT NOT NULL UNIQUE COLLATE NOCASE,
                    ssh_target TEXT NOT NULL,
                    sources_json TEXT NOT NULL DEFAULT '[]',
                    status TEXT NOT NULL DEFAULT 'unknown',
                    protocol_version INTEGER,
                    package_version TEXT,
                    capabilities_json TEXT NOT NULL DEFAULT '[]',
                    last_seen REAL NOT NULL DEFAULT 0,
                    last_attempt_at REAL NOT NULL DEFAULT 0,
                    last_error TEXT,
                    created_at REAL NOT NULL,
                    updated_at REAL NOT NULL
                );
                CREATE TABLE IF NOT EXISTS remote_snapshots (
                    node_id TEXT PRIMARY KEY,
                    payload_json TEXT NOT NULL,
                    captured_at REAL NOT NULL
                );
                CREATE TABLE IF NOT EXISTS ignored_node_candidates (
                    candidate_key TEXT PRIMARY KEY,
                    ignored_at REAL NOT NULL
                );
                """
            )
            owner_columns = db.execute("PRAGMA table_info(live_owners)").fetchall()
            owner_pk = [
                str(row["name"])
                for row in sorted(owner_columns, key=lambda row: int(row["pk"]))
                if row["pk"]
            ]
            if owner_pk == ["provider", "session_id"]:
                db.executescript(
                    """
                    ALTER TABLE live_owners RENAME TO live_owners_legacy;
                    CREATE TABLE live_owners (
                        provider TEXT NOT NULL,
                        session_id TEXT NOT NULL,
                        pid INTEGER NOT NULL,
                        start_time INTEGER,
                        owner_token TEXT NOT NULL DEFAULT '',
                        last_seen REAL NOT NULL,
                        PRIMARY KEY (provider, session_id, pid, owner_token)
                    );
                    INSERT OR IGNORE INTO live_owners(
                        provider,session_id,pid,start_time,owner_token,last_seen
                    )
                    SELECT provider,session_id,pid,NULL,'',last_seen
                    FROM live_owners_legacy;
                    DROP TABLE live_owners_legacy;
                    """
                )
                owner_columns = db.execute("PRAGMA table_info(live_owners)").fetchall()
                owner_pk = [
                    str(row["name"])
                    for row in sorted(owner_columns, key=lambda row: int(row["pk"]))
                    if row["pk"]
                ]
            if owner_pk == ["provider", "session_id", "pid"]:
                db.executescript(
                    """
                    ALTER TABLE live_owners RENAME TO live_owners_pid_legacy;
                    CREATE TABLE live_owners (
                        provider TEXT NOT NULL,
                        session_id TEXT NOT NULL,
                        pid INTEGER NOT NULL,
                        start_time INTEGER,
                        owner_token TEXT NOT NULL DEFAULT '',
                        last_seen REAL NOT NULL,
                        PRIMARY KEY (provider, session_id, pid, owner_token)
                    );
                    INSERT OR IGNORE INTO live_owners(
                        provider,session_id,pid,start_time,owner_token,last_seen
                    )
                    SELECT provider,session_id,pid,start_time,'',last_seen
                    FROM live_owners_pid_legacy;
                    DROP TABLE live_owners_pid_legacy;
                    """
                )
                owner_columns = db.execute("PRAGMA table_info(live_owners)").fetchall()
            live_owner_columns = {str(row["name"]) for row in owner_columns}
            if "start_time" not in live_owner_columns:
                db.execute("ALTER TABLE live_owners ADD COLUMN start_time INTEGER")
            reservation_columns = {
                str(row["name"])
                for row in db.execute(
                    "PRAGMA table_info(launch_reservations)"
                ).fetchall()
            }
            if "owner_pid" not in reservation_columns:
                db.execute(
                    "ALTER TABLE launch_reservations ADD COLUMN owner_pid INTEGER"
                )
            if "owner_start_time" not in reservation_columns:
                db.execute(
                    "ALTER TABLE launch_reservations "
                    "ADD COLUMN owner_start_time INTEGER"
                )
            pending_columns = {
                str(row["name"])
                for row in db.execute("PRAGMA table_info(pending_launches)").fetchall()
            }
            if "expected_session_id" not in pending_columns:
                db.execute(
                    "ALTER TABLE pending_launches ADD COLUMN expected_session_id TEXT"
                )
            if "root_pid" not in pending_columns:
                db.execute("ALTER TABLE pending_launches ADD COLUMN root_pid INTEGER")
            if "root_pid_start" not in pending_columns:
                db.execute(
                    "ALTER TABLE pending_launches ADD COLUMN root_pid_start INTEGER"
                )
            if "preexisting_session_ids_json" not in pending_columns:
                db.execute(
                    "ALTER TABLE pending_launches "
                    "ADD COLUMN preexisting_session_ids_json TEXT"
                )
            if "candidate_session_id" not in pending_columns:
                db.execute(
                    "ALTER TABLE pending_launches ADD COLUMN candidate_session_id TEXT"
                )
            if "candidate_observed_at" not in pending_columns:
                db.execute(
                    "ALTER TABLE pending_launches ADD COLUMN candidate_observed_at REAL"
                )
            session_columns = {
                str(row["name"])
                for row in db.execute("PRAGMA table_info(sessions)").fetchall()
            }
            if "attention_reason" not in session_columns:
                db.execute("ALTER TABLE sessions ADD COLUMN attention_reason TEXT")
            if "active_thread_id" not in session_columns:
                db.execute("ALTER TABLE sessions ADD COLUMN active_thread_id TEXT")
            db.execute(
                "CREATE INDEX IF NOT EXISTS sessions_active_thread_idx "
                "ON sessions(provider, active_thread_id)"
            )
            expert_columns = {
                str(row["name"])
                for row in db.execute("PRAGMA table_info(expert_profiles)").fetchall()
            }
            if "source" not in expert_columns:
                db.execute(
                    "ALTER TABLE expert_profiles "
                    "ADD COLUMN source TEXT NOT NULL DEFAULT 'self'"
                )
            if "transcript_mtime_ns" not in expert_columns:
                db.execute(
                    "ALTER TABLE expert_profiles ADD COLUMN transcript_mtime_ns INTEGER"
                )
            if "transcript_size" not in expert_columns:
                db.execute(
                    "ALTER TABLE expert_profiles ADD COLUMN transcript_size INTEGER"
                )
            if "current_state" not in expert_columns:
                db.execute(
                    "ALTER TABLE expert_profiles "
                    "ADD COLUMN current_state TEXT NOT NULL DEFAULT ''"
                )
            for column, declaration in (
                ("scope_updated_at", "REAL NOT NULL DEFAULT 0"),
                ("current_state_updated_at", "REAL NOT NULL DEFAULT 0"),
                ("current_state_mtime_ns", "INTEGER"),
                ("current_state_size", "INTEGER"),
            ):
                if column not in expert_columns:
                    db.execute(f"ALTER TABLE expert_profiles ADD COLUMN {column} {declaration}")
            # Existing full cards provide a checkpoint for both kinds of content.
            db.execute("UPDATE expert_profiles SET scope_updated_at=updated_at WHERE scope_updated_at=0")
            db.execute(
                "UPDATE expert_profiles SET current_state_updated_at=updated_at,"
                "current_state_mtime_ns=transcript_mtime_ns,current_state_size=transcript_size "
                "WHERE current_state_updated_at=0 AND current_state<>''"
            )
            event_columns = {
                str(row["name"])
                for row in db.execute("PRAGMA table_info(session_events)").fetchall()
            }
            if "event_id" not in event_columns:
                db.executescript(
                    """
                    ALTER TABLE session_events RENAME TO session_events_legacy;
                    CREATE TABLE session_events (
                        event_id INTEGER PRIMARY KEY AUTOINCREMENT,
                        provider TEXT NOT NULL,
                        session_id TEXT NOT NULL,
                        event_at REAL NOT NULL,
                        status TEXT NOT NULL,
                        attention_reason TEXT,
                        error TEXT,
                        UNIQUE (provider, session_id, event_at, status)
                    );
                    INSERT OR IGNORE INTO session_events(
                        provider,session_id,event_at,status,attention_reason,error
                    )
                    SELECT provider,session_id,event_at,status,attention_reason,error
                    FROM session_events_legacy ORDER BY event_at;
                    DROP TABLE session_events_legacy;
                    DROP INDEX IF EXISTS session_events_time_idx;
                    CREATE INDEX session_events_time_idx
                    ON session_events(event_at);
                    """
                )
            # Migrate the legacy materialized state only after every older
            # schema has received the columns referenced by the projection.
            # Identity interruptions contain lifecycle truth hidden by a
            # current fail-closed safety observation.
            db.execute(
                """
                INSERT OR IGNORE INTO session_status_observations(
                    provider,session_id,kind,status,unread,attention_reason,
                    error,observed_at,source
                )
                SELECT provider,session_id,
                       CASE
                         WHEN status IN ('ERROR','OPEN TWICE')
                              AND attention_reason='identity' THEN 'safety'
                         WHEN status='ERROR' AND attention_reason='exited'
                              THEN 'runtime'
                         ELSE 'lifecycle'
                       END,
                       status,unread,attention_reason,error,last_event_at,'legacy'
                FROM sessions
                """
            )
            db.execute(
                """
                INSERT OR IGNORE INTO session_status_observations(
                    provider,session_id,kind,status,unread,attention_reason,
                    error,observed_at,source
                )
                SELECT provider,session_id,'lifecycle',status,unread,
                       attention_reason,error,last_event_at,'identity-interruption'
                FROM identity_interruptions
                """
            )
            fleet_columns = {
                str(row["name"])
                for row in db.execute("PRAGMA table_info(fleet_nodes)").fetchall()
            }
            if "last_attempt_at" not in fleet_columns:
                db.execute(
                    "ALTER TABLE fleet_nodes ADD COLUMN last_attempt_at "
                    "REAL NOT NULL DEFAULT 0"
                )
            if "package_version" not in fleet_columns:
                db.execute("ALTER TABLE fleet_nodes ADD COLUMN package_version TEXT")
        try:
            os.chmod(self.path, 0o600)
        except OSError:
            pass
        self._initialized = True

    @contextmanager
    def connect(self) -> Iterator[sqlite3.Connection]:
        db = sqlite3.connect(self.path, timeout=5)
        db.row_factory = sqlite3.Row
        db.execute("PRAGMA busy_timeout=5000")
        if not self._initialized:
            # WAL selection is persistent and initialization is file-locked.
            # Re-negotiating it on every hook connection creates a lock race.
            db.execute("PRAGMA journal_mode=WAL")
        try:
            yield db
            db.commit()
        finally:
            db.close()

    def upsert_session(self, session: Session, *, preserve_name: bool = False) -> None:
        self.initialize()
        now = time.time()
        created = session.created_at or now
        activity = session.last_activity_at or session.updated_at or now
        event_at = session.last_event_at or now
        with self.connect() as db:
            if db.execute(
                "SELECT 1 FROM untracked_sessions WHERE provider=? AND session_id=?",
                session.key,
            ).fetchone():
                return
            existing = db.execute(
                "SELECT name,status,unread,attention_reason,error "
                "FROM sessions WHERE provider=? AND session_id=?",
                session.key,
            ).fetchone()
            name = (
                existing["name"]
                if preserve_name and existing and existing["name"]
                else session.name
            )
            if not (
                session.status in {Status.ERROR.value, Status.OPEN_TWICE.value}
                and session.attention_reason == "identity"
            ):
                # A real provider lifecycle event may arrive while identity is
                # ambiguous. Preserve the newest lifecycle behind that fault.
                db.execute(
                    """
                    UPDATE identity_interruptions
                    SET status=?,unread=?,attention_reason=?,error=?,last_event_at=?
                    WHERE provider=? AND session_id=?
                    """,
                    (
                        session.status,
                        int(session.unread),
                        session.attention_reason,
                        session.error,
                        event_at,
                        session.provider,
                        session.session_id,
                    ),
                )
            db.execute(
                """
                INSERT INTO sessions (
                    provider, session_id, active_thread_id, name, cwd, branch, transcript_path,
                    tmux_session, tmux_pane, root_pid, status, unread, model,
                    source, managed, error, attention_reason, created_at, updated_at,
                    last_event_at, last_activity_at
                ) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)
                ON CONFLICT(provider, session_id) DO UPDATE SET
                    active_thread_id=COALESCE(excluded.active_thread_id, sessions.active_thread_id),
                    name=COALESCE(excluded.name, sessions.name),
                    cwd=COALESCE(excluded.cwd, sessions.cwd),
                    branch=COALESCE(excluded.branch, sessions.branch),
                    transcript_path=COALESCE(excluded.transcript_path, sessions.transcript_path),
                    tmux_session=COALESCE(excluded.tmux_session, sessions.tmux_session),
                    tmux_pane=COALESCE(excluded.tmux_pane, sessions.tmux_pane),
                    root_pid=COALESCE(excluded.root_pid, sessions.root_pid),
                    status=excluded.status,
                    unread=excluded.unread,
                    model=COALESCE(excluded.model, sessions.model),
                    source=excluded.source,
                    managed=MAX(sessions.managed, excluded.managed),
                    error=excluded.error,
                    attention_reason=excluded.attention_reason,
                    updated_at=excluded.updated_at,
                    last_event_at=MAX(sessions.last_event_at, excluded.last_event_at),
                    last_activity_at=MAX(sessions.last_activity_at, excluded.last_activity_at)
                """,
                (
                    session.provider,
                    session.session_id,
                    session.active_thread_id,
                    name,
                    session.cwd,
                    session.branch,
                    session.transcript_path,
                    session.tmux_session,
                    session.tmux_pane,
                    session.root_pid,
                    session.status,
                    int(session.unread),
                    session.model,
                    session.source,
                    int(session.managed),
                    session.error,
                    session.attention_reason,
                    created,
                    now,
                    event_at,
                    activity,
                ),
            )
            db.execute(
                """
                INSERT OR IGNORE INTO session_status_observations(
                    provider,session_id,kind,status,unread,attention_reason,
                    error,observed_at,source
                ) VALUES (?,?,?,?,?,?,?,?,?)
                """,
                (
                    session.provider,
                    session.session_id,
                    "safety"
                    if session.status in {Status.ERROR.value, Status.OPEN_TWICE.value}
                    and session.attention_reason == "identity"
                    else "runtime"
                    if session.status == Status.ERROR.value
                    and session.attention_reason == "exited"
                    else "lifecycle",
                    session.status,
                    int(session.unread),
                    session.attention_reason,
                    session.error,
                    event_at,
                    "initial",
                ),
            )
            if self._became_actionable(
                existing,
                status=session.status,
                unread=session.unread,
                attention_reason=session.attention_reason,
                error=session.error,
            ):
                self._insert_session_event(db, session, event_at)

    def get_session(self, provider: str, session_id: str) -> Session | None:
        self.initialize()
        with self.connect() as db:
            row = db.execute(
                "SELECT * FROM sessions WHERE provider=? AND session_id=?",
                (provider, session_id),
            ).fetchone()
        return self._row_to_session(row) if row else None

    def get_session_by_thread(self, provider: str, thread_id: str) -> Session | None:
        """Resolve either Pika's stable key or the provider's active leaf UUID."""
        self.initialize()
        with self.connect() as db:
            row = db.execute(
                "SELECT * FROM sessions WHERE provider=? AND "
                "(session_id=? OR active_thread_id=?) "
                "ORDER BY CASE WHEN session_id=? THEN 0 ELSE 1 END LIMIT 1",
                (provider, thread_id, thread_id, thread_id),
            ).fetchone()
        return self._row_to_session(row) if row else None

    def list_sessions(self) -> list[Session]:
        self.initialize()
        with self.connect() as db:
            rows = db.execute(
                "SELECT sessions.* FROM sessions "
                "LEFT JOIN untracked_sessions USING(provider,session_id) "
                "WHERE untracked_sessions.session_id IS NULL"
            ).fetchall()
        return [self._row_to_session(row) for row in rows]

    def list_untracked_sessions(self) -> list[Session]:
        self.initialize()
        with self.connect() as db:
            rows = db.execute(
                "SELECT sessions.* FROM sessions "
                "JOIN untracked_sessions USING(provider,session_id)"
            ).fetchall()
        return [self._row_to_session(row) for row in rows]

    def is_untracked(self, provider: str, session_id: str) -> bool:
        self.initialize()
        with self.connect() as db:
            row = db.execute(
                "SELECT 1 FROM untracked_sessions WHERE provider=? AND session_id=?",
                (provider, session_id),
            ).fetchone()
        return row is not None

    def untracked_session_keys(self) -> set[tuple[str, str]]:
        self.initialize()
        with self.connect() as db:
            rows = db.execute(
                "SELECT provider,session_id FROM untracked_sessions"
            ).fetchall()
        return {(str(row["provider"]), str(row["session_id"])) for row in rows}

    def restore_tracking(self, provider: str, session_id: str) -> None:
        self.initialize()
        with self.connect() as db:
            db.execute(
                "DELETE FROM untracked_sessions WHERE provider=? AND session_id=?",
                (provider, session_id),
            )

    def untrack_session(self, provider: str, session_id: str) -> None:
        """Remove operational tracking while retaining provider data and expertise."""
        self.initialize()
        with self.connect() as db:
            db.execute("BEGIN IMMEDIATE")
            db.execute(
                "INSERT INTO untracked_sessions(provider,session_id,untracked_at) "
                "VALUES (?,?,?) ON CONFLICT(provider,session_id) DO UPDATE SET "
                "untracked_at=excluded.untracked_at",
                (provider, session_id, time.time()),
            )
            for table in (
                "usage_cache",
                "session_events",
                "session_status_observations",
                "identity_interruptions",
                "expert_refresh_attempts",
                "live_owners",
                "recovery_owners",
                "launch_reservations",
            ):
                db.execute(
                    f"DELETE FROM {table} WHERE provider=? AND session_id=?",
                    (provider, session_id),
                )
            db.execute(
                "DELETE FROM launch_bindings WHERE provider=? AND session_id=?",
                (provider, session_id),
            )
            db.execute(
                """
                UPDATE sessions
                SET tmux_session=NULL,tmux_pane=NULL,root_pid=NULL,
                    status=?,unread=0,error=NULL,attention_reason=NULL,updated_at=?
                WHERE provider=? AND session_id=?
                """,
                (Status.PARKED.value, time.time(), provider, session_id),
            )

    def update_session(self, provider: str, session_id: str, **fields: Any) -> None:
        if not fields:
            return
        allowed = {
            "active_thread_id",
            "name",
            "cwd",
            "branch",
            "transcript_path",
            "tmux_session",
            "tmux_pane",
            "root_pid",
            "status",
            "unread",
            "model",
            "source",
            "managed",
            "error",
            "attention_reason",
            "updated_at",
            "last_event_at",
            "last_activity_at",
        }
        unknown = set(fields) - allowed
        if unknown:
            raise ValueError(
                f"Unsupported session fields: {', '.join(sorted(unknown))}"
            )
        now = time.time()
        fields.setdefault("updated_at", now)
        if "unread" in fields:
            fields["unread"] = int(bool(fields["unread"]))
        if "managed" in fields:
            fields["managed"] = int(bool(fields["managed"]))
        assignments = ", ".join(f"{key}=?" for key in fields)
        values = list(fields.values()) + [provider, session_id]
        self.initialize()
        with self.connect() as db:
            existing = db.execute(
                "SELECT status,unread,attention_reason,error,last_event_at "
                "FROM sessions WHERE provider=? AND session_id=?",
                (provider, session_id),
            ).fetchone()
            status = str(fields.get("status", existing["status"] if existing else ""))
            unread = bool(fields.get("unread", existing["unread"] if existing else 0))
            attention_reason = fields.get(
                "attention_reason",
                existing["attention_reason"] if existing else None,
            )
            error = fields.get("error", existing["error"] if existing else None)
            became_actionable = self._became_actionable(
                existing,
                status=status,
                unread=unread,
                attention_reason=attention_reason,
                error=error,
            )
            if became_actionable and "last_event_at" not in fields:
                fields["last_event_at"] = now
                assignments = ", ".join(f"{key}=?" for key in fields)
                values = list(fields.values()) + [provider, session_id]
            db.execute(
                f"UPDATE sessions SET {assignments} WHERE provider=? AND session_id=?",
                values,
            )
            if became_actionable:
                self._insert_session_event(
                    db,
                    Session(
                        provider,
                        session_id,
                        status=status,
                        unread=unread,
                        attention_reason=(
                            str(attention_reason)
                            if attention_reason is not None
                            else None
                        ),
                        error=str(error) if error is not None else None,
                    ),
                    float(fields.get("last_event_at", now)),
                )

    def record_status_observation(
        self,
        provider: str,
        session_id: str,
        *,
        kind: str,
        status: str,
        unread: bool,
        attention_reason: str | None,
        error: str | None,
        observed_at: float,
        source: str,
    ) -> bool:
        """Persist the newest fact of one kind without accepting stale replay."""
        if kind not in {"lifecycle", "runtime", "safety"}:
            raise ValueError(f"Unsupported status observation kind: {kind}")
        self.initialize()
        with self.connect() as db:
            cursor = db.execute(
                """
                INSERT INTO session_status_observations(
                    provider,session_id,kind,status,unread,attention_reason,
                    error,observed_at,source
                ) VALUES (?,?,?,?,?,?,?,?,?)
                ON CONFLICT(provider,session_id,kind) DO UPDATE SET
                    status=excluded.status,
                    unread=excluded.unread,
                    attention_reason=excluded.attention_reason,
                    error=excluded.error,
                    observed_at=excluded.observed_at,
                    source=excluded.source
                WHERE excluded.observed_at >= session_status_observations.observed_at
                """,
                (
                    provider,
                    session_id,
                    kind,
                    status,
                    int(unread),
                    attention_reason,
                    error,
                    observed_at,
                    source,
                ),
            )
        return cursor.rowcount == 1

    def status_observations(
        self, provider: str, session_id: str
    ) -> tuple[StatusObservation, ...]:
        self.initialize()
        with self.connect() as db:
            rows = db.execute(
                """
                SELECT kind,status,unread,attention_reason,error,observed_at,source
                FROM session_status_observations
                WHERE provider=? AND session_id=?
                """,
                (provider, session_id),
            ).fetchall()
        return tuple(
            StatusObservation(
                kind=str(row["kind"]),
                status=str(row["status"]),
                unread=bool(row["unread"]),
                attention_reason=(
                    str(row["attention_reason"])
                    if row["attention_reason"] is not None
                    else None
                ),
                error=str(row["error"]) if row["error"] is not None else None,
                observed_at=float(row["observed_at"]),
                source=str(row["source"]),
            )
            for row in rows
        )

    def clear_status_observation(
        self, provider: str, session_id: str, kind: str
    ) -> None:
        self.initialize()
        with self.connect() as db:
            db.execute(
                "DELETE FROM session_status_observations "
                "WHERE provider=? AND session_id=? AND kind=?",
                (provider, session_id, kind),
            )

    def delete_session(
        self,
        provider: str,
        session_id: str,
        *,
        preserve_live_owners: bool = False,
    ) -> None:
        self.initialize()
        with self.connect() as db:
            tables = [
                "usage_cache",
                "session_events",
                "session_status_observations",
                "identity_interruptions",
                "expert_profiles",
                "expert_refresh_attempts",
                "recovery_owners",
                "launch_reservations",
            ]
            if not preserve_live_owners:
                tables.append("live_owners")
            for table in tables:
                db.execute(
                    f"DELETE FROM {table} WHERE provider=? AND session_id=?",
                    (provider, session_id),
                )
            db.execute(
                "DELETE FROM launch_bindings WHERE provider=? AND session_id=?",
                (provider, session_id),
            )
            db.execute(
                "DELETE FROM sessions WHERE provider=? AND session_id=?",
                (provider, session_id),
            )

    def put_expert_profile(self, profile: ExpertProfile) -> ExpertProfile:
        self.initialize()
        updated_at = profile.updated_at or time.time()
        with self.connect() as db:
            db.execute("BEGIN IMMEDIATE")
            if not db.execute(
                "SELECT 1 FROM sessions WHERE provider=? AND session_id=?",
                profile.key,
            ).fetchone():
                raise ValueError("Expert profile requires a tracked Pika session")
            row = db.execute(
                "SELECT * FROM expert_profiles WHERE provider=? AND session_id=?", profile.key
            ).fetchone()
            existing = self._row_to_expert_profile(row) if row else None
            scope_changed = existing is None or (
                profile.summary, profile.topics, profile.artifacts
            ) != (existing.summary, existing.topics, existing.artifacts)
            work_changed = existing is None or profile.current_state != existing.current_state
            verified_interview = (
                profile.source == "interview"
                and profile.transcript_mtime_ns is not None
                and profile.transcript_size is not None
            )
            if existing and not scope_changed and not work_changed:
                if not verified_interview:
                    return existing
                # A paid interview can reaffirm unchanged content against a new
                # transcript. Refresh the evidence, not the publication clocks.
                verified = replace(
                    existing,
                    transcript_mtime_ns=profile.transcript_mtime_ns,
                    transcript_size=profile.transcript_size,
                    current_state_mtime_ns=profile.transcript_mtime_ns,
                    current_state_size=profile.transcript_size,
                )
                if verified != existing:
                    db.execute(
                        "UPDATE expert_profiles SET transcript_mtime_ns=?,transcript_size=?,"
                        "current_state_mtime_ns=?,current_state_size=? "
                        "WHERE provider=? AND session_id=?",
                        (
                            verified.transcript_mtime_ns, verified.transcript_size,
                            verified.current_state_mtime_ns, verified.current_state_size,
                            *verified.key,
                        ),
                    )
                return verified
            profile = replace(
                profile,
                updated_at=updated_at,
                scope_updated_at=updated_at if scope_changed else existing.scope_updated_at,
                current_state_updated_at=(
                    updated_at if work_changed and profile.current_state else
                    existing.current_state_updated_at if existing else 0.0
                ),
                current_state_mtime_ns=(
                    profile.transcript_mtime_ns if work_changed or verified_interview else existing.current_state_mtime_ns
                ),
                current_state_size=(
                    profile.transcript_size if work_changed or verified_interview else existing.current_state_size
                ),
            )
            db.execute(
                """
                INSERT INTO expert_profiles(
                    provider,session_id,summary,current_state,topics_json,
                    artifacts_json,source,transcript_mtime_ns,transcript_size,
                    updated_at,scope_updated_at,current_state_updated_at,
                    current_state_mtime_ns,current_state_size
                ) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?)
                ON CONFLICT(provider,session_id) DO UPDATE SET
                    summary=excluded.summary,
                    current_state=excluded.current_state,
                    topics_json=excluded.topics_json,
                    artifacts_json=excluded.artifacts_json,
                    source=excluded.source,
                    transcript_mtime_ns=excluded.transcript_mtime_ns,
                    transcript_size=excluded.transcript_size,
                    updated_at=excluded.updated_at,
                    scope_updated_at=excluded.scope_updated_at,
                    current_state_updated_at=excluded.current_state_updated_at,
                    current_state_mtime_ns=excluded.current_state_mtime_ns,
                    current_state_size=excluded.current_state_size
                """,
                (
                    profile.provider,
                    profile.session_id,
                    profile.summary,
                    profile.current_state,
                    json.dumps(profile.topics, ensure_ascii=False),
                    json.dumps(profile.artifacts, ensure_ascii=False),
                    profile.source,
                    profile.transcript_mtime_ns,
                    profile.transcript_size,
                    updated_at,
                    profile.scope_updated_at,
                    profile.current_state_updated_at,
                    profile.current_state_mtime_ns,
                    profile.current_state_size,
                ),
            )
        return profile

    def put_expert_current_state(
        self, provider: str, session_id: str, current_state: str, *,
        transcript_mtime_ns: int | None = None, transcript_size: int | None = None,
    ) -> ExpertProfile:
        """Publish a small work update without rewriting durable expertise.

        No provider is contacted and unchanged content does not refresh its age.
        """
        from .experts import clean_current_state

        clean = clean_current_state(current_state)
        self.initialize()
        with self.connect() as db:
            db.execute("BEGIN IMMEDIATE")
            row = db.execute(
                "SELECT * FROM expert_profiles WHERE provider=? AND session_id=?",
                (provider, session_id),
            ).fetchone()
            if row is None:
                raise ValueError("Publish an expert profile before a current-work update")
            existing = self._row_to_expert_profile(row)
            if clean == existing.current_state:
                return existing
            now = time.time()
            db.execute(
                "UPDATE expert_profiles SET current_state=?,current_state_updated_at=?,"
                "current_state_mtime_ns=?,current_state_size=?,updated_at=? "
                "WHERE provider=? AND session_id=?",
                (clean, now, transcript_mtime_ns, transcript_size, now, provider, session_id),
            )
            return replace(
                existing, current_state=clean, current_state_updated_at=now,
                current_state_mtime_ns=transcript_mtime_ns,
                current_state_size=transcript_size, updated_at=now,
            )

    def get_expert_profile(
        self, provider: str, session_id: str
    ) -> ExpertProfile | None:
        self.initialize()
        with self.connect() as db:
            row = db.execute(
                "SELECT * FROM expert_profiles WHERE provider=? AND session_id=?",
                (provider, session_id),
            ).fetchone()
        return self._row_to_expert_profile(row) if row else None

    def list_expert_profiles(self) -> list[ExpertProfile]:
        self.initialize()
        with self.connect() as db:
            rows = db.execute(
                "SELECT * FROM expert_profiles ORDER BY updated_at DESC"
            ).fetchall()
        return [self._row_to_expert_profile(row) for row in rows]

    def delete_expert_profile(self, provider: str, session_id: str) -> bool:
        self.initialize()
        with self.connect() as db:
            cursor = db.execute(
                "DELETE FROM expert_profiles WHERE provider=? AND session_id=?",
                (provider, session_id),
            )
        return cursor.rowcount == 1

    def put_expert_refresh_attempt(
        self, attempt: ExpertRefreshAttempt
    ) -> ExpertRefreshAttempt:
        self.initialize()
        attempted_at = attempt.attempted_at or time.time()
        with self.connect() as db:
            db.execute(
                """
                INSERT INTO expert_refresh_attempts(
                    provider,session_id,reset_at,status,detail,attempted_at
                ) VALUES (?,?,?,?,?,?)
                ON CONFLICT(provider,session_id,reset_at) DO UPDATE SET
                    status=excluded.status,
                    detail=excluded.detail,
                    attempted_at=excluded.attempted_at
                """,
                (
                    attempt.provider,
                    attempt.session_id,
                    attempt.reset_at,
                    attempt.status,
                    attempt.detail,
                    attempted_at,
                ),
            )
        return ExpertRefreshAttempt(
            attempt.provider,
            attempt.session_id,
            attempt.reset_at,
            attempt.status,
            attempt.detail,
            attempted_at,
        )

    def get_expert_refresh_attempt(
        self, provider: str, session_id: str, reset_at: int
    ) -> ExpertRefreshAttempt | None:
        self.initialize()
        with self.connect() as db:
            row = db.execute(
                """
                SELECT * FROM expert_refresh_attempts
                WHERE provider=? AND session_id=? AND reset_at=?
                """,
                (provider, session_id, reset_at),
            ).fetchone()
        if not row:
            return None
        return ExpertRefreshAttempt(
            provider=str(row["provider"]),
            session_id=str(row["session_id"]),
            reset_at=int(row["reset_at"]),
            status=str(row["status"]),
            detail=str(row["detail"]) if row["detail"] is not None else None,
            attempted_at=float(row["attempted_at"]),
        )

    @staticmethod
    def _became_actionable(
        existing: sqlite3.Row | None,
        *,
        status: str,
        unread: bool,
        attention_reason: object,
        error: object,
    ) -> bool:
        if not unread or status not in {
            Status.NEEDS_YOU.value,
            Status.READY.value,
            Status.ERROR.value,
            Status.OPEN_TWICE.value,
        }:
            return False
        if existing is None:
            return True
        return not (
            bool(existing["unread"])
            and str(existing["status"]) == status
            and existing["attention_reason"] == attention_reason
            and existing["error"] == error
        )

    @staticmethod
    def _insert_session_event(
        db: sqlite3.Connection, session: Session, event_at: float
    ) -> None:
        db.execute(
            """
            INSERT OR IGNORE INTO session_events(
                provider,session_id,event_at,status,attention_reason,error
            ) VALUES (?,?,?,?,?,?)
            """,
            (
                session.provider,
                session.session_id,
                event_at,
                session.status,
                session.attention_reason,
                session.error,
            ),
        )

    def attention_event_counts(
        self, *, since: float, until: float | None = None
    ) -> dict[str, int]:
        self.initialize()
        end = time.time() if until is None else until
        with self.connect() as db:
            rows = db.execute(
                """
                SELECT status,COUNT(*) AS count
                FROM session_events
                WHERE event_at>? AND event_at<=?
                GROUP BY status
                """,
                (since, end),
            ).fetchall()
        return {str(row["status"]): int(row["count"]) for row in rows}

    def list_activity_events(self, *, limit: int = 20) -> list[ActivityEvent]:
        """Return a transcript-free catch-up feed, newest first."""
        if limit < 1 or limit > 500:
            raise ValueError("Activity event limit must be between 1 and 500")
        self.initialize()
        with self.connect() as db:
            rows = db.execute(
                """
                SELECT events.event_id,events.provider,events.session_id,
                       sessions.name,events.status,events.attention_reason,
                       events.error,events.event_at
                FROM session_events AS events
                LEFT JOIN sessions USING(provider,session_id)
                ORDER BY events.event_id DESC
                LIMIT ?
                """,
                (limit,),
            ).fetchall()
        return [
            ActivityEvent(
                event_id=int(row["event_id"]),
                provider=str(row["provider"]),
                session_id=str(row["session_id"]),
                name=str(row["name"]) if row["name"] is not None else None,
                status=str(row["status"]),
                attention_reason=(
                    str(row["attention_reason"])
                    if row["attention_reason"] is not None
                    else None
                ),
                error=str(row["error"]) if row["error"] is not None else None,
                event_at=float(row["event_at"]),
            )
            for row in rows
        ]

    def capture_identity_interruption(self, provider: str, session_id: str) -> None:
        """Remember the lifecycle state hidden by a temporary identity fault."""
        self.initialize()
        with self.connect() as db:
            db.execute(
                """
                INSERT OR IGNORE INTO identity_interruptions(
                    provider,session_id,status,unread,attention_reason,error,
                    last_event_at
                )
                SELECT provider,session_id,status,unread,attention_reason,error,
                       last_event_at
                FROM sessions
                WHERE provider=? AND session_id=?
                """,
                (provider, session_id),
            )
            db.execute(
                """
                INSERT OR IGNORE INTO session_status_observations(
                    provider,session_id,kind,status,unread,attention_reason,
                    error,observed_at,source
                )
                SELECT provider,session_id,'lifecycle',status,unread,
                       attention_reason,error,last_event_at,'identity-interruption'
                FROM sessions
                WHERE provider=? AND session_id=?
                """,
                (provider, session_id),
            )

    def get_identity_interruption(
        self, provider: str, session_id: str
    ) -> dict[str, object] | None:
        self.initialize()
        with self.connect() as db:
            row = db.execute(
                "SELECT * FROM identity_interruptions "
                "WHERE provider=? AND session_id=?",
                (provider, session_id),
            ).fetchone()
        if not row:
            return None
        return {
            "status": str(row["status"]),
            "unread": bool(row["unread"]),
            "attention_reason": row["attention_reason"],
            "error": row["error"],
            "last_event_at": float(row["last_event_at"]),
        }

    def clear_identity_interruption(self, provider: str, session_id: str) -> None:
        self.initialize()
        with self.connect() as db:
            db.execute(
                "DELETE FROM identity_interruptions WHERE provider=? AND session_id=?",
                (provider, session_id),
            )
            db.execute(
                "DELETE FROM session_status_observations "
                "WHERE provider=? AND session_id=? AND kind='safety'",
                (provider, session_id),
            )

    def restore_identity_interruption(
        self, provider: str, session_id: str, *, live: bool
    ) -> bool:
        """Atomically restore the lifecycle hidden by the same identity fault."""
        self.initialize()
        with self.connect() as db:
            db.execute("BEGIN IMMEDIATE")
            current = db.execute(
                "SELECT status,attention_reason FROM sessions "
                "WHERE provider=? AND session_id=?",
                (provider, session_id),
            ).fetchone()
            if not current:
                return False
            if not (
                str(current["status"]) in {Status.ERROR.value, Status.OPEN_TWICE.value}
                and current["attention_reason"] == "identity"
            ):
                # A provider lifecycle event already replaced the fault. It is
                # newer truth, so discard the now-redundant saved lifecycle.
                db.execute(
                    "DELETE FROM identity_interruptions "
                    "WHERE provider=? AND session_id=?",
                    (provider, session_id),
                )
                db.execute(
                    "DELETE FROM session_status_observations "
                    "WHERE provider=? AND session_id=? AND kind='safety'",
                    (provider, session_id),
                )
                return False
            interrupted = db.execute(
                "SELECT * FROM identity_interruptions "
                "WHERE provider=? AND session_id=?",
                (provider, session_id),
            ).fetchone()
            if interrupted:
                interrupted_status = str(interrupted["status"])
                if not live and interrupted_status in {
                    Status.WORKING.value,
                    Status.UNBOUND.value,
                }:
                    values = (
                        Status.PARKED.value,
                        0,
                        None,
                        None,
                        float(interrupted["last_event_at"]),
                    )
                else:
                    values = (
                        interrupted_status,
                        int(interrupted["unread"]),
                        interrupted["attention_reason"],
                        interrupted["error"],
                        float(interrupted["last_event_at"]),
                    )
            else:
                values = (
                    Status.WORKING.value if live else Status.PARKED.value,
                    0,
                    None,
                    None,
                    time.time(),
                )
            db.execute(
                """
                UPDATE sessions
                SET status=?,unread=?,attention_reason=?,error=?,last_event_at=?,
                    updated_at=?
                WHERE provider=? AND session_id=?
                  AND status IN (?,?) AND attention_reason='identity'
                """,
                (
                    *values,
                    time.time(),
                    provider,
                    session_id,
                    Status.ERROR.value,
                    Status.OPEN_TWICE.value,
                ),
            )
            db.execute(
                "DELETE FROM identity_interruptions WHERE provider=? AND session_id=?",
                (provider, session_id),
            )
            db.execute(
                "DELETE FROM session_status_observations "
                "WHERE provider=? AND session_id=? AND kind='safety'",
                (provider, session_id),
            )
            return True

    def discard_healthy_identity_interruption(
        self, provider: str, session_id: str
    ) -> None:
        """Drop stale saved state only while the current lifecycle is healthy."""
        self.initialize()
        with self.connect() as db:
            db.execute("BEGIN IMMEDIATE")
            db.execute(
                """
                DELETE FROM identity_interruptions
                WHERE provider=? AND session_id=?
                  AND EXISTS (
                    SELECT 1 FROM sessions
                    WHERE provider=? AND session_id=?
                      AND NOT (status IN (?,?) AND attention_reason='identity')
                  )
                """,
                (
                    provider,
                    session_id,
                    provider,
                    session_id,
                    Status.ERROR.value,
                    Status.OPEN_TWICE.value,
                ),
            )
            db.execute(
                """
                DELETE FROM session_status_observations
                WHERE provider=? AND session_id=? AND kind='safety'
                  AND EXISTS (
                    SELECT 1 FROM sessions
                    WHERE provider=? AND session_id=?
                      AND NOT (status IN (?,?) AND attention_reason='identity')
                  )
                """,
                (
                    provider,
                    session_id,
                    provider,
                    session_id,
                    Status.ERROR.value,
                    Status.OPEN_TWICE.value,
                ),
            )

    def add_pending(
        self,
        launch_token: str,
        provider: str,
        name: str,
        cwd: str,
        tmux_session: str | None = None,
        tmux_pane: str | None = None,
        expected_session_id: str | None = None,
        preexisting_session_ids: list[str] | None = None,
    ) -> bool:
        self.initialize()
        with self.connect() as db:
            db.execute("BEGIN IMMEDIATE")
            competing = db.execute(
                """
                SELECT launch_token FROM pending_launches
                WHERE provider=? AND name=? COLLATE NOCASE AND launch_token!=?
                LIMIT 1
                """,
                (provider, name, launch_token),
            ).fetchone()
            if competing:
                return False
            db.execute(
                """
                INSERT INTO pending_launches
                    (launch_token, provider, name, cwd, tmux_session, tmux_pane,
                     expected_session_id, preexisting_session_ids_json, created_at)
                VALUES (?,?,?,?,?,?,?,?,?)
                ON CONFLICT(launch_token) DO UPDATE SET
                    tmux_session=COALESCE(excluded.tmux_session, pending_launches.tmux_session),
                    tmux_pane=COALESCE(excluded.tmux_pane, pending_launches.tmux_pane),
                    expected_session_id=COALESCE(
                        excluded.expected_session_id,
                        pending_launches.expected_session_id
                    ),
                    preexisting_session_ids_json=COALESCE(
                        excluded.preexisting_session_ids_json,
                        pending_launches.preexisting_session_ids_json
                    )
                """,
                (
                    launch_token,
                    provider,
                    name,
                    cwd,
                    tmux_session,
                    tmux_pane,
                    expected_session_id,
                    (
                        json.dumps(sorted(set(preexisting_session_ids)))
                        if preexisting_session_ids is not None
                        else None
                    ),
                    time.time(),
                ),
            )
        return True

    def list_pending(self) -> list[dict[str, Any]]:
        self.initialize()
        with self.connect() as db:
            rows = db.execute(
                "SELECT * FROM pending_launches ORDER BY created_at"
            ).fetchall()
        return [dict(row) for row in rows]

    def observe_pending_candidate(
        self, launch_token: str, session_id: str
    ) -> float | None:
        """Record one stable recovery candidate; return its first-seen time."""
        self.initialize()
        now = time.time()
        with self.connect() as db:
            db.execute("BEGIN IMMEDIATE")
            row = db.execute(
                "SELECT candidate_session_id,candidate_observed_at "
                "FROM pending_launches WHERE launch_token=?",
                (launch_token,),
            ).fetchone()
            if row is None:
                return None
            if str(row["candidate_session_id"] or "") == session_id:
                return (
                    float(row["candidate_observed_at"])
                    if row["candidate_observed_at"] is not None
                    else None
                )
            db.execute(
                "UPDATE pending_launches "
                "SET candidate_session_id=?,candidate_observed_at=? "
                "WHERE launch_token=?",
                (session_id, now, launch_token),
            )
        return None

    def get_pending(self, launch_token: str) -> dict[str, Any] | None:
        self.initialize()
        with self.connect() as db:
            row = db.execute(
                "SELECT * FROM pending_launches WHERE launch_token=?", (launch_token,)
            ).fetchone()
        return dict(row) if row else None

    def find_pending_for_pane(self, pane_id: str) -> dict[str, Any] | None:
        self.initialize()
        with self.connect() as db:
            row = db.execute(
                "SELECT * FROM pending_launches WHERE tmux_pane=? ORDER BY created_at DESC LIMIT 1",
                (pane_id,),
            ).fetchone()
        return dict(row) if row else None

    def finalize_pending_pane(
        self,
        launch_token: str,
        tmux_session: str,
        tmux_pane: str,
        root_pid: int | None = None,
        root_pid_start: int | None = None,
    ) -> tuple[str, str] | None:
        """Record a created pane without resurrecting an already-bound launch."""
        self.initialize()
        with self.connect() as db:
            db.execute("BEGIN IMMEDIATE")
            binding = db.execute(
                "SELECT provider, session_id FROM launch_bindings WHERE launch_token=?",
                (launch_token,),
            ).fetchone()
            if binding:
                db.execute(
                    """
                    UPDATE pending_launches
                    SET tmux_session=?, tmux_pane=?, root_pid=?, root_pid_start=?
                    WHERE launch_token=?
                    """,
                    (
                        tmux_session,
                        tmux_pane,
                        root_pid,
                        root_pid_start,
                        launch_token,
                    ),
                )
                return str(binding["provider"]), str(binding["session_id"])
            db.execute(
                """
                UPDATE pending_launches
                SET tmux_session=?, tmux_pane=?, root_pid=?, root_pid_start=?
                WHERE launch_token=?
                """,
                (tmux_session, tmux_pane, root_pid, root_pid_start, launch_token),
            )
        return None

    def delete_pending(self, launch_token: str) -> None:
        self.initialize()
        with self.connect() as db:
            db.execute(
                "DELETE FROM pending_launches WHERE launch_token=?", (launch_token,)
            )

    def prune_pending(self, older_than_seconds: float = 300) -> int:
        self.initialize()
        with self.connect() as db:
            cur = db.execute(
                "DELETE FROM pending_launches WHERE created_at < ?",
                (time.time() - older_than_seconds,),
            )
            db.execute(
                "DELETE FROM launch_bindings WHERE created_at < ?",
                (time.time() - 7 * 24 * 60 * 60,),
            )
            return cur.rowcount

    def bind_launch(self, launch_token: str, provider: str, session_id: str) -> bool:
        """Claim a launch once; a competing UUID can never overwrite it."""
        self.initialize()
        with self.connect() as db:
            db.execute("BEGIN IMMEDIATE")
            existing = db.execute(
                "SELECT provider,session_id FROM launch_bindings WHERE launch_token=?",
                (launch_token,),
            ).fetchone()
            if existing:
                matches = (
                    str(existing["provider"]),
                    str(existing["session_id"]),
                ) == (
                    provider,
                    session_id,
                )
                if not matches:
                    return False
            else:
                db.execute(
                    """
                    INSERT INTO launch_bindings(
                        launch_token, provider, session_id, created_at
                    ) VALUES (?,?,?,?)
                    """,
                    (launch_token, provider, session_id, time.time()),
                )
        return True

    def certify_launch(
        self,
        launch_token: str,
        provider: str,
        session_id: str,
        pid: int,
        start_time: int,
    ) -> bool:
        """Publish exact process ownership only after its pane is tagged.

        Binding a provider UUID and proving its physical Pika home are separate
        commits.  Keeping this second commit explicit prevents a failed tmux tag
        from publishing exact identity and handles either hook/finalizer order.
        """
        self.initialize()
        with self.connect() as db:
            db.execute("BEGIN IMMEDIATE")
            binding = db.execute(
                "SELECT provider,session_id FROM launch_bindings WHERE launch_token=?",
                (launch_token,),
            ).fetchone()
            if binding is None or (
                str(binding["provider"]), str(binding["session_id"])
            ) != (provider, session_id):
                return False
            pending = db.execute(
                "SELECT provider,root_pid,root_pid_start FROM pending_launches "
                "WHERE launch_token=?",
                (launch_token,),
            ).fetchone()
            if pending is not None and (
                str(pending["provider"]) != provider
                or pending["root_pid"] is None
                or pending["root_pid_start"] is None
                or int(pending["root_pid"]) != pid
                or int(pending["root_pid_start"]) != start_time
            ):
                return False
            db.execute(
                """
                INSERT INTO recovery_owners(
                    provider,session_id,pid,start_time,launch_token,created_at
                ) VALUES (?,?,?,?,?,?)
                ON CONFLICT(provider,session_id) DO UPDATE SET
                    pid=excluded.pid,
                    start_time=excluded.start_time,
                    launch_token=excluded.launch_token,
                    created_at=excluded.created_at
                """,
                (provider, session_id, pid, start_time, launch_token, time.time()),
            )
            db.execute(
                "DELETE FROM pending_launches WHERE launch_token=?", (launch_token,)
            )
        return True

    def get_launch_binding(self, launch_token: str) -> tuple[str, str] | None:
        self.initialize()
        with self.connect() as db:
            row = db.execute(
                "SELECT provider, session_id FROM launch_bindings WHERE launch_token=?",
                (launch_token,),
            ).fetchone()
        if not row:
            return None
        return str(row["provider"]), str(row["session_id"])

    def switch_launch_binding(
        self,
        launch_token: str,
        provider: str,
        from_session_id: str,
        to_session_id: str,
        pid: int,
    ) -> bool:
        """Move one certified managed client to the root it now displays."""
        self.initialize()
        start_time = process_start_time(pid)
        if start_time is None:
            return False
        with self.connect() as db:
            db.execute("BEGIN IMMEDIATE")
            binding = db.execute(
                "SELECT provider,session_id FROM launch_bindings WHERE launch_token=?",
                (launch_token,),
            ).fetchone()
            proof = db.execute(
                "SELECT pid,start_time,launch_token FROM recovery_owners "
                "WHERE provider=? AND session_id=?",
                (provider, from_session_id),
            ).fetchone()
            target_proof = db.execute(
                "SELECT pid,start_time,launch_token FROM recovery_owners "
                "WHERE provider=? AND session_id=?",
                (provider, to_session_id),
            ).fetchone()
            target_leases = db.execute(
                "SELECT pid,start_time,last_seen FROM live_owners "
                "WHERE provider=? AND session_id=?",
                (provider, to_session_id),
            ).fetchall()
            competing_target = bool(
                target_proof is not None
                and (
                    int(target_proof["pid"]),
                    int(target_proof["start_time"]),
                    str(target_proof["launch_token"]),
                )
                != (pid, start_time, launch_token)
                and process_start_time(int(target_proof["pid"]))
                == int(target_proof["start_time"])
            )
            competing_target_lease = any(
                lease["start_time"] is not None
                and (int(lease["pid"]), int(lease["start_time"]))
                != (pid, start_time)
                and process_start_time(int(lease["pid"]))
                == int(lease["start_time"])
                and time.time() - float(lease["last_seen"])
                <= LIVE_OWNER_LEASE_SECONDS
                for lease in target_leases
            )
            if (
                binding is None
                or (str(binding["provider"]), str(binding["session_id"]))
                != (provider, from_session_id)
                or proof is None
                or int(proof["pid"]) != pid
                or int(proof["start_time"]) != start_time
                or str(proof["launch_token"]) != launch_token
                or competing_target
                or competing_target_lease
            ):
                return False
            db.execute(
                "UPDATE launch_bindings SET session_id=?,created_at=? "
                "WHERE launch_token=? AND provider=? AND session_id=?",
                (
                    to_session_id,
                    time.time(),
                    launch_token,
                    provider,
                    from_session_id,
                ),
            )
            db.execute(
                "DELETE FROM recovery_owners WHERE provider=? AND session_id=?",
                (provider, from_session_id),
            )
            db.execute(
                """
                INSERT INTO recovery_owners(
                    provider,session_id,pid,start_time,launch_token,created_at
                ) VALUES (?,?,?,?,?,?)
                ON CONFLICT(provider,session_id) DO UPDATE SET
                    pid=excluded.pid,
                    start_time=excluded.start_time,
                    launch_token=excluded.launch_token,
                    created_at=excluded.created_at
                """,
                (
                    provider,
                    to_session_id,
                    pid,
                    start_time,
                    launch_token,
                    time.time(),
                ),
            )
        return True

    def delete_launch_binding(self, launch_token: str) -> None:
        self.initialize()
        with self.connect() as db:
            db.execute(
                "DELETE FROM launch_bindings WHERE launch_token=?", (launch_token,)
            )

    def set_live_owner(
        self,
        provider: str,
        session_id: str,
        pid: int,
        *,
        owner_token: str | None = None,
    ) -> bool:
        """Renew a hook-owner lease using non-recyclable process identity."""
        self.initialize()
        start_time = process_start_time(pid)
        if start_time is None:
            return False
        claim = owner_token or ""
        with self.connect() as db:
            if db.execute(
                "SELECT 1 FROM untracked_sessions WHERE provider=? AND session_id=?",
                (provider, session_id),
            ).fetchone():
                return False
            db.execute(
                """
                INSERT INTO live_owners(
                    provider,session_id,pid,start_time,owner_token,last_seen
                ) VALUES (?,?,?,?,?,?)
                ON CONFLICT(provider,session_id,pid,owner_token) DO UPDATE SET
                    start_time=excluded.start_time,
                    last_seen=excluded.last_seen
                """,
                (provider, session_id, pid, start_time, claim, time.time()),
            )
        return True

    def get_live_owner_leases(
        self, provider: str, session_id: str
    ) -> list[tuple[int, int | None, float, str]]:
        """Return hook claims with timestamps required for lease validation."""
        self.initialize()
        with self.connect() as db:
            rows = db.execute(
                "SELECT pid,start_time,last_seen,owner_token FROM live_owners "
                "WHERE provider=? AND session_id=? ORDER BY pid,owner_token",
                (provider, session_id),
            ).fetchall()
        return [
            (
                int(row["pid"]),
                int(row["start_time"]) if row["start_time"] is not None else None,
                float(row["last_seen"]),
                str(row["owner_token"]),
            )
            for row in rows
        ]

    def get_live_owners(
        self, provider: str, session_id: str
    ) -> list[tuple[int, int | None]]:
        owners: dict[int, int | None] = {}
        for pid, start_time, _last_seen, _owner_token in self.get_live_owner_leases(
            provider, session_id
        ):
            if pid not in owners or owners[pid] is None:
                owners[pid] = start_time
        return sorted(owners.items())

    def list_live_owners(
        self,
    ) -> list[tuple[str, str, int, int | None, float, str]]:
        self.initialize()
        with self.connect() as db:
            rows = db.execute(
                "SELECT provider,session_id,pid,start_time,last_seen,owner_token "
                "FROM live_owners ORDER BY provider,session_id,pid,owner_token"
            ).fetchall()
        return [
            (
                str(row["provider"]),
                str(row["session_id"]),
                int(row["pid"]),
                int(row["start_time"]) if row["start_time"] is not None else None,
                float(row["last_seen"]),
                str(row["owner_token"]),
            )
            for row in rows
        ]

    def set_recovery_owner(
        self,
        provider: str,
        session_id: str,
        pid: int,
        start_time: int,
        launch_token: str,
    ) -> None:
        """Persist command-recovery provenance separately from hook leases."""
        self.initialize()
        with self.connect() as db:
            db.execute(
                """
                INSERT INTO recovery_owners(
                    provider,session_id,pid,start_time,launch_token,created_at
                ) VALUES (?,?,?,?,?,?)
                ON CONFLICT(provider,session_id) DO UPDATE SET
                    pid=excluded.pid,
                    start_time=excluded.start_time,
                    launch_token=excluded.launch_token,
                    created_at=excluded.created_at
                """,
                (provider, session_id, pid, start_time, launch_token, time.time()),
            )

    def get_recovery_owner(
        self, provider: str, session_id: str
    ) -> tuple[int, int, str] | None:
        self.initialize()
        with self.connect() as db:
            row = db.execute(
                "SELECT pid,start_time,launch_token FROM recovery_owners "
                "WHERE provider=? AND session_id=?",
                (provider, session_id),
            ).fetchone()
        if row is None:
            return None
        return int(row["pid"]), int(row["start_time"]), str(row["launch_token"])

    def delete_recovery_owner(self, provider: str, session_id: str) -> None:
        self.initialize()
        with self.connect() as db:
            db.execute(
                "DELETE FROM recovery_owners WHERE provider=? AND session_id=?",
                (provider, session_id),
            )

    def delete_live_owner(
        self,
        provider: str,
        session_id: str,
        pid: int | None = None,
        *,
        owner_token: str | None = None,
    ) -> None:
        self.initialize()
        with self.connect() as db:
            if pid is None and owner_token is None:
                db.execute(
                    "DELETE FROM live_owners WHERE provider=? AND session_id=?",
                    (provider, session_id),
                )
            elif pid is not None and owner_token is None:
                db.execute(
                    "DELETE FROM live_owners WHERE provider=? AND session_id=? AND pid=?",
                    (provider, session_id, pid),
                )
            elif pid is None:
                db.execute(
                    "DELETE FROM live_owners WHERE provider=? AND session_id=? "
                    "AND owner_token=?",
                    (provider, session_id, owner_token),
                )
            else:
                db.execute(
                    "DELETE FROM live_owners WHERE provider=? AND session_id=? "
                    "AND pid=? AND owner_token=?",
                    (provider, session_id, pid, owner_token),
                )

    def delete_other_live_owner_sessions(
        self, provider: str, pid: int, keep_session_id: str
    ) -> None:
        """Revoke prior roots when a single-session provider switches identity."""
        self.initialize()
        with self.connect() as db:
            db.execute(
                "DELETE FROM live_owners WHERE provider=? AND pid=? AND session_id<>?",
                (provider, pid, keep_session_id),
            )

    def reserve_resume(
        self,
        provider: str,
        session_id: str,
        token: str,
    ) -> bool:
        """Atomically reserve an identity while a tmux process is being started."""
        self.initialize()
        owner_pid = os.getpid()
        owner_start_time = process_start_time(owner_pid)
        if owner_start_time is None:
            return False
        with self.connect() as db:
            db.execute("BEGIN IMMEDIATE")
            existing = db.execute(
                "SELECT * FROM launch_reservations WHERE provider=? AND session_id=?",
                (provider, session_id),
            ).fetchone()
            if existing:
                existing_pid = existing["owner_pid"]
                existing_start = existing["owner_start_time"]
                owner_alive = bool(
                    existing_pid
                    and existing_start is not None
                    and process_start_time(int(existing_pid)) == int(existing_start)
                )
                # Process identity, not age, proves whether the lock is stale.
                # Legacy rows without owner evidence remain fail-closed.
                if owner_alive or existing_pid is None or existing_start is None:
                    return False
                db.execute(
                    "DELETE FROM launch_reservations WHERE provider=? AND session_id=?",
                    (provider, session_id),
                )
            try:
                db.execute(
                    """
                    INSERT INTO launch_reservations(
                        provider, session_id, token, owner_pid,
                        owner_start_time, created_at
                    ) VALUES (?,?,?,?,?,?)
                    """,
                    (
                        provider,
                        session_id,
                        token,
                        owner_pid,
                        owner_start_time,
                        time.time(),
                    ),
                )
            except sqlite3.IntegrityError:
                return False
        return True

    def release_resume(self, provider: str, session_id: str, token: str) -> None:
        self.initialize()
        with self.connect() as db:
            db.execute(
                """
                DELETE FROM launch_reservations
                WHERE provider=? AND session_id=? AND token=?
                """,
                (provider, session_id, token),
            )

    def local_node_id(self) -> str:
        """Return this installation's stable identity without exposing host secrets."""
        self.initialize()
        proposed = str(uuid.uuid4())
        with self.connect() as db:
            db.execute("BEGIN IMMEDIATE")
            db.execute(
                "INSERT OR IGNORE INTO meta(key,value) VALUES ('fleet:node_id',?)",
                (proposed,),
            )
            row = db.execute(
                "SELECT value FROM meta WHERE key='fleet:node_id'"
            ).fetchone()
        assert row is not None
        try:
            return str(uuid.UUID(str(row["value"])))
        except ValueError:
            raise ValueError("Pika's stored fleet node UUID is invalid") from None

    def upsert_fleet_node(self, node: FleetNode) -> FleetNode:
        self.initialize()
        now = time.time()
        created_at = node.created_at or now
        updated_at = node.updated_at or now
        with self.connect() as db:
            existing = db.execute(
                "SELECT last_seen,last_attempt_at,created_at FROM fleet_nodes "
                "WHERE node_id=?",
                (node.node_id,),
            ).fetchone()
            if existing:
                created_at = node.created_at or float(existing["created_at"])
            last_seen = node.last_seen or (
                float(existing["last_seen"]) if existing else 0.0
            )
            last_attempt_at = node.last_attempt_at or (
                float(existing["last_attempt_at"]) if existing else updated_at
            )
            collision = db.execute(
                "SELECT node_id FROM fleet_nodes WHERE alias=? COLLATE NOCASE",
                (node.alias,),
            ).fetchone()
            if collision and str(collision["node_id"]) != node.node_id:
                raise ValueError(
                    f"Machine alias {node.alias!r} already belongs to another node"
                )
            db.execute(
                """
                INSERT INTO fleet_nodes(
                    node_id,alias,ssh_target,sources_json,status,protocol_version,
                    package_version,capabilities_json,last_seen,last_attempt_at,last_error,
                    created_at,updated_at
                ) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?)
                ON CONFLICT(node_id) DO UPDATE SET
                    alias=excluded.alias,
                    ssh_target=excluded.ssh_target,
                    sources_json=excluded.sources_json,
                    status=excluded.status,
                    protocol_version=excluded.protocol_version,
                    package_version=excluded.package_version,
                    capabilities_json=excluded.capabilities_json,
                    last_seen=excluded.last_seen,
                    last_attempt_at=excluded.last_attempt_at,
                    last_error=excluded.last_error,
                    updated_at=excluded.updated_at
                """,
                (
                    node.node_id,
                    node.alias,
                    node.ssh_target,
                    json.dumps(node.sources, ensure_ascii=False),
                    node.status,
                    node.protocol_version,
                    node.package_version,
                    json.dumps(node.capabilities, ensure_ascii=False),
                    last_seen,
                    last_attempt_at,
                    node.last_error,
                    created_at,
                    updated_at,
                ),
            )
        return FleetNode(
            node_id=node.node_id,
            alias=node.alias,
            ssh_target=node.ssh_target,
            sources=node.sources,
            status=node.status,
            protocol_version=node.protocol_version,
            package_version=node.package_version,
            capabilities=node.capabilities,
            last_seen=last_seen,
            last_attempt_at=last_attempt_at,
            last_error=node.last_error,
            created_at=created_at,
            updated_at=updated_at,
        )

    def list_fleet_nodes(self) -> list[FleetNode]:
        self.initialize()
        with self.connect() as db:
            rows = db.execute(
                "SELECT * FROM fleet_nodes ORDER BY alias COLLATE NOCASE"
            ).fetchall()
        return [self._row_to_fleet_node(row) for row in rows]

    def get_fleet_node(self, value: str) -> FleetNode | None:
        self.initialize()
        with self.connect() as db:
            rows = db.execute(
                "SELECT * FROM fleet_nodes WHERE node_id=? OR alias=? COLLATE NOCASE",
                (value, value),
            ).fetchall()
        if len(rows) > 1:
            raise ValueError(f"Ambiguous machine identity {value!r}")
        return self._row_to_fleet_node(rows[0]) if rows else None

    def delete_fleet_node(self, node_id: str) -> bool:
        self.initialize()
        with self.connect() as db:
            db.execute("BEGIN IMMEDIATE")
            db.execute("DELETE FROM remote_snapshots WHERE node_id=?", (node_id,))
            db.execute(
                "DELETE FROM meta WHERE key LIKE ? OR key LIKE ?",
                (
                    f"fleet:pending-adopt:{node_id}:%",
                    f"fleet:pending-untrack:{node_id}:%",
                ),
            )
            cursor = db.execute("DELETE FROM fleet_nodes WHERE node_id=?", (node_id,))
        return cursor.rowcount == 1

    def mark_fleet_node_error(self, node_id: str, status: str, error: str) -> None:
        allowed = {"unreachable", "auth", "incompatible", "quarantined", "error"}
        if status not in allowed:
            raise ValueError(f"Unsupported fleet node status: {status}")
        self.initialize()
        with self.connect() as db:
            db.execute(
                "UPDATE fleet_nodes SET status=?,last_error=?,last_attempt_at=?,updated_at=? "
                "WHERE node_id=?",
                (status, error, time.time(), time.time(), node_id),
            )

    def put_remote_snapshot(
        self, node_id: str, payload: dict[str, Any], *, captured_at: float | None = None
    ) -> None:
        self.initialize()
        captured_at = captured_at or time.time()
        encoded = json.dumps(payload, ensure_ascii=False, separators=(",", ":"))
        with self.connect() as db:
            db.execute("BEGIN IMMEDIATE")
            if not db.execute(
                "SELECT 1 FROM fleet_nodes WHERE node_id=?", (node_id,)
            ).fetchone():
                raise ValueError("Remote snapshot requires an adopted fleet node")
            db.execute(
                """
                INSERT INTO remote_snapshots(node_id,payload_json,captured_at)
                VALUES (?,?,?) ON CONFLICT(node_id) DO UPDATE SET
                    payload_json=excluded.payload_json,
                    captured_at=excluded.captured_at
                """,
                (node_id, encoded, captured_at),
            )
            db.execute(
                "UPDATE fleet_nodes SET status='ready',last_seen=?,last_error=NULL,"
                "last_attempt_at=?,updated_at=? WHERE node_id=?",
                (captured_at, captured_at, time.time(), node_id),
            )

    def get_remote_snapshot(self, node_id: str) -> tuple[dict[str, Any], float] | None:
        self.initialize()
        with self.connect() as db:
            row = db.execute(
                "SELECT payload_json,captured_at FROM remote_snapshots WHERE node_id=?",
                (node_id,),
            ).fetchone()
        if not row:
            return None
        payload = json.loads(str(row["payload_json"]))
        if not isinstance(payload, dict):
            raise ValueError("Stored remote snapshot is not an object")
        return payload, float(row["captured_at"])

    def ignore_node_candidate(self, candidate_key: str) -> None:
        self.initialize()
        with self.connect() as db:
            db.execute(
                "INSERT INTO ignored_node_candidates(candidate_key,ignored_at) "
                "VALUES (?,?) ON CONFLICT(candidate_key) DO UPDATE SET "
                "ignored_at=excluded.ignored_at",
                (candidate_key.casefold(), time.time()),
            )

    def ignored_node_candidate_keys(self) -> set[str]:
        self.initialize()
        with self.connect() as db:
            rows = db.execute(
                "SELECT candidate_key FROM ignored_node_candidates"
            ).fetchall()
        return {str(row["candidate_key"]) for row in rows}

    def set_meta(self, key: str, value: str) -> None:
        self.initialize()
        with self.connect() as db:
            db.execute(
                "INSERT INTO meta(key,value) VALUES (?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                (key, value),
            )

    def record_hook_observation(
        self,
        provider: str,
        fingerprint: str,
        event_name: str,
        session_id: str,
        *,
        source: str | None = None,
        managed: bool = False,
        observed_at: float | None = None,
    ) -> None:
        self.initialize()
        with self.connect() as db:
            db.execute(
                """
                INSERT INTO hook_observations(
                    provider,fingerprint,event_name,session_id,observed_at,source,managed
                ) VALUES (?,?,?,?,?,?,?)
                ON CONFLICT(provider) DO UPDATE SET
                    fingerprint=excluded.fingerprint,
                    event_name=excluded.event_name,
                    session_id=excluded.session_id,
                    observed_at=excluded.observed_at,
                    source=excluded.source,
                    managed=excluded.managed
                """,
                (
                    provider,
                    fingerprint,
                    event_name,
                    session_id,
                    observed_at if observed_at is not None else time.time(),
                    source,
                    int(managed),
                ),
            )

    def get_hook_observation(self, provider: str) -> dict[str, Any] | None:
        self.initialize()
        with self.connect() as db:
            row = db.execute(
                "SELECT * FROM hook_observations WHERE provider=?", (provider,)
            ).fetchone()
        return dict(row) if row else None

    def get_meta(self, key: str) -> str | None:
        self.initialize()
        with self.connect() as db:
            row = db.execute("SELECT value FROM meta WHERE key=?", (key,)).fetchone()
        return row["value"] if row else None

    def claim_monitor_visit(self, timestamp: float) -> float | None:
        """Atomically claim a successful monitor visit and return its predecessor."""
        previous, _counts = self.claim_monitor_handoff(timestamp)
        return previous

    def claim_monitor_handoff(
        self, timestamp: float
    ) -> tuple[float | None, dict[str, int]]:
        """Claim one committed ledger watermark and summarize its predecessor."""
        self.initialize()
        with self.connect() as db:
            db.execute("BEGIN IMMEDIATE")
            seen_row = db.execute(
                "SELECT value FROM meta WHERE key=?", ("monitor:last_seen_at",)
            ).fetchone()
            cursor_row = db.execute(
                "SELECT value FROM meta WHERE key=?",
                ("monitor:last_event_id",),
            ).fetchone()
            try:
                previous_cursor = int(cursor_row["value"]) if cursor_row else 0
            except (TypeError, ValueError):
                previous_cursor = 0
            current_row = db.execute(
                "SELECT COALESCE(MAX(event_id),0) AS event_id FROM session_events"
            ).fetchone()
            current_cursor = int(current_row["event_id"])
            rows = db.execute(
                """
                SELECT status,COUNT(*) AS count
                FROM session_events
                WHERE event_id>? AND event_id<=?
                GROUP BY status
                """,
                (previous_cursor, current_cursor),
            ).fetchall()
            db.execute(
                """
                INSERT INTO meta(key,value) VALUES (?,?)
                ON CONFLICT(key) DO UPDATE SET value=excluded.value
                """,
                ("monitor:last_seen_at", str(timestamp)),
            )
            db.execute(
                """
                INSERT INTO meta(key,value) VALUES (?,?)
                ON CONFLICT(key) DO UPDATE SET value=excluded.value
                """,
                ("monitor:last_event_id", str(current_cursor)),
            )
        counts = {str(row["status"]): int(row["count"]) for row in rows}
        if not seen_row:
            return None, counts
        try:
            return float(seen_row["value"]), counts
        except (TypeError, ValueError):
            return None, counts

    def collect_result(
        self,
        provider: str,
        session_id: str,
        *,
        expected_event_at: float,
    ) -> int | None:
        """Collect one exact READY event and count remaining results atomically."""
        self.initialize()
        with self.connect() as db:
            db.execute("BEGIN IMMEDIATE")
            cursor = db.execute(
                """
                UPDATE sessions
                SET unread=0, updated_at=?
                WHERE provider=? AND session_id=?
                  AND status=? AND unread=1 AND last_event_at=?
                """,
                (
                    time.time(),
                    provider,
                    session_id,
                    Status.READY.value,
                    expected_event_at,
                ),
            )
            if cursor.rowcount != 1:
                return None
            db.execute(
                """
                UPDATE session_status_observations
                SET unread=0
                WHERE provider=? AND session_id=? AND status=? AND observed_at=?
                """,
                (
                    provider,
                    session_id,
                    Status.READY.value,
                    expected_event_at,
                ),
            )
            row = db.execute(
                "SELECT COUNT(*) AS count FROM sessions WHERE status=? AND unread=1",
                (Status.READY.value,),
            ).fetchone()
            return int(row["count"])

    def delete_meta(self, key: str) -> None:
        self.initialize()
        with self.connect() as db:
            db.execute("DELETE FROM meta WHERE key=?", (key,))

    def record_attach(self, provider: str, session_id: str) -> None:
        current = json.dumps([provider, session_id])
        previous = self.get_meta("last_attached")
        if previous and previous != current:
            self.set_meta("previous_attached", previous)
        self.set_meta("last_attached", current)

    def acknowledge_attention(
        self,
        provider: str,
        session_id: str,
        *,
        expected_event_at: float,
        attaching: bool = False,
    ) -> bool:
        """Clear unread only if no newer lifecycle event replaced the view."""
        self.initialize()
        statuses = ["READY"]
        if attaching:
            statuses.append("ERROR")
        placeholders = ",".join("?" for _ in statuses)
        with self.connect() as db:
            db.execute("BEGIN IMMEDIATE")
            current = db.execute(
                """
                SELECT status, unread, last_event_at
                FROM sessions WHERE provider=? AND session_id=?
                """,
                (provider, session_id),
            ).fetchone()
            if (
                not current
                or not current["unread"]
                or str(current["status"]) not in statuses
                or float(current["last_event_at"]) != expected_event_at
            ):
                return False
            cursor = db.execute(
                f"""
                UPDATE sessions
                SET unread=0, updated_at=?
                WHERE provider=? AND session_id=?
                  AND unread=1
                  AND last_event_at=?
                  AND status IN ({placeholders})
                """,
                (
                    time.time(),
                    provider,
                    session_id,
                    expected_event_at,
                    *statuses,
                ),
            )
            if cursor.rowcount == 1:
                db.execute(
                    f"""
                    UPDATE session_status_observations
                    SET unread=0
                    WHERE provider=? AND session_id=? AND unread=1
                      AND observed_at=? AND status IN ({placeholders})
                    """,
                    (
                        provider,
                        session_id,
                        expected_event_at,
                        *statuses,
                    ),
                )
            return cursor.rowcount == 1

    def previous_attached(self) -> tuple[str, str] | None:
        value = self.get_meta("previous_attached")
        if not value:
            return None
        try:
            provider, session_id = json.loads(value)
            return str(provider), str(session_id)
        except (ValueError, TypeError):
            return None

    def get_cached_usage(
        self, provider: str, session_id: str, path: Path
    ) -> Usage | None:
        try:
            stat = path.stat()
        except OSError:
            return None
        self.initialize()
        with self.connect() as db:
            row = db.execute(
                "SELECT * FROM usage_cache WHERE provider=? AND session_id=?",
                (provider, session_id),
            ).fetchone()
        if not row or row["source_path"] != str(path):
            return None
        if (
            row["source_mtime_ns"] != stat.st_mtime_ns
            or row["source_size"] != stat.st_size
        ):
            return None
        return Usage(
            model=row["model"],
            input_tokens=row["input_tokens"],
            output_tokens=row["output_tokens"],
            cached_input_tokens=row["cached_input_tokens"],
            cache_write_tokens=row["cache_write_tokens"],
            total_tokens=row["total_tokens"],
            estimated_cost_usd=row["estimated_cost_usd"],
        )

    def put_cached_usage(
        self, provider: str, session_id: str, path: Path, usage: Usage
    ) -> None:
        try:
            stat = path.stat()
        except OSError:
            return
        self.initialize()
        with self.connect() as db:
            db.execute(
                """
                INSERT INTO usage_cache (
                    provider,session_id,source_path,source_mtime_ns,source_size,model,
                    input_tokens,output_tokens,cached_input_tokens,cache_write_tokens,
                    total_tokens,estimated_cost_usd,updated_at
                ) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?)
                ON CONFLICT(provider,session_id) DO UPDATE SET
                    source_path=excluded.source_path,
                    source_mtime_ns=excluded.source_mtime_ns,
                    source_size=excluded.source_size,
                    model=excluded.model,
                    input_tokens=excluded.input_tokens,
                    output_tokens=excluded.output_tokens,
                    cached_input_tokens=excluded.cached_input_tokens,
                    cache_write_tokens=excluded.cache_write_tokens,
                    total_tokens=excluded.total_tokens,
                    estimated_cost_usd=excluded.estimated_cost_usd,
                    updated_at=excluded.updated_at
                """,
                (
                    provider,
                    session_id,
                    str(path),
                    stat.st_mtime_ns,
                    stat.st_size,
                    usage.model,
                    usage.input_tokens,
                    usage.output_tokens,
                    usage.cached_input_tokens,
                    usage.cache_write_tokens,
                    usage.total_tokens,
                    usage.estimated_cost_usd,
                    time.time(),
                ),
            )

    @staticmethod
    def _row_to_session(row: sqlite3.Row) -> Session:
        return Session(
            provider=row["provider"],
            session_id=row["session_id"],
            active_thread_id=row["active_thread_id"],
            name=row["name"],
            cwd=row["cwd"],
            branch=row["branch"],
            transcript_path=row["transcript_path"],
            tmux_session=row["tmux_session"],
            tmux_pane=row["tmux_pane"],
            root_pid=row["root_pid"],
            status=row["status"],
            unread=bool(row["unread"]),
            model=row["model"],
            source=row["source"],
            managed=bool(row["managed"]),
            error=row["error"],
            attention_reason=row["attention_reason"],
            created_at=row["created_at"],
            updated_at=row["updated_at"],
            last_event_at=row["last_event_at"],
            last_activity_at=row["last_activity_at"],
        )

    @staticmethod
    def _row_to_expert_profile(row: sqlite3.Row) -> ExpertProfile:
        topics = json.loads(row["topics_json"])
        artifacts = json.loads(row["artifacts_json"])
        if not isinstance(topics, list) or not all(
            isinstance(item, str) for item in topics
        ):
            raise ValueError("Invalid expert profile topics")
        if not isinstance(artifacts, list) or not all(
            isinstance(item, str) for item in artifacts
        ):
            raise ValueError("Invalid expert profile artifacts")
        return ExpertProfile(
            provider=str(row["provider"]),
            session_id=str(row["session_id"]),
            summary=str(row["summary"]),
            topics=tuple(topics),
            artifacts=tuple(artifacts),
            updated_at=float(row["updated_at"]),
            source=str(row["source"]),
            transcript_mtime_ns=(
                int(row["transcript_mtime_ns"])
                if row["transcript_mtime_ns"] is not None
                else None
            ),
            transcript_size=(
                int(row["transcript_size"])
                if row["transcript_size"] is not None
                else None
            ),
            current_state=str(row["current_state"]),
            scope_updated_at=float(row["scope_updated_at"]),
            current_state_updated_at=float(row["current_state_updated_at"]),
            current_state_mtime_ns=row["current_state_mtime_ns"],
            current_state_size=row["current_state_size"],
        )

    @staticmethod
    def _row_to_fleet_node(row: sqlite3.Row) -> FleetNode:
        return FleetNode(
            node_id=str(row["node_id"]),
            alias=str(row["alias"]),
            ssh_target=str(row["ssh_target"]),
            sources=tuple(json.loads(str(row["sources_json"]))),
            status=str(row["status"]),
            protocol_version=(
                int(row["protocol_version"])
                if row["protocol_version"] is not None
                else None
            ),
            package_version=(
                str(row["package_version"])
                if row["package_version"] is not None
                else None
            ),
            capabilities=tuple(json.loads(str(row["capabilities_json"]))),
            last_seen=float(row["last_seen"]),
            last_attempt_at=float(row["last_attempt_at"]),
            last_error=(
                str(row["last_error"]) if row["last_error"] is not None else None
            ),
            created_at=float(row["created_at"]),
            updated_at=float(row["updated_at"]),
        )


def load_config(path: Path | None = None) -> dict[str, Any]:
    target = path or config_path()
    data = dict(DEFAULT_CONFIG)
    try:
        parsed = json.loads(target.read_text())
        if isinstance(parsed, dict):
            data.update(parsed)
    except (OSError, ValueError):
        pass
    return data


def write_config(config: dict[str, Any], path: Path | None = None) -> None:
    target = path or config_path()
    target.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    try:
        os.chmod(target.parent, 0o700)
    except OSError:
        pass
    temp = target.with_name(f".{target.name}.{os.getpid()}.tmp")
    temp.write_text(json.dumps(config, indent=2, sort_keys=True) + "\n")
    os.chmod(temp, 0o600)
    os.replace(temp, target)
