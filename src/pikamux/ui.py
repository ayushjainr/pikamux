from __future__ import annotations

import json
import os
import shutil
import sys
import time
from collections.abc import Iterable
from datetime import datetime, timezone
from pathlib import Path

from .models import ATTENTION_ORDER, Candidate, Session
from .pricing import PRICING_AS_OF

PROVIDER_MARK = {"codex": "C", "claude": "A"}


class SelectionCancelled(ValueError):
    pass


def terminal_text(value: object) -> str:
    """Keep provider-controlled labels from emitting terminal control codes."""
    return "".join(
        character if character.isprintable() else "�" for character in str(value)
    )


def human_age(timestamp: float) -> str:
    if not timestamp:
        return "—"
    seconds = max(0, int(time.time() - timestamp))
    if seconds < 60:
        return f"{seconds}s"
    minutes = seconds // 60
    if minutes < 60:
        return f"{minutes}m"
    hours = minutes // 60
    if hours < 48:
        return f"{hours}h"
    days = hours // 24
    if days < 60:
        return f"{days}d"
    return datetime.fromtimestamp(timestamp, tz=timezone.utc).strftime("%Y-%m-%d")


def short_path(value: str | None, width: int = 28) -> str:
    if not value:
        return "—"
    try:
        home = str(Path.home())
        if value.startswith(home + os.sep):
            value = "~" + value[len(home) :]
    except OSError:
        pass
    value = terminal_text(value)
    if len(value) <= width:
        return value
    return "…" + value[-(width - 1) :]


def format_bytes(kb: int | None) -> str:
    if kb is None:
        return "—"
    if kb < 1024:
        return f"{kb}K"
    mb = kb / 1024
    if mb < 1024:
        return f"{mb:.0f}M"
    return f"{mb / 1024:.1f}G"


def format_tokens(value: int | None) -> str:
    if value is None:
        return "—"
    if value < 1000:
        return str(value)
    if value < 1_000_000:
        return f"{value / 1000:.1f}k"
    return f"{value / 1_000_000:.2f}m"


def format_cost(value: float | None) -> str:
    if value is None:
        return "—"
    if value < 0.01:
        return f"~${value:.3f}"
    return f"~${value:.2f}"


def sorted_sessions(sessions: Iterable[Session]) -> list[Session]:
    return sorted(
        sessions,
        key=lambda item: (
            ATTENTION_ORDER.get(item.status, 99),
            0 if item.unread else 1,
            -item.last_activity_at,
            item.display_name.casefold(),
        ),
    )


def sorted_attention_sessions(sessions: Iterable[Session]) -> list[Session]:
    """Order actionable work exactly as `pika next`: priority, then oldest."""
    return sorted(
        sessions,
        key=lambda item: (
            ATTENTION_ORDER.get(item.status, 99),
            item.last_activity_at,
            item.display_name.casefold(),
        ),
    )


