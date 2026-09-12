"""Disposable adapter around the frozen Python v0.5.0a4 runtime.

The Rust integration test sets PYTHONPATH to its private copy of the reference.
This adapter uses only synthetic state and in-memory/fake Pika surfaces.
"""

from __future__ import annotations

import argparse
import dataclasses
import io
import importlib.util
import json
import sys
import types
from pathlib import Path
from types import SimpleNamespace
from typing import Any

# macOS still ships Python 3.9. The released package requires 3.10 only because
# `dataclass(slots=True)` was added there; slots do not affect these wire/store
# contracts. This adapter removes only that decorator argument while leaving
# the frozen reference files byte-for-byte untouched.
if sys.version_info < (3, 10):
    _stdlib_dataclass = dataclasses.dataclass

    def _compat_dataclass(_cls: Any = None, **kwargs: Any) -> Any:
        kwargs.pop("slots", None)
        if _cls is None:
            return lambda cls: _stdlib_dataclass(cls, **kwargs)
        return _stdlib_dataclass(_cls, **kwargs)

    dataclasses.dataclass = _compat_dataclass

if sys.platform == "darwin" and importlib.util.find_spec("psutil") is None:
    psutil = types.ModuleType("psutil")
    psutil.Error = RuntimeError
    psutil.Process = lambda _pid: (_ for _ in ()).throw(RuntimeError("disabled"))
    psutil.pids = lambda: []
    for _status in (
        "RUNNING",
        "SLEEPING",
        "DISK_SLEEP",
        "STOPPED",
        "TRACING_STOP",
        "ZOMBIE",
        "DEAD",
        "IDLE",
    ):
        setattr(psutil, f"STATUS_{_status}", _status)
    sys.modules["psutil"] = psutil

from pikamux import __version__
from pikamux.fleet import (
    CAPABILITIES,
    FleetManager,
    NodeCandidate,
    handle_fleet_stdio,
    validate_snapshot,
)
from pikamux.hooks import handle_hook
from pikamux.models import FleetSession, Session, Status
from pikamux.store import Store


SESSION_ID = "11111111-1111-4111-8111-111111111111"


class FakeTmux:
    def get_pane(self, value: str) -> object | None:
        return SimpleNamespace(pane_id=value) if value == "%compat" else None


class FakePika:
    def __init__(self, database: Path) -> None:
        self.store = Store(database)
        self.store.initialize()
        if self.store.get_meta("compat:seeded") is None:
            self.store.upsert_session(
                Session(
                    "codex",
                    SESSION_ID,
                    name="python-expert",
                    cwd="/synthetic/project",
                    tmux_session="pika-compat",
                    tmux_pane="%compat",
                    status=Status.READY.value,
                    unread=True,
                    source="compat-python",
                    managed=True,
                    created_at=100.0,
                    updated_at=100.0,
                    last_event_at=4242.0,
                    last_activity_at=4242.0,
                )
            )
            self.store.set_meta("compat:seeded", "1")
        self.tmux = FakeTmux()

    def refresh(self, *, usage: bool = False) -> list[Session]:
        assert usage is False
        return self.store.list_sessions()

    def hidden_session_keys(self, sessions: list[Session]) -> set[tuple[str, str]]:
        return set()

    def expert_card_states(self, sessions: list[Session]) -> list[Any]:
        return []

    def discover_import_candidates(self, *, include_unconfirmed: bool = False) -> list[Any]:
        return []

    def capture(self, session: Session, lines: int) -> str:
        assert session.session_id == SESSION_ID
        return f"python-tail:{min(lines, 2000)}"

    def acknowledge(self, session: Session) -> bool:
        return self.store.acknowledge_attention(
            *session.key, expected_event_at=session.last_event_at
        )

    def untrack(self, session: Session) -> int:
        self.store.untrack_session(*session.key)
        return 1


class TranscriptTransport:
    def __init__(self, responses: list[dict[str, Any]]) -> None:
        self.responses = responses

    def request(
        self, target: str, payload: dict[str, Any], *, mutating: bool = False
    ) -> dict[str, Any]:
        assert target == "rust.invalid"
        assert isinstance(mutating, bool)
        if not self.responses:
            raise AssertionError(f"no Rust response left for {payload!r}")
        return self.responses.pop(0)

    def run_exact(self, node: Any, arguments: list[str], *, tty: bool) -> int:
        raise AssertionError("attach is outside this compatibility test")


