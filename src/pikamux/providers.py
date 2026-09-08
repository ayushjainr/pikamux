from __future__ import annotations

import json
import os
import select
import sqlite3
import subprocess
import time
import uuid
from abc import ABC, abstractmethod
from collections.abc import Iterable
from datetime import datetime
from pathlib import Path
from typing import Any

from . import __version__
from .executables import (
    configured_executable,
    executable_available,
    executable_version,
    provider_compatibility_error,
)
from .models import Candidate, Session, Status, Usage
from .paths import claude_home, codex_home, opencode_data_home
from .pricing import estimate_cost
from .processes import find_processes_with_session_id, provider_process
from .store import Store, load_config


class ProviderError(RuntimeError):
    pass


def opencode_native_placeholder_title(value: str | None) -> bool:
    """Return true only for titles OpenCode generates without user intent."""
    title = str(value or "")
    return title.startswith("New session - ") or (
        " (fork #" in title and title.endswith(")")
    )


def codex_transcript_metadata(
    transcript_path: str | os.PathLike[str] | None,
) -> dict[str, Any]:
    """Read the first immutable Codex metadata record."""
    if not transcript_path:
        return {}
    try:
        with Path(transcript_path).open(errors="replace") as stream:
            first = json.loads(stream.readline())
    except (OSError, TypeError, ValueError):
        return {}
    if first.get("type") != "session_meta":
        return {}
    payload = first.get("payload")
    if not isinstance(payload, dict):
        return {}
    return payload


def codex_session_metadata(
    session_id: str,
    transcript_path: str | os.PathLike[str] | None,
) -> dict[str, Any]:
    """Read UUID-matched Codex lineage without inspecting conversation text."""
    payload = codex_transcript_metadata(transcript_path)
    recorded_id = str(payload.get("id") or payload.get("session_id") or "")
    if recorded_id != session_id:
        return {}
    return payload


def _codex_originator(
    session_id: str,
    transcript_path: str | os.PathLike[str] | None,
) -> str | None:
    payload = codex_session_metadata(session_id, transcript_path)
    originator = payload.get("originator")
    return str(originator).strip() if originator else None


def codex_lifecycle_status(
    transcript_path: str | os.PathLike[str] | None,
) -> str | None:
    """Return the newest structured turn state from a rollout tail."""
    if not transcript_path:
        return None
    try:
        for line in _reverse_lines(Path(transcript_path)):
            if '"event_msg"' not in line or not (
                '"task_started"' in line or '"task_complete"' in line
            ):
                continue
            try:
                data = json.loads(line)
            except ValueError:
                continue
            if data.get("type") != "event_msg":
                continue
            payload = data.get("payload")
            if not isinstance(payload, dict):
                continue
            event = payload.get("type")
            if event == "task_started":
                return Status.WORKING.value
            if event == "task_complete":
                return Status.READY.value
    except OSError:
        return None
    return None


def codex_worker_originator(
    session_id: str,
    transcript_path: str | os.PathLike[str] | None,
    *,
    originator: object = None,
) -> str | None:
    """Return a configured automation origin, otherwise preserve the session."""
    metadata = codex_session_metadata(session_id, transcript_path)
    source = metadata.get("source")
    reported_originator = str(originator).strip() if originator else None
    metadata_originator = (
        str(metadata.get("originator")).strip()
        if metadata.get("originator")
        else None
    )
    if source == "exec" or any(
        value and value.casefold() == "codex_exec"
        for value in (reported_originator, metadata_originator)
    ):
        # ``codex exec`` is a non-interactive execution worker. It can be
        # launched by another managed agent and inherit that parent's Pika
        # environment, but it is not a resumable user conversation.
        return "codex-exec"
    if metadata.get("thread_source") == "subagent" or (
        isinstance(source, dict) and "subagent" in source
    ):
        # Native /side, /btw, and delegated worker threads are subordinate to
        # their parent conversation and must never become Pika workstreams.
        return "codex-subagent"
    value = reported_originator or metadata_originator
    if not value:
        return None
    configured = load_config().get("codex_worker_originators", ())
    if isinstance(configured, str):
        configured = [configured]
    if not isinstance(configured, (list, tuple, set, frozenset)):
        return None
    origins = {str(item).strip().casefold() for item in configured if str(item).strip()}
    return value if value.casefold() in origins else None


def _timestamp(value: Any) -> float:
    try:
        number = float(value)
    except (TypeError, ValueError):
        if isinstance(value, str):
            try:
                return datetime.fromisoformat(value.replace("Z", "+00:00")).timestamp()
            except ValueError:
                pass
        return 0.0
    return number / 1000 if number > 10_000_000_000 else number