def print_sessions(sessions: list[Session], *, as_json: bool = False) -> None:
    sessions = sorted_sessions(sessions)
    if as_json:
        print(
            json.dumps([item.to_dict() for item in sessions], indent=2, sort_keys=True)
        )
        return
    if not sessions:
        print("No Pika sessions yet. Run `pika setup` or `pika new NAME`.")
        return
    counts = {
        "waiting on you": sum(
            item.status == "NEEDS YOU" for item in sessions
        ),
        "failed": sum(item.status == "ERROR" for item in sessions),
        "result unread": sum(
            item.status == "READY" and item.unread for item in sessions
        ),
        "working": sum(item.status == "WORKING" for item in sessions),
        "parked": sum(item.status == "PARKED" for item in sessions),
        "unbound": sum(item.status == "UNBOUND" for item in sessions),
    }
    briefing = [
        f"{value} {label if value == 1 else label.replace('result', 'results')}"
        for label, value in counts.items()
        if value
    ]
    print("Pika briefing · " + (" · ".join(briefing) if briefing else "all caught up"))
    print()
    columns = [
        ("NEW", 3),
        ("AG", 2),
        ("NAME", 20),
        ("STATE", 10),
        ("WHY", 10),
        ("VIEW", 4),
        ("REPO", 20),
        ("BRANCH", 12),
        ("AGE", 6),
        ("CPU", 6),
        ("RAM", 6),
        ("TOKENS", 7),
        ("~API$", 7),
    ]
    width = shutil.get_terminal_size((140, 24)).columns
    if width < 138:
        columns = columns[:9]
    header = "  ".join(label.ljust(size) for label, size in columns)
    print(header.rstrip())
    print("  ".join("─" * size for _, size in columns).rstrip())
    for item in sessions:
        values = [
            "yes" if item.unread else "",
            PROVIDER_MARK.get(item.provider, "?"),
            item.display_name,
            item.status,
            item.attention_reason or "—",
            "yes" if item.attached else ("no" if item.tmux_session else "—"),
            short_path(item.cwd, 20),
            item.branch or "—",
            human_age(item.last_activity_at),
            f"{item.cpu_percent:.1f}%" if item.cpu_percent is not None else "—",
            format_bytes(item.rss_kb),
            format_tokens(item.total_tokens),
            format_cost(item.estimated_cost_usd),
        ]
        cells: list[str] = []
        for value, (_, size) in zip(values, columns):
            value = terminal_text(value)
            if len(value) > size:
                value = value[: max(1, size - 1)] + "…"
            cells.append(value.ljust(size))
        print("  ".join(cells).rstrip())
    exceptions = [
        item
        for item in sessions
        if item.status in {"ERROR", "UNBOUND"}
    ]
    if exceptions:
        print("\nExceptions:")
        for item in exceptions:
            detail = item.error or (
                "provider reported an error"
                if item.status == "ERROR"
                else "not yet managed in a Pika tmux home"
            )
            print(
                f"  {terminal_text(item.display_name)} ({item.provider.title()}): "
                f"{terminal_text(detail)}"
            )
    legend = "C=Codex  A=Claude  NEW=unread  VIEW=exact pane visible"
    if any(label == "~API$" for label, _size in columns):
        legend += f"  ~API$=API-equivalent estimate ({PRICING_AS_OF})"
    print(f"\n{legend}")


def choose_session(
    sessions: list[Session],
    prompt: str = "Choose a continuation",
    *,
    attention_order: bool = False,
) -> Session:
    if len(sessions) == 1:
        return sessions[0]
    if not sys.stdin.isatty():
        names = ", ".join(f"{item.provider}:{item.session_id[:8]}" for item in sessions)
        raise ValueError(
            f"Multiple continuations match; run interactively or use a session UUID: {names}"
        )
    providers = {item.provider for item in sessions}
    names = {item.display_name.casefold() for item in sessions}
    if providers == {"codex", "claude"} and len(names) == 1:
        name = terminal_text(sessions[0].display_name)
        print(f'Both Codex and Claude have "{name}". Which continuation do you mean?')
    else:
        print(f"{prompt}; Pika will not guess:")
    ordered = (
        sorted_attention_sessions(sessions)
        if attention_order
        else sorted_sessions(sessions)
    )
    for index, item in enumerate(ordered, 1):
        location = short_path(item.cwd, 36)
        branch = f" [{terminal_text(item.branch)}]" if item.branch else ""
        print(
            f"  {index}. {item.provider.title():<6} "
            f"{terminal_text(item.display_name)} "
            f"· id {item.session_id[:8]} · {location}{branch}, "
            f"{human_age(item.last_activity_at)} ago, {item.status}"
        )
    while True:
        try:
            raw = input(f"Enter 1-{len(sessions)}, or q to cancel: ").strip()
            if raw.casefold() == "q" or raw == "\x1b":
                raise SelectionCancelled("Selection cancelled; nothing was opened")
            index = int(raw)
        except SelectionCancelled:
            raise
        except EOFError as exc:
            raise SelectionCancelled("Selection cancelled; nothing was opened") from exc
        except ValueError:
            print("Please enter one of the displayed numbers.", file=sys.stderr)
            continue
        if 1 <= index <= len(ordered):
            return ordered[index - 1]
        print("Please enter one of the displayed numbers.", file=sys.stderr)


def choose_candidates(candidates: list[Candidate]) -> list[Candidate]:
    if not candidates:
        return []
    print("\nPika found these named or live conversations:")
    for index, item in enumerate(candidates, 1):
        live = " live" if item.live else ""
        print(
            f"  {index:>2}. {item.provider:<6} "
            f"{terminal_text(item.display_name):<28} "
            f"{short_path(item.cwd, 34)}{live}"
        )
    if not sys.stdin.isatty():
        return []
    raw = input("Adopt numbers, `all`, or press Enter for none: ").strip().lower()
    if not raw:
        return []
    if raw == "all":
        return candidates
    selected: list[Candidate] = []
    for part in raw.replace(",", " ").split():
        try:
            index = int(part)
        except ValueError:
            continue
        if 1 <= index <= len(candidates):
            selected.append(candidates[index - 1])
    return selected
