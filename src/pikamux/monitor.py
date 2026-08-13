from __future__ import annotations

import argparse
import concurrent.futures
import os
import re
import select
import sys
import termios
import threading
import time
import tty
from dataclasses import dataclass, field, replace
from datetime import datetime
from textwrap import wrap
from typing import Callable, Protocol

from .experts import ExpertCardState
from .models import ExpertProfile, Session, Status
from .pricing import PRICING_AS_OF
from .ui import (
    PROVIDER_MARK,
    format_bytes,
    format_cost,
    format_tokens,
    short_path,
    sorted_attention_sessions,
    sorted_sessions,
    terminal_text,
)

REFRESH_SECONDS = 2.0
USAGE_REFRESH_SECONDS = 30.0
EXPERT_CARD_REFRESH_SECONDS = 300.0
PREVIEW_REFRESH_SECONDS = 2.0
PLAYBOOK_ROTATION_SECONDS = 300
MORNING_GAP_SECONDS = 6 * 60 * 60
HANDOFF_DISPLAY_SECONDS = 15.0
SLOW_REFRESH_SECONDS = 1.0
MIN_WIDTH = 58
MIN_HEIGHT = 15
SPLIT_MIN_WIDTH = 104
SPLIT_MIN_HEIGHT = 20
SPINNER = ("◐", "◓", "◑", "◒")
PLAYBOOK_TIPS = (
    (
        "experts",
        (
            "pika experts QUERY finds UUID-bound firsthand expertise across "
            "projects."
        ),
        "pika experts QUERY finds the agent who did the work.",
    ),
    (
        "ask",
        (
            "pika ask NAME opens a multi-turn side consultation without touching "
            "the parent transcript."
        ),
        "pika ask NAME explores without touching the parent.",
    ),
    (
        "detach",
        "Delegate, then Ctrl-b d. The agent keeps working; /exit stops it.",
        "Ctrl-b d keeps work alive; /exit stops it.",
    ),
    (
        "switch",
        "pika . opens this repository's conversation; "
        "pika - returns to the previous one.",
        "pika . opens here; pika - goes back.",
    ),
    (
        "next",
        "pika next opens the oldest workstream needing you — "
        "no inventory triage required.",
        "pika next opens the oldest needed work.",
    ),
    (
        "peek",
        "pika peek NAME inspects without switching; "
        "redirected peeks keep unread state.",
        "pika peek NAME inspects without switching.",
    ),
    (
        "wait",
        "pika wait NAME --for ready --timeout 600 turns delegation "
        "into a shell primitive.",
        "pika wait NAME --for ready is scriptable.",
    ),
    (
        "name",
        "Use task-specific names: the briefing should read like a delegation ledger.",
        "Use task names; make Pika read as a ledger.",
    ),
    (
        "collision",
        "Codex and Claude may share a name; Pika asks for provider + UUID.",
        "Same name? Pika asks provider + UUID.",
    ),
    (
        "doctor",
        "Run pika doctor before relying on disconnects; safe means every check passed.",
        "Run pika doctor before disconnecting.",
    ),
    (
        "refresh",
        "Press r to run a deliberate reconciliation; "
        "stale state remains visibly marked.",
        "Press r to reconcile stale state now.",
    ),
    (
        "adopt",
        "A live process outside an exact home stays unbound; "
        "use pika adopt to protect it.",
        "UNBOUND? Run pika adopt for an exact home.",
    ),
    (
        "resume",
        "For a parked workstream, pika NAME checks its saved history "
        "and resumes only if exact.",
        "PARKED? pika NAME checks its recovery path.",
    ),
)

RESET = "\x1b[0m"
BOLD = "\x1b[1m"
DIM = "\x1b[2m"
REVERSE = "\x1b[7m"
FG_RED = "\x1b[31m"
FG_GREEN = "\x1b[32m"
FG_YELLOW = "\x1b[33m"
FG_BLUE = "\x1b[34m"
FG_MAGENTA = "\x1b[35m"
FG_CYAN = "\x1b[36m"
FG_BRIGHT_BLACK = "\x1b[90m"

_ANSI_ESCAPE = re.compile(
    r"(?:\x1b\][^\x07]*(?:\x07|\x1b\\))"
    r"|(?:\x1bP.*?\x1b\\)"
    r"|(?:\x1b\[[0-?]*[ -/]*[@-~])",
    re.DOTALL,
)
_MOUSE = re.compile(rb"^\x1b\[<(\d+);(\d+);(\d+)([Mm])")


class MonitorPika(Protocol):
    def refresh(self, *, usage: bool = False) -> list[Session]: ...

    def hydrate_usage(self, sessions: list[Session]) -> list[Session]: ...

    def open(self, session: Session, *, attach: bool = True) -> int: ...

    def acknowledge(self, session: Session, *, attaching: bool = False) -> bool: ...

    def next_attention(
        self, sessions: list[Session] | None = None
    ) -> Session | None: ...

    def expert_card_states(
        self, sessions: list[Session] | None = None
    ) -> list[ExpertCardState]: ...

    @property
    def tmux(self): ...

    @property
    def store(self): ...


@dataclass(frozen=True, slots=True)
class MonitorFrame:
    ansi: str
    plain: str
    selected_key: tuple[str, str] | None


@dataclass(frozen=True, slots=True)
class HandoffSummary:
    since: float
    finished: int
    decisions: int
    errors: int
    first: bool = False

    @property
    def changed(self) -> bool:
        return bool(self.finished or self.decisions or self.errors)


@dataclass(slots=True)
class MonitorState:
    sessions: list[Session] = field(default_factory=list)
    selected_key: tuple[str, str] | None = None
    last_update: float = 0.0
    refresh_error: str | None = None
    refresh_warning: str | None = None
    usage_warning: str | None = None
    toast: str | None = None
    toast_until: float = 0.0
    mode: str = "sessions"
    peek_lines: list[str] = field(default_factory=list)
    peek_offset: int = 0
    show_usage: bool = False
    refresh_started_at: float = 0.0
    emphasize_refresh: bool = False
    handoff: HandoffSummary | None = None
    handoff_until: float = 0.0
    expert_cards: dict[tuple[str, str], ExpertCardState] = field(default_factory=dict)
    expert_cards_updated_at: float = 0.0
    preview_key: tuple[str, str] | None = None
    preview_lines: list[str] = field(default_factory=list)
    preview_error: str | None = None
    preview_updated_at: float = 0.0

    def ordered(self) -> list[Session]:
        return sorted_sessions(self.sessions)

    def selected(self) -> Session | None:
        ordered = self.ordered()
        if not ordered:
            self.selected_key = None
            return None
        for item in ordered:
            if item.key == self.selected_key:
                return item
        attention = [item for item in ordered if item.needs_attention]
        selected = attention[0] if attention else ordered[0]
        self.selected_key = selected.key
        return selected

    def move(self, amount: int) -> None:
        ordered = self.ordered()
        selected = self.selected()
        if not ordered or selected is None:
            return
        index = next(i for i, item in enumerate(ordered) if item.key == selected.key)
        index = max(0, min(len(ordered) - 1, index + amount))
        self.selected_key = ordered[index].key

    def edge(self, *, last: bool) -> None:
        ordered = self.ordered()
        if ordered:
            self.selected_key = ordered[-1 if last else 0].key

    def notify(self, message: str, *, now: float | None = None) -> None:
        self.toast = terminal_text(message)
        self.toast_until = (now or time.monotonic()) + 3.0

    def expert_card(self, session: Session | None) -> ExpertCardState | None:
        return self.expert_cards.get(session.key) if session is not None else None


def strip_terminal_sequences(value: str) -> str:
    return terminal_text(_ANSI_ESCAPE.sub("", value))


def _fit(value: object, width: int, *, align: str = "left") -> str:
    text = terminal_text(value)
    if width <= 0:
        return ""
    if len(text) > width:
        text = text[: max(1, width - 1)] + "…"
    return text.rjust(width) if align == "right" else text.ljust(width)


def _paint(value: str, code: str, enabled: bool) -> str:
    return f"{code}{value}{RESET}" if enabled and value else value


def _status_code(status: str) -> str:
    return {
        Status.NEEDS_YOU.value: FG_YELLOW,
        Status.OPEN_TWICE.value: FG_RED,
        Status.ERROR.value: FG_RED,
        Status.READY.value: FG_GREEN,
        Status.WORKING.value: FG_CYAN,
        Status.PARKED.value: FG_BRIGHT_BLACK,
        Status.UNBOUND.value: FG_MAGENTA,
    }.get(status, "")