class Provider(ABC):
    name: str

    @abstractmethod
    def discover(self) -> list[Candidate]:
        raise NotImplementedError

    @abstractmethod
    def new_argv(self, name: str, session_id: str | None = None) -> list[str]:
        raise NotImplementedError

    @abstractmethod
    def resume_argv(self, session_id: str) -> list[str]:
        raise NotImplementedError

    @abstractmethod
    def usage(self, session: Session, store: Store) -> Usage | None:
        raise NotImplementedError

    def installed(self) -> bool:
        return executable_available(self.executable()) and not self.compatibility_error()

    def executable(self) -> str | None:
        return configured_executable(self.name)

    def version(self) -> str | None:
        return executable_version(self.executable())

    def compatibility_error(self) -> str | None:
        if not executable_available(self.executable()):
            return None
        return provider_compatibility_error(self.name, self.version())

    def import_candidates(self) -> list[Candidate]:
        """Return setup suggestions backed by explicit naming evidence."""
        return self.discover()

    def browse_candidates(self) -> list[Candidate]:
        """Explicit opt-in to titles whose naming intent is not established."""
        return self.discover()

    def launch_candidates(self) -> list[Candidate]:
        """Return launch-time identities, including unnamed provider records."""
        return self.browse_candidates()

    def find_candidates(self, query: str) -> list[Candidate]:
        """Return exact name/UUID matches without a bulk resumability pass."""
        folded = query.casefold()
        return [
            item
            for item in self.browse_candidates()
            if item.session_id == query
            or (item.name and item.name.casefold() == folded)
        ]

    def active_pids(self, session_id: str) -> list[int]:
        return find_processes_with_session_id(session_id, self.name)

    def is_resumable(self, session_id: str) -> bool:
        return any(item.session_id == session_id for item in self.discover())

    def selection_evidence(
        self, session_id: str, transcript_path: str | None = None
    ) -> tuple[str, Candidate | None]:
        """Fresh provider activity for name selection; absence is not deletion."""
        matches = [item for item in self.find_candidates(session_id)
                   if item.session_id == session_id]
        if not matches:
            return "unknown", None
        item = max(matches, key=lambda value: value.updated_at)
        return ("available" if item.live or self.is_resumable(session_id)
                else "unknown"), item

    def tracked_candidates(self, sessions: Iterable[Session]) -> list[Candidate]:
        return []

    def hidden_session_ids(self) -> set[str]:
        """Return provider-owned conversations that must stay out of Pika."""
        return set()

    def worker_originator(
        self, session_id: str, transcript_path: str | None
    ) -> str | None:
        """Return non-interactive automation provenance, when proven."""
        return None

    def valid_session_id(self, value: str) -> bool:
        try:
            parsed = uuid.UUID(value)
        except (ValueError, AttributeError):
            return False
        return str(parsed) == value.lower()


