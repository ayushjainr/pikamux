from __future__ import annotations

import argparse
import concurrent.futures
import os
import re
import select
import sys
import termios
import time
import tty
from dataclasses import dataclass, field
from datetime import datetime
from typing import Protocol

from .models import Session, Status
from .ui import (
    PROVIDER_MARK,
    format_bytes,
    format_cost,
    format_tokens,
    human_age,
    short_path,
    sorted_attention_sessions,
    sorted_sessions,
    terminal_text,
)


REFRESH_SECONDS = 2.0
USAGE_REFRESH_SECONDS = 30.0
PLAYBOOK_ROTATION_SECONDS = 300
MIN_WIDTH = 58
MIN_HEIGHT = 15
SPINNER = ("◐", "◓", "◑", "◒")
PLAYBOOK_TIPS = (
    "Delegate, then Ctrl-b d. The agent keeps working; /exit stops it.",
    "pika . opens this repository's conversation; pika - returns to the previous one.",
    "pika next opens the oldest workstream needing you — no inventory triage required.",
    "pika peek NAME inspects without switching; redirected peeks keep unread state.",
    "pika wait NAME --for ready --timeout 600 turns delegation into a shell primitive.",
    "Use task-specific names: the briefing should read like a delegation ledger.",
    "Codex and Claude may share a name; Pika asks for provider + UUID.",
    "Run pika doctor before relying on disconnects; safe means every check passed.",
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

    def open(self, session: Session, *, attach: bool = True) -> int: ...

    def acknowledge(self, session: Session, *, attaching: bool = False) -> None: ...

    def next_attention(
        self, sessions: list[Session] | None = None
    ) -> Session | None: ...

    @property
    def tmux(self): ...


@dataclass(frozen=True, slots=True)
class MonitorFrame:
    ansi: str
    plain: str
    selected_key: tuple[str, str] | None


@dataclass(slots=True)
class MonitorState:
    sessions: list[Session] = field(default_factory=list)
    selected_key: tuple[str, str] | None = None
    last_update: float = 0.0
    refresh_error: str | None = None
    toast: str | None = None
    toast_until: float = 0.0
    mode: str = "sessions"
    peek_lines: list[str] = field(default_factory=list)
    peek_offset: int = 0

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


def _counts(sessions: list[Session]) -> list[tuple[str, int, str]]:
    return [
        (
            "NEEDS YOU",
            sum(item.status == Status.NEEDS_YOU.value for item in sessions),
            FG_YELLOW,
        ),
        (
            "UNREAD",
            sum(item.needs_attention and item.unread for item in sessions),
            FG_GREEN,
        ),
        (
            "WORKING",
            sum(item.status == Status.WORKING.value for item in sessions),
            FG_CYAN,
        ),
        (
            "PARKED",
            sum(item.status == Status.PARKED.value for item in sessions),
            FG_BRIGHT_BLACK,
        ),
        (
            "ERROR",
            sum(item.status == Status.ERROR.value for item in sessions),
            FG_RED,
        ),
        (
            "UNBOUND",
            sum(item.status == Status.UNBOUND.value for item in sessions),
            FG_MAGENTA,
        ),
    ]


def _metric_labels(width: int) -> list[tuple[str, str]]:
    if width < 80:
        return [
            ("NEEDS YOU", "NEED"),
            ("UNREAD", "NEW"),
            ("WORKING", "RUN"),
            ("PARKED", "PARK"),
            ("ERROR", "ERR"),
            ("UNBOUND", "UNBOUND"),
        ]
    return [(label, label) for label, _value, _code in _counts([])]


@dataclass(frozen=True, slots=True)
class _Column:
    label: str
    width: int
    field: str


def _columns(width: int) -> list[_Column]:
    columns = [
        _Column("AG", 2, "provider"),
        _Column("NAME", 20, "name"),
        _Column("STATE", 10, "status"),
        _Column("AGE", 6, "age"),
    ]
    if width >= 74:
        columns.insert(3, _Column("WHY", 10, "why"))
        columns.insert(4, _Column("VIEW", 4, "view"))
    if width >= 104:
        columns.insert(-1, _Column("REPO", 22, "repo"))
        columns.extend([_Column("CPU", 6, "cpu"), _Column("RAM", 6, "ram")])
    if width >= 132:
        repo_index = next(i for i, item in enumerate(columns) if item.field == "repo")
        columns.insert(repo_index + 1, _Column("BRANCH", 12, "branch"))
        columns.extend(
            [_Column("TOKENS", 8, "tokens"), _Column("~API$", 8, "cost")]
        )
    used = 3 + sum(item.width for item in columns) + 2 * (len(columns) - 1)
    extra = max(0, width - used)
    mutable = list(columns)
    name_index = next(i for i, item in enumerate(mutable) if item.field == "name")
    name_extra = min(extra, 12)
    item = mutable[name_index]
    mutable[name_index] = _Column(item.label, item.width + name_extra, item.field)
    extra -= name_extra
    repo_index = next(
        (i for i, item in enumerate(mutable) if item.field == "repo"), None
    )
    if repo_index is not None and extra:
        item = mutable[repo_index]
        mutable[repo_index] = _Column(item.label, item.width + extra, item.field)
    return mutable


def _field(session: Session, field: str, width: int) -> str:
    values = {
        "provider": PROVIDER_MARK.get(session.provider, "?"),
        "name": session.display_name,
        "status": session.status,
        "why": session.attention_reason or "—",
        "view": "yes" if session.attached else ("no" if session.tmux_session else "—"),
        "repo": short_path(session.cwd, width),
        "branch": session.branch or "—",
        "age": human_age(session.last_activity_at),
        "cpu": (
            f"{session.cpu_percent:.1f}%"
            if session.cpu_percent is not None
            else "—"
        ),
        "ram": format_bytes(session.rss_kb),
        "tokens": format_tokens(session.total_tokens),
        "cost": format_cost(session.estimated_cost_usd),
    }
    return values[field]


def _table_row(
    session: Session,
    columns: list[_Column],
    *,
    selected: bool,
    color: bool,
) -> tuple[str, str]:
    marker = ("›" if selected else " ") + ("◆" if session.unread else " ")
    plain_cells = [
        _fit(_field(session, item.field, item.width), item.width)
        for item in columns
    ]
    row_width = (
        sum(item.width for item in columns) + 2 * (len(columns) - 1) + 3
    )
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


def _help_lines(width: int, slots: int) -> list[str]:
    items = [
        "↑/k  previous workstream        ↓/j  next workstream",
        "Enter open or exact-resume      n    open oldest attention",
        "p     peek recent pane output   r    reconcile now",
        "g/G   first / last              ?    close this help",
        "q/Esc close overlay or quit",
        "",
        "Pika never guesses identity. Enter acts on the selected provider + UUID.",
        "The monitor refreshes operational state every 2s and usage every 30s.",
    ]
    return [_fit(value, width) for value in items[:slots]]


def playbook_tip(now: float) -> tuple[int, str]:
    index = int(now // PLAYBOOK_ROTATION_SECONDS) % len(PLAYBOOK_TIPS)
    return index, PLAYBOOK_TIPS[index]


def _peek_view(state: MonitorState, width: int, slots: int) -> list[str]:
    lines = state.peek_lines
    if not lines:
        return [_fit("No pane output available.", width)]
    end = max(0, len(lines) - state.peek_offset)
    start = max(0, end - slots)
    visible = lines[start:end]
    return [_fit(line, width) for line in visible]


def render_monitor(
    state: MonitorState,
    *,
    width: int,
    height: int,
    now: float | None = None,
    refreshing: bool = False,
    color: bool = True,
) -> MonitorFrame:
    now = now or time.time()
    width = max(20, width)
    height = max(6, height)
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

    spinner = SPINNER[int(now * 4) % len(SPINNER)] if refreshing else "●"
    freshness = (
        "waiting for first scan"
        if not state.last_update
        else f"updated {max(0, int(time.time() - state.last_update))}s ago"
    )
    clock = datetime.fromtimestamp(now).strftime("%H:%M:%S")
    header_plain = _line(
        f"PIKA // LIVE OPERATIONS  {len(state.sessions)} HOMES",
        f"{spinner} {freshness}  {clock}",
        width,
    )
    header_ansi = _paint(header_plain, BOLD + FG_CYAN, color)

    labels = dict(_metric_labels(width))
    metric_plain_parts = [
        f"{labels[label]} {value}"
        for label, value, _code in _counts(state.sessions)
    ]
    metric_ansi_parts = [
        _paint(f"{labels[label]} {value}", code, color)
        for label, value, code in _counts(state.sessions)
    ]
    metric_text = "  ".join(metric_plain_parts)
    metrics_plain = _fit(metric_text, width)
    metrics_ansi = "  ".join(metric_ansi_parts) + " " * max(
        0, width - len(metric_text)
    )
    if not color or len(metric_text) > width:
        metrics_ansi = metrics_plain

    rule = "─" * width
    columns = _columns(width)
    table_width = 3 + sum(item.width for item in columns) + 2 * (len(columns) - 1)
    table_header = _fit(
        "   " + "  ".join(_fit(item.label, item.width) for item in columns),
        width,
    )

    row_slots = max(1, height - 10)
    ordered = state.ordered()
    rows, start = _visible_rows(ordered, selected, row_slots)
    table_plain: list[str] = []
    table_ansi: list[str] = []

    if state.mode == "help":
        title = _fit("HELP // KEYS", width)
        table_plain = [title, *_help_lines(width, row_slots - 1)]
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
            f"{index + 1}/{len(ordered)}  id {selected.session_id[:8]}",
            width,
        )
        location = short_path(selected.cwd, max(12, width // 2))
        branch = f" · {selected.branch}" if selected.branch else ""
        identity = (
            f"{location}{branch}  ·  "
            f"pane {selected.tmux_pane or '—'}  ·  "
            f"{'LIVE' if selected.live else 'PARKED'}"
        )
        signal = (
            selected.error
            or selected.attention_reason
            or "No exception reported"
        )
        resources = (
            f"CPU {selected.cpu_percent:.1f}%"
            if selected.cpu_percent is not None
            else "CPU —"
        ) + (
            f"  RAM {format_bytes(selected.rss_kb)}  "
            f"TOKENS {format_tokens(selected.total_tokens)}"
        )
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

    footer_text = (
        "↑↓ move  Enter open  n next  p peek  ? help  q quit"
        if width < 80
        else "↑↓/jk move  Enter open  n next  p peek  r refresh  ? help  q quit"
    )
    footer_plain = _fit(footer_text, width)
    footer_ansi = _paint(footer_plain, REVERSE, color)
    tip_index, tip = playbook_tip(now)
    playbook_plain = _fit(
        f"PIKA PLAYBOOK {tip_index + 1}/{len(PLAYBOOK_TIPS)} // {tip}", width
    )
    playbook_ansi = _paint(playbook_plain, FG_BLUE, color)

    plain_lines = [
        header_plain,
        metrics_plain,
        rule,
        *table_plain,
        rule,
        *detail_plain,
        playbook_plain,
        footer_plain,
    ]
    ansi_lines = [
        header_ansi,
        metrics_ansi,
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
                "n": "next",
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
        pika.acknowledge(session)


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
    elif key == "enter":
        session = state.selected()
        if session and session.status == Status.UNBOUND.value and session.live:
            state.notify("Unbound live process — adopt it before opening")
        elif session:
            return "open", session
    elif key == "next":
        session = pika.next_attention(state.sessions)
        if session is None:
            state.notify("No workstream currently needs attention")
        else:
            return "open", session
    elif key == "refresh":
        return "refresh", None
    return "continue", None


def run_monitor(
    pika: MonitorPika,
    *,
    input_fd: int | None = None,
    output_fd: int | None = None,
    refresh_seconds: float = REFRESH_SECONDS,
) -> int:
    input_fd = sys.stdin.fileno() if input_fd is None else input_fd
    output_fd = sys.stdout.fileno() if output_fd is None else output_fd
    state = MonitorState()
    usage_cache: dict[tuple[str, str], dict[str, object]] = {}
    input_buffer = bytearray()
    future: concurrent.futures.Future[list[Session]] | None = None
    future_includes_usage = False
    next_refresh = 0.0
    next_usage = 0.0
    selected_to_open: Session | None = None
    last_frame: str | None = None
    color = "NO_COLOR" not in os.environ and os.environ.get("TERM") != "dumb"

    with concurrent.futures.ThreadPoolExecutor(max_workers=1) as executor:
        with _Terminal(input_fd, output_fd) as terminal:
            while True:
                monotonic = time.monotonic()
                if future is not None and future.done():
                    try:
                        sessions = future.result()
                    except Exception as exc:  # noqa: BLE001 - retain last good screen
                        state.refresh_error = terminal_text(exc)
                    else:
                        _carry_usage(sessions, usage_cache)
                        state.sessions = sessions
                        state.selected()
                        state.last_update = time.time()
                        state.refresh_error = None
                    if future_includes_usage:
                        next_usage = monotonic + USAGE_REFRESH_SECONDS
                    future = None
                    next_refresh = monotonic + refresh_seconds

                if future is None and monotonic >= next_refresh:
                    future_includes_usage = monotonic >= next_usage
                    future = executor.submit(
                        pika.refresh, usage=future_includes_usage
                    )

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

                readable, _, _ = select.select([input_fd], [], [], 0.12)
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
                        break
                    if action == "open":
                        selected_to_open = session
                        future = None
                        break
                    if action == "refresh":
                        next_refresh = 0.0
                else:
                    continue
                break

    if selected_to_open is None:
        return 0
    return pika.open(selected_to_open)


def _demo_sessions(now: float) -> list[Session]:
    return [
        Session(
            "codex",
            "11111111-1111-4111-8111-111111111111",
            name="factor-history",
            cwd="/work/quant/SMART",
            branch="main",
            tmux_pane="%12",
            status=Status.NEEDS_YOU.value,
            unread=True,
            attention_reason="permission",
            last_activity_at=now - 42,
            live=True,
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
            status=Status.WORKING.value,
            last_activity_at=now - 8,
            live=True,
            cpu_percent=8.8,
            rss_kb=532_000,
        ),
        Session(
            "codex",
            "33333333-3333-4333-8333-333333333333",
            name="research-paper",
            cwd="/work/research",
            status=Status.READY.value,
            unread=True,
            attention_reason="completed",
            last_activity_at=now - 3600,
        ),
        Session(
            "codex",
            "44444444-4444-4444-8444-444444444444",
            name="older-parked",
            cwd="/work/archive",
            status=Status.PARKED.value,
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
    state = MonitorState(
        sessions=_demo_sessions(now),
        last_update=now - 1,
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
