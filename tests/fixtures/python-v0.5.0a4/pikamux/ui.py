from __future__ import annotations

import json
import os
import shutil
import sys
import time
from collections.abc import Callable, Iterable
from dataclasses import asdict
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

from .experts import (
    ExpertMatch,
    card_state,
    profile_freshness_label,
    profile_source_label,
    project_label,
)
from .models import (
    ATTENTION_ORDER,
    Candidate,
    FleetNode,
    FleetSession,
    NodeCandidate,
    NodeDiscoveryReport,
    Session,
)
from .pricing import PRICING_AS_OF

PROVIDER_MARK = {"codex": "C", "claude": "A", "opencode": "O"}


def provider_label(provider: str) -> str:
    return "OpenCode" if provider == "opencode" else provider.title()


def provider_identity_label(provider: str) -> str:
    return "session ID" if provider == "opencode" else "UUID"


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
    if value < 1_000_000_000:
        return f"{value / 1_000_000:.2f}m"
    return f"{value / 1_000_000_000:.2f}b"


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
        print("No Pika sessions yet. Run `pika NAME` to start one safely.")
        return
    counts = {
        "waiting on you": sum(item.status == "NEEDS YOU" for item in sessions),
        "open twice": sum(item.status == "OPEN TWICE" for item in sessions),
        "failed": sum(item.status == "ERROR" for item in sessions),
        "result unread": sum(
            item.status == "READY" and item.unread for item in sessions
        ),
        "working": sum(item.status == "WORKING" for item in sessions),
        "starting": sum(item.status == "STARTING" for item in sessions),
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
        item for item in sessions if item.status in {"ERROR", "OPEN TWICE", "UNBOUND"}
    ]
    if exceptions:
        print("\nExceptions:")
        for item in exceptions:
            detail = item.error or (
                "provider reported an error"
                if item.status in {"ERROR", "OPEN TWICE"}
                else "not yet managed in a Pika tmux home"
            )
            print(
                f"  {terminal_text(item.display_name)} ({provider_label(item.provider)}): "
                f"{terminal_text(detail)}"
            )
    legend = "C=Codex  A=Claude  NEW=unread  VIEW=tracked pane visible"
    if any(label == "~API$" for label, _size in columns):
        legend += f"  ~API$=API-equivalent estimate ({PRICING_AS_OF})"
    print(f"\n{legend}")


def print_fleet_sessions(
    sessions: list[Session | FleetSession], *, as_json: bool = False
) -> None:
    """Print the cache-only cross-machine view without changing local JSON."""
    ordered = sorted_sessions(sessions)
    if as_json:
        print(
            json.dumps(
                {
                    "schema": "pikamux-fleet-list/v1",
                    "generated_at": time.time(),
                    "sessions": [
                        (
                            item.to_dict()
                            if isinstance(item, FleetSession)
                            else {
                                "node_id": "local",
                                "machine": "this-machine",
                                "stale": False,
                                "remote_error": None,
                                "seen_at": time.time(),
                                "session": item.to_dict(),
                            }
                        )
                        for item in ordered
                    ],
                },
                indent=2,
                sort_keys=True,
            )
        )
        return
    if not ordered:
        print("No Pika sessions on this machine or its adopted nodes.")
        return
    print("Pika fleet · cached remote metadata · transcripts stay on their machines\n")
    columns = [
        ("AG", 2),
        ("THREAD@MACHINE", 34),
        ("STATE", 11),
        ("FRESH", 8),
        ("REPO", 24),
        ("AGE", 7),
        ("ID", 8),
    ]
    print("  ".join(label.ljust(size) for label, size in columns).rstrip())
    print("  ".join("─" * size for _, size in columns).rstrip())
    now = time.time()
    for item in ordered:
        remote = isinstance(item, FleetSession)
        machine = item.node_name if remote else "here"
        name = (
            f"{item.session.display_name}@{machine}"
            if remote
            else f"{item.display_name}@here"
        )
        freshness = (
            f"{human_age(item.seen_at)}{'*' if item.stale else ''}" if remote else "now"
        )
        state = "CACHED" if remote and item.stale else item.status
        values = [
            PROVIDER_MARK.get(item.provider, "?"),
            name,
            state,
            freshness,
            short_path(item.cwd, 24),
            _fleet_age(item.last_activity_at, now),
            item.session_id[:8],
        ]
        cells = []
        for value, (_label, size) in zip(values, columns):
            text_value = terminal_text(value)
            if len(text_value) > size:
                text_value = text_value[: max(1, size - 1)] + "…"
            cells.append(text_value.ljust(size))
        print("  ".join(cells).rstrip())
    stale_nodes = sorted(
        {
            item.node_name
            for item in ordered
            if isinstance(item, FleetSession) and item.stale
        }
    )
    if stale_nodes:
        print(
            "\n* Cached from unavailable or overdue machines: " + ", ".join(stale_nodes)
        )


def _fleet_age(timestamp: float, now: float) -> str:
    if not timestamp:
        return "—"
    return human_age(min(timestamp, now))


