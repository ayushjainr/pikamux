from __future__ import annotations

import json
import os
import select
import shutil
import sqlite3
import subprocess
import time
from abc import ABC, abstractmethod
from collections.abc import Iterable
from datetime import datetime
from pathlib import Path
from typing import Any

from .models import Candidate, Session, Usage
from .paths import claude_home, codex_home
from .pricing import estimate_cost
from .processes import find_processes_with_session_id, provider_process
from .store import Store


class ProviderError(RuntimeError):
    pass


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
        return shutil.which(self.name) is not None

    def version(self) -> str | None:
        if not self.installed():
            return None
        try:
            proc = subprocess.run(
                [self.name, "--version"],
                capture_output=True,
                text=True,
                timeout=3,
                check=False,
            )
        except (OSError, subprocess.TimeoutExpired):
            return None
        return (proc.stdout or proc.stderr).strip() or None

    def import_candidates(self) -> list[Candidate]:
        """Return the broader, potentially slower one-time import surface."""
        return self.discover()

    def active_pids(self, session_id: str) -> list[int]:
        return find_processes_with_session_id(session_id, self.name)

    def is_resumable(self, session_id: str) -> bool:
        return any(item.session_id == session_id for item in self.discover())

    def tracked_candidates(self, sessions: Iterable[Session]) -> list[Candidate]:
        return []

    def hidden_session_ids(self) -> set[str]:
        """Return provider-owned conversations that must stay out of Pika."""
        return set()


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
        return self.enrich(named)

    def import_candidates(self) -> list[Candidate]:
        """Offer only current authoritative names during commissioning.

        The legacy append-only index does not say whether a title was generated
        or explicitly chosen, so it is useful for reconciling already-known
        identities but too ambiguous for a one-time adoption prompt.
        """
        archived_ids = self.hidden_session_ids()
        return [
            item
            for item in self._database_records()
            if item.session_id not in archived_ids
        ]

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
        ]

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
        return [
            Candidate(
                provider=self.name,
                session_id=str(row["id"]),
                name=str(row["name"]) if row["name"] else None,
                cwd=row["cwd"],
                branch=row["git_branch"],
                transcript_path=row["rollout_path"],
                model=row["model"],
                updated_at=_timestamp(row["updated_at_ms"] or row["updated_at"]),
                source="codex-state",
            )
            for row in rows
        ]

    def new_argv(self, name: str, session_id: str | None = None) -> list[str]:
        return ["codex"]

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
                ["codex", "app-server", "--stdio"],
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
                        "clientInfo": {"name": "pikamux", "version": "0.1.0"},
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
            item.transcript_path and Path(item.transcript_path).is_file()
            for item in records
        )

    def resume_argv(self, session_id: str) -> list[str]:
        return ["codex", "resume", session_id]

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
                live=bool(pid),
                pid=pid,
                source="claude-live",
            )
            transcript = self._find_transcript(session_id)
            if transcript:
                candidate.transcript_path = str(transcript)
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
        records = {item.session_id: item for item in self.discover()}
        for item in self._historical_titles(explicit_only=True):
            existing = records.get(item.session_id)
            if existing:
                if not existing.name:
                    existing.name = item.name
                    existing.source = "claude-live+explicit-history"
                existing.transcript_path = (
                    existing.transcript_path or item.transcript_path
                )
                existing.updated_at = max(existing.updated_at, item.updated_at)
            else:
                records[item.session_id] = item
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
        return self._find_transcript(session_id) is not None or any(
            item.session_id == session_id for item in self.discover()
        )

    def _historical_titles(
        self, limit: int = 1000, *, explicit_only: bool = False
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
            title = self._title_from_transcript(path, explicit_only=explicit_only)
            if title:
                result.append(
                    Candidate(
                        provider=self.name,
                        session_id=session_id,
                        name=str(title),
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
        title: str | None = None
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
                        title = (
                            data.get("title")
                            or data.get("sessionTitle")
                            or data.get("name")
                        )
                    elif data.get("sessionTitle"):
                        title = data["sessionTitle"]
                    elif (
                        not explicit_only
                        and record_type == "ai-title"
                        and data.get("aiTitle")
                    ):
                        title = data["aiTitle"]
        except OSError:
            return None
        return str(title) if title else None

    def _find_transcript(self, session_id: str) -> Path | None:
        projects = self.home / "projects"
        if not projects.exists():
            return None
        try:
            return next(projects.glob(f"*/{session_id}.jsonl"))
        except StopIteration:
            return None

    def new_argv(self, name: str, session_id: str | None = None) -> list[str]:
        argv = ["claude", "--name", name]
        if session_id:
            argv.extend(["--session-id", session_id])
        return argv

    def resume_argv(self, session_id: str) -> list[str]:
        return ["claude", "--resume", session_id]

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
    return {"codex": CodexProvider(), "claude": ClaudeProvider()}