def _line(left: str, right: str, width: int) -> str:
    left = terminal_text(left)
    right = terminal_text(right)
    if len(right) + 2 >= width:
        return _fit(right, width, align="right")
    room = width - len(right) - 2
    return _fit(left, room) + "  " + right


def _session_counts(sessions: list[Session]) -> dict[str, int]:
    return {
        "decisions": sum(item.status == Status.NEEDS_YOU.value for item in sessions),
        "results": sum(
            item.status == Status.READY.value and item.unread for item in sessions
        ),
        "working": sum(item.status == Status.WORKING.value for item in sessions),
        "parked": sum(item.status == Status.PARKED.value for item in sessions),
        "errors": sum(
            item.status in {Status.ERROR.value, Status.OPEN_TWICE.value} and item.unread
            for item in sessions
        ),
        "unbound": sum(item.status == Status.UNBOUND.value for item in sessions),
        "protected": sum(item.exact_home for item in sessions),
    }


def _noun(value: int, singular: str, plural: str | None = None) -> str:
    return singular if value == 1 else (plural or singular + "s")


def build_handoff(
    sessions: list[Session], *, since: float, now: float
) -> HandoffSummary | None:
    if not since or now - since < MORNING_GAP_SECONDS:
        return None
    recent = [item for item in sessions if item.last_event_at > since]
    return HandoffSummary(
        since=since,
        finished=sum(
            item.status == Status.READY.value and item.unread for item in recent
        ),
        decisions=sum(item.status == Status.NEEDS_YOU.value for item in recent),
        errors=sum(
            item.status in {Status.ERROR.value, Status.OPEN_TWICE.value}
            for item in recent
        ),
    )


def _handoff_text(handoff: HandoffSummary) -> str:
    if handoff.first:
        return (
            f"FIRST HANDOFF // CURRENT STATE · {handoff.finished} results · "
            f"{handoff.decisions} decisions · {handoff.errors} errors"
        )
    if not handoff.changed:
        return "SINCE YOUR LAST VISIT · no new handoffs"
    return (
        f"SINCE YOUR LAST VISIT · {handoff.finished} finished · "
        f"{handoff.decisions} {_noun(handoff.decisions, 'decision')} · "
        f"{handoff.errors} {_noun(handoff.errors, 'error')}"
    )


def _briefing_lines(
    sessions: list[Session],
    *,
    width: int,
    handoff: HandoffSummary | None,
    initialized: bool,
    refresh_error: str | None,
) -> tuple[str, str]:
    if not initialized:
        return (
            _fit("INITIAL HANDOFF · scanning current workstreams", width),
            _fit("No state is shown until the first reconciliation completes", width),
        )
    counts = _session_counts(sessions)
    interventions = counts["decisions"] + counts["errors"] + counts["unbound"]
    if not interventions and not counts["results"]:
        prefix = "LAST KNOWN" if refresh_error else "NO ATTENTION PENDING"
        headline = (
            f"{prefix} · {counts['working']} working · "
            f"{counts['parked']} parked · {counts['protected']} exact live"
        )
    elif not interventions:
        headline = (
            f"{counts['results']} {_noun(counts['results'], 'RESULT')} "
            "READY TO COLLECT · "
            f"{counts['working']} STILL WORKING"
        )
    elif width < 80:
        headline = (
            f"NEED {counts['decisions']} · RESULTS {counts['results']} · "
            f"FAILED {counts['errors']} · UNBOUND {counts['unbound']}"
        )
    else:
        headline = (
            f"YOU'RE NEEDED IN {interventions} "
            f"{_noun(interventions, 'PLACE')} · "
            f"{counts['results']} "
            f"{_noun(counts['results'], 'RESULT', 'RESULTS')} WAITING · "
            f"{counts['working']} STILL WORKING"
        )
        if counts["errors"]:
            headline += f" · {counts['errors']} FAILED"
        if counts["unbound"]:
            headline += f" · {counts['unbound']} UNBOUND"

    if handoff is not None:
        context = _handoff_text(handoff)
    else:
        attention = sorted_attention_sessions(
            item for item in sessions if item.needs_attention
        )
        if attention:
            next_item = attention[0]
            reason = next_item.error or next_item.attention_reason or next_item.status
            context = f"NEXT → {next_item.display_name} · {reason} · n to open"
        else:
            unbound = next(
                (item for item in sessions if item.status == Status.UNBOUND.value),
                None,
            )
            context = (
                f"NEXT → {unbound.display_name} · run pika adopt"
                if unbound
                else (
                    f"WORKING {counts['working']} · PARKED {counts['parked']} · "
                    f"UNBOUND {counts['unbound']}"
                )
            )
    return _fit(headline, width), _fit(context, width)


@dataclass(frozen=True, slots=True)
class _Column:
    label: str
    width: int
    field: str


def _columns(width: int, *, show_usage: bool) -> list[_Column]:
    columns = [
        _Column("AG", 2, "provider"),
        _Column("NAME", 20, "name"),
        _Column("STATE", 10, "status"),
        _Column("SINCE", 9, "age"),
    ]
    if show_usage:
        if width >= 74:
            columns.extend([_Column("CPU", 6, "cpu"), _Column("RAM", 6, "ram")])
        if width >= 94:
            columns.append(_Column("TOKENS", 8, "tokens"))
        if width >= 112:
            columns.append(_Column("MODEL", 14, "model"))
        if width >= 128:
            columns.append(_Column("API-EQUIV", 10, "cost"))
    else:
        if width >= 74:
            columns.insert(3, _Column("WHY", 10, "why"))
            columns.insert(4, _Column("VIEW", 4, "view"))
        if width >= 104:
            columns.insert(-1, _Column("REPO", 18, "repo"))
        if width >= 122:
            repo_index = next(
                i for i, item in enumerate(columns) if item.field == "repo"
            )
            columns.insert(repo_index + 1, _Column("BRANCH", 12, "branch"))
    used = 3 + sum(item.width for item in columns) + 2 * (len(columns) - 1)
    extra = max(0, width - used)
    mutable = list(columns)
    name_index = next(i for i, item in enumerate(mutable) if item.field == "name")
    name_extra = min(extra, 12)
    item = mutable[name_index]
    mutable[name_index] = _Column(item.label, item.width + name_extra, item.field)
    extra -= name_extra
    flexible_index = next(
        (i for i, item in enumerate(mutable) if item.field == "repo"),
        name_index,
    )
    if extra:
        item = mutable[flexible_index]
        mutable[flexible_index] = _Column(item.label, item.width + extra, item.field)
    return mutable


def _human_age_at(timestamp: float, now: float) -> str:
    if not timestamp:
        return "—"
    seconds = max(0, int(now - timestamp))
    if seconds < 60:
        return f"{seconds}s"
    minutes = seconds // 60
    if minutes < 60:
        return f"{minutes}m"
    hours = minutes // 60
    if hours < 48:
        return f"{hours}h"
    days = hours // 24
    return f"{days}d" if days < 60 else f"{days // 30}mo"


def semantic_age(session: Session, now: float) -> str:
    prefix = {
        Status.NEEDS_YOU.value: "WAIT",
        Status.OPEN_TWICE.value: "DUPLICATE",
        Status.READY.value: "RESULT",
        Status.WORKING.value: "ACTIVE",
        Status.PARKED.value: "IDLE",
        Status.ERROR.value: "FAILED",
        Status.UNBOUND.value: "UNBOUND",
    }.get(session.status, "AGE")
    timestamp = (
        session.last_event_at
        if session.status
        in {
            Status.NEEDS_YOU.value,
            Status.READY.value,
            Status.ERROR.value,
            Status.OPEN_TWICE.value,
        }
        else session.last_activity_at
    )
    return f"{prefix} {_human_age_at(timestamp, now)}"


def _field(session: Session, field: str, width: int, now: float) -> str:
    values = {
        "provider": PROVIDER_MARK.get(session.provider, "?"),
        "name": session.display_name,
        "status": session.status,
        "why": session.attention_reason or "—",
        "view": "yes" if session.attached else ("no" if session.tmux_session else "—"),
        "repo": short_path(session.cwd, width),
        "branch": session.branch or "—",
        "age": semantic_age(session, now),
        "cpu": (
            f"{session.cpu_percent:.1f}%" if session.cpu_percent is not None else "—"
        ),
        "ram": format_bytes(session.rss_kb),
        "tokens": format_tokens(session.total_tokens),
        "cost": format_cost(session.estimated_cost_usd),
        "model": session.model or "—",
    }
    return values[field]