class CodexProvider(Provider):
    name = "codex"

    def __init__(self, home: Path | None = None):
        self.home = home or codex_home()

    def discover(self) -> list[Candidate]:
        archived_ids = self.hidden_session_ids()
        records: dict[str, Candidate] = {}
        for item in self._database_records():
            if item.session_id in archived_ids:
                continue
            records[item.session_id] = item
        index = self.home / "session_index.jsonl"
        try:
            lines = index.open(errors="replace")
        except OSError:
            lines = []
        with lines if hasattr(lines, "__enter__") else _null_context(lines) as stream:
            for line in stream:
                try:
                    data = json.loads(line)
                except (ValueError, TypeError):
                    continue
                session_id = str(data.get("id") or "")
                name = data.get("thread_name")
                if not session_id or not name or session_id in archived_ids:
                    continue
                existing = records.get(session_id)
                if existing:
                    existing.name = str(name)
                    existing.updated_at = max(
                        existing.updated_at, _timestamp(data.get("updated_at"))
                    )
                else:
                    records[session_id] = Candidate(
                        provider=self.name,
                        session_id=session_id,
                        name=str(name),
                        updated_at=_timestamp(data.get("updated_at")),
                        source="codex-index",
                    )
        named = sorted(
            (item for item in records.values() if item.name),
            key=lambda item: item.updated_at,
            reverse=True,
        )
        return [
            item
            for item in self.enrich(named)
            if not self.worker_originator(item.session_id, item.transcript_path)
        ]

    def import_candidates(self) -> list[Candidate]:
        """Do not infer naming intent from either native or index titles.

        Neither current threads.name nor session_index.thread_name records the
        author of a name. Both can be populated by a client, not just /rename.
        Previously chosen Pika homes remain tracked. Other named threads stay
        available through exact lookup and explicit Browse all, including older
        clients' genuine index-only renames. This is not a worker classifier.
        """
        return []

    def selection_evidence(
        self, session_id: str, transcript_path: str | None = None
    ) -> tuple[str, Candidate | None]:
        """Distinguish a missing thread from an unavailable provider store.

        Do not fall back to an older database after the newest one fails. An
        index entry is a name hint, not proof of durable conversation existence.
        Conversely, absent DB rows alone do not invalidate legacy rollouts.
        """
        try:
            databases = sorted(self.home.glob("state_*.sqlite"),
                               key=lambda path: path.stat().st_mtime, reverse=True)
            if not databases:
                return "unknown", None
            database = databases[0]
            archived = self._query_archived_ids(database)
            records = self._query_database(database, named_only=False,
                                           session_ids=[session_id])
            if archived is None or records is None:
                return "unknown", None
            if session_id in archived:
                return "archived", None
            if records:
                item = records[0]
                if item.transcript_path and Path(item.transcript_path).is_file():
                    return "available", item
                return "unknown", item
            if transcript_path:
                try:
                    Path(transcript_path).stat()
                    return "unknown", None
                except FileNotFoundError:
                    pass
            root = self.home / "sessions"
            # An absent/unmounted sessions tree is not proof of deletion.
            if not root.is_dir():
                return "unknown", None
            def unreadable(error: OSError) -> None:
                raise error
            for _directory, directories, files in os.walk(root, onerror=unreadable):
                # Do not infer absence across links to unavailable storage.
                if any((Path(_directory) / name).is_symlink() for name in directories):
                    return "unknown", None
                if any(session_id in name for name in files):
                    return "unknown", None
            return "missing", None
        except (OSError, sqlite3.Error):
            return "unknown", None

    def find_candidates(self, query: str) -> list[Candidate]:
        folded = query.casefold()
        records = {
            item.session_id: item
            for item in self.discover()
            if item.session_id == query
            or (item.name and item.name.casefold() == folded)
        }
        if query not in self.hidden_session_ids():
            for item in self._query_current_database(session_ids=[query]):
                if not self.worker_originator(item.session_id, item.transcript_path):
                    records[item.session_id] = item
        return sorted(records.values(), key=lambda item: item.updated_at, reverse=True)

    def thread_candidate(
        self, session_id: str, transcript_path: str | None = None
    ) -> Candidate | None:
        """Return one exact provider thread with immutable lineage metadata."""
        records = self._query_current_database(session_ids=[session_id])
        if records:
            return records[0]
        if not transcript_path:
            return None
        path = Path(transcript_path)
        if not path.is_file():
            return None
        metadata = codex_session_metadata(session_id, path)
        if not metadata:
            return None
        parent = metadata.get("forked_from_id")
        return Candidate(
            self.name,
            session_id,
            None,
            cwd=metadata.get("cwd"),
            transcript_path=str(path),
            updated_at=path.stat().st_mtime,
            source="codex-rollout",
            parent_session_id=str(parent) if parent else None,
            created_at=_timestamp(metadata.get("timestamp")),
            lifecycle_status=codex_lifecycle_status(path),
        )

    def _database_records(self) -> list[Candidate]:
        return self._query_current_database(named_only=True)

    def enrich(self, candidates: Iterable[Candidate]) -> list[Candidate]:
        candidates = list(candidates)
        by_id = {
            item.session_id: item
            for item in self._query_current_database(
                session_ids=[item.session_id for item in candidates]
            )
        }
        result: list[Candidate] = []
        for item in candidates:
            detail = by_id.get(item.session_id)
            if detail:
                detail.name = item.name or detail.name
                detail.updated_at = max(item.updated_at, detail.updated_at)
                result.append(detail)
            else:
                result.append(item)
        return result

    def _all_database_records(self) -> list[Candidate]:
        archived_ids = self.hidden_session_ids()
        return [
            item
            for item in self._query_current_database()
            if item.session_id not in archived_ids
            and not self.worker_originator(item.session_id, item.transcript_path)
        ]

    def browse_candidates(self) -> list[Candidate]:
        records = {item.session_id: item for item in self._all_database_records()}
        records.update((item.session_id, item) for item in self.discover())
        return sorted(records.values(), key=lambda item: item.updated_at, reverse=True)

    def launch_candidates(self) -> list[Candidate]:
        return self._all_database_records()

    def hidden_session_ids(self) -> set[str]:
        databases = sorted(
            self.home.glob("state_*.sqlite"),
            key=lambda p: p.stat().st_mtime,
            reverse=True,
        )
        for path in databases:
            result = self._query_archived_ids(path)
            if result is not None:
                return result
        return set()

    def worker_originator(
        self, session_id: str, transcript_path: str | None
    ) -> str | None:
        path = transcript_path
        if not path:
            records = self._query_current_database(session_ids=[session_id])
            path = records[0].transcript_path if records else None
        return codex_worker_originator(session_id, path)

    @staticmethod
    def _query_archived_ids(path: Path) -> set[str] | None:
        db: sqlite3.Connection | None = None
        try:
            db = sqlite3.connect(f"file:{path}?mode=ro", uri=True, timeout=1)
            db.row_factory = sqlite3.Row
            columns = {
                str(row["name"])
                for row in db.execute("PRAGMA table_info(threads)").fetchall()
            }
            if "id" not in columns:
                return None
            if "archived" not in columns:
                return set()
            rows = db.execute(
                "SELECT id FROM threads WHERE COALESCE(archived, 0) != 0"
            ).fetchall()
            return {str(row["id"]) for row in rows}
        except (sqlite3.Error, OSError):
            return None
        finally:
            if db is not None:
                db.close()

    def _query_current_database(
        self,
        *,
        named_only: bool = False,
        session_ids: list[str] | None = None,
    ) -> list[Candidate]:
        databases = sorted(
            self.home.glob("state_*.sqlite"),
            key=lambda p: p.stat().st_mtime,
            reverse=True,
        )
        for path in databases:
            rows = self._query_database(
                path, named_only=named_only, session_ids=session_ids
            )
            if rows is not None:
                return rows
        return []

    def _query_database(
        self,
        path: Path,
        *,
        named_only: bool,
        session_ids: list[str] | None,
    ) -> list[Candidate] | None:
        db: sqlite3.Connection | None = None
        try:
            db = sqlite3.connect(f"file:{path}?mode=ro", uri=True, timeout=1)
            db.row_factory = sqlite3.Row
            columns = {
                str(row["name"])
                for row in db.execute("PRAGMA table_info(threads)").fetchall()
            }
            if "id" not in columns or (named_only and "name" not in columns):
                return None
            wanted = (
                "id",
                "name",
                "cwd",
                "git_branch",
                "rollout_path",
                "model",
                "created_at",
                "created_at_ms",
                "updated_at",
                "updated_at_ms",
            )
            select_fields = ", ".join(
                field if field in columns else f"NULL AS {field}" for field in wanted
            )
            clauses: list[str] = []
            parameters: list[str] = []
            if named_only:
                clauses.append("name IS NOT NULL AND name != ''")
            if session_ids is not None:
                if not session_ids:
                    return []
                clauses.append("id IN (" + ",".join("?" for _ in session_ids) + ")")
                parameters.extend(session_ids)
            where = " WHERE " + " AND ".join(clauses) if clauses else ""
            rows = db.execute(
                f"SELECT {select_fields} FROM threads{where}", parameters
            ).fetchall()
        except (sqlite3.Error, OSError):
            return None
        finally:
            if db is not None:
                db.close()
        result: list[Candidate] = []
        for row in rows:
            session_id = str(row["id"])
            transcript_path = row["rollout_path"]
            metadata = codex_session_metadata(session_id, transcript_path)
            parent = metadata.get("forked_from_id")
            result.append(
                Candidate(
                    provider=self.name,
                    session_id=session_id,
                    name=str(row["name"]) if row["name"] else None,
                    cwd=row["cwd"],
                    branch=row["git_branch"],
                    transcript_path=transcript_path,
                    model=row["model"],
                    updated_at=_timestamp(
                        row["updated_at_ms"] or row["updated_at"]
                    ),
                    source="codex-state",
                    parent_session_id=str(parent) if parent else None,
                    created_at=_timestamp(
                        row["created_at_ms"] or row["created_at"]
                    ),
                    lifecycle_status=codex_lifecycle_status(transcript_path),
                )
            )
        return result

    def new_argv(self, name: str, session_id: str | None = None) -> list[str]:
        return [self.executable() or self.name]

    def set_native_name(
        self, session_id: str, name: str, *, timeout: float = 3.5
    ) -> bool:
        """Set Codex's native thread name through the app-server protocol."""
        if not self.installed() or not name.strip():
            return False
        try:
            environment = os.environ.copy()
            environment["CODEX_HOME"] = str(self.home)
            process = subprocess.Popen(
                [self.executable() or self.name, "app-server", "--stdio"],
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
                stderr=subprocess.DEVNULL,
                text=True,
                env=environment,
            )
        except OSError:
            return False
        assert process.stdin is not None and process.stdout is not None

        def send(payload: dict[str, Any]) -> None:
            process.stdin.write(json.dumps(payload, separators=(",", ":")) + "\n")
            process.stdin.flush()

        deadline = time.monotonic() + timeout
        initialized = False
        try:
            send(
                {
                    "method": "initialize",
                    "id": 1,
                    "params": {
                        "clientInfo": {"name": "pikamux", "version": __version__},
                        "capabilities": {"experimentalApi": True},
                    },
                }
            )
            while time.monotonic() < deadline:
                ready, _, _ = select.select(
                    [process.stdout], [], [], max(0, deadline - time.monotonic())
                )
                if not ready:
                    break
                line = process.stdout.readline()
                if not line:
                    break
                try:
                    response = json.loads(line)
                except ValueError:
                    continue
                if response.get("id") == 1 and not initialized:
                    initialized = True
                    send({"method": "initialized"})
                    send(
                        {
                            "method": "thread/name/set",
                            "id": 2,
                            "params": {"threadId": session_id, "name": name},
                        }
                    )
                elif response.get("id") == 2:
                    return "result" in response and not response.get("error")
        except (BrokenPipeError, OSError, ValueError):
            return False
        finally:
            process.terminate()
            try:
                process.wait(timeout=1)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=1)
        return False

    def is_resumable(self, session_id: str) -> bool:
        if session_id in self.hidden_session_ids():
            return False
        records = self._query_current_database(session_ids=[session_id])
        return any(
            item.transcript_path
            and Path(item.transcript_path).is_file()
            and not self.worker_originator(item.session_id, item.transcript_path)
            for item in records
        )

    def resume_argv(self, session_id: str) -> list[str]:
        return [self.executable() or self.name, "resume", session_id]

    def usage(self, session: Session, store: Store) -> Usage | None:
        if not session.transcript_path:
            return None
        path = Path(session.transcript_path)
        cached = store.get_cached_usage(self.name, session.session_id, path)
        if cached:
            return cached
        latest: dict[str, int] | None = None
        model = session.model
        try:
            for line in _reverse_lines(path):
                if "token_usage" not in line:
                    continue
                try:
                    data = json.loads(line)
                except ValueError:
                    continue
                payload = data.get("payload")
                if not isinstance(payload, dict):
                    continue
                info = payload.get("info")
                if not isinstance(info, dict):
                    continue
                usage = info.get("total_token_usage")
                if isinstance(usage, dict):
                    latest = usage
                    break
            if not model:
                for line in _reverse_lines(path):
                    if '"model"' not in line:
                        continue
                    try:
                        payload = json.loads(line).get("payload")
                    except (AttributeError, ValueError):
                        continue
                    if isinstance(payload, dict) and isinstance(
                        payload.get("model"), str
                    ):
                        model = payload["model"]
                        break
        except OSError:
            return None
        if latest is None:
            return None
        result = Usage(
            model=model,
            input_tokens=int(latest.get("input_tokens") or 0),
            output_tokens=int(latest.get("output_tokens") or 0),
            cached_input_tokens=int(latest.get("cached_input_tokens") or 0),
            total_tokens=int(latest.get("total_tokens") or 0),
        )
        result.estimated_cost_usd = estimate_cost(result)
        store.put_cached_usage(self.name, session.session_id, path, result)
        return result


