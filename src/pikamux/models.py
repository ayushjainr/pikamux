from __future__ import annotations

from dataclasses import asdict, dataclass
from enum import Enum
from typing import Any


class Status(str, Enum):
    STARTING = "STARTING"
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
    Status.STARTING.value: 5,
    Status.PARKED.value: 6,
    Status.UNBOUND.value: 7,
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
    # Pika's stable conversation key can outlive a provider-created continuation.
    # When set, provider operations resume and verify this exact current thread.
    active_thread_id: str | None = None

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

    @property
    def provider_thread_id(self) -> str:
        return self.active_thread_id or self.session_id

    def to_dict(self) -> dict[str, Any]:
        values = asdict(self)
        # Reconciliation-only proof expires as soon as the process topology
        # changes; keep it out of the stable inventory JSON contract.
        values.pop("home_state", None)
        return values


@dataclass(frozen=True, slots=True)
class NodeCandidate:
    """A passively discovered address.  It is not trusted until handshake."""

    alias: str
    ssh_target: str
    sources: tuple[str, ...]
    hostname: str | None = None
    online: bool | None = None
    os_name: str | None = None

    @property
    def key(self) -> str:
        return self.ssh_target.casefold()


@dataclass(frozen=True, slots=True)
class NodeDiscoveryReport:
    candidates: tuple[NodeCandidate, ...]
    ssh_aliases: int = 0
    ssh_config_files: int = 0
    tailscale_total: int = 0
    tailscale_compatible: int = 0
    excluded_non_linux: int = 0
    excluded_no_target: int = 0
    tailscale_error: str | None = None


@dataclass(frozen=True, slots=True)
class FleetNode:
    """An explicitly adopted Pika node with immutable remote identity."""

    node_id: str
    alias: str
    ssh_target: str
    sources: tuple[str, ...] = ()
    status: str = "unknown"
    protocol_version: int | None = None
    package_version: str | None = None
    capabilities: tuple[str, ...] = ()
    last_seen: float = 0.0
    last_attempt_at: float = 0.0
    last_error: str | None = None
    created_at: float = 0.0
    updated_at: float = 0.0


@dataclass(slots=True)
class FleetSession:
    """A routed, read-through view of a session authoritative on another node."""

    node_id: str
    node_name: str
    session: Session
    stale: bool = False
    remote_error: str | None = None
    seen_at: float = 0.0
    card_status: str | None = None
    card_detail: str | None = None

    @property
    def key(self) -> tuple[str, str, str]:
        return self.node_id, self.session.provider, self.session.session_id

    @property
    def local_key(self) -> tuple[str, str]:
        return self.session.key

    @property
    def provider(self) -> str:
        return self.session.provider

    @property
    def session_id(self) -> str:
        return self.session.session_id

    @property
    def name(self) -> str | None:
        return self.session.name

    @property
    def display_name(self) -> str:
        return f"{self.session.display_name}@{self.node_name}"

    @property
    def qualified_name(self) -> str:
        return self.display_name

    @property
    def needs_attention(self) -> bool:
        return not self.stale and self.session.needs_attention

    def __getattr__(self, name: str) -> Any:
        # The presentation layer can reuse Session renderers without allowing a
        # remote row into local persistence or identity code.
        return getattr(self.session, name)

    def to_dict(self) -> dict[str, Any]:
        return {
            "node_id": self.node_id,
            "machine": self.node_name,
            "stale": self.stale,
            "remote_error": self.remote_error,
            "seen_at": self.seen_at,
            "card_status": self.card_status,
            "card_detail": self.card_detail,
            "session": self.session.to_dict(),
        }


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
    parent_session_id: str | None = None
    created_at: float = 0.0
    lifecycle_status: str | None = None

    @property
    def display_name(self) -> str:
        return self.name or f"{self.provider}-{self.session_id[:8]}"


@dataclass(slots=True)
class PendingLaunch:
    launch_token: str
    provider: str
    name: str
    cwd: str
    tmux_session: str | None
    tmux_pane: str | None
    created_at: float
    expected_session_id: str | None = None
    root_pid: int | None = None
    root_pid_start: int | None = None
    status: str = Status.STARTING.value
    live: bool = False
    attached: bool = False
    error: str | None = None
    attention_reason: str | None = "identity pending"
    last_activity_at: float = 0.0
    last_event_at: float = 0.0
    unread: bool = False
    managed: bool = True
    source: str = "pending-launch"
    branch: str | None = None
    transcript_path: str | None = None
    model: str | None = None
    home_state: str = "identity-pending"
    cpu_percent: float | None = None
    rss_kb: int | None = None
    input_tokens: int | None = None
    output_tokens: int | None = None
    cached_input_tokens: int | None = None
    cache_write_tokens: int | None = None
    total_tokens: int | None = None
    estimated_cost_usd: float | None = None
    active_thread_id: str | None = None

    @property
    def key(self) -> tuple[str, str]:
        return "pending", self.launch_token

    @property
    def session_id(self) -> str:
        """Compatibility fingerprint for presentation; never provider identity."""
        return self.launch_token

    @property
    def display_name(self) -> str:
        return self.name

    @property
    def needs_attention(self) -> bool:
        return self.status == Status.ERROR.value and self.unread

    @property
    def exact_home(self) -> bool:
        return False

    @property
    def provider_thread_id(self) -> str:
        return self.expected_session_id or self.launch_token

    def to_dict(self) -> dict[str, Any]:
        return {
            "kind": "pending-launch",
            "launch_token": self.launch_token,
            "provider": self.provider,
            "name": self.name,
            "cwd": self.cwd,
            "tmux_session": self.tmux_session,
            "tmux_pane": self.tmux_pane,
            "created_at": self.created_at,
            "expected_session_id": self.expected_session_id,
            "status": self.status,
            "live": self.live,
            "attached": self.attached,
            "error": self.error,
        }


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