def print_machines(
    nodes: list[FleetNode],
    *,
    local_name: str | None = None,
    local_node_id: str | None = None,
    as_json: bool = False,
) -> None:
    if as_json:
        print(
            json.dumps(
                {
                    "schema": "pikamux-machines/v1",
                    "local": {
                        "alias": local_name,
                        "node_id": local_node_id,
                        "status": "ready" if local_node_id else "not-commissioned",
                    },
                    "remote": [asdict(item) for item in nodes],
                },
                indent=2,
                sort_keys=True,
            )
        )
        return
    print("Pika machines")
    if local_name:
        fingerprint = local_node_id[:8] if local_node_id else "not set"
        print(f"  {local_name:<18} HERE         node {fingerprint} · coordinator")
    for node in nodes:
        detail = f" · {terminal_text(node.last_error)}" if node.last_error else ""
        version = f" · pika {node.package_version}" if node.package_version else ""
        print(
            f"  {node.alias:<18} {node.status.upper():<12} "
            f"node {node.node_id[:8]} · {node.ssh_target}{version}{detail}"
        )
    if not nodes:
        print(
            "\nNo remote nodes adopted. Run `pika setup` or `pika machines discover`."
        )


def print_node_candidates(candidates: list[NodeCandidate]) -> None:
    if not candidates:
        print("No new SSH or Tailscale machine candidates found.")
        return
    print("Pika found these machine candidates without connecting:")
    previous_group = None
    for index, item in enumerate(candidates, 1):
        group = (
            "SSH CONFIG · your configured connections"
            if "ssh-config" in item.sources
            else "TAILSCALE · other discovered machines"
            if "tailscale" in item.sources
            else "OTHER CANDIDATES"
        )
        if group != previous_group:
            print(f"\n{group}")
            previous_group = group
        presence = (
            "online"
            if item.online is True
            else "offline"
            if item.online is False
            else "unknown"
        )
        print(
            f"  {index:>2}. {item.alias:<18} {item.ssh_target:<38} "
            f"{'+'.join(item.sources)} · {presence}"
        )


def print_node_discovery_report(report: NodeDiscoveryReport) -> None:
    print(
        f"SSH configuration: {report.ssh_aliases} named host(s) from "
        f"{report.ssh_config_files} file(s)"
    )
    if report.tailscale_error:
        print(f"Tailscale: unavailable · {terminal_text(report.tailscale_error)}")
    else:
        excluded = report.excluded_non_linux + report.excluded_no_target
        print(
            f"Tailscale: {report.tailscale_total} peer(s) · "
            f"{report.tailscale_compatible} compatible Linux/macOS host(s) · "
            f"{excluded} excluded"
        )
        if excluded:
            print(
                f"  excluded: {report.excluded_non_linux} unsupported OS · "
                f"{report.excluded_no_target} without an address"
            )
    print_node_candidates(list(report.candidates))


def choose_node_candidates(
    candidates: list[NodeCandidate], *, report: NodeDiscoveryReport | None = None
) -> list[NodeCandidate]:
    if report is not None:
        print_node_discovery_report(report)
    else:
        print_node_candidates(candidates)
    if not candidates or not sys.stdin.isatty():
        return []
    raw = input("Add machine numbers, `all`, or press Enter for none: ").strip().lower()
    return _choose_numbered(candidates, raw)


def choose_fleet_candidates(
    values: list[tuple[FleetNode | None, Candidate]],
    *,
    browse: Callable[[], list[tuple[FleetNode | None, Candidate]]] | None = None,
) -> list[tuple[FleetNode | None, Candidate]]:
    if not values and browse is None:
        return []
    print("\nPika conversations · 2/2 ADD")
    print(
        "Selecting a remote item updates only Pika on that machine; no transcript is copied."
    )
    def identity(value):
        node, item = value
        return (node.node_id if node else None, item.provider, item.session_id)

    values = sorted(values, key=lambda value: value[1].updated_at, reverse=True)
    named_ids = {identity(value) for value in values}
    selected = []
    broader = None
    stage = "named" if browse is not None else "all"
    while True:
        if stage == "named":
            print("\n1/2 · PERSONALLY NAMED · confirmed naming evidence")
            print("Next: recent conversations without a confirmed personal name.")
        elif stage == "recent":
            print("\n2/2 · RECENT · no confirmed personal name")
            print("Up to 10 from the last 14 days, newest first. Titles may be automatic.")
            print("Codex/OpenCode renames with unknown authorship also appear here.")
        else:
            print("\nBROWSE ALL · includes generated and unconfirmed titles; nothing added yet.")
        if not values:
            print("No additional conversations in this view.")
        for index, (node, item) in enumerate(values, 1):
            machine = node.alias if node else "here"
            live = " live" if item.live else ""
            label = item.name or f"<unnamed · {item.session_id[:8]}>"
            print(
                f"  {index:>2}. {item.provider:<6} {terminal_text(label):<28} "
                f"@{terminal_text(machine):<16} {short_path(item.cwd, 28)}{live}"
                f" · {human_age(item.updated_at)} · …{terminal_text(item.session_id[-8:])}"
            )
        if not sys.stdin.isatty():
            return []
        more = "`b` to Browse all, " if stage != "all" else ""
        enter = "continue to recent" if stage == "named" else "skip"
        raw = input(
            f"Add numbers, `all` in this view, {more}`q` to finish, or Enter to {enter}: "
        ).strip().lower()
        if raw in {"q", "quit"}:
            return selected
        if stage != "all" and raw in {"b", "browse"}:
            broader = broader if broader is not None else browse()
            chosen_ids = {identity(value) for value in selected}
            values = sorted(
                (value for value in broader if identity(value) not in chosen_ids),
                key=lambda value: value[1].updated_at, reverse=True,
            )
            stage = "all"
            continue
        selected.extend(_choose_numbered(values, raw))
        if stage != "named":
            return selected
        broader = browse()
        cutoff = time.time() - 14 * 86400
        values = sorted(
            (value for value in broader
             if identity(value) not in named_ids and value[1].updated_at >= cutoff),
            key=lambda value: value[1].updated_at, reverse=True,
        )[:10]
        stage = "recent"


