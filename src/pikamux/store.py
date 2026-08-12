from __future__ import annotations

import json
import os
import sqlite3
import time
from collections.abc import Iterator
from contextlib import contextmanager
from pathlib import Path
from typing import Any

from .models import Session, Usage
from .paths import config_path, database_path
from .processes import process_start_time

DEFAULT_CONFIG: dict[str, Any] = {
    "version": 1,
    "default_provider": "codex",
    "alerts": "tmux",
    "peek_lines": 200,
}


class Store:
    def __init__(self, path: Path | None = None):
        self.path = path or database_path()
        self._initialized = False

    def initialize(self) -> None:
        if self._initialized and self.path.exists():
            return
        self.path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
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
                    last_seen REAL NOT NULL,
                    PRIMARY KEY (provider, session_id, pid)
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
                        last_seen REAL NOT NULL,
                        PRIMARY KEY (provider, session_id, pid)
                    );
                    INSERT OR IGNORE INTO live_owners(
                        provider,session_id,pid,start_time,last_seen
                    )
                    SELECT provider,session_id,pid,NULL,last_seen FROM live_owners_legacy;
                    DROP TABLE live_owners_legacy;
                    """
                )
                owner_columns = db.execute(
                    "PRAGMA table_info(live_owners)"
                ).fetchall()
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
            session_columns = {
                str(row["name"])
                for row in db.execute("PRAGMA table_info(sessions)").fetchall()
            }
            if "attention_reason" not in session_columns:
                db.execute("ALTER TABLE sessions ADD COLUMN attention_reason TEXT")
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
            existing = db.execute(
                "SELECT name FROM sessions WHERE provider=? AND session_id=?",
                session.key,
            ).fetchone()
            name = (
                existing["name"]
                if preserve_name and existing and existing["name"]
                else session.name
            )
            db.execute(
                """
                INSERT INTO sessions (
                    provider, session_id, name, cwd, branch, transcript_path,
                    tmux_session, tmux_pane, root_pid, status, unread, model,
                    source, managed, error, attention_reason, created_at, updated_at,
                    last_event_at, last_activity_at
                ) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)
                ON CONFLICT(provider, session_id) DO UPDATE SET
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

    def get_session(self, provider: str, session_id: str) -> Session | None:
        self.initialize()
        with self.connect() as db:
            row = db.execute(
                "SELECT * FROM sessions WHERE provider=? AND session_id=?",
                (provider, session_id),
            ).fetchone()
        return self._row_to_session(row) if row else None

    def list_sessions(self) -> list[Session]:
        self.initialize()
        with self.connect() as db:
            rows = db.execute("SELECT * FROM sessions").fetchall()
        return [self._row_to_session(row) for row in rows]

    def update_session(self, provider: str, session_id: str, **fields: Any) -> None:
        if not fields:
            return
        allowed = {
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
        fields.setdefault("updated_at", time.time())
        if "unread" in fields:
            fields["unread"] = int(bool(fields["unread"]))
        if "managed" in fields:
            fields["managed"] = int(bool(fields["managed"]))
        assignments = ", ".join(f"{key}=?" for key in fields)
        values = list(fields.values()) + [provider, session_id]
        self.initialize()
        with self.connect() as db:
            db.execute(
                f"UPDATE sessions SET {assignments} WHERE provider=? AND session_id=?",
                values,
            )

    def delete_session(self, provider: str, session_id: str) -> None:
        self.initialize()
        with self.connect() as db:
            db.execute(
                "DELETE FROM usage_cache WHERE provider=? AND session_id=?",
                (provider, session_id),
            )
            db.execute(
                "DELETE FROM sessions WHERE provider=? AND session_id=?",
                (provider, session_id),
            )

    def add_pending(
        self,
        launch_token: str,
        provider: str,
        name: str,
        cwd: str,
        tmux_session: str | None = None,
        tmux_pane: str | None = None,
    ) -> None:
        self.initialize()
        with self.connect() as db:
            db.execute(
                """
                INSERT INTO pending_launches
                    (launch_token, provider, name, cwd, tmux_session, tmux_pane, created_at)
                VALUES (?,?,?,?,?,?,?)
                ON CONFLICT(launch_token) DO UPDATE SET
                    tmux_session=COALESCE(excluded.tmux_session, pending_launches.tmux_session),
                    tmux_pane=COALESCE(excluded.tmux_pane, pending_launches.tmux_pane)
                """,
                (
                    launch_token,
                    provider,
                    name,
                    cwd,
                    tmux_session,
                    tmux_pane,
                    time.time(),
                ),
            )

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
                    "DELETE FROM pending_launches WHERE launch_token=?",
                    (launch_token,),
                )
                return str(binding["provider"]), str(binding["session_id"])
            db.execute(
                """
                UPDATE pending_launches
                SET tmux_session=?, tmux_pane=?
                WHERE launch_token=?
                """,
                (tmux_session, tmux_pane, launch_token),
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

    def bind_launch(self, launch_token: str, provider: str, session_id: str) -> None:
        self.initialize()
        with self.connect() as db:
            db.execute(
                """
                INSERT INTO launch_bindings(launch_token, provider, session_id, created_at)
                VALUES (?,?,?,?)
                ON CONFLICT(launch_token) DO UPDATE SET
                    provider=excluded.provider,
                    session_id=excluded.session_id,
                    created_at=excluded.created_at
                """,
                (launch_token, provider, session_id, time.time()),
            )

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

    def delete_launch_binding(self, launch_token: str) -> None:
        self.initialize()
        with self.connect() as db:
            db.execute(
                "DELETE FROM launch_bindings WHERE launch_token=?", (launch_token,)
            )

    def set_live_owner(self, provider: str, session_id: str, pid: int) -> bool:
        """Record a hook owner using non-recyclable Linux process identity."""
        self.initialize()
        start_time = process_start_time(pid)
        if start_time is None:
            return False
        with self.connect() as db:
            db.execute(
                """
                INSERT INTO live_owners(
                    provider,session_id,pid,start_time,last_seen
                ) VALUES (?,?,?,?,?)
                ON CONFLICT(provider,session_id,pid) DO UPDATE SET
                    start_time=excluded.start_time,
                    last_seen=excluded.last_seen
                """,
                (provider, session_id, pid, start_time, time.time()),
            )
        return True

    def get_live_owners(
        self, provider: str, session_id: str
    ) -> list[tuple[int, int | None]]:
        self.initialize()
        with self.connect() as db:
            rows = db.execute(
                "SELECT pid,start_time FROM live_owners "
                "WHERE provider=? AND session_id=? ORDER BY pid",
                (provider, session_id),
            ).fetchall()
        return [
            (
                int(row["pid"]),
                int(row["start_time"]) if row["start_time"] is not None else None,
            )
            for row in rows
        ]

    def list_live_owners(
        self,
    ) -> list[tuple[str, str, int, int | None, float]]:
        self.initialize()
        with self.connect() as db:
            rows = db.execute(
                "SELECT provider,session_id,pid,start_time,last_seen "
                "FROM live_owners ORDER BY provider,session_id,pid"
            ).fetchall()
        return [
            (
                str(row["provider"]),
                str(row["session_id"]),
                int(row["pid"]),
                int(row["start_time"]) if row["start_time"] is not None else None,
                float(row["last_seen"]),
            )
            for row in rows
        ]

    def delete_live_owner(
        self, provider: str, session_id: str, pid: int | None = None
    ) -> None:
        self.initialize()
        with self.connect() as db:
            if pid is None:
                db.execute(
                    "DELETE FROM live_owners WHERE provider=? AND session_id=?",
                    (provider, session_id),
                )
            else:
                db.execute(
                    "DELETE FROM live_owners WHERE provider=? AND session_id=? AND pid=?",
                    (provider, session_id, pid),
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
                    "DELETE FROM launch_reservations "
                    "WHERE provider=? AND session_id=?",
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

    def set_meta(self, key: str, value: str) -> None:
        self.initialize()
        with self.connect() as db:
            db.execute(
                "INSERT INTO meta(key,value) VALUES (?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                (key, value),
            )

    def get_meta(self, key: str) -> str | None:
        self.initialize()
        with self.connect() as db:
            row = db.execute("SELECT value FROM meta WHERE key=?", (key,)).fetchone()
        return row["value"] if row else None

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
