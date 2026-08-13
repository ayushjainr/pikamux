from __future__ import annotations

from dataclasses import asdict, dataclass
from enum import Enum
from typing import Any


class Status(str, Enum):
    NEEDS_YOU = "NEEDS YOU"
    OPEN_TWICE = "OPEN TWICE"
    WORKING = "WORKING"
    READY = "READY"
    PARKED = "PARKED"
    UNBOUND = "UNBOUND"
    ERROR = "ERROR"


ATTENTION_ORDER = {
    Status.NEEDS_YOU.value: 0,
    Status.OPEN_TWICE.value: 1,
    Status.ERROR.value: 2,
    Status.READY.value: 3,
    Status.WORKING.value: 4,
    Status.PARKED.value: 5,
    Status.UNBOUND.value: 6,
}


@dataclass(slots=True)
class Session:
    provider: str
    session_id: str
    name: str | None = None
    cwd: str | None = None
    branch: str | None = None
    transcript_path: str | None = None
    tmux_session: str | None = None
    tmux_pane: str | None = None
    root_pid: int | None = None
    status: str = Status.PARKED.value
    unread: bool = False
    model: str | None = None
    source: str = "managed"
    managed: bool = True
    error: str | None = None
    attention_reason: str | None = None
    created_at: float = 0.0
    updated_at: float = 0.0
    last_event_at: float = 0.0
    last_activity_at: float = 0.0
    live: bool = False
    attached: bool = False
    home_state: str = "unknown"
    cpu_percent: float | None = None
    rss_kb: int | None = None
    input_tokens: int | None = None
    output_tokens: int | None = None
    cached_input_tokens: int | None = None
    cache_write_tokens: int | None = None
    total_tokens: int | None = None
    estimated_cost_usd: float | None = None

    @property
    def key(self) -> tuple[str, str]:
        return self.provider, self.session_id

    @property
    def display_name(self) -> str:
        return self.name or f"{self.provider}-{self.session_id[:8]}"

    @property
    def needs_attention(self) -> bool:
        return self.status == Status.NEEDS_YOU.value or (
            self.unread
            and self.status
            in {
                Status.READY.value,
                Status.ERROR.value,
                Status.OPEN_TWICE.value,
            }
        )

    @property
    def exact_home(self) -> bool:
        return self.home_state == "exact-live"

    def to_dict(self) -> dict[str, Any]:
        values = asdict(self)
        # Reconciliation-only proof expires as soon as the process topology
        # changes; keep it out of the stable inventory JSON contract.
        values.pop("home_state", None)
        return values


@dataclass(slots=True)
class Candidate:
    provider: str
    session_id: str
    name: str | None
    cwd: str | None = None
    branch: str | None = None
    transcript_path: str | None = None
    model: str | None = None
    updated_at: float = 0.0
    live: bool = False
    pid: int | None = None
    source: str = "discovered"

    @property
    def display_name(self) -> str:
        return self.name or f"{self.provider}-{self.session_id[:8]}"


@dataclass(slots=True)
class Pane:
    session_name: str
    pane_id: str
    pane_pid: int
    cwd: str
    current_command: str
    attached: bool
    dead: bool
    dead_status: int | None
    activity: float
    created: float
    pika_provider: str | None = None
    pika_session_id: str | None = None
    pika_name: str | None = None
    pika_launch_token: str | None = None

    @property
    def target(self) -> str:
        return self.pane_id


@dataclass(slots=True)
class Usage:
    model: str | None = None
    input_tokens: int = 0
    output_tokens: int = 0
    cached_input_tokens: int = 0
    cache_write_tokens: int = 0
    total_tokens: int = 0
    estimated_cost_usd: float | None = None


@dataclass(slots=True)
class ExpertProfile:
    provider: str
    session_id: str
    summary: str
    topics: tuple[str, ...]
    artifacts: tuple[str, ...] = ()
    updated_at: float = 0.0
    source: str = "self"
    transcript_mtime_ns: int | None = None
    transcript_size: int | None = None
    current_state: str = ""

    @property
    def key(self) -> tuple[str, str]:
        return self.provider, self.session_id

    @property
    def scope(self) -> str:
        """The durable mandate retained in the legacy summary field."""
        return self.summary


@dataclass(slots=True)
class ExpertRefreshAttempt:
    provider: str
    session_id: str
    reset_at: int
    status: str
    detail: str | None = None
    attempted_at: float = 0.0

    @property
    def key(self) -> tuple[str, str, int]:
        return self.provider, self.session_id, self.reset_at


@dataclass(frozen=True, slots=True)
class ExpertRefreshResult:
    provider: str
    status: str
    detail: str
    session_id: str | None = None
    name: str | None = None
    remaining_percent: float | None = None
    reset_at: int | None = None
    consultation_mode: str | None = None
    model: str | None = None
    effort: str | None = None

    def to_dict(self) -> dict[str, Any]:
        return asdict(self)