def _table_row(
    session: Session,
    columns: list[_Column],
    *,
    selected: bool,
    color: bool,
    now: float,
) -> tuple[str, str]:
    marker = ("›" if selected else " ") + ("◆" if session.unread else " ")
    plain_cells = [
        _fit(_field(session, item.field, item.width, now), item.width)
        for item in columns
    ]
    row_width = sum(item.width for item in columns) + 2 * (len(columns) - 1) + 3
    plain = _fit(f"{marker} " + "  ".join(plain_cells), row_width)
    if selected:
        return plain, _paint(plain, REVERSE + BOLD, color)
    rendered = list(plain_cells)
    status_index = next(i for i, item in enumerate(columns) if item.field == "status")
    rendered[status_index] = _paint(
        rendered[status_index], _status_code(session.status), color
    )
    if session.unread:
        marker = " " + _paint("◆", FG_MAGENTA, color)
    return plain, f"{marker} " + "  ".join(rendered)


def _visible_rows(
    ordered: list[Session], selected: Session | None, slots: int
) -> tuple[list[Session], int]:
    if len(ordered) <= slots:
        return ordered, 0
    selected_index = 0
    if selected is not None:
        selected_index = next(
            (i for i, item in enumerate(ordered) if item.key == selected.key), 0
        )
    start = max(0, min(len(ordered) - slots, selected_index - slots // 2))
    return ordered[start : start + slots], start


def _help_lines(state: MonitorState, width: int, slots: int) -> list[str]:
    counts = _session_counts(state.sessions)
    if width < 80:
        items = [
            "↑↓/jk move · Enter open · n next",
            "a ask · p peek · u usage · r refresh",
            "g/G ends · ? close · q/Esc close",
            (
                f"Need {counts['decisions']} · results {counts['results']} · "
                f"failed {counts['errors']} · unbound {counts['unbound']}"
            ),
        ]
    else:
        items = [
            "↑/k  previous workstream        ↓/j  next workstream",
            "Enter open selected             n    open oldest attention",
            "a     ask selected privately     p    peek recent pane output",
            "u     operations / usage view    r    reconcile now",
            "g/G   first / last               ?    close this help",
            "q/Esc close this help",
            "",
            (
                f"Inventory: {counts['decisions']} decisions · "
                f"{counts['results']} results · "
                f"{counts['working']} working · {counts['parked']} parked · "
                f"{counts['errors']} errors · {counts['unbound']} unbound"
            ),
            "Pika never guesses identity. Enter acts on the selected provider + UUID.",
            "Operations update every 2s; visible usage updates every 30s.",
        ]
    return [_fit(value, width) for value in items[:slots]]


def _playbook_options(
    sessions: list[Session],
    *,
    selected: Session | None,
    refresh_error: str | None,
) -> list[tuple[str, str, str]]:
    if refresh_error:
        categories = ["refresh", "doctor"]
    elif selected and selected.status in {
        Status.ERROR.value,
        Status.OPEN_TWICE.value,
    }:
        categories = ["doctor", "peek"]
    elif selected and selected.status == Status.UNBOUND.value:
        categories = ["adopt", "doctor"]
    elif any(
        item.status in {Status.ERROR.value, Status.OPEN_TWICE.value}
        for item in sessions
    ):
        categories = ["doctor", "next"]
    elif any(item.status == Status.UNBOUND.value for item in sessions):
        categories = ["adopt", "doctor"]
    elif any(item.status == Status.NEEDS_YOU.value for item in sessions):
        categories = ["next", "peek"]
    elif any(item.status == Status.READY.value and item.unread for item in sessions):
        categories = ["peek", "next"]
    elif any(item.status == Status.WORKING.value for item in sessions):
        categories = ["detach", "wait", "experts", "ask"]
    elif any(item.status == Status.PARKED.value for item in sessions):
        categories = ["resume", "switch", "experts", "ask"]
    else:
        categories = ["name", "switch", "experts", "ask", "doctor"]
    names: dict[str, set[str]] = {}
    for item in sessions:
        names.setdefault(item.display_name.casefold(), set()).add(item.provider)
    if any(providers == {"codex", "claude"} for providers in names.values()):
        categories.insert(0, "collision")
    ordered: list[tuple[str, str, str]] = []
    for category in dict.fromkeys(categories):
        ordered.extend(item for item in PLAYBOOK_TIPS if item[0] == category)
    return ordered or list(PLAYBOOK_TIPS)


def playbook_tip(
    now: float,
    sessions: list[Session] | None = None,
    *,
    selected: Session | None = None,
    refresh_error: str | None = None,
    compact: bool = False,
) -> tuple[int, int, str]:
    options = _playbook_options(
        sessions or [], selected=selected, refresh_error=refresh_error
    )
    index = int(now // PLAYBOOK_ROTATION_SECONDS) % len(options)
    _category, wide, narrow = options[index]
    return index, len(options), narrow if compact else wide


def _peek_view(state: MonitorState, width: int, slots: int) -> list[str]:
    lines = state.peek_lines
    if not lines:
        return [_fit("No pane output available.", width)]
    end = max(0, len(lines) - state.peek_offset)
    start = max(0, end - slots)
    visible = lines[start:end]
    return [_fit(line, width) for line in visible]


def _identity_text(session: Session) -> str:
    provider = session.provider.title()
    fingerprint = session.session_id[:8]
    if session.home_state == "exact-live":
        return f"EXACT HOME · {provider} · id {fingerprint} · PROTECTED LIVE"
    if session.home_state == "identity-error":
        return f"IDENTITY UNVERIFIED · {provider} · id {fingerprint} · OPEN BLOCKED"
    if session.home_state == "open-twice":
        return f"OPEN TWICE · {provider} · id {fingerprint} · CLOSE ONE COPY"
    if session.home_state == "unbound":
        return f"UNBOUND PROCESS · {provider} · id {fingerprint} · ADOPT REQUIRED"
    if session.home_state == "outside-live":
        return f"LIVE OUTSIDE PIKA · {provider} · id {fingerprint} · NOT PROTECTED"
    if session.home_state == "saved-idle":
        return f"SAVED HOME · {provider} · id {fingerprint} · NOT LIVE"
    if session.home_state == "no-live-home":
        return f"TRACKED WORKSTREAM · {provider} · id {fingerprint} · NO LIVE HOME"
    return f"HOME STATE UNKNOWN · {provider} · id {fingerprint}"


def _age_phrase(session: Session, now: float) -> str:
    event_age = _human_age_at(session.last_event_at, now)
    activity_age = _human_age_at(session.last_activity_at, now)
    return {
        Status.NEEDS_YOU.value: f"waiting on you for {event_age}",
        Status.READY.value: f"result ready for {event_age}",
        Status.WORKING.value: f"last active {activity_age} ago",
        Status.PARKED.value: f"last active {activity_age} ago",
        Status.OPEN_TWICE.value: f"duplicate open for {event_age}",
        Status.ERROR.value: f"failed {event_age} ago",
        Status.UNBOUND.value: f"last active {activity_age} ago",
    }.get(session.status, f"last active {activity_age} ago")


def _needs_you_group(session: Session) -> bool:
    return session.status in {
        Status.NEEDS_YOU.value,
        Status.ERROR.value,
        Status.OPEN_TWICE.value,
    } or (session.status == Status.READY.value and session.unread)


def _split_groups(sessions: list[Session]) -> list[tuple[str, list[Session]]]:
    ordered = sorted_sessions(sessions)
    definitions = (
        ("NEEDS YOU", _needs_you_group),
        ("WORKING", lambda item: item.status == Status.WORKING.value),
        ("UNBOUND", lambda item: item.status == Status.UNBOUND.value),
        (
            "READY",
            lambda item: item.status == Status.READY.value and not item.unread,
        ),
        ("PARKED", lambda item: item.status == Status.PARKED.value),
    )
    assigned: set[tuple[str, str]] = set()
    groups: list[tuple[str, list[Session]]] = []
    for label, predicate in definitions:
        members = [item for item in ordered if predicate(item) and item.key not in assigned]
        if members:
            assigned.update(item.key for item in members)
            groups.append((label, members))
    other = [item for item in ordered if item.key not in assigned]
    if other:
        groups.append(("OTHER", other))
    return groups


def _group_color(label: str) -> str:
    return {
        "NEEDS YOU": FG_RED,
        "WORKING": FG_CYAN,
        "UNBOUND": FG_MAGENTA,
        "READY": FG_GREEN,
        "PARKED": FG_BRIGHT_BLACK,
    }.get(label, FG_BLUE)


def _split_left_pane(
    state: MonitorState,
    *,
    width: int,
    height: int,
    now: float,
    color: bool,
) -> tuple[list[str], list[str]]:
    selected = state.selected()
    entries: list[tuple[str, Session | None]] = []
    for index, (label, members) in enumerate(_split_groups(state.sessions)):
        if index:
            entries.append(("", None))
        entries.append((label, None))
        entries.extend(("", item) for item in members)

    selected_index = next(
        (
            index
            for index, (_label, item) in enumerate(entries)
            if item is not None and selected is not None and item.key == selected.key
        ),
        0,
    )
    if len(entries) > height:
        start = max(0, min(len(entries) - height, selected_index - height // 2))
        entries = entries[start : start + height]

    plain_lines: list[str] = []
    ansi_lines: list[str] = []
    for label, item in entries:
        if item is None:
            plain = _fit(label, width)
            ansi = _paint(plain, DIM + _group_color(label), color) if label else plain
        else:
            is_selected = selected is not None and item.key == selected.key
            marker = "›" if is_selected else " "
            signal = "◆" if _needs_you_group(item) else "□" if item.live else "·"
            provider = PROVIDER_MARK.get(item.provider, "?")
            age = (
                "adopt"
                if item.status == Status.UNBOUND.value
                else _human_age_at(
                    item.last_event_at
                    if item.status
                    in {
                        Status.NEEDS_YOU.value,
                        Status.READY.value,
                        Status.ERROR.value,
                        Status.OPEN_TWICE.value,
                    }
                    else item.last_activity_at,
                    now,
                )
            )
            prefix = f"{marker}{signal} {provider} "
            name_width = max(4, width - len(prefix) - len(age) - 1)
            plain = _fit(
                f"{prefix}{_fit(item.display_name, name_width)} {age}", width
            )
            if is_selected:
                ansi = _paint(plain, REVERSE + BOLD, color)
            else:
                ansi = _paint(plain, _status_code(item.status), color)
        plain_lines.append(plain)
        ansi_lines.append(ansi)

    while len(plain_lines) < height:
        plain_lines.append(" " * width)
        ansi_lines.append(" " * width)
    return plain_lines[:height], ansi_lines[:height]


def _detail_value(label: str, value: str, width: int) -> str:
    label_width = 9
    return _fit(f"{_fit(label, label_width)}{terminal_text(value)}", width)


def _wrapped_detail(
    value: str, *, width: int, first_prefix: str = "", continuation: str = ""
) -> list[str]:
    available = max(1, width - len(first_prefix))
    chunks = wrap(
        terminal_text(value),
        width=available,
        break_long_words=False,
        break_on_hyphens=False,
    ) or [""]
    lines = [_fit(first_prefix + chunks[0], width)]
    continuation = continuation or " " * len(first_prefix)
    following_width = max(1, width - len(continuation))
    for chunk in chunks[1:]:
        for part in wrap(
            chunk,
            width=following_width,
            break_long_words=False,
            break_on_hyphens=False,
        ) or [""]:
            lines.append(_fit(continuation + part, width))
    return lines


def _card_display_status(card: ExpertCardState | None) -> str:
    if card is None:
        return "LOADING"
    return {
        "CURRENT": "CURRENT",
        "STALE": "+NEW CONTEXT",
        "MISSING": "NOT INTERVIEWED",
        "UNKNOWN": "SOURCE UNAVAILABLE",
    }.get(card.status, card.status)


def _split_right_pane(
    state: MonitorState,
    *,
    width: int,
    height: int,
    now: float,
    color: bool,
) -> tuple[list[str], list[str]]:
    selected = state.selected()
    if selected is None:
        plain = [
            _fit("NO WORKSTREAM SELECTED", width),
            _fit("Choose a workstream on the left.", width),
        ]
        return (
            (plain + [" " * width] * height)[:height],
            ([_paint(plain[0], BOLD, color), plain[1]] + [" " * width] * height)[
                :height
            ],
        )

    card = state.expert_card(selected)
    profile = card.profile if card else None
    signal = selected.error or selected.attention_reason or "no exception reported"
    status_mark = "◆" if _needs_you_group(selected) else "□"
    title = _line(selected.display_name, f"{status_mark} {selected.status}", width)
    active_handoff = (
        state.handoff
        if state.handoff and time.monotonic() < state.handoff_until
        else None
    )

    plain: list[str] = [title]
    ansi: list[str] = [_paint(title, BOLD, color)]

    if state.refresh_error:
        notice = _fit(f"REFRESH ERROR // {state.refresh_error}", width)
        plain.append(notice)
        ansi.append(_paint(notice, FG_RED, color))
    elif state.toast and time.monotonic() < state.toast_until:
        notice = _fit(f"NOTICE // {state.toast}", width)
        plain.append(notice)
        ansi.append(_paint(notice, FG_YELLOW, color))
    elif active_handoff is not None:
        notice = _fit(_handoff_text(active_handoff), width)
        plain.append(notice)
        ansi.append(_paint(notice, FG_MAGENTA, color))

    metadata = [
        _detail_value("signal", f"{signal} · {_age_phrase(selected, now)}", width),
        _detail_value(
            "agent", f"{selected.provider.title()} · id {selected.session_id[:8]}", width
        ),
        _detail_value("path", selected.cwd or "—", width),
        _detail_value("branch", selected.branch or "—", width),
        _detail_value("home", _identity_text(selected), width),
    ]
    plain.extend(metadata)
    ansi.extend(
        [
            metadata[0],
            _paint(metadata[1], DIM, color),
            metadata[2],
            metadata[3],
            _paint(metadata[4], FG_GREEN if selected.exact_home else FG_MAGENTA, color),
        ]
    )

    plain.append(" " * width)
    ansi.append(" " * width)
    card_heading = _line(
        "EXPERT CARD",
        _card_display_status(card),
        width,
    )
    plain.append(card_heading)
    card_color = (
        FG_GREEN
        if card and card.status == "CURRENT"
        else FG_YELLOW
        if card and card.status == "STALE"
        else FG_MAGENTA
    )
    ansi.append(_paint(card_heading, BOLD + card_color, color))

    if profile is not None:
        summary_lines = _wrapped_detail(profile.summary, width=width)
        topic_lines = _wrapped_detail(
            " · ".join(profile.topics),
            width=width,
            first_prefix="knows    ",
            continuation="         ",
        )
        card_plain = [*summary_lines[:2], *topic_lines[:2]]
        card_ansi = [
            *summary_lines[:2],
            *[_paint(line, FG_BLUE, color) for line in topic_lines[:2]],
        ]
        if profile.artifacts and height >= 24:
            artifact = _detail_value("artifact", profile.artifacts[0], width)
            card_plain.append(artifact)
            card_ansi.append(_paint(artifact, DIM, color))
        plain.extend(card_plain)
        ansi.extend(card_ansi)
    elif card is not None:
        message = _fit(f"{card.detail}. Pika will keep this state explicit.", width)
        plain.append(message)
        ansi.append(_paint(message, DIM, color))
    else:
        message = _fit("Loading the UUID-bound expert card…", width)
        plain.append(message)
        ansi.append(_paint(message, DIM, color))

    if state.show_usage:
        plain.append(" " * width)
        ansi.append(" " * width)
        usage_heading = _fit(
            f"USAGE // PROVIDER COUNTERS · API-EQUIV EST {PRICING_AS_OF}", width
        )
        usage = _fit(
            (
                f"CPU {selected.cpu_percent:.1f}%"
                if selected.cpu_percent is not None
                else "CPU —"
            )
            + (
                f" · RAM {format_bytes(selected.rss_kb)} · "
                f"TOKENS {format_tokens(selected.total_tokens)} · "
                f"API-EQUIV {format_cost(selected.estimated_cost_usd)}"
            ),
            width,
        )
        plain.extend([usage_heading, usage])
        ansi.extend([_paint(usage_heading, BOLD + FG_CYAN, color), usage])

    action_heading = _fit("DO SOMETHING", width)
    if selected.status == Status.UNBOUND.value and selected.live:
        actions = "Enter blocked · run pika adopt to create an exact home"
        reassurance = "Pika will not guess ownership for a live external process."
    elif selected.home_state in {"identity-error", "open-twice"}:
        ask = " · [a] ask privately" if selected.transcript_path else ""
        actions = f"[Enter] blocked{ask} · run pika doctor --verbose"
        reassurance = "The saved conversation remains visible; pane identity is fail-closed."
    else:
        open_label = (
            "collect result"
            if selected.status == Status.READY.value and selected.unread
            else "open"
        )
        ask = " · [a] ask privately" if selected.transcript_path else ""
        actions = f"[Enter] {open_label}{ask} · [p] peek"
        reassurance = (
            "Private asks are ephemeral; the parent transcript remains unchanged."
            if selected.transcript_path
            else "This conversation has no durable transcript available for side asks."
        )
    action_block = [
        " " * width,
        action_heading,
        _fit(actions, width),
        _fit(reassurance, width),
    ]

    preview_slots = height - len(plain) - len(action_block)
    if preview_slots >= 3:
        preview_heading = _line("LIVE PANE TAIL", "READ-ONLY · UNREAD PRESERVED", width)
        preview_body_slots = preview_slots - 2
        if state.preview_key == selected.key and state.preview_lines:
            preview = state.preview_lines[-preview_body_slots:]
        elif state.preview_key == selected.key and state.preview_error:
            preview = [f"Preview unavailable: {state.preview_error}"]
        elif selected.tmux_pane or selected.tmux_session:
            preview = ["Waiting for the first selected-pane capture…"]
        elif selected.home_state in {"identity-error", "open-twice"}:
            preview = ["No trusted pane. Opening stays blocked until identity recovers."]
        else:
            preview = ["No live pane. Enter restores this exact saved conversation."]
        preview = [_fit(line, width) for line in preview[:preview_body_slots]]
        while len(preview) < preview_body_slots:
            preview.append(" " * width)
        plain.extend([" " * width, preview_heading, *preview])
        ansi.extend(
            [
                " " * width,
                _paint(preview_heading, DIM + FG_CYAN, color),
                *[_paint(line, FG_BRIGHT_BLACK, color) for line in preview],
            ]
        )
    if len(plain) + len(action_block) <= height:
        plain.extend(action_block)
        ansi.extend(
            [
                " " * width,
                _paint(action_heading, DIM + FG_MAGENTA, color),
                _paint(_fit(actions, width), BOLD, color),
                _paint(_fit(reassurance, width), DIM, color),
            ]
        )

    while len(plain) < height:
        plain.append(" " * width)
        ansi.append(" " * width)
    return plain[:height], ansi[:height]


def _render_split_monitor(
    state: MonitorState,
    *,
    width: int,
    height: int,
    now: float,
    refreshing: bool,
    color: bool,
) -> MonitorFrame:
    counts = _session_counts(state.sessions)
    need_count = sum(_needs_you_group(item) for item in state.sessions)
    expert_count = sum(
        card.profile is not None for card in state.expert_cards.values()
    )
    refresh_age = max(0.0, now - state.refresh_started_at)
    show_refresh = refreshing and (
        not state.last_update
        or state.emphasize_refresh
        or refresh_age >= SLOW_REFRESH_SECONDS
    )
    if state.refresh_error:
        sync = (
            f"STALE {max(0, int(now - state.last_update))}s"
            if state.last_update
            else "SCAN FAILED"
        )
    elif state.refresh_warning:
        sync = "PARTIAL"
    elif show_refresh:
        sync = f"{SPINNER[int(now * 4) % len(SPINNER)]} SYNCING"
    else:
        sync = "●"
    clock = datetime.fromtimestamp(now).strftime("%H:%M:%S")
    summary = [
        f"{need_count} need you",
        f"{counts['working']} working",
        f"{counts['unbound']} unbound",
    ]
    if state.expert_cards_updated_at:
        summary.append(f"{expert_count} {_noun(expert_count, 'expert')}")
    status_clock = (
        f"{sync} {clock}"
        if sync == "●" or any(sync.startswith(frame) for frame in SPINNER)
        else f"{sync} · {clock}"
    )
    summary.append(status_clock)
    view = " · USAGE" if state.show_usage else ""
    header_plain = _line(
        f"PIKA // LIVE OPERATIONS{view}",
        " · ".join(summary),
        width,
    )
    header_ansi = _paint(header_plain, BOLD + FG_CYAN, color)

    body_height = max(1, height - 3)
    left_width = max(32, min(44, width // 3))
    divider = " │ "
    right_width = width - left_width - len(divider)
    left_plain, left_ansi = _split_left_pane(
        state, width=left_width, height=body_height, now=now, color=color
    )
    right_plain, right_ansi = _split_right_pane(
        state, width=right_width, height=body_height, now=now, color=color
    )
    divider_ansi = _paint(divider, FG_BRIGHT_BLACK, color)
    body_plain = [
        left_plain[index] + divider + right_plain[index]
        for index in range(body_height)
    ]
    body_ansi = [
        left_ansi[index] + divider_ansi + right_ansi[index]
        for index in range(body_height)
    ]

    selected = state.selected()
    tip_index, tip_total, tip = playbook_tip(
        now,
        state.sessions,
        selected=selected,
        refresh_error=state.refresh_error,
        compact=width < 120,
    )
    playbook_plain = _fit(
        f"PIKA PLAYBOOK {tip_index + 1}/{tip_total} // {tip}", width
    )
    playbook_ansi = _paint(playbook_plain, FG_BLUE, color)
    footer_plain = _fit(
        "↑↓/jk move  Enter open  a ask privately  n next needed  p peek  "
        f"u {'operations' if state.show_usage else 'usage'}  r refresh  ? keys  q quit",
        width,
    )
    footer_ansi = _paint(footer_plain, REVERSE, color)
    plain_lines = [header_plain, *body_plain, playbook_plain, footer_plain]
    ansi_lines = [header_ansi, *body_ansi, playbook_ansi, footer_ansi]
    return MonitorFrame(
        "\n".join(ansi_lines[:height]),
        "\n".join(_fit(line, width) for line in plain_lines[:height]),
        state.selected_key,
    )


def render_monitor(
    state: MonitorState,
    *,
    width: int,
    height: int,
    now: float | None = None,
    refreshing: bool = False,
    color: bool = True,
) -> MonitorFrame:
    now = time.time() if now is None else now
    width = max(1, width)
    height = max(1, height)
    selected = state.selected()

    if width < MIN_WIDTH or height < MIN_HEIGHT:
        minimum = [
            _line(
                "PIKA // LIVE",
                datetime.fromtimestamp(now).strftime("%H:%M:%S"),
                width,
            ),
            "─" * width,
            _fit(f"Terminal too small · need {MIN_WIDTH}x{MIN_HEIGHT}", width),
            _fit(f"Current viewport · {width}x{height}", width),
            "",
            _fit("q quit", width),
        ]
        minimum = (minimum + [""] * height)[:height]
        plain = "\n".join(_fit(line, width) for line in minimum)
        return MonitorFrame(plain, plain, state.selected_key)

    if (
        width >= SPLIT_MIN_WIDTH
        and height >= SPLIT_MIN_HEIGHT
        and state.mode == "sessions"
        and state.sessions
    ):
        return _render_split_monitor(
            state,
            width=width,
            height=height,
            now=now,
            refreshing=refreshing,
            color=color,
        )

    refresh_age = max(0.0, now - state.refresh_started_at)
    show_refresh = refreshing and (
        not state.last_update
        or state.emphasize_refresh
        or refresh_age >= SLOW_REFRESH_SECONDS
    )
    if state.refresh_error:
        freshness = (
            f"! STALE · last update {max(0, int(now - state.last_update))}s ago"
            if state.last_update
            else "! INITIAL SCAN FAILED"
        )
    elif state.refresh_warning:
        freshness = f"! PARTIAL · UPDATED {max(0, int(now - state.last_update))}s AGO"
    elif show_refresh:
        spinner = SPINNER[int(now * 4) % len(SPINNER)]
        freshness = f"{spinner} {'SCANNING' if not state.last_update else 'SYNCING'}"
    elif state.last_update:
        freshness = f"● UPDATED {max(0, int(now - state.last_update))}s AGO"
    else:
        freshness = "○ WAITING FOR FIRST SCAN"
    clock = datetime.fromtimestamp(now).strftime("%H:%M:%S")
    view_label = " · USAGE VIEW" if state.show_usage else ""
    if state.show_usage and width >= 112:
        view_label += f" · PROVIDER COUNTERS · API-EQUIV EST {PRICING_AS_OF}"
    if state.show_usage and state.usage_warning:
        view_label += " · USAGE PARTIAL"
    header_plain = _line(
        f"PIKA // LIVE OPERATIONS{view_label}  {len(state.sessions)} WORKSTREAMS",
        f"{freshness}  {clock}",
        width,
    )
    header_ansi = _paint(header_plain, BOLD + FG_CYAN, color)

    active_handoff = (
        state.handoff
        if state.handoff and time.monotonic() < state.handoff_until
        else None
    )
    briefing_plain = _briefing_lines(
        state.sessions,
        width=width,
        handoff=active_handoff,
        initialized=bool(state.last_update),
        refresh_error=state.refresh_error,
    )
    counts = _session_counts(state.sessions)
    has_attention = bool(
        counts["decisions"]
        or counts["results"]
        or counts["errors"]
        or counts["unbound"]
    )
    briefing_ansi = (
        _paint(
            briefing_plain[0],
            BOLD + (FG_YELLOW if has_attention else FG_GREEN),
            color,
        ),
        _paint(
            briefing_plain[1],
            FG_MAGENTA if active_handoff else FG_BLUE,
            color,
        ),
    )

    rule = "─" * width
    columns = _columns(width, show_usage=state.show_usage)
    table_width = 3 + sum(item.width for item in columns) + 2 * (len(columns) - 1)
    table_header = _fit(
        "   " + "  ".join(_fit(item.label, item.width) for item in columns),
        width,
    )

    row_slots = max(1, height - 11)
    ordered = state.ordered()
    rows, start = _visible_rows(ordered, selected, row_slots)
    table_plain: list[str] = []
    table_ansi: list[str] = []

    if state.mode == "help":
        title = _fit("HELP // KEYS", width)
        table_plain = [title, *_help_lines(state, width, row_slots - 1)]
        table_ansi = [_paint(title, BOLD + FG_BLUE, color), *table_plain[1:]]
    elif state.mode == "peek":
        name = selected.display_name if selected else "—"
        title = _fit(f"PEEK // {name}  ↑↓ scroll · p/Esc close · Enter open", width)
        table_plain = [title, *_peek_view(state, width, row_slots - 1)]
        table_ansi = [_paint(title, BOLD + FG_MAGENTA, color), *table_plain[1:]]
    elif not ordered:
        message = "Scanning provider state…" if refreshing else "No managed homes yet."
        table_plain = [
            table_header,
            _fit("", width),
            _fit(message, width),
            _fit("Start one with: pika new NAME --agent codex|claude", width),
        ]
        table_ansi = [
            _paint(table_header, DIM, color),
            table_plain[1],
            _paint(table_plain[2], BOLD, color),
            table_plain[3],
        ]
    else:
        table_plain.append(table_header)
        table_ansi.append(_paint(table_header, DIM, color))
        for item in rows:
            plain_row, ansi_row = _table_row(
                item,
                columns,
                selected=bool(selected and item.key == selected.key),
                color=color,
                now=now,
            )
            table_plain.append(_fit(plain_row, width))
            table_ansi.append(ansi_row + " " * max(0, width - table_width))
        if len(ordered) > len(rows):
            position = f"rows {start + 1}-{start + len(rows)} of {len(ordered)}"
            table_plain[-1] = _line(table_plain[-1], position, width)
            if not color:
                table_ansi[-1] = table_plain[-1]

    while len(table_plain) < row_slots + 1:
        table_plain.append(" " * width)
        table_ansi.append(" " * width)
    table_plain = table_plain[: row_slots + 1]
    table_ansi = table_ansi[: row_slots + 1]

    if selected:
        index = next(
            (i for i, item in enumerate(ordered) if item.key == selected.key),
            0,
        )
        selection = _line(
            (
                f"SELECTED // {selected.display_name}  "
                f"{selected.provider.title()} · {selected.status}"
            ),
            (
                f"{index + 1}/{len(ordered)}  "
                f"{short_path(selected.cwd, max(8, width // 4))}"
                f"{' · ' + selected.branch if selected.branch else ''}"
            ),
            width,
        )
        identity = _identity_text(selected)
        signal = selected.error or selected.attention_reason or "No exception reported"
        if state.show_usage:
            resources = (
                f"CPU {selected.cpu_percent:.1f}%"
                if selected.cpu_percent is not None
                else "CPU —"
            ) + (
                f" · RAM {format_bytes(selected.rss_kb)} · "
                f"TOKENS {format_tokens(selected.total_tokens)} · "
                f"API-EQUIV {format_cost(selected.estimated_cost_usd)}"
            )
        else:
            resources = _age_phrase(selected, now)
        detail_plain = [
            _fit(selection, width),
            _fit(identity, width),
            _line(f"SIGNAL // {signal}", resources, width),
        ]
    else:
        detail_plain = [
            _fit("SELECTED // —", width),
            _fit("No workstream selected", width),
            " " * width,
        ]

    detail_ansi = [
        _paint(detail_plain[0], BOLD, color),
        _paint(detail_plain[1], DIM, color),
        detail_plain[2],
    ]
    if state.refresh_error:
        detail_plain[2] = _fit(f"REFRESH ERROR // {state.refresh_error}", width)
        detail_ansi[2] = _paint(detail_plain[2], FG_RED, color)
    elif state.toast and time.monotonic() < state.toast_until:
        detail_plain[2] = _fit(f"NOTICE // {state.toast}", width)
        detail_ansi[2] = _paint(detail_plain[2], FG_YELLOW, color)

    exit_copy = "q close" if state.mode in {"help", "peek"} else "q quit"
    footer_text = (
        f"↑↓ move  ↵ open  a ask  p peek  u usage  ? keys  {exit_copy}"
        if width < 80
        else (
            "↑↓/jk move  Enter open  a ask  n next  p peek  "
            f"u {'operations' if state.show_usage else 'usage'}  "
            f"? keys  {exit_copy}"
        )
    )
    footer_plain = _fit(footer_text, width)
    footer_ansi = _paint(footer_plain, REVERSE, color)
    tip_index, tip_total, tip = playbook_tip(
        now,
        state.sessions,
        selected=selected,
        refresh_error=state.refresh_error,
        compact=width < 90,
    )
    tip_label = "PIKA TIP" if width < 90 else "PIKA PLAYBOOK"
    playbook_plain = _fit(f"{tip_label} {tip_index + 1}/{tip_total} // {tip}", width)
    playbook_ansi = _paint(playbook_plain, FG_BLUE, color)

    plain_lines = [
        header_plain,
        *briefing_plain,
        rule,
        *table_plain,
        rule,
        *detail_plain,
        playbook_plain,
        footer_plain,
    ]
    ansi_lines = [
        header_ansi,
        *briefing_ansi,
        rule,
        *table_ansi,
        rule,
        *detail_ansi,
        playbook_ansi,
        footer_ansi,
    ]
    plain_lines = [_fit(line, width) for line in plain_lines[:height]]
    ansi_lines = ansi_lines[:height]
    while len(plain_lines) < height:
        plain_lines.append(" " * width)
        ansi_lines.append(" " * width)
    return MonitorFrame(
        "\n".join(ansi_lines),
        "\n".join(plain_lines),
        state.selected_key,
    )


def decode_keys(buffer: bytearray) -> list[str]:
    keys: list[str] = []
    while buffer:
        mouse = _MOUSE.match(buffer)
        if mouse:
            button = int(mouse.group(1))
            keys.append("up" if button == 64 else "down" if button == 65 else "mouse")
            del buffer[: mouse.end()]
            continue
        sequences = {
            b"\x1b[A": "up",
            b"\x1b[B": "down",
            b"\x1b[5~": "pageup",
            b"\x1b[6~": "pagedown",
        }
        matched = next(
            (
                (sequence, key)
                for sequence, key in sequences.items()
                if buffer.startswith(sequence)
            ),
            None,
        )
        if matched:
            sequence, key = matched
            keys.append(key)
            del buffer[: len(sequence)]
            continue
        if buffer[0] == 0x1B:
            if len(buffer) == 1:
                keys.append("escape")
                buffer.clear()
                continue
            if buffer.startswith(b"\x1b[") and len(buffer) < 6:
                break
            keys.append("escape")
            del buffer[0]
            continue
        value = chr(buffer.pop(0))
        keys.append(
            {
                "\r": "enter",
                "\n": "enter",
                "j": "down",
                "k": "up",
                "g": "first",
                "G": "last",
                "q": "quit",
                "?": "help",
                "p": "peek",
                "a": "ask",
                "n": "next",
                "u": "usage",
                "r": "refresh",
                "\x0c": "refresh",
            }.get(value, value)
        )
    return keys


class _Terminal:
    def __init__(self, input_fd: int, output_fd: int) -> None:
        self.input_fd = input_fd
        self.output_fd = output_fd
        self.previous: list | None = None

    def __enter__(self) -> _Terminal:
        self.previous = termios.tcgetattr(self.input_fd)
        tty.setcbreak(self.input_fd, termios.TCSANOW)
        os.write(
            self.output_fd,
            b"\x1b[?1049h\x1b[?25l\x1b[?1000h\x1b[?1006h",
        )
        return self

    def draw(self, frame: str) -> None:
        os.write(self.output_fd, ("\x1b[H" + frame).encode(errors="replace"))

    def __exit__(self, _type, _value, _traceback) -> None:
        os.write(
            self.output_fd,
            b"\x1b[0m\x1b[?1006l\x1b[?1000l\x1b[?25h\x1b[?1049l",
        )
        if self.previous is not None:
            termios.tcsetattr(self.input_fd, termios.TCSADRAIN, self.previous)


def _carry_usage(
    sessions: list[Session], cache: dict[tuple[str, str], dict[str, object]]
) -> None:
    fields = (
        "model",
        "input_tokens",
        "output_tokens",
        "cached_input_tokens",
        "cache_write_tokens",
        "total_tokens",
        "estimated_cost_usd",
    )
    for session in sessions:
        values = {field: getattr(session, field) for field in fields}
        if any(value is not None for value in values.values()):
            previous = cache.setdefault(session.key, {})
            previous.update(
                {key: value for key, value in values.items() if value is not None}
            )
        for key, value in cache.get(session.key, {}).items():
            if getattr(session, key) is None:
                setattr(session, key, value)


def _open_peek(pika: MonitorPika, state: MonitorState) -> None:
    session = state.selected()
    if session is None:
        state.notify("No workstream selected")
        return
    target = session.tmux_pane or session.tmux_session
    if not target:
        state.notify("No surviving Pika pane to peek")
        return
    try:
        captured = pika.tmux.capture(target, 300)
    except Exception as exc:  # noqa: BLE001 - monitor stays recoverable
        state.notify(f"Peek failed: {exc}")
        return
    state.peek_lines = [
        strip_terminal_sequences(line) for line in captured.splitlines()
    ]
    state.peek_offset = 0
    state.mode = "peek"
    if session.status == Status.READY.value and session.unread:
        if pika.acknowledge(session):
            session.unread = False
            state.notify("RESULT SEEN · unread state cleared")


def _capture_preview(
    pika: MonitorPika, session: Session
) -> tuple[tuple[str, str], list[str], str | None]:
    target = session.tmux_pane or session.tmux_session
    if not target:
        return session.key, [], None
    try:
        captured = pika.tmux.capture(target, 12)
    except Exception as exc:  # noqa: BLE001 - preview is optional and read-only
        return session.key, [], terminal_text(exc)
    lines = [strip_terminal_sequences(line) for line in captured.splitlines()]
    return session.key, lines[-12:], None


def _handle_key(
    key: str,
    pika: MonitorPika,
    state: MonitorState,
) -> tuple[str, Session | None]:
    if state.mode in {"help", "peek"} and key in {"quit", "escape"}:
        state.mode = "sessions"
        return "continue", None
    if state.mode == "help":
        if key == "help":
            state.mode = "sessions"
        return "continue", None
    if state.mode == "peek":
        if key in {"peek"}:
            state.mode = "sessions"
        elif key in {"up", "pageup"}:
            state.peek_offset = min(
                max(0, len(state.peek_lines) - 1),
                state.peek_offset + (10 if key == "pageup" else 1),
            )
        elif key in {"down", "pagedown"}:
            state.peek_offset = max(
                0, state.peek_offset - (10 if key == "pagedown" else 1)
            )
        elif key == "first":
            state.peek_offset = max(0, len(state.peek_lines) - 1)
        elif key == "last":
            state.peek_offset = 0
        elif key == "enter":
            return "open", state.selected()
        return "continue", None

    if key == "quit" or key == "escape":
        return "quit", None
    if key == "up":
        state.move(-1)
    elif key == "down":
        state.move(1)
    elif key == "pageup":
        state.move(-8)
    elif key == "pagedown":
        state.move(8)
    elif key == "first":
        state.edge(last=False)
    elif key == "last":
        state.edge(last=True)
    elif key == "help":
        state.mode = "help"
    elif key == "peek":
        _open_peek(pika, state)
    elif key == "usage":
        state.show_usage = not state.show_usage
        return "usage", None
    elif key == "enter":
        session = state.selected()
        if session and session.status == Status.UNBOUND.value and session.live:
            state.notify("Unbound live process — adopt it before opening")
        elif session and session.home_state in {"identity-error", "open-twice"}:
            state.notify("Exact pane identity is unverified — opening remains blocked")
        elif session:
            return "open", session
    elif key == "ask":
        session = state.selected()
        if session and session.status == Status.UNBOUND.value and session.live:
            state.notify("Unbound live process — adopt it before consulting")
        elif session and not session.transcript_path:
            state.notify("No durable provider transcript available for a side ask")
        elif session:
            return "ask", session
    elif key == "next":
        session = pika.next_attention(state.sessions)
        if session is None:
            state.notify("No workstream currently needs attention")
        else:
            return "open", session
    elif key == "refresh":
        return "refresh", None
    elif key != "mouse":
        state.notify("Unknown key · press ? for controls")
    return "continue", None


def _morning_handoff(
    pika: MonitorPika,
    sessions: list[Session],
    *,
    now: float,
) -> HandoffSummary | None:
    try:
        previous, counts = pika.store.claim_monitor_handoff(now)
    except (AttributeError, OSError):
        try:
            previous = pika.store.claim_monitor_visit(now)
        except (AttributeError, OSError):
            previous = None
        counts = {}
    if previous is None:
        counts = _session_counts(sessions)
        return HandoffSummary(
            since=now,
            finished=counts["results"],
            decisions=counts["decisions"],
            errors=counts["errors"],
            first=True,
        )
    snapshot = build_handoff(sessions, since=previous, now=now)
    if snapshot is None:
        return None
    if not counts:
        return snapshot
    journal = HandoffSummary(
        since=previous,
        finished=int(counts.get(Status.READY.value, 0)),
        decisions=int(counts.get(Status.NEEDS_YOU.value, 0)),
        errors=int(counts.get(Status.ERROR.value, 0))
        + int(counts.get(Status.OPEN_TWICE.value, 0)),
    )
    return journal if journal.changed or not snapshot.changed else snapshot


class _DaemonExecutor:
    """Tiny executor whose in-flight optional work cannot hold the TUI open."""

    def __enter__(self):
        return self

    def __exit__(self, *_args: object) -> None:
        return None

    @staticmethod
    def submit(callable_, *args, **kwargs):
        future = concurrent.futures.Future()

        def run() -> None:
            if not future.set_running_or_notify_cancel():
                return
            try:
                result = callable_(*args, **kwargs)
            except BaseException as exc:
                future.set_exception(exc)
            else:
                future.set_result(result)

        threading.Thread(
            target=run,
            name="pika-monitor-worker",
            daemon=True,
        ).start()
        return future


def run_monitor(
    pika: MonitorPika,
    *,
    input_fd: int | None = None,
    output_fd: int | None = None,
    refresh_seconds: float = REFRESH_SECONDS,
    ask_handler: Callable[[Session], int] | None = None,
) -> int:
    input_fd = sys.stdin.fileno() if input_fd is None else input_fd
    output_fd = sys.stdout.fileno() if output_fd is None else output_fd
    state = MonitorState()
    usage_cache: dict[tuple[str, str], dict[str, object]] = {}
    input_buffer = bytearray()
    future: concurrent.futures.Future[list[Session]] | None = None
    usage_future: concurrent.futures.Future[list[Session]] | None = None
    expert_future: concurrent.futures.Future[list[ExpertCardState]] | None = None
    preview_future: concurrent.futures.Future[
        tuple[tuple[str, str], list[str], str | None]
    ] | None = None
    next_refresh = 0.0
    next_usage = 0.0
    next_expert = 0.0
    next_preview = 0.0
    manual_refresh_pending = False
    selected_to_open: Session | None = None
    selected_to_ask: Session | None = None
    last_frame: str | None = None
    color = "NO_COLOR" not in os.environ and os.environ.get("TERM") != "dumb"
    first_scan = True

    try:
        with _DaemonExecutor() as executor:
            with _Terminal(input_fd, output_fd) as terminal:
                while True:
                    monotonic = time.monotonic()
                    if future is not None and future.done():
                        try:
                            sessions = future.result()
                        except Exception as exc:  # noqa: BLE001
                            state.refresh_error = terminal_text(exc)
                        else:
                            _carry_usage(sessions, usage_cache)
                            state.sessions = sessions
                            state.selected()
                            state.last_update = time.time()
                            state.refresh_error = None
                            discovery_errors = getattr(pika, "discovery_errors", [])
                            state.refresh_warning = (
                                "; ".join(map(terminal_text, discovery_errors))
                                if discovery_errors
                                else None
                            )
                            if first_scan:
                                state.handoff = _morning_handoff(
                                    pika,
                                    sessions,
                                    now=state.last_update,
                                )
                                if state.handoff is not None:
                                    state.handoff_until = (
                                        monotonic + HANDOFF_DISPLAY_SECONDS
                                    )
                                first_scan = False
                        future = None
                        state.emphasize_refresh = False
                        next_refresh = (
                            0.0
                            if manual_refresh_pending
                            else monotonic + refresh_seconds
                        )

                    if usage_future is not None and usage_future.done():
                        try:
                            usage_sessions = usage_future.result()
                        except Exception as exc:  # noqa: BLE001 - usage is optional
                            state.usage_warning = terminal_text(exc)
                        else:
                            _carry_usage(usage_sessions, usage_cache)
                            _carry_usage(state.sessions, usage_cache)
                            usage_errors = getattr(pika, "usage_errors", [])
                            state.usage_warning = (
                                "; ".join(map(terminal_text, usage_errors))
                                if usage_errors
                                else None
                            )
                        usage_future = None
                        next_usage = monotonic + USAGE_REFRESH_SECONDS

                    if expert_future is not None and expert_future.done():
                        try:
                            cards = expert_future.result()
                        except Exception:  # noqa: BLE001 - cards never block operations
                            pass
                        else:
                            state.expert_cards = {item.session.key: item for item in cards}
                            state.expert_cards_updated_at = time.time()
                        expert_future = None
                        next_expert = monotonic + EXPERT_CARD_REFRESH_SECONDS

                    if preview_future is not None and preview_future.done():
                        try:
                            preview_key, preview_lines, preview_error = (
                                preview_future.result()
                            )
                        except Exception:  # noqa: BLE001 - optional surface
                            pass
                        else:
                            state.preview_key = preview_key
                            state.preview_lines = preview_lines
                            state.preview_error = preview_error
                            state.preview_updated_at = time.time()
                        preview_future = None
                        next_preview = monotonic + PREVIEW_REFRESH_SECONDS

                    if future is None and monotonic >= next_refresh:
                        state.refresh_started_at = time.time()
                        state.emphasize_refresh = manual_refresh_pending or not bool(
                            state.last_update
                        )
                        manual_refresh_pending = False
                        future = executor.submit(pika.refresh, usage=False)

                    if (
                        state.show_usage
                        and usage_future is None
                        and state.sessions
                        and monotonic >= next_usage
                    ):
                        usage_future = executor.submit(
                            pika.hydrate_usage,
                            [replace(item) for item in state.sessions],
                        )

                    expert_loader = getattr(pika, "expert_card_states", None)
                    if (
                        callable(expert_loader)
                        and state.sessions
                        and expert_future is None
                        and monotonic >= next_expert
                    ):
                        expert_future = executor.submit(
                            expert_loader,
                            [replace(item) for item in state.sessions],
                        )

                    preview_session = state.selected()
                    preview_target = (
                        preview_session.tmux_pane or preview_session.tmux_session
                        if preview_session is not None
                        else None
                    )
                    if (
                        preview_session is not None
                        and preview_target
                        and preview_future is None
                        and (
                            state.preview_key != preview_session.key
                            or monotonic >= next_preview
                        )
                    ):
                        preview_future = executor.submit(
                            _capture_preview, pika, replace(preview_session)
                        )
                    elif (
                        preview_session is not None
                        and not preview_target
                        and preview_future is None
                        and state.preview_key != preview_session.key
                    ):
                        state.preview_key = preview_session.key
                        state.preview_lines = []
                        state.preview_error = None

                    size = os.get_terminal_size(output_fd)
                    frame = render_monitor(
                        state,
                        width=size.columns,
                        height=size.lines,
                        refreshing=future is not None,
                        color=color,
                    )
                    if frame.ansi != last_frame:
                        terminal.draw(frame.ansi)
                        last_frame = frame.ansi

                    poll_seconds = min(0.12, max(0.01, refresh_seconds / 2))
                    readable, _, _ = select.select([input_fd], [], [], poll_seconds)
                    if not readable:
                        continue
                    data = os.read(input_fd, 256)
                    if not data:
                        break
                    input_buffer.extend(data)
                    for key in decode_keys(input_buffer):
                        action, session = _handle_key(key, pika, state)
                        if action == "quit":
                            future = None
                            selected_to_open = None
                            selected_to_ask = None
                            break
                        if action == "open":
                            selected_to_open = session
                            future = None
                            break
                        if action == "ask":
                            selected_to_ask = session
                            future = None
                            break
                        if action == "refresh":
                            manual_refresh_pending = True
                            state.emphasize_refresh = True
                            next_refresh = 0.0
                            next_expert = 0.0
                        if action == "usage" and state.show_usage:
                            next_usage = 0.0
                    else:
                        continue
                    break
    finally:
        pass
    if selected_to_open is not None:
        return pika.open(selected_to_open)
    if selected_to_ask is not None and ask_handler is not None:
        return ask_handler(selected_to_ask)
    return 0


def _demo_sessions(now: float) -> list[Session]:
    return [
        Session(
            "codex",
            "11111111-1111-4111-8111-111111111111",
            name="factor-history",
            cwd="/work/quant/SMART",
            branch="main",
            transcript_path="/work/quant/SMART/.codex/factor-history.jsonl",
            tmux_pane="%12",
            status=Status.NEEDS_YOU.value,
            unread=True,
            attention_reason="permission",
            last_event_at=now - 42,
            last_activity_at=now - 42,
            live=True,
            home_state="exact-live",
            cpu_percent=3.2,
            rss_kb=241_000,
            total_tokens=184_220,
            estimated_cost_usd=1.42,
        ),
        Session(
            "claude",
            "22222222-2222-4222-8222-222222222222",
            name="plugin-cleanup-with-a-deliberately-long-name",
            cwd="/work/qes/plugin",
            transcript_path="/work/qes/plugin/.claude/plugin-cleanup.jsonl",
            status=Status.WORKING.value,
            last_event_at=now - 8,
            last_activity_at=now - 8,
            live=True,
            home_state="exact-live",
            cpu_percent=8.8,
            rss_kb=532_000,
        ),
        Session(
            "codex",
            "33333333-3333-4333-8333-333333333333",
            name="research-paper",
            cwd="/work/research",
            transcript_path="/work/research/.codex/research-paper.jsonl",
            status=Status.READY.value,
            unread=True,
            attention_reason="completed",
            last_event_at=now - 3600,
            last_activity_at=now - 3600,
        ),
        Session(
            "codex",
            "44444444-4444-4444-8444-444444444444",
            name="older-parked",
            cwd="/work/archive",
            transcript_path="/work/archive/.codex/older-parked.jsonl",
            status=Status.PARKED.value,
            last_event_at=now - 172_800,
            last_activity_at=now - 172_800,
        ),
    ]


def main() -> None:
    parser = argparse.ArgumentParser(description="Render a Pika monitor fixture")
    parser.add_argument("--demo", action="store_true", required=True)
    parser.add_argument("--width", type=int, default=120)
    parser.add_argument("--height", type=int, default=30)
    args = parser.parse_args()
    now = time.time()
    sessions = _demo_sessions(now)
    selected = sessions[0]
    profile = ExpertProfile(
        selected.provider,
        selected.session_id,
        "Built and verified the SMART factor-history publication workflow.",
        ("monthly factor weights", "S3 publication", "idempotent reruns"),
        ("SMART/qis/SMART_factor_weights_history.parquet",),
        now - 300,
        "interview",
    )
    state = MonitorState(
        sessions=sessions,
        last_update=now - 1,
        expert_cards={
            selected.key: ExpertCardState(
                selected, profile, "CURRENT", "matches transcript"
            )
        },
        expert_cards_updated_at=now - 1,
        refresh_started_at=now,
        preview_key=selected.key,
        preview_lines=[
            "17:13:02  reading monthly factor weights",
            "17:13:44  verified 38 tests",
            "17:14:11  unchanged rerun wrote nothing",
            "17:14:47  done · history publication is idempotent",
        ],
        preview_updated_at=now - 1,
    )
    print(
        render_monitor(
            state,
            width=args.width,
            height=args.height,
            now=now,
            refreshing=True,
            color=False,
        ).plain
    )


if __name__ == "__main__":
    main()