def hook(database: Path, provider: str, event: str) -> None:
    store = Store(database)
    result = handle_hook(
        provider,
        {
            "session_id": SESSION_ID,
            "hook_event_name": event,
            "cwd": "/synthetic/project",
            "source": "compat-python-hook",
            "session_title": "compat-thread",
        },
        store=store,
    )
    session = store.get_session(provider, SESSION_ID)
    observation = store.get_hook_observation(provider)
    print(
        json.dumps(
            {
                "version": __version__,
                "result": result,
                "session": session.to_dict() if session else None,
                "hook": observation,
            },
            separators=(",", ":"),
        )
    )


def inspect(database: Path, provider: str) -> None:
    store = Store(database)
    store.initialize()
    session = store.get_session(provider, SESSION_ID)
    observation = store.get_hook_observation(provider)
    print(
        json.dumps(
            {
                "version": __version__,
                "session": session.to_dict() if session else None,
                "hook": observation,
                "untracked": store.is_untracked(provider, SESSION_ID),
            },
            separators=(",", ":"),
        )
    )


def fleet_serve(database: Path) -> None:
    handle_fleet_stdio(FakePika(database), sys.stdin, sys.stdout)


def seed_board(database: Path, count: int) -> None:
    store = Store(database)
    store.initialize()
    statuses = (
        (Status.NEEDS_YOU.value, True, "question"),
        (Status.WORKING.value, False, None),
        (Status.READY.value, True, "result"),
        (Status.PARKED.value, False, None),
    )
    providers = ("codex", "claude", "opencode")
    for index in range(count):
        provider = providers[index % len(providers)]
        identity = (
            f"ses_{index:012d}"
            if provider == "opencode"
            else f"00000000-0000-4000-8000-{index:012d}"
        )
        status, unread, reason = statuses[index % len(statuses)]
        store.upsert_session(
            Session(
                provider,
                identity,
                name=f"fixture_{index:04d}",
                cwd="/synthetic/project",
                status=status,
                unread=unread,
                attention_reason=reason,
                source="performance-fixture",
                created_at=1000.0 + index,
                updated_at=2000.0 + index,
                last_event_at=2000.0 + index,
                last_activity_at=2000.0 + index,
            )
        )
    print(json.dumps({"seeded": count}, separators=(",", ":")))


def validate_rust_transcript(database: Path, transcript: Path) -> None:
    responses = [json.loads(line) for line in transcript.read_text().splitlines()]
    assert len(responses) == 6
    store = Store(database)
    store.initialize()
    transport = TranscriptTransport(responses)
    manager = FleetManager(store, transport=transport)
    node = manager.add(
        NodeCandidate("rust-node", "rust.invalid", ("compat",)),
        alias="rust-node",
    )
    sessions = manager.cached_sessions(node.node_id)
    assert len(sessions) == 1
    session: FleetSession = sessions[0]
    assert manager.capture(session, 17) == "rust-tail:17"
    assert manager.acknowledge(session) is True
    assert manager.untrack(
        session, request_id="33333333-3333-4333-8333-333333333333"
    ) == 2
    assert manager.cached_sessions(node.node_id) == []
    assert transport.responses == []
    print(
        json.dumps(
            {
                "version": __version__,
                "node_id": node.node_id,
                "operations": ["hello", "snapshot", "peek", "ack", "untrack"],
            },
            separators=(",", ":"),
        )
    )


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "command",
        choices=("hook", "inspect", "fleet-serve", "validate-rust", "seed-board"),
    )
    parser.add_argument("database", type=Path)
    parser.add_argument("extra", nargs="*")
    args = parser.parse_args()
    if args.command == "hook":
        hook(args.database, args.extra[0], args.extra[1])
    elif args.command == "inspect":
        inspect(args.database, args.extra[0])
    elif args.command == "fleet-serve":
        fleet_serve(args.database)
    elif args.command == "seed-board":
        seed_board(args.database, int(args.extra[0]))
    else:
        validate_rust_transcript(args.database, Path(args.extra[0]))


if __name__ == "__main__":
    main()