class ClaudeProvider(Provider):
    name = "claude"

    def __init__(self, home: Path | None = None):
        self.home = home or claude_home()

    def discover(self) -> list[Candidate]:
        records: dict[str, Candidate] = {}
        sessions_dir = self.home / "sessions"
        paths = sessions_dir.glob("*.json") if sessions_dir.exists() else []
        for path in paths:
            try:
                data = json.loads(path.read_text())
            except (OSError, ValueError):
                continue
            session_id = str(data.get("sessionId") or "")
            if not session_id or data.get("kind") != "interactive":
                continue
            name_source = data.get("nameSource")
            display_name = data.get("name")
            explicit_name = None if name_source == "derived" else display_name
            raw_pid = int(data["pid"]) if data.get("pid") else None
            pid = provider_process(raw_pid, self.name)
            candidate = Candidate(
                provider=self.name,
                session_id=session_id,
                name=str(explicit_name) if explicit_name else None,
                cwd=data.get("cwd"),
                updated_at=_timestamp(data.get("updatedAt") or data.get("startedAt")),
                created_at=_timestamp(data.get("startedAt")),
                live=bool(pid),
                pid=pid,
                source="claude-live-custom" if name_source == "custom" else "claude-live",
            )
            transcript = self._find_transcript(session_id)
            if transcript:
                candidate.transcript_path = str(transcript)
                if self.worker_originator(session_id, str(transcript)):
                    continue
            existing = records.get(session_id)
            if existing:
                name = candidate.name or existing.name
                if existing.updated_at > candidate.updated_at:
                    existing.name = name
                    continue
                candidate.name = name
            records[session_id] = candidate
        return sorted(records.values(), key=lambda item: item.updated_at, reverse=True)

    def import_candidates(self) -> list[Candidate]:
        return self._import_titles(explicit_only=True)

    def browse_candidates(self) -> list[Candidate]:
        return self._import_titles(explicit_only=False)

    def _import_titles(self, *, explicit_only: bool) -> list[Candidate]:
        records = {item.session_id: item for item in self.discover()}
        for item in self._historical_titles(
            explicit_only=explicit_only, include_unnamed=not explicit_only,
        ):
            existing = records.get(item.session_id)
            if existing:
                # A user-authored title is authoritative even when Claude's
                # live registry still carries an older generated title.
                existing.name = item.name or existing.name
                existing.source = (
                    "claude-live+explicit-history" if explicit_only else "claude-live+history"
                )
                existing.transcript_path = (
                    existing.transcript_path or item.transcript_path
                )
                existing.updated_at = max(existing.updated_at, item.updated_at)
            else:
                records[item.session_id] = item
        return sorted(
            (
                item for item in records.values()
                if not explicit_only or (
                    item.name and item.name.strip() and item.source in {
                        "claude-live-custom", "claude-live+explicit-history", "claude-history"
                    }
                )
            ),
            key=lambda item: item.updated_at, reverse=True,
        )

    def find_candidates(self, query: str) -> list[Candidate]:
        folded = query.casefold()
        records = {
            item.session_id: item
            for item in self.discover()
            if item.session_id == query
            or (item.name and item.name.casefold() == folded)
        }
        for item in self._historical_titles(explicit_only=True):
            if item.session_id != query and not (
                item.name and item.name.casefold() == folded
            ):
                continue
            existing = records.get(item.session_id)
            if existing:
                existing.name = item.name or existing.name
                existing.transcript_path = (
                    existing.transcript_path or item.transcript_path
                )
            else:
                records[item.session_id] = item
        # Names stay explicit, but an exact immutable UUID must remain
        # recoverable even when Claude never assigned a user-facing title.
        if query not in records:
            transcript = self._find_transcript(query)
            if transcript is not None and not self.worker_originator(
                query, str(transcript)
            ):
                records[query] = Candidate(
                    provider=self.name,
                    session_id=query,
                    name=self._title_from_transcript(transcript),
                    transcript_path=str(transcript),
                    updated_at=transcript.stat().st_mtime,
                    source="claude-history",
                )
        return sorted(records.values(), key=lambda item: item.updated_at, reverse=True)

    def tracked_candidates(self, sessions: Iterable[Session]) -> list[Candidate]:
        result: list[Candidate] = []
        for session in sessions:
            path = (
                Path(session.transcript_path)
                if session.transcript_path
                else self._find_transcript(session.session_id)
            )
            if path is None:
                continue
            if self.worker_originator(session.session_id, str(path)):
                continue
            title = self._title_from_transcript(path)
            if not title:
                continue
            try:
                updated_at = path.stat().st_mtime
            except OSError:
                continue
            result.append(
                Candidate(
                    provider=self.name,
                    session_id=session.session_id,
                    name=title,
                    cwd=session.cwd,
                    branch=session.branch,
                    transcript_path=str(path),
                    model=session.model,
                    updated_at=updated_at,
                    live=session.live,
                    pid=session.root_pid,
                    source="claude-history",
                )
            )
        return result

    def active_pids(self, session_id: str) -> list[int]:
        result: set[int] = set(super().active_pids(session_id))
        sessions_dir = self.home / "sessions"
        for path in sessions_dir.glob("*.json") if sessions_dir.exists() else []:
            try:
                data = json.loads(path.read_text())
                raw_pid = int(data["pid"])
            except (KeyError, OSError, TypeError, ValueError):
                continue
            if str(data.get("sessionId") or "") != session_id:
                continue
            pid = provider_process(raw_pid, self.name)
            if pid:
                result.add(pid)
        return sorted(result)

    def is_resumable(self, session_id: str) -> bool:
        transcript = self._find_transcript(session_id)
        if transcript is not None and self.worker_originator(
            session_id, str(transcript)
        ):
            return False
        return transcript is not None or any(
            item.session_id == session_id for item in self.discover()
        )

    def _historical_titles(
        self, limit: int = 1000, *, explicit_only: bool = False,
        include_unnamed: bool = False,
    ) -> list[Candidate]:
        projects = self.home / "projects"
        if not projects.exists():
            return []
        paths = sorted(
            projects.glob("*/*.jsonl"), key=lambda p: p.stat().st_mtime, reverse=True
        )[:limit]
        result: list[Candidate] = []
        for path in paths:
            session_id = path.stem
            if self.worker_originator(session_id, str(path)):
                continue
            title = self._title_from_transcript(path, explicit_only=explicit_only)
            if title or include_unnamed:
                result.append(
                    Candidate(
                        provider=self.name,
                        session_id=session_id,
                        name=str(title) if title else None,
                        transcript_path=str(path),
                        updated_at=path.stat().st_mtime,
                        source="claude-history",
                    )
                )
        return result

    @staticmethod
    def _title_from_transcript(
        path: Path, *, explicit_only: bool = False
    ) -> str | None:
        explicit_title: str | None = None
        generated_title: str | None = None
        try:
            with path.open(errors="replace") as stream:
                for line in stream:
                    if not any(
                        token in line
                        for token in (
                            "custom-title",
                            "session-title",
                            "sessionTitle",
                            "ai-title",
                            "aiTitle",
                        )
                    ):
                        continue
                    try:
                        data = json.loads(line)
                    except ValueError:
                        continue
                    record_type = str(data.get("type") or "")
                    if record_type in {"custom-title", "session-title"}:
                        explicit_title = (
                            data.get("customTitle")
                            or data.get("title")
                            or data.get("sessionTitle")
                            or data.get("name")
                        )
                    elif data.get("sessionTitle"):
                        explicit_title = data["sessionTitle"]
                    elif (
                        not explicit_only
                        and record_type == "ai-title"
                        and data.get("aiTitle")
                    ):
                        generated_title = data["aiTitle"]
        except OSError:
            return None
        title = explicit_title or (None if explicit_only else generated_title)
        return str(title) if title else None

    @staticmethod
    def _transcript_entrypoint(
        path: Path, session_id: str, *, record_limit: int = 128
    ) -> str | None:
        """Return the first root launch mode authored for this exact UUID."""
        try:
            with path.open(errors="replace") as stream:
                for index, line in enumerate(stream):
                    if index >= record_limit:
                        break
                    if '"entrypoint"' not in line:
                        continue
                    try:
                        data = json.loads(line)
                    except ValueError:
                        continue
                    if str(data.get("sessionId") or "") != session_id:
                        continue
                    if data.get("isSidechain") is not False:
                        continue
                    entrypoint = data.get("entrypoint")
                    if isinstance(entrypoint, str) and entrypoint:
                        return entrypoint
        except OSError:
            return None
        return None

    def worker_originator(
        self, session_id: str, transcript_path: str | None
    ) -> str | None:
        """Prove non-interactive Claude print/SDK runs from native metadata.

        Claude records ``claude -p`` sessions as top-level (``isSidechain=false``),
        but their immutable ``entrypoint=sdk-cli`` metadata still distinguishes
        them from interactive conversations. They are automation runs, not Pika
        homes, even if an inherited hook accidentally gave them a native title.
        """
        path = Path(transcript_path) if transcript_path else self._find_transcript(
            session_id
        )
        if path is None or not path.is_file():
            return None
        if self._transcript_entrypoint(path, session_id) == "sdk-cli":
            return "claude-sdk-cli"
        return None

    def _find_transcript(self, session_id: str) -> Path | None:
        projects = self.home / "projects"
        if not projects.exists():
            return None
        try:
            return next(projects.glob(f"*/{session_id}.jsonl"))
        except StopIteration:
            return None

    def new_argv(self, name: str, session_id: str | None = None) -> list[str]:
        argv = [self.executable() or self.name, "--name", name]
        if session_id:
            argv.extend(["--session-id", session_id])
        return argv

    def resume_argv(self, session_id: str) -> list[str]:
        return [self.executable() or self.name, "--resume", session_id]

    def usage(self, session: Session, store: Store) -> Usage | None:
        if not session.transcript_path:
            return None
        path = Path(session.transcript_path)
        cached = store.get_cached_usage(self.name, session.session_id, path)
        if cached:
            return cached
        result = Usage(model=session.model)
        found = False
        try:
            with path.open(errors="replace") as stream:
                for line in stream:
                    if '"usage"' not in line:
                        continue
                    try:
                        data = json.loads(line)
                    except ValueError:
                        continue
                    message = data.get("message")
                    if not isinstance(message, dict):
                        continue
                    usage = message.get("usage")
                    if not isinstance(usage, dict):
                        continue
                    found = True
                    if isinstance(message.get("model"), str):
                        result.model = message["model"]
                    result.input_tokens += int(usage.get("input_tokens") or 0)
                    result.output_tokens += int(usage.get("output_tokens") or 0)
                    result.cached_input_tokens += int(
                        usage.get("cache_read_input_tokens") or 0
                    )
                    result.cache_write_tokens += int(
                        usage.get("cache_creation_input_tokens") or 0
                    )
        except OSError:
            return None
        if not found:
            return None
        result.total_tokens = (
            result.input_tokens
            + result.output_tokens
            + result.cached_input_tokens
            + result.cache_write_tokens
        )
        result.estimated_cost_usd = estimate_cost(result, cached_in_input=False)
        store.put_cached_usage(self.name, session.session_id, path, result)
        return result