def _choose_numbered(values: list[Any], raw: str) -> list[Any]:
    if not raw:
        return []
    if raw == "all":
        return values
    selected = []
    for part in raw.replace(",", " ").split():
        try:
            index = int(part)
        except ValueError:
            continue
        if 1 <= index <= len(values) and values[index - 1] not in selected:
            selected.append(values[index - 1])
    return selected


def print_experts(
    matches: list[ExpertMatch], *, query: str = "", as_json: bool = False
) -> None:
    if as_json:
        print(
            json.dumps([item.to_dict() for item in matches], indent=2, sort_keys=True)
        )
        return
    if not matches:
        suffix = f" matching {query!r}" if query else ""
        print(f"No expert threads{suffix}.")
        print("Check profile coverage with `pika expert status`.")
        return
    title = f"Expert threads for {query!r}" if query else "Expert threads"
    print(title)
    print()
    columns = [
        ("AG", 2),
        ("THREAD", 22),
        ("PROJECT", 20),
        ("STATE", 10),
        ("PROFILE", 22),
        ("CACHE", 12),
        ("TOPICS", 32),
        ("WHY", 15),
        ("ID", 8),
    ]
    print("  ".join(label.ljust(size) for label, size in columns).rstrip())
    print("  ".join("─" * size for _, size in columns).rstrip())
    for match in matches:
        values = [
            PROVIDER_MARK.get(match.session.provider, "?"),
            match.session.display_name,
            project_label(match.session.cwd),
            match.session.status,
            f"{profile_source_label(match.profile)} · "
            + profile_freshness_label(
                (
                    match.session.card_status or "UNKNOWN"
                    if isinstance(match.session, FleetSession)
                    else card_state(match.session, match.profile).status
                )
            ),
            (
                f"CACHED {_fleet_age(match.session.seen_at, time.time())}"
                if isinstance(match.session, FleetSession) and match.session.stale
                else "FRESH"
            ),
            ", ".join(match.profile.topics),
            ", ".join(match.matched_on)
            if query
            else human_age(match.profile.updated_at),
            match.session.session_id[:8],
        ]
        cells: list[str] = []
        for value, (_label, size) in zip(values, columns):
            value = terminal_text(value)
            if len(value) > size:
                value = value[: max(1, size - 1)] + "…"
            cells.append(value.ljust(size))
        print("  ".join(cells).rstrip())
    print(
        "\nProfiles are bound to exact provider identities. INTERVIEWED is "
        "conversation-reported; SELF-PUBLISHED is explicit pane input."
    )


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
            "Multiple continuations match; run interactively or use a "
            f"provider-native session ID: {names}"
        )
    providers = {item.provider for item in sessions}
    names = {item.display_name.casefold() for item in sessions}
    if len(providers) > 1 and len(names) == 1:
        name = terminal_text(sessions[0].display_name)
        labels = [
            "OpenCode" if value == "opencode" else value.title()
            for value in sorted(providers)
        ]
        if labels == ["Claude", "Codex"]:
            owners = "Both Codex and Claude"
        else:
            owners = ", ".join(labels[:-1]) + f", and {labels[-1]}"
        print(f'{owners} have "{name}". Which continuation do you mean?')
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
            f"  {index}. {provider_label(item.provider):<8} "
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
    print("\nPika found conversations worth adding:")
    for index, item in enumerate(candidates, 1):
        live = " live" if item.live else ""
        label = item.name if item.name else f"<unnamed live · {item.session_id[:8]}>"
        lineage = (
            f" · fork of {item.parent_session_id[:8]}"
            if item.parent_session_id
            else ""
        )
        print(
            f"  {index:>2}. {item.provider:<6} "
            f"{terminal_text(label):<28} "
            f"{short_path(item.cwd, 34)}{live}{lineage}"
        )
    if not sys.stdin.isatty():
        return []
    raw = input("Add numbers, `all`, or press Enter for none: ").strip().lower()
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