class OpenCodeProvider(Provider):
    """OpenCode's native SQLite session store and opaque ``ses_`` identities."""

    name = "opencode"

    def __init__(self, home: Path | None = None):
        self.home = home or opencode_data_home()
        self.database = self.home / "opencode.db"

    def _connect(self) -> sqlite3.Connection:
        db = sqlite3.connect(
            f"file:{self.database}?mode=ro", uri=True, timeout=1
        )
        db.row_factory = sqlite3.Row
        return db

    def durable_state(self, session_id: str) -> str:
        """Return provider-certified presence without conflating I/O failure.

        OpenCode's delete command does not consistently deliver a plugin event,
        so Pika also verifies the native store during ordinary reconciliation.
        """
        if not self.database.is_file():
            return "unknown"
        try:
            with self._connect() as db:
                columns = {
                    str(row["name"])
                    for row in db.execute("PRAGMA table_info(session)").fetchall()
                }
                if "id" not in columns:
                    return "unknown"
                archived = (
                    "time_archived" if "time_archived" in columns else "NULL"
                )
                row = db.execute(
                    f"SELECT {archived} AS time_archived FROM session WHERE id=?",
                    (session_id,),
                ).fetchone()
        except (OSError, sqlite3.Error):
            return "unknown"
        if row is None:
            return "deleted"
        return "archived" if row["time_archived"] is not None else "present"

    def valid_session_id(self, value: str) -> bool:
        return (
            value.startswith("ses_")
            and 8 <= len(value) <= 128
            and value[4:].isalnum()
        )

    def active_pids(self, session_id: str) -> list[int]:
        # A TUI retains its launch-time --session argument after navigating to
        # another root. Current-root hook leases, not stale argv, are the exact
        # runtime authority. Pika still treats an unclaimed argv match as an
        # ambiguous safety hint in _outside_processes.
        return []

    @staticmethod
    def _model(value: object) -> str | None:
        if not value:
            return None
        try:
            parsed = json.loads(str(value))
        except (TypeError, ValueError):
            return str(value)
        if not isinstance(parsed, dict):
            return str(value)
        provider = str(parsed.get("providerID") or "").strip()
        model = str(parsed.get("id") or parsed.get("modelID") or "").strip()
        variant = str(parsed.get("variant") or "").strip()
        if not model:
            return None
        label = f"{provider}/{model}" if provider else model
        return f"{label}[{variant}]" if variant else label

    def _tree_ids(self, db: sqlite3.Connection, session_id: str) -> list[str]:
        rows = db.execute(
            "WITH RECURSIVE tree(id) AS ("
            "SELECT id FROM session WHERE id=? AND time_archived IS NULL "
            "UNION ALL "
            "SELECT child.id FROM session AS child JOIN tree ON child.parent_id=tree.id "
            "WHERE child.time_archived IS NULL"
            ") SELECT id FROM tree",
            (session_id,),
        ).fetchall()
        return [str(row["id"]) for row in rows]

    def _tree_updated_at(self, db: sqlite3.Connection, session_id: str) -> float:
        ids = self._tree_ids(db, session_id)
        if not ids:
            return 0.0
        marks = ",".join("?" for _ in ids)
        row = db.execute(
            f"SELECT MAX(time_updated) AS value FROM session WHERE id IN ({marks})",
            ids,
        ).fetchone()
        return _timestamp(row["value"]) if row else 0.0

    def _lifecycle(self, db: sqlite3.Connection, session_id: str) -> str | None:
        # A root may be idle while its delegated child is still running. A
        # newest incomplete user turn anywhere in the tree keeps it WORKING.
        ids = self._tree_ids(db, session_id)
        found_completed = False
        for child_id in ids:
            row = db.execute(
                "SELECT time_created, data FROM message WHERE session_id=? "
                "ORDER BY time_created DESC, id DESC LIMIT 1",
                (child_id,),
            ).fetchone()
            if row is None:
                continue
            try:
                data = json.loads(str(row["data"]))
            except ValueError:
                continue
            role = str(data.get("role") or "")
            timing = data.get("time")
            completed = bool(
                isinstance(timing, dict) and timing.get("completed")
            )
            if role == "user" or (role == "assistant" and not completed):
                return Status.WORKING.value
            found_completed = found_completed or (role == "assistant" and completed)
        if found_completed:
            return Status.READY.value
        return None

    def _records(
        self, *, query: str | None = None, named_only: bool = True
    ) -> list[Candidate]:
        if not self.database.is_file():
            return []
        try:
            with self._connect() as db:
                columns = {
                    str(row["name"])
                    for row in db.execute("PRAGMA table_info(session)").fetchall()
                }
                if not {"id", "title", "directory", "parent_id"}.issubset(columns):
                    return []
                clauses = ["parent_id IS NULL", "time_archived IS NULL"]
                params: list[str] = []
                if named_only:
                    clauses.append("title IS NOT NULL AND trim(title) != ''")
                if query is not None:
                    clauses.append("(id=? OR lower(title)=lower(?))")
                    params.extend((query, query))
                wanted = (
                    "id", "title", "directory", "parent_id", "time_created",
                    "time_updated", "model",
                )
                fields = ", ".join(
                    field if field in columns else f"NULL AS {field}"
                    for field in wanted
                )
                rows = db.execute(
                    f"SELECT {fields} FROM session WHERE " + " AND ".join(clauses),
                    params,
                ).fetchall()
                result = [
                    Candidate(
                        provider=self.name,
                        session_id=str(row["id"]),
                        name=str(row["title"]) if row["title"] else None,
                        cwd=str(row["directory"]) if row["directory"] else None,
                        transcript_path=str(self.database),
                        model=self._model(row["model"]),
                        updated_at=self._tree_updated_at(db, str(row["id"])),
                        source="opencode-state",
                        created_at=_timestamp(row["time_created"]),
                        lifecycle_status=self._lifecycle(db, str(row["id"])),
                    )
                    for row in rows
                ]
        except (OSError, sqlite3.Error):
            return []
        if query is not None:
            for item in result:
                active = self.active_pids(item.session_id)
                item.live = bool(active)
                item.pid = active[0] if len(active) == 1 else None
        return sorted(result, key=lambda item: item.updated_at, reverse=True)

    native_placeholder_title = staticmethod(opencode_native_placeholder_title)

    @staticmethod
    def _automation_title_prefix(title: str | None, directory: str | None) -> str | None:
        if "opencode-runtime" not in Path(str(directory or "")).parts:
            return None
        configured = load_config().get("opencode_worker_title_prefixes", ())
        if isinstance(configured, str):
            configured = [configured]
        if not isinstance(configured, (list, tuple, set, frozenset)):
            return None
        folded = str(title or "").casefold()
        for value in configured:
            prefix = str(value).strip().casefold()
            if prefix and folded.startswith(prefix):
                return prefix
        return None

    def worker_originator(
        self, session_id: str, transcript_path: str | None
    ) -> str | None:
        if not self.database.is_file():
            return None
        try:
            with self._connect() as db:
                row = db.execute(
                    "SELECT title, directory, parent_id FROM session WHERE id=?",
                    (session_id,),
                ).fetchone()
        except (OSError, sqlite3.Error):
            return None
        if row is None:
            return None
        if row["parent_id"]:
            return "opencode-subagent"
        return self._automation_title_prefix(row["title"], row["directory"])

    def hidden_session_ids(self) -> set[str]:
        return {
            item.session_id
            for item in self._records(named_only=False)
            if self._automation_title_prefix(item.name, item.cwd)
        }

    def discover(self) -> list[Candidate]:
        return [
            item
            for item in self._records()
            if not self.native_placeholder_title(item.name)
            and not self._automation_title_prefix(item.name, item.cwd)
        ]

    def import_candidates(self) -> list[Candidate]:
        # The current native schema exposes a title but not whether it was
        # generated or explicitly renamed. Existing Pika homes stay tracked;
        # other titles remain accessible through browse and exact lookup.
        return []

    def browse_candidates(self) -> list[Candidate]:
        return [
            item for item in self._records(named_only=False)
            if not self._automation_title_prefix(item.name, item.cwd)
        ]

    def launch_candidates(self) -> list[Candidate]:
        return self._records(named_only=False)

    def find_candidates(self, query: str) -> list[Candidate]:
        return [
            item
            for item in self._records(query=query, named_only=False)
            if not self._automation_title_prefix(item.name, item.cwd)
        ]

    def tracked_candidates(self, sessions: Iterable[Session]) -> list[Candidate]:
        wanted = {item.session_id for item in sessions}
        return [
            item
            for item in self._records(named_only=False)
            if item.session_id in wanted
            and not self._automation_title_prefix(item.name, item.cwd)
        ]

    def is_resumable(self, session_id: str) -> bool:
        return any(item.session_id == session_id for item in self._records(query=session_id, named_only=False))

    def new_argv(self, name: str, session_id: str | None = None) -> list[str]:
        # OpenCode creates its own opaque ses_ identity. The installed Pika
        # plugin consumes PIKA_NAME and renames the root session at creation.
        return [self.executable() or self.name]

    def resume_argv(self, session_id: str) -> list[str]:
        return [self.executable() or self.name, "--session", session_id]

    def usage(self, session: Session, store: Store) -> Usage | None:
        if not self.database.is_file():
            return None
        try:
            with self._connect() as db:
                ids = self._tree_ids(db, session.provider_thread_id)
                if not ids:
                    return None
                marks = ",".join("?" for _ in ids)
                row = db.execute(
                    "SELECT MAX(CASE WHEN id=? THEN model END) AS model, "
                    "SUM(COALESCE(cost,0)) AS cost, "
                    "SUM(COALESCE(tokens_input,0)) AS tokens_input, "
                    "SUM(COALESCE(tokens_output,0)) AS tokens_output, "
                    "SUM(COALESCE(tokens_reasoning,0)) AS tokens_reasoning, "
                    "SUM(COALESCE(tokens_cache_read,0)) AS tokens_cache_read, "
                    "SUM(COALESCE(tokens_cache_write,0)) AS tokens_cache_write "
                    f"FROM session WHERE id IN ({marks}) AND time_archived IS NULL",
                    [session.provider_thread_id, *ids],
                ).fetchone()
        except (OSError, sqlite3.Error):
            return None
        if row is None:
            return None
        result = Usage(
            model=self._model(row["model"]) or session.model,
            input_tokens=int(row["tokens_input"] or 0),
            output_tokens=int(row["tokens_output"] or 0),
            cached_input_tokens=int(row["tokens_cache_read"] or 0),
            cache_write_tokens=int(row["tokens_cache_write"] or 0),
        )
        # OpenCode exposes reasoning separately; Pika's existing stable usage
        # shape does not. Include it only in the exact cumulative total.
        result.total_tokens = (
            result.input_tokens
            + result.output_tokens
            + int(row["tokens_reasoning"] or 0)
            + result.cached_input_tokens
            + result.cache_write_tokens
        )
        result.estimated_cost_usd = float(row["cost"] or 0)
        return result


class _null_context:
    def __init__(self, value: Any):
        self.value = value

    def __enter__(self) -> Any:
        return self.value

    def __exit__(self, *_args: object) -> None:
        return None


def _reverse_lines(path: Path, *, block_size: int = 65536) -> Iterable[str]:
    """Yield UTF-8 text lines from the end without loading a large rollout."""
    with path.open("rb") as stream:
        stream.seek(0, os.SEEK_END)
        position = stream.tell()
        remainder = b""
        while position > 0:
            size = min(block_size, position)
            position -= size
            stream.seek(position)
            chunk = stream.read(size) + remainder
            lines = chunk.split(b"\n")
            remainder = lines.pop(0)
            for line in reversed(lines):
                if line:
                    yield line.decode(errors="replace")
        if remainder:
            yield remainder.decode(errors="replace")


def providers() -> dict[str, Provider]:
    return {
        "codex": CodexProvider(),
        "claude": ClaudeProvider(),
        "opencode": OpenCodeProvider(),
    }
