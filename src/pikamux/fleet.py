from __future__ import annotations

import glob
import json
import math
import os
import re
import selectors
import shlex
import socket
import subprocess
import threading
import time
import uuid
from collections.abc import Iterable
from pathlib import Path
from typing import Any

from . import __version__
from .consult import ConsultationError, ConsultationPolicy
from .experts import ExpertCardState, ExpertMatch, rank_experts, profile_freshness, expert_availability
from .models import (
    Candidate,
    ExpertProfile,
    FleetNode,
    FleetSession,
    NodeCandidate,
    NodeDiscoveryReport,
    Session,
    Status,
)
from .store import Store, load_config
from .ui import choose_session, terminal_text

PROTOCOL_VERSION = 2
PROTOCOL_NAME = "pikamux-fleet"
CAPABILITIES = (
    "inventory",
    "candidates",
    "adopt",
    "attach",
    "peek",
    "acknowledge",
    "untrack",
    "experts",
    "ask-jsonl",
    "provider-opencode",
    "expert-directory-v1",
)
REMOTE_STALE_SECONDS = 45.0
MAX_MESSAGE_BYTES = 4 * 1024 * 1024
MAX_STDERR_BYTES = 64 * 1024
REMOTE_INSTALL_ARGV = (
    "python3",
    "-m",
    "pip",
    "install",
    "--user",
    f"git+https://github.com/ayushjainr/pikamux.git@v{__version__}",
)

_ALIAS = re.compile(r"^[a-z0-9][a-z0-9._-]{0,62}$")
_SESSION_ID = re.compile(r"^[A-Za-z0-9:._%+-]{1,128}$")
_SESSION_FIELDS = {
    "provider",
    "session_id",
    "name",
    "cwd",
    "branch",
    "status",
    "unread",
    "model",
    "source",
    "managed",
    "error",
    "attention_reason",
    "created_at",
    "updated_at",
    "last_event_at",
    "last_activity_at",
    "live",
    "attached",
    "exact_home",
    "identity_kind",
    "pane_visible",
    "cpu_percent",
    "rss_kb",
}
_SNAPSHOT_FIELDS = {
    "type",
    "protocol",
    "version",
    "node_id",
    "machine",
    "captured_at",
    "sessions",
    "profiles",
    "cards",
}
_HELLO_FIELDS = {
    "type",
    "protocol",
    "version",
    "node_id",
    "machine",
    "package_version",
    "capabilities",
}
_CANDIDATE_FIELDS = {
    "provider",
    "session_id",
    "name",
    "cwd",
    "branch",
    "model",
    "updated_at",
    "live",
    "source",
}
_PROFILE_FIELDS = {
    "provider",
    "session_id",
    "scope",
    "current_state",
    "topics",
    "artifacts",
    "updated_at",
    "source",
    "scope_updated_at",
    "current_state_updated_at",
}
_CARD_FIELDS = {"provider", "session_id", "status", "detail", "watched", "availability", "current_state_status"}
_BOOLEAN_SESSION_FIELDS = {
    "unread",
    "managed",
    "live",
    "attached",
    "exact_home",
    "pane_visible",
}
_NUMBER_SESSION_FIELDS = {
    "created_at",
    "updated_at",
    "last_event_at",
    "last_activity_at",
    "cpu_percent",
    "rss_kb",
}
_MAX_TEXT_CHARS = 8192
_MAX_EXPERT_ITEMS = 64


class FleetError(RuntimeError):
    def __init__(self, message: str, *, kind: str = "error") -> None:
        super().__init__(message)
        self.kind = kind


def machine_alias(value: str) -> str:
    alias = re.sub(r"[^a-z0-9._-]+", "-", value.casefold()).strip("-._")
    alias = alias[:63]
    if not alias or not _ALIAS.fullmatch(alias) or "@" in alias:
        raise ValueError(f"Invalid Pika machine alias {value!r}")
    return alias


def suggest_alias(target: str) -> str:
    host = target.rsplit("@", 1)[-1]
    if host.startswith("[") and "]" in host:
        host = host[1 : host.index("]")]
    elif host.count(":") == 1:
        host = host.split(":", 1)[0]
    return machine_alias(host.split(".", 1)[0])


def local_machine_name() -> str:
    configured = load_config().get("machine_alias")
    if isinstance(configured, str) and configured.strip():
        return machine_alias(configured)
    return machine_alias(socket.gethostname().split(".", 1)[0])


def suggest_local_machine_alias(
    *, executable: str = "tailscale", timeout: float = 2.0
) -> str:
    """Suggest a human local alias without contacting any peer."""
    try:
        result = subprocess.run(
            [executable, "status", "--json"],
            capture_output=True,
            text=True,
            timeout=timeout,
            check=False,
        )
        payload = json.loads(result.stdout) if result.returncode == 0 else {}
        self_record = payload.get("Self", {}) if isinstance(payload, dict) else {}
        dns = str(self_record.get("DNSName") or "").rstrip(".")
        hostname = str(self_record.get("HostName") or "").strip()
        if dns or hostname:
            return suggest_alias(dns or hostname)
    except (OSError, subprocess.TimeoutExpired, ValueError):
        pass
    return local_machine_name()


def _safe_target(value: str) -> str:
    if not value or value.startswith("-") or any(ch in value for ch in "\r\n\x00"):
        raise ValueError(f"Unsafe SSH target {value!r}")
    return value


def _ssh_config_files(root: Path) -> list[Path]:
    result: list[Path] = []
    seen: set[Path] = set()

    def visit(path: Path) -> None:
        try:
            resolved = path.expanduser().resolve()
        except OSError:
            return
        if resolved in seen or not resolved.is_file():
            return
        seen.add(resolved)
        result.append(resolved)
        try:
            lines = resolved.read_text(errors="replace").splitlines()
        except OSError:
            return
        for raw in lines:
            try:
                parts = shlex.split(raw, comments=True)
            except ValueError:
                continue
            if len(parts) < 2 or parts[0].casefold() != "include":
                continue
            for pattern in parts[1:]:
                expanded = Path(pattern).expanduser()
                if not expanded.is_absolute():
                    expanded = root / expanded
                for match in sorted(glob.glob(str(expanded))):
                    visit(Path(match))

    visit(root / "config")
    return result


def discover_ssh_candidates(ssh_root: Path | None = None) -> list[NodeCandidate]:
    """Enumerate concrete SSH aliases without opening a connection."""
    root = ssh_root or Path.home() / ".ssh"
    aliases: dict[str, NodeCandidate] = {}
    for path in _ssh_config_files(root):
        try:
            lines = path.read_text(errors="replace").splitlines()
        except OSError:
            continue
        for raw in lines:
            try:
                parts = shlex.split(raw, comments=True)
            except ValueError:
                continue
            if len(parts) < 2 or parts[0].casefold() != "host":
                continue
            for value in parts[1:]:
                if value.startswith("!") or any(ch in value for ch in "*?[]"):
                    continue
                try:
                    alias = machine_alias(value)
                    target = _safe_target(value)
                except ValueError:
                    continue
                aliases[target.casefold()] = NodeCandidate(
                    alias=alias,
                    ssh_target=target,
                    sources=("ssh-config",),
                    hostname=value,
                )
    return sorted(aliases.values(), key=lambda item: item.alias)


def discover_tailscale_candidates(
    *, executable: str = "tailscale", timeout: float = 3.0
) -> list[NodeCandidate]:
    """Read Tailscale's local status document; never probe a peer."""
    try:
        result = subprocess.run(
            [executable, "status", "--json"],
            capture_output=True,
            text=True,
            timeout=timeout,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired):
        return []
    if result.returncode != 0:
        return []
    try:
        payload = json.loads(result.stdout)
    except ValueError:
        return []
    peers = payload.get("Peer", {}) if isinstance(payload, dict) else {}
    records = peers.values() if isinstance(peers, dict) else peers
    found: dict[str, NodeCandidate] = {}
    for record in records if isinstance(records, Iterable) else ():
        if not isinstance(record, dict):
            continue
        dns = str(record.get("DNSName") or "").rstrip(".")
        hostname = str(record.get("HostName") or "").strip()
        os_name = str(record.get("OS") or "").casefold()
        # Pika's exact PID/process-tree contract is Linux-specific. Other
        # tailnet devices remain explicitly addable by SSH target, but should
        # not flood the automatic setup picker.
        if os_name and os_name != "linux":
            continue
        ips = record.get("TailscaleIPs")
        ip = str(ips[0]) if isinstance(ips, list) and ips else ""
        target = dns or ip
        if not target:
            continue
        try:
            alias = machine_alias(hostname or dns.split(".", 1)[0] or ip)
            target = _safe_target(target)
        except ValueError:
            continue
        found[target.casefold()] = NodeCandidate(
            alias=alias,
            ssh_target=target,
            sources=("tailscale",),
            hostname=hostname or dns or None,
            online=bool(record.get("Online")),
            os_name=str(record.get("OS")) if record.get("OS") else None,
        )
    return sorted(found.values(), key=lambda item: item.alias)


def tailscale_discovery_summary(
    *, executable: str = "tailscale", timeout: float = 3.0
) -> dict[str, int | str | None]:
    """Explain passive Tailscale filtering without contacting any peer."""
    empty: dict[str, int | str | None] = {
        "total": 0,
        "compatible": 0,
        "non_linux": 0,
        "no_target": 0,
        "error": None,
    }
    try:
        result = subprocess.run(
            [executable, "status", "--json"],
            capture_output=True,
            text=True,
            timeout=timeout,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired) as exc:
        return {**empty, "error": str(exc)}
    if result.returncode != 0:
        return {
            **empty,
            "error": (result.stderr or "tailscale status failed").strip(),
        }
    try:
        payload = json.loads(result.stdout)
    except ValueError:
        return {**empty, "error": "invalid Tailscale status JSON"}
    peers = payload.get("Peer", {}) if isinstance(payload, dict) else {}
    records = list(peers.values()) if isinstance(peers, dict) else list(peers or [])
    total = compatible = non_linux = no_target = 0
    for record in records:
        if not isinstance(record, dict):
            continue
        total += 1
        os_name = str(record.get("OS") or "").casefold()
        if os_name and os_name != "linux":
            non_linux += 1
            continue
        ips = record.get("TailscaleIPs")
        if not (record.get("DNSName") or (isinstance(ips, list) and ips)):
            no_target += 1
            continue
        compatible += 1
    return {
        "total": total,
        "compatible": compatible,
        "non_linux": non_linux,
        "no_target": no_target,
        "error": None,
    }


def discover_node_candidates(store: Store) -> list[NodeCandidate]:
    existing_targets = {node.ssh_target.casefold() for node in store.list_fleet_nodes()}
    ignored = store.ignored_node_candidate_keys()
    merged: dict[str, NodeCandidate] = {}
    for item in [*discover_ssh_candidates(), *discover_tailscale_candidates()]:
        if item.key in existing_targets or item.key in ignored:
            continue
        previous = merged.get(item.key)
        if previous is None:
            merged[item.key] = item
            continue
        merged[item.key] = NodeCandidate(
            alias=previous.alias,
            ssh_target=previous.ssh_target,
            sources=tuple(dict.fromkeys((*previous.sources, *item.sources))),
            hostname=previous.hostname or item.hostname,
            online=(item.online if previous.online is None else previous.online),
            os_name=previous.os_name or item.os_name,
        )
    result: list[NodeCandidate] = []
    used: set[str] = set()
    for item in sorted(
        merged.values(), key=lambda value: (value.alias, value.ssh_target)
    ):
        alias = item.alias
        suffix = 2
        while alias in used:
            tail = f"-{suffix}"
            alias = f"{item.alias[: 63 - len(tail)]}{tail}"
            suffix += 1
        used.add(alias)
        result.append(
            NodeCandidate(
                alias,
                item.ssh_target,
                item.sources,
                item.hostname,
                item.online,
                item.os_name,
            )
        )
    return result


def session_to_wire(session: Session, *, extended: bool = False) -> dict[str, Any]:
    """Export metadata needed for operations, never transcript or process identity."""
    result = {
        "provider": session.provider,
        "session_id": session.session_id,
        "name": session.name,
        "cwd": session.cwd,
        "branch": session.branch,
        "status": session.status,
        "unread": session.unread,
        "model": session.model,
        "source": session.source,
        "managed": session.managed,
        "error": session.error,
        "attention_reason": session.attention_reason,
        "created_at": session.created_at,
        "updated_at": session.updated_at,
        "last_event_at": session.last_event_at,
        "last_activity_at": session.last_activity_at,
        "live": session.live,
        "attached": session.attached,
        "exact_home": session.exact_home,
        "identity_kind": (
            "placeholder"
            if session.session_id.startswith("unbound:")
            else "conversation"
        ),
        "pane_visible": bool(session.tmux_pane or session.tmux_session),
        "cpu_percent": session.cpu_percent,
        "rss_kb": session.rss_kb,
    }
    if extended:
        result["active_thread_id"] = session.active_thread_id
    return result


def candidate_to_wire(candidate: Candidate) -> dict[str, Any]:
    return {
        "provider": candidate.provider,
        "session_id": candidate.session_id,
        "name": candidate.name,
        "cwd": candidate.cwd,
        "branch": candidate.branch,
        "model": candidate.model,
        "updated_at": candidate.updated_at,
        "live": candidate.live,
        "source": candidate.source,
    }


def _profile_to_wire(profile: ExpertProfile, *, extended: bool = False) -> dict[str, Any]:
    result = {
        "provider": profile.provider,
        "session_id": profile.session_id,
        "scope": profile.scope,
        "current_state": profile.current_state,
        "topics": list(profile.topics),
        "artifacts": list(profile.artifacts),
        "updated_at": profile.updated_at,
        "source": profile.source,
    }
    if extended:
        result.update(scope_updated_at=profile.scope_updated_at or profile.updated_at,
                      current_state_updated_at=profile.current_state_updated_at or profile.updated_at)
    return result


def _session_from_wire(value: object) -> Session:
    if not isinstance(value, dict):
        raise FleetError("Remote session record is not an object", kind="incompatible")
    unknown = set(value) - _SESSION_FIELDS - {"active_thread_id"}
    if unknown:
        raise FleetError(
            "Remote session contains unsupported fields: " + ", ".join(sorted(unknown)),
            kind="incompatible",
        )
    provider = value.get("provider")
    session_id = value.get("session_id")
    status = value.get("status")
    if provider not in {"codex", "claude", "opencode"}:
        raise FleetError("Remote session has an invalid provider", kind="incompatible")
    if not isinstance(session_id, str) or not _SESSION_ID.fullmatch(session_id):
        raise FleetError(
            "Remote session has an invalid conversation identity", kind="incompatible"
        )
    active_thread_id = value.get("active_thread_id")
    if active_thread_id is not None and (not isinstance(active_thread_id, str) or not _SESSION_ID.fullmatch(active_thread_id)):
        raise FleetError("Remote session has an invalid active conversation identity", kind="incompatible")
    if status not in {item.value for item in Status}:
        raise FleetError("Remote session has an invalid state", kind="incompatible")
    identity_kind = value.get(
        "identity_kind",
        "placeholder" if session_id.startswith("unbound:") else "conversation",
    )
    if identity_kind not in {"conversation", "placeholder"}:
        raise FleetError(
            "Remote session has an invalid identity kind", kind="incompatible"
        )
    if (identity_kind == "placeholder") != session_id.startswith("unbound:"):
        raise FleetError(
            "Remote session identity kind contradicts its identifier",
            kind="incompatible",
        )
    if identity_kind == "placeholder" and status != Status.UNBOUND.value:
        raise FleetError(
            "Remote placeholder session is not UNBOUND", kind="incompatible"
        )
    string_fields = (
        "name",
        "cwd",
        "branch",
        "model",
        "source",
        "error",
        "attention_reason",
    )
    for field in string_fields:
        if value.get(field) is not None and not isinstance(value.get(field), str):
            raise FleetError(
                f"Remote session field {field} has the wrong type",
                kind="incompatible",
            )
        if isinstance(value.get(field), str) and len(value[field]) > _MAX_TEXT_CHARS:
            raise FleetError(
                f"Remote session field {field} exceeds the safety limit",
                kind="incompatible",
            )
    for field in _BOOLEAN_SESSION_FIELDS:
        if field in value and not isinstance(value[field], bool):
            raise FleetError(
                f"Remote session field {field} has the wrong type",
                kind="incompatible",
            )
    numbers: dict[str, float | int | None] = {}
    for field in _NUMBER_SESSION_FIELDS:
        raw = value.get(field)
        if raw is None:
            numbers[field] = None
            continue
        if isinstance(raw, bool) or not isinstance(raw, (int, float)):
            raise FleetError(
                f"Remote session field {field} has the wrong type",
                kind="incompatible",
            )
        number = float(raw)
        if not math.isfinite(number) or number < 0:
            raise FleetError(
                f"Remote session field {field} has an invalid value",
                kind="incompatible",
            )
        if field == "rss_kb" and not isinstance(raw, int):
            raise FleetError(
                "Remote session field rss_kb must be an integer",
                kind="incompatible",
            )
        numbers[field] = raw
    session = Session(
        provider=provider,
        session_id=session_id,
        active_thread_id=active_thread_id,
        name=value.get("name"),
        cwd=value.get("cwd"),
        branch=value.get("branch"),
        transcript_path=None,
        tmux_session="remote" if value.get("pane_visible") else None,
        status=status,
        unread=bool(value.get("unread")),
        model=value.get("model"),
        source=str(value.get("source") or "remote"),
        managed=bool(value.get("managed", True)),
        error=value.get("error"),
        attention_reason=value.get("attention_reason"),
        created_at=float(numbers["created_at"] or 0),
        updated_at=float(numbers["updated_at"] or 0),
        last_event_at=float(numbers["last_event_at"] or 0),
        last_activity_at=float(numbers["last_activity_at"] or 0),
        live=bool(value.get("live")),
        attached=bool(value.get("attached")),
        home_state="exact-live" if value.get("exact_home") else "unknown",
        cpu_percent=(
            float(numbers["cpu_percent"])
            if numbers["cpu_percent"] is not None
            else None
        ),
        rss_kb=(int(numbers["rss_kb"]) if numbers["rss_kb"] is not None else None),
    )
    return session


def _expert_identity(value: dict[str, Any], *, label: str) -> tuple[str, str]:
    provider = value.get("provider")
    session_id = value.get("session_id")
    if provider not in {"codex", "claude", "opencode"}:
        raise FleetError(f"Remote {label} has an invalid provider", kind="incompatible")
    if not isinstance(session_id, str) or not _SESSION_ID.fullmatch(session_id):
        raise FleetError(
            f"Remote {label} has an invalid conversation identity",
            kind="incompatible",
        )
    return provider, session_id


def _expert_text(value: object, *, label: str) -> str:
    if not isinstance(value, str) or len(value) > _MAX_TEXT_CHARS:
        raise FleetError(
            f"Remote {label} has the wrong type or size", kind="incompatible"
        )
    return value


def _expert_list(value: object, *, label: str) -> list[str]:
    if not isinstance(value, list) or len(value) > _MAX_EXPERT_ITEMS:
        raise FleetError(f"Remote {label} is malformed", kind="incompatible")
    return [_expert_text(item, label=label) for item in value]


def _validate_profiles(values: object) -> list[dict[str, Any]]:
    if not isinstance(values, list):
        raise FleetError("Remote expert profiles are malformed", kind="incompatible")
    result: list[dict[str, Any]] = []
    seen: set[tuple[str, str]] = set()
    for value in values:
        if not isinstance(value, dict) or set(value) - _PROFILE_FIELDS:
            raise FleetError("Remote expert profile is malformed", kind="incompatible")
        provider, session_id = _expert_identity(value, label="expert profile")
        if (provider, session_id) in seen:
            raise FleetError("Remote expert profile is duplicated", kind="incompatible")
        seen.add((provider, session_id))
        updated_at = value.get("updated_at")
        if (
            isinstance(updated_at, bool)
            or not isinstance(updated_at, (int, float))
            or not math.isfinite(float(updated_at))
            or float(updated_at) < 0
        ):
            raise FleetError(
                "Remote expert profile has an invalid timestamp", kind="incompatible"
            )
        clocks = {}
        for field in ("scope_updated_at", "current_state_updated_at"):
            if field in value:
                clock = value[field]
                if isinstance(clock, bool) or not isinstance(clock, (int, float)) or not math.isfinite(clock) or clock < 0:
                    raise FleetError("Remote expert clock is invalid", kind="incompatible")
                clocks[field] = float(clock)
        result.append(
            {
                "provider": provider,
                "session_id": session_id,
                "scope": _expert_text(value.get("scope"), label="expert scope"),
                "current_state": _expert_text(
                    value.get("current_state"), label="expert current state"
                ),
                "topics": _expert_list(value.get("topics"), label="expert topics"),
                "artifacts": _expert_list(
                    value.get("artifacts"), label="expert artifacts"
                ),
                "updated_at": float(updated_at),
                "source": _expert_text(value.get("source"), label="expert source"),
                **clocks,
            }
        )
    return result


def _validate_cards(values: object) -> list[dict[str, Any]]:
    if not isinstance(values, list):
        raise FleetError("Remote thread profiles are malformed", kind="incompatible")
    result: list[dict[str, Any]] = []
    seen: set[tuple[str, str]] = set()
    for value in values:
        if not isinstance(value, dict) or set(value) - _CARD_FIELDS:
            raise FleetError("Remote thread profile is malformed", kind="incompatible")
        provider, session_id = _expert_identity(value, label="thread profile")
        if (provider, session_id) in seen:
            raise FleetError("Remote thread profile is duplicated", kind="incompatible")
        seen.add((provider, session_id))
        status = value.get("status")
        if status not in {"CURRENT", "STALE", "MISSING", "UNKNOWN"}:
            raise FleetError(
                "Remote thread profile has an invalid state", kind="incompatible"
            )
        extensions = {}
        if "watched" in value:
            if not isinstance(value["watched"], bool):
                raise FleetError("Remote watched flag is invalid", kind="incompatible")
            extensions["watched"] = value["watched"]
        if "availability" in value:
            if value["availability"] not in {"source-available", "source-unavailable", "archived", "deleted", "provider-unavailable", "excluded-worker", "requires-reconciliation"}:
                raise FleetError("Remote expert availability is invalid", kind="incompatible")
            extensions["availability"] = value["availability"]
        if "current_state_status" in value:
            if value["current_state_status"] not in {"CURRENT", "STALE", "MISSING", "UNKNOWN"}:
                raise FleetError("Remote work freshness is invalid", kind="incompatible")
            extensions["current_state_status"] = value["current_state_status"]
        result.append(
            {
                "provider": provider,
                "session_id": session_id,
                "status": status,
                "detail": _expert_text(
                    value.get("detail"), label="thread profile detail"
                ),
                **extensions,
            }
        )
    return result


def validate_snapshot(
    value: object, *, expected_node_id: str | None = None
) -> dict[str, Any]:
    if not isinstance(value, dict) or value.get("type") != "snapshot":
        raise FleetError(
            "Remote did not return a complete snapshot", kind="incompatible"
        )
    if set(value) - {"expert_sessions"} != _SNAPSHOT_FIELDS:
        raise FleetError("Remote snapshot envelope is malformed", kind="incompatible")
    if (
        value.get("protocol") != PROTOCOL_NAME
        or value.get("version") != PROTOCOL_VERSION
    ):
        raise FleetError(
            f"Remote Pika is incompatible with fleet protocol {PROTOCOL_VERSION}",
            kind="incompatible",
        )
    node_id = value.get("node_id")
    try:
        node_id = str(uuid.UUID(str(node_id)))
    except ValueError:
        raise FleetError(
            "Remote Pika returned an invalid node identity", kind="incompatible"
        ) from None
    if expected_node_id and node_id != expected_node_id:
        raise FleetError(
            f"NODE IDENTITY CHANGED: expected {expected_node_id[:8]}, received {node_id[:8]}",
            kind="quarantined",
        )
    sessions_raw = value.get("sessions")
    if not isinstance(sessions_raw, list):
        raise FleetError(
            "Remote snapshot has no complete session list", kind="incompatible"
        )
    if any(
        not isinstance(item, dict) or set(item) - {"active_thread_id"} != _SESSION_FIELDS
        for item in sessions_raw
    ):
        raise FleetError(
            "Remote snapshot has a malformed session envelope", kind="incompatible"
        )
    sessions = [_session_from_wire(item) for item in sessions_raw]
    expert_raw = value.get("expert_sessions", [])
    if not isinstance(expert_raw, list) or any(not isinstance(item, dict) or set(item) - {"active_thread_id"} != _SESSION_FIELDS for item in expert_raw):
        raise FleetError("Remote expert inventory is malformed", kind="incompatible")
    expert_sessions = [_session_from_wire(item) for item in expert_raw]
    keys = [item.key for item in sessions]
    if len(keys) != len(set(keys)):
        raise FleetError(
            "Remote snapshot repeats a session identity", kind="incompatible"
        )
    expert_keys = [item.key for item in expert_sessions]
    if len(expert_keys) != len(set(expert_keys)) or set(expert_keys) & set(keys):
        raise FleetError("Remote expert inventory repeats an identity", kind="incompatible")
    profiles = _validate_profiles(value.get("profiles"))
    cards = _validate_cards(value.get("cards"))
    captured_at = value.get("captured_at")
    if (
        isinstance(captured_at, bool)
        or not isinstance(captured_at, (int, float))
        or not math.isfinite(float(captured_at))
        or float(captured_at) < 0
    ):
        raise FleetError(
            "Remote snapshot has an invalid timestamp", kind="incompatible"
        )
    machine = value.get("machine")
    if not isinstance(machine, str) or not machine or len(machine) > 63:
        raise FleetError(
            "Remote snapshot has an invalid machine name", kind="incompatible"
        )
    result = {
        "type": "snapshot",
        "protocol": PROTOCOL_NAME,
        "version": PROTOCOL_VERSION,
        "node_id": node_id,
        "machine": terminal_text(machine),
        "captured_at": float(captured_at),
        "sessions": [session_to_wire(item, extended="active_thread_id" in raw) for item, raw in zip(sessions, sessions_raw)],
        "profiles": profiles,
        "cards": cards,
    }
    if "expert_sessions" in value:
        result["expert_sessions"] = [session_to_wire(item, extended="active_thread_id" in raw) for item, raw in zip(expert_sessions, expert_raw)]
    return result


def _validate_mutation_receipt(
    value: object,
    *,
    operation: str,
    expected_node_id: str,
    expected_request_id: str,
) -> dict[str, Any]:
    allowed = (
        {"type", "node_id", "request_id", "session"}
        if operation == "adopted"
        else {"type", "node_id", "request_id", "panes_cleared", "reconciled"}
    )
    if not isinstance(value, dict) or set(value) - allowed:
        raise FleetError(f"Remote {operation} receipt is malformed", kind="quarantined")
    if value.get("type") != operation:
        raise FleetError(f"Remote did not confirm {operation}", kind="quarantined")
    if value.get("node_id") != expected_node_id:
        raise FleetError(
            f"Remote {operation} receipt has the wrong node identity",
            kind="quarantined",
        )
    if value.get("request_id") != expected_request_id:
        raise FleetError(
            f"Remote {operation} receipt has the wrong request identity",
            kind="quarantined",
        )
    if operation == "untracked":
        panes = value.get("panes_cleared")
        if isinstance(panes, bool) or not isinstance(panes, int) or panes < 0:
            raise FleetError(
                "Remote untracked receipt has an invalid pane count",
                kind="quarantined",
            )
        if "reconciled" in value and not isinstance(value["reconciled"], bool):
            raise FleetError(
                "Remote untracked receipt has an invalid reconciliation flag",
                kind="quarantined",
            )
    return value


class SSHTransport:
    def __init__(
        self,
        *,
        executable: str = "ssh",
        connect_timeout: float = 5.0,
        overall_timeout: float = 12.0,
    ) -> None:
        self.executable = executable
        self.connect_timeout = connect_timeout
        self.overall_timeout = overall_timeout

    def base(self, target: str, *, tty: bool = False) -> list[str]:
        return [
            self.executable,
            "-tt" if tty else "-T",
            "-o",
            "BatchMode=yes",
            "-o",
            f"ConnectTimeout={max(1, int(self.connect_timeout))}",
            _safe_target(target),
        ]

    def _bounded_run(
        self, command: list[str], input_bytes: bytes
    ) -> subprocess.CompletedProcess[str]:
        process = subprocess.Popen(
            command,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        assert process.stdin is not None
        assert process.stdout is not None
        assert process.stderr is not None
        try:
            process.stdin.write(input_bytes)
            process.stdin.close()
        except BrokenPipeError:
            process.stdin.close()
        output = bytearray()
        errors = bytearray()
        selector = selectors.DefaultSelector()
        selector.register(process.stdout, selectors.EVENT_READ, output)
        selector.register(process.stderr, selectors.EVENT_READ, errors)
        deadline = time.monotonic() + self.overall_timeout
        try:
            while selector.get_map():
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise subprocess.TimeoutExpired(command, self.overall_timeout)
                events = selector.select(remaining)
                if not events:
                    raise subprocess.TimeoutExpired(command, self.overall_timeout)
                for key, _mask in events:
                    try:
                        chunk = os.read(key.fd, 65536)
                    except BlockingIOError:
                        continue
                    if not chunk:
                        selector.unregister(key.fileobj)
                        continue
                    target = key.data
                    target.extend(chunk)
                    if len(output) + len(errors) > MAX_MESSAGE_BYTES:
                        raise FleetError(
                            "Remote Pika response exceeded the safety limit",
                            kind="incompatible",
                        )
            remaining = max(0.01, deadline - time.monotonic())
            returncode = process.wait(timeout=remaining)
        except BaseException:
            process.kill()
            process.wait()
            raise
        finally:
            selector.close()
            process.stdout.close()
            process.stderr.close()
        return subprocess.CompletedProcess(
            command,
            returncode,
            bytes(output).decode("utf-8", errors="replace"),
            bytes(errors).decode("utf-8", errors="replace"),
        )

    def request(
        self, target: str, payload: dict[str, Any], *, mutating: bool = False
    ) -> dict[str, Any]:
        line = json.dumps(payload, ensure_ascii=False, separators=(",", ":")) + "\n"
        if len(line.encode("utf-8")) > MAX_MESSAGE_BYTES:
            raise FleetError(
                "Fleet request exceeded the safety limit", kind="invalid_request"
            )
        try:
            result = self._bounded_run(
                [*self.base(target), "pika", "_fleet", "--stdio"],
                line.encode("utf-8"),
            )
        except FileNotFoundError:
            raise FleetError(
                "OpenSSH is not installed on this machine", kind="auth"
            ) from None
        except subprocess.TimeoutExpired:
            kind = "outcome_unknown" if mutating else "unreachable"
            raise FleetError(
                f"SSH timed out after {self.overall_timeout:g}s",
                kind=kind,
            ) from None
        if result.returncode != 0:
            message = terminal_text((result.stderr or result.stdout).strip())[:500]
            folded = message.casefold()
            if (
                result.returncode == 127
                or "pika: command not found" in folded
                or "pika: not found" in folded
            ):
                kind = "missing"
            elif "invalid choice" in folded and "_fleet" in folded:
                kind = "incompatible"
            elif any(
                term in folded
                for term in (
                    "permission denied",
                    "host key verification",
                    "no identities",
                )
            ):
                kind = "auth"
            elif mutating:
                kind = "outcome_unknown"
            else:
                kind = "unreachable"
            raise FleetError(message or f"SSH exited {result.returncode}", kind=kind)
        raw = result.stdout
        lines = [line for line in raw.splitlines() if line.strip()]
        if len(lines) != 1:
            raise FleetError(
                "Remote Pika emitted a malformed JSONL response", kind="incompatible"
            )
        try:
            response = json.loads(lines[0])
        except ValueError:
            raise FleetError(
                "Remote Pika emitted non-JSON output", kind="incompatible"
            ) from None
        if not isinstance(response, dict):
            raise FleetError(
                "Remote Pika response is not an object", kind="incompatible"
            )
        if response.get("type") == "error":
            raise FleetError(
                terminal_text(response.get("message") or "remote Pika error"),
                kind=str(response.get("kind") or "error"),
            )
        return response

    def run_exact(self, node: FleetNode, arguments: list[str], *, tty: bool) -> int:
        result = subprocess.run(
            [*self.base(node.ssh_target, tty=tty), "pika", *arguments],
            check=False,
        )
        return result.returncode

    def install(self, target: str) -> tuple[int, str]:
        try:
            result = subprocess.run(
                [*self.base(target), *REMOTE_INSTALL_ARGV],
                capture_output=True,
                text=True,
                timeout=180,
                check=False,
            )
        except (OSError, subprocess.TimeoutExpired) as exc:
            return 1, terminal_text(exc)
        detail = (result.stdout or result.stderr).strip()
        return result.returncode, terminal_text(detail)[-1000:]


class FleetManager:
    def __init__(self, store: Store, transport: SSHTransport | None = None) -> None:
        self.store = store
        self.transport = transport or SSHTransport()

    def discover(self) -> list[NodeCandidate]:
        return discover_node_candidates(self.store)

    def discover_report(self) -> NodeDiscoveryReport:
        candidates = self.discover()
        ssh_files = _ssh_config_files(Path.home() / ".ssh")
        tailscale = tailscale_discovery_summary()
        return NodeDiscoveryReport(
            candidates=tuple(candidates),
            ssh_aliases=len(discover_ssh_candidates()),
            ssh_config_files=len(ssh_files),
            tailscale_total=int(tailscale["total"] or 0),
            tailscale_compatible=int(tailscale["compatible"] or 0),
            excluded_non_linux=int(tailscale["non_linux"] or 0),
            excluded_no_target=int(tailscale["no_target"] or 0),
            tailscale_error=str(tailscale["error"]) if tailscale["error"] else None,
        )

    def nodes(self) -> list[FleetNode]:
        return self.store.list_fleet_nodes()

    def handshake(
        self, candidate: NodeCandidate, *, alias: str | None = None
    ) -> FleetNode:
        response = self.transport.request(
            candidate.ssh_target,
            {"op": "hello", "protocol": PROTOCOL_NAME, "version": PROTOCOL_VERSION},
        )
        if set(response) != _HELLO_FIELDS:
            raise FleetError(
                "Remote Pika handshake envelope is malformed", kind="incompatible"
            )
        if response.get("type") != "hello" or response.get("protocol") != PROTOCOL_NAME:
            raise FleetError(
                "Remote did not return a Pika fleet handshake", kind="incompatible"
            )
        if response.get("version") != PROTOCOL_VERSION:
            raise FleetError(
                f"Remote Pika protocol {response.get('version')} is incompatible with {PROTOCOL_VERSION}",
                kind="incompatible",
            )
        if (
            not isinstance(response.get("machine"), str)
            or not response["machine"]
            or len(response["machine"]) > 63
        ):
            raise FleetError(
                "Remote Pika handshake has an invalid machine name",
                kind="incompatible",
            )
        try:
            node_id = str(uuid.UUID(str(response.get("node_id"))))
        except ValueError:
            raise FleetError(
                "Remote Pika returned an invalid node identity", kind="incompatible"
            ) from None
        if node_id == self.store.local_node_id():
            raise FleetError(
                "That SSH target resolves back to this Pika node", kind="quarantined"
            )
        existing = next(
            (item for item in self.nodes() if item.node_id == node_id), None
        )
        chosen_alias = (
            existing.alias if existing else machine_alias(alias or candidate.alias)
        )
        capabilities = response.get("capabilities")
        if (
            not isinstance(capabilities, list)
            or not all(isinstance(item, str) for item in capabilities)
            or len(capabilities) != len(set(capabilities))
            or not set(CAPABILITIES).issubset(capabilities)
        ):
            raise FleetError(
                "Remote Pika lacks required fleet capabilities", kind="incompatible"
            )
        package_version = response.get("package_version")
        if (
            not isinstance(package_version, str)
            or not package_version
            or len(package_version) > 64
        ):
            raise FleetError(
                "Remote Pika did not report a valid package version",
                kind="incompatible",
            )
        now = time.time()
        return FleetNode(
            node_id=node_id,
            alias=chosen_alias,
            ssh_target=candidate.ssh_target,
            sources=candidate.sources,
            status="ready",
            protocol_version=PROTOCOL_VERSION,
            package_version=package_version,
            capabilities=tuple(str(item) for item in capabilities),
            last_seen=existing.last_seen if existing else 0.0,
            last_attempt_at=now,
            created_at=existing.created_at if existing else now,
            updated_at=now,
        )

    def add(self, candidate: NodeCandidate, *, alias: str | None = None) -> FleetNode:
        node = self.handshake(candidate, alias=alias)
        response = self.transport.request(
            node.ssh_target,
            {
                "op": "snapshot",
                "expert_directory": True,
                "protocol": PROTOCOL_NAME,
                "version": PROTOCOL_VERSION,
                "expected_node_id": node.node_id,
            },
        )
        snapshot = validate_snapshot(response, expected_node_id=node.node_id)
        self.store.upsert_fleet_node(node)
        self.store.put_remote_snapshot(node.node_id, snapshot)
        saved = self.store.get_fleet_node(node.node_id)
        assert saved is not None
        return saved

    def refresh_node(self, value: str) -> list[FleetSession]:
        node = self.store.get_fleet_node(value)
        if node is None:
            raise FleetError(f"Unknown Pika machine {value!r}")
        try:
            response = self.transport.request(
                node.ssh_target,
                {
                    "op": "snapshot",
                    "expert_directory": True,
                    "protocol": PROTOCOL_NAME,
                    "version": PROTOCOL_VERSION,
                    "expected_node_id": node.node_id,
                },
            )
            snapshot = validate_snapshot(response, expected_node_id=node.node_id)
        except FleetError as exc:
            status = (
                exc.kind
                if exc.kind in {"unreachable", "auth", "incompatible", "quarantined"}
                else "error"
            )
            self.store.mark_fleet_node_error(node.node_id, status, str(exc))
            raise
        self.store.put_remote_snapshot(node.node_id, snapshot)
        return self.cached_sessions(node.node_id)

    def cached_sessions(self, node_id: str | None = None) -> list[FleetSession]:
        now = time.time()
        result: list[FleetSession] = []
        nodes = self.nodes()
        if node_id:
            nodes = [item for item in nodes if item.node_id == node_id]
        for node in nodes:
            stored = self.store.get_remote_snapshot(node.node_id)
            if stored is None:
                continue
            payload, fetched_at = stored
            try:
                validated = validate_snapshot(payload, expected_node_id=node.node_id)
            except FleetError:
                continue
            stale = node.status != "ready" or now - fetched_at > REMOTE_STALE_SECONDS
            cards = {
                (str(item.get("provider")), str(item.get("session_id"))): item
                for item in validated.get("cards", [])
                if isinstance(item, dict)
            }
            for raw in validated["sessions"]:
                session = _session_from_wire(raw)
                card = cards.get(session.key, {})
                result.append(
                    FleetSession(
                        node.node_id,
                        node.alias,
                        session,
                        stale=stale,
                        remote_error=node.last_error,
                        seen_at=fetched_at,
                        card_status=(
                            str(card.get("status")) if card.get("status") else None
                        ),
                        card_detail=(
                            str(card.get("detail")) if card.get("detail") else None
                        ),
                    )
                )
        return result

    def cached_expert_sessions(self, node_id: str | None = None) -> list[FleetSession]:
        """Read retained expertise without adding unwatched rows to the board."""
        result = self.cached_sessions(node_id)
        now = time.time()
        for node in self.nodes():
            if node_id is not None and node.node_id != node_id:
                continue
            stored = self.store.get_remote_snapshot(node.node_id)
            if stored is None:
                continue
            try:
                payload = validate_snapshot(stored[0], expected_node_id=node.node_id)
            except FleetError:
                continue
            fetched_at = stored[1]
            for raw in payload.get("expert_sessions", []):
                result.append(FleetSession(
                    node.node_id, node.alias, _session_from_wire(raw),
                    stale=node.status != "ready" or now - fetched_at > REMOTE_STALE_SECONDS,
                    remote_error=node.last_error, seen_at=fetched_at, watched=False,
                ))
            cards = {(item["provider"], item["session_id"]): item for item in payload["cards"]}
            profiles = {(item["provider"], item["session_id"]): item for item in payload["profiles"]}
            for session in result:
                if session.node_id != node.node_id:
                    continue
                card = cards.get(session.local_key, {})
                profile = profiles.get(session.local_key, {})
                session.card_status = card.get("status")
                session.card_detail = card.get("detail")
                session.availability = card.get("availability")
                session.scope_updated_at = profile.get("scope_updated_at")
                session.current_state_updated_at = profile.get("current_state_updated_at")
                session.current_state_status = card.get("current_state_status")
        return result

    def remote_candidates(self, node: FleetNode) -> list[Candidate]:
        response = self.transport.request(
            node.ssh_target,
            {
                "op": "candidates",
                "protocol": PROTOCOL_NAME,
                "version": PROTOCOL_VERSION,
                "expected_node_id": node.node_id,
            },
        )
        values = response.get("candidates")
        if (
            set(response) != {"type", "node_id", "candidates"}
            or response.get("type") != "candidates"
            or response.get("node_id") != node.node_id
            or not isinstance(values, list)
        ):
            raise FleetError(
                "Remote candidate inventory is malformed", kind="incompatible"
            )
        result: list[Candidate] = []
        for value in values:
            if not isinstance(value, dict) or set(value) != _CANDIDATE_FIELDS:
                raise FleetError("Remote candidate is malformed", kind="incompatible")
            session = _session_from_wire(
                {
                    **value,
                    "status": Status.UNBOUND.value
                    if value.get("live")
                    else Status.PARKED.value,
                    "unread": False,
                }
            )
            result.append(
                Candidate(
                    session.provider,
                    session.session_id,
                    session.name,
                    session.cwd,
                    session.branch,
                    None,
                    session.model,
                    session.updated_at,
                    session.live,
                    None,
                    str(value.get("source") or "remote"),
                )
            )
        return result

    def adopt(
        self, node: FleetNode, candidate: Candidate, *, request_id: str | None = None
    ) -> Session:
        pending_key = (
            f"fleet:pending-adopt:{node.node_id}:"
            f"{candidate.provider}:{candidate.session_id}"
        )
        request_id = request_id or self.store.get_meta(pending_key) or str(uuid.uuid4())
        # Persist before crossing SSH. If the coordinator dies after the remote
        # mutation but before reading its receipt, the next attempt replays the
        # same idempotency key instead of creating a second operation.
        self.store.set_meta(pending_key, request_id)
        try:
            response = self.transport.request(
                node.ssh_target,
                {
                    "op": "adopt",
                    "protocol": PROTOCOL_NAME,
                    "version": PROTOCOL_VERSION,
                    "expected_node_id": node.node_id,
                    "provider": candidate.provider,
                    "session_id": candidate.session_id,
                    "request_id": request_id,
                },
                mutating=True,
            )
        except FleetError as exc:
            if exc.kind == "outcome_unknown":
                raise FleetError(
                    f"ADOPTION OUTCOME UNKNOWN on {node.alias} · request {request_id}",
                    kind=exc.kind,
                ) from exc
            raise
        receipt = _validate_mutation_receipt(
            response,
            operation="adopted",
            expected_node_id=node.node_id,
            expected_request_id=request_id,
        )
        raw_session = receipt.get("session")
        if not isinstance(raw_session, dict) or set(raw_session) != _SESSION_FIELDS:
            raise FleetError(
                "Remote adoption receipt has a malformed session envelope",
                kind="quarantined",
            )
        session = _session_from_wire(raw_session)
        if session.key != (candidate.provider, candidate.session_id):
            raise FleetError(
                "Remote adoption receipt has the wrong identity", kind="quarantined"
            )
        self.store.delete_meta(pending_key)
        # The durable mutation receipt is authoritative. A subsequent snapshot
        # failure makes the cache stale, but must not relabel a proven adoption
        # as failed or invite the user to repeat it.
        try:
            self.refresh_node(node.node_id)
        except FleetError:
            pass
        return session

    def resolve(self, query: str, *, fresh: bool = True, include_experts: bool = False) -> FleetSession | None:
        if "@" not in query:
            return None
        thread, alias = query.rsplit("@", 1)
        node = self.store.get_fleet_node(alias)
        if node is None:
            return None
        if fresh:
            self.refresh_node(node.node_id)
        sessions = (self.cached_expert_sessions(node.node_id) if include_experts else self.cached_sessions(node.node_id))
        if fresh and any(item.stale for item in sessions):
            raise FleetError(
                f"{alias} is not current; cached metadata was kept but no action was taken",
                kind="unreachable",
            )
        exact_id = [item for item in sessions if item.session_id == thread]
        matches = exact_id or [
            item
            for item in sessions
            if item.name and item.name.casefold() == thread.casefold()
        ]
        if not matches:
            prefixes = [item for item in sessions if item.session_id.startswith(thread)]
            if len(prefixes) == 1:
                return prefixes[0]
            raise FleetError(f"NOT FOUND ON FRESH LOOKUP: {query}", kind="not_found")
        if len(matches) == 1:
            return matches[0]
        selected = choose_session(
            [item.session for item in matches],
            f"Multiple providers have {thread!r} on {alias}",
        )
        return next(item for item in matches if item.local_key == selected.key)

    def attach(self, session: FleetSession) -> int:
        current = self.resolve(f"{session.session_id}@{session.node_name}", fresh=True)
        assert current is not None
        node = self.store.get_fleet_node(session.node_id)
        assert node is not None
        print(f"ROUTE VERIFIED · {node.alias} · node {node.node_id[:8]}")
        return self.transport.run_exact(
            node,
            [
                "_fleet-open",
                "--expected-node-id",
                node.node_id,
                "--provider",
                current.provider,
                "--session-id",
                current.session_id,
            ],
            tty=True,
        )

    def capture(self, session: FleetSession, lines: int) -> str:
        response = self._session_request(session, "peek", lines=lines)
        if (
            set(response) != {"type", "node_id", "text"}
            or response.get("type") != "peek"
            or response.get("node_id") != session.node_id
            or not isinstance(response.get("text"), str)
        ):
            raise FleetError("Remote peek receipt is malformed", kind="quarantined")
        return response["text"]

    def acknowledge(self, session: FleetSession) -> bool:
        response = self._session_request(session, "acknowledge", mutating=True)
        if (
            set(response) != {"type", "node_id", "acknowledged"}
            or response.get("type") != "acknowledged"
            or response.get("node_id") != session.node_id
            or not isinstance(response.get("acknowledged"), bool)
        ):
            raise FleetError(
                "Remote acknowledgement receipt is malformed", kind="outcome_unknown"
            )
        return response["acknowledged"]

    def untrack(self, session: FleetSession, *, request_id: str | None = None) -> int:
        pending_key = (
            f"fleet:pending-untrack:{session.node_id}:"
            f"{session.provider}:{session.session_id}"
        )
        request_id = request_id or self.store.get_meta(pending_key) or str(uuid.uuid4())
        self.store.set_meta(pending_key, request_id)
        try:
            response = self._session_request(
                session,
                "untrack",
                mutating=True,
                request_id=request_id,
            )
        except FleetError as exc:
            if exc.kind == "outcome_unknown":
                raise FleetError(
                    f"UNTRACK OUTCOME UNKNOWN on {session.node_name} · "
                    f"request {request_id}",
                    kind=exc.kind,
                ) from exc
            raise
        receipt = _validate_mutation_receipt(
            response,
            operation="untracked",
            expected_node_id=session.node_id,
            expected_request_id=request_id,
        )
        self.store.delete_meta(pending_key)
        try:
            self.refresh_node(session.node_id)
        except FleetError:
            pass
        return int(receipt["panes_cleared"])

    def _session_request(
        self,
        session: FleetSession,
        op: str,
        *,
        mutating: bool = False,
        **extra: object,
    ) -> dict[str, Any]:
        node = self.store.get_fleet_node(session.node_id)
        if node is None:
            raise FleetError(f"Machine {session.node_name!r} is no longer adopted")
        return self.transport.request(
            node.ssh_target,
            {
                "op": op,
                "protocol": PROTOCOL_NAME,
                "version": PROTOCOL_VERSION,
                "expected_node_id": node.node_id,
                "provider": session.provider,
                "session_id": session.session_id,
                **extra,
            },
            mutating=mutating,
        )

    def expert_matches(self, query: str = "") -> list[ExpertMatch]:
        matches: list[ExpertMatch] = []
        by_node: dict[str, list[FleetSession]] = {}
        for session in self.cached_expert_sessions():
            by_node.setdefault(session.node_id, []).append(session)
        for node_id, sessions in by_node.items():
            stored = self.store.get_remote_snapshot(node_id)
            if stored is None:
                continue
            profiles: list[ExpertProfile] = []
            for raw in stored[0].get("profiles", []):
                if not isinstance(raw, dict):
                    continue
                try:
                    profiles.append(
                        ExpertProfile(
                            str(raw["provider"]),
                            str(raw["session_id"]),
                            str(raw["scope"]),
                            tuple(map(str, raw.get("topics", []))),
                            tuple(map(str, raw.get("artifacts", []))),
                            float(raw.get("updated_at") or 0),
                            str(raw.get("source") or "remote"),
                            current_state=str(raw.get("current_state") or ""),
                            scope_updated_at=raw.get("scope_updated_at"),
                            current_state_updated_at=raw.get("current_state_updated_at"),
                        )
                    )
                except (KeyError, TypeError, ValueError):
                    continue
            # Rank per node against local-shaped copies, then restore the fleet
            # wrapper so equal provider UUIDs on two nodes never collide.
            wrappers = {item.local_key: item for item in sessions}
            for match in rank_experts(
                profiles, [item.session for item in sessions], query
            ):
                wrapper = wrappers.get(match.session.key)
                if wrapper is not None:
                    matches.append(
                        ExpertMatch(
                            match.profile,
                            wrapper,
                            match.score,
                            match.matched_on,
                            watched=wrapper.watched,
                        )
                    )
        return sorted(
            matches,
            key=lambda item: (
                not isinstance(item.session, FleetSession) or not item.session.stale,
                item.score,
                item.profile.updated_at,
            ),
            reverse=True,
        )

    def expert_card_states(self) -> list[ExpertCardState]:
        result: list[ExpertCardState] = []
        sessions = {item.key: item for item in self.cached_expert_sessions()}
        profiles = {item.session.key: item.profile for item in self.expert_matches("")}
        for node in self.nodes():
            stored = self.store.get_remote_snapshot(node.node_id)
            if stored is None:
                continue
            for raw in stored[0].get("cards", []):
                if not isinstance(raw, dict):
                    continue
                key = (
                    node.node_id,
                    str(raw.get("provider")),
                    str(raw.get("session_id")),
                )
                session = sessions.get(key)
                if session is None:
                    continue
                result.append(
                    ExpertCardState(
                        session,
                        profiles.get(key),
                        session.card_status or "UNKNOWN",
                        session.card_detail or "remote card state unavailable",
                    )
                )
        return result

    def consultation(
        self, session: FleetSession, policy: ConsultationPolicy
    ) -> RemoteConsultation:
        node = self.store.get_fleet_node(session.node_id)
        if node is None:
            raise ConsultationError(
                f"Machine {session.node_name!r} is no longer adopted"
            )
        return RemoteConsultation(node, session, policy, self.transport)


class RemoteConsultation:
    def __init__(
        self,
        node: FleetNode,
        session: FleetSession,
        policy: ConsultationPolicy,
        transport: SSHTransport,
    ) -> None:
        self.node = node
        self.session = session
        self.policy = policy
        self._progress_callback = None
        self._cleanup_confirmed = False
        self._transport_aborted = False
        self._reader_lock = threading.RLock()
        command = [
            *transport.base(node.ssh_target),
            "pika",
            "_fleet-ask",
            "--expected-node-id",
            node.node_id,
            "--provider",
            session.provider,
            "--session-id",
            session.session_id,
        ]
        if policy.mode == "fast":
            command.append("--fast")
        try:
            self.process = subprocess.Popen(
                command,
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                bufsize=0,
            )
        except OSError as exc:
            raise ConsultationError(
                f"Could not start remote side channel: {exc}"
            ) from exc
        self._stdout_buffer = bytearray()
        self._stderr_buffer = bytearray()
        try:
            opened = self._read_event(240.0)
        except BaseException as exc:
            self._abort()
            exc.cleanup = "unknown"
            raise
        if (
            opened.get("type") != "opened"
            or opened.get("parent_id") != session.provider_thread_id
            or opened.get("workstream_id", opened.get("parent_id")) != session.session_id
            or opened.get("provider") != session.provider
        ):
            self._abort()
            raise ConsultationError(
                "Remote side channel returned a different conversation identity; no question was sent. "
                f"Refresh expert lookup for {session.session_id}@{session.node_name} before retrying."
            )
        receipt = policy.receipt()
        for key in ("consultation_mode", "model", "effort"):
            if opened.get(key) != receipt.get(key):
                self._abort()
                raise ConsultationError(
                    f"Remote side channel did not honor requested {key.replace('_', ' ')}"
                )

    def __enter__(self) -> RemoteConsultation:
        return self

    def __exit__(self, *_args: object) -> None:
        self.close()

    def set_progress_callback(self, callback) -> None:
        self._progress_callback = callback

    def _read_event(self, timeout: float) -> dict[str, Any]:
        if not hasattr(self, "_reader_lock"):
            self._reader_lock = threading.RLock()
        with self._reader_lock:
            return self._read_event_serial(timeout)

    def _read_event_serial(self, timeout: float) -> dict[str, Any]:
        deadline = time.monotonic() + timeout
        try:
            while True:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise ConsultationError("Remote side channel timed out")
                event = self._read_event_bounded(remaining)
                if event.get("type") != "progress":
                    return event
                stage = event.get("stage")
                delivery = event.get("delivery")
                if (
                    self._progress_callback
                    and stage in {"prepare", "turn", "response", "cleanup"}
                    and delivery in {"not_sent", "unknown", "confirmed"}
                ):
                    self._progress_callback(stage, delivery)
        except ConsultationError as exc:
            # A remote provider error is followed by its cleanup receipt. Keep
            # the channel alive so close() can collect that evidence.
            if not isinstance(getattr(exc, "receipt", None), dict):
                self._abort()
            raise

    def _read_event_bounded(self, timeout: float) -> dict[str, Any]:
        assert self.process.stdout is not None
        assert self.process.stderr is not None
        deadline = time.monotonic() + timeout
        selector = selectors.DefaultSelector()
        selector.register(self.process.stdout, selectors.EVENT_READ, "stdout")
        selector.register(self.process.stderr, selectors.EVENT_READ, "stderr")
        line: bytes | None = None
        try:
            while line is None:
                newline = self._stdout_buffer.find(b"\n")
                if newline >= 0:
                    line = bytes(self._stdout_buffer[:newline])
                    del self._stdout_buffer[: newline + 1]
                    break
                if len(self._stdout_buffer) > MAX_MESSAGE_BYTES:
                    raise ConsultationError(
                        "Remote side channel frame exceeded the safety limit"
                    )
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise ConsultationError("Remote side channel timed out")
                events = selector.select(remaining)
                if not events:
                    raise ConsultationError("Remote side channel timed out")
                for key, _mask in events:
                    try:
                        chunk = os.read(key.fd, 65536)
                    except BlockingIOError:
                        continue
                    if not chunk:
                        selector.unregister(key.fileobj)
                        if key.data == "stdout":
                            if self._stdout_buffer:
                                raise ConsultationError(
                                    "Remote side channel ended with a partial JSONL frame"
                                )
                            detail = bytes(self._stderr_buffer).decode(
                                "utf-8", errors="replace"
                            )
                            raise ConsultationError(
                                terminal_text(detail).strip()[-500:]
                                or "Remote side channel closed"
                            )
                        continue
                    if key.data == "stdout":
                        self._stdout_buffer.extend(chunk)
                    else:
                        self._stderr_buffer.extend(chunk)
                        if len(self._stderr_buffer) > MAX_STDERR_BYTES:
                            raise ConsultationError(
                                "Remote side channel stderr exceeded the safety limit"
                            )
        finally:
            selector.close()
        assert line is not None
        if len(line) > MAX_MESSAGE_BYTES:
            raise ConsultationError(
                "Remote side channel frame exceeded the safety limit"
            )
        try:
            event = json.loads(line.decode("utf-8"))
        except (UnicodeDecodeError, ValueError):
            raise ConsultationError(
                "Remote side channel emitted malformed JSONL"
            ) from None
        if not isinstance(event, dict):
            raise ConsultationError("Remote side channel event is not an object")
        if event.get("type") == "error":
            error = ConsultationError(
                str(event.get("message") or "remote consultation failed")
            )
            error.receipt = {
                key: event[key] for key in (
                    "stage", "delivery", "cleanup", "answers_received", "turn",
                    "elapsed_seconds", "stage_elapsed_seconds", "retry_safe",
                ) if key in event
            }
            raise error
        return event

    def _abort(self) -> None:
        self._transport_aborted = True
        process = getattr(self, "process", None)
        if process is None or process.poll() is not None:
            return
        try:
            process.terminate()
            process.wait(timeout=1)
        except (OSError, subprocess.TimeoutExpired):
            process.kill()

    def ask(self, question: str) -> str:
        with self._reader_lock:
            return self._ask_serial(question)

    def _ask_serial(self, question: str) -> str:
        if self._progress_callback:
            self._progress_callback("turn", "not_sent")
        if not question.strip():
            raise ConsultationError("Question cannot be empty")
        if self._cleanup_confirmed or self._transport_aborted:
            raise ConsultationError("Remote side channel is closed")
        assert self.process.stdin is not None
        if self._progress_callback:
            self._progress_callback("turn", "unknown")
        self.process.stdin.write(
            (json.dumps({"question": question}, ensure_ascii=False) + "\n").encode(
                "utf-8"
            )
        )
        self.process.stdin.flush()
        event = self._read_event(1200.0)
        if event.get("type") != "answer" or not isinstance(event.get("text"), str):
            raise ConsultationError("Remote side channel returned an invalid answer")
        if self._progress_callback:
            self._progress_callback("response", "confirmed")
        return str(event["text"])

    def close(self) -> None:
        if not self._reader_lock.acquire(blocking=False):
            # The board can cancel from another thread. Do not run a second
            # stdout reader and accidentally consume the answer as a receipt.
            # SSH teardown alone cannot prove remote provider cleanup.
            self._abort()
            error = ConsultationError(
                "Remote side connection closed while an answer was pending; cleanup is unconfirmed. "
                f"Do not resend the question; inspect the consultation on {self.node.ssh_target}."
            )
            error.receipt = {"stage": "cleanup", "cleanup": "unknown"}
            raise error
        try:
            self._close_serial()
        finally:
            self._reader_lock.release()

    def _close_serial(self) -> None:
        if self._cleanup_confirmed:
            return
        process = getattr(self, "process", None)
        if process is None or self._transport_aborted:
            error = ConsultationError(
                "Remote cleanup is unconfirmed after the connection closed. "
                "Do not resend the question; inspect the consultation on "
                f"{self.node.ssh_target}."
            )
            error.receipt = {"stage": "cleanup", "cleanup": "unknown"}
            raise error
        deadline = time.monotonic() + 30.0
        try:
            if process.stdin is not None and not process.stdin.closed:
                try:
                    process.stdin.write(b'{"close":true}\n')
                    process.stdin.flush()
                    process.stdin.close()
                except BrokenPipeError:
                    # The remote may already have failed and queued a terminal
                    # cleanup receipt before closing its input.
                    pass
            while True:
                try:
                    event = self._read_event(max(0.0, deadline - time.monotonic()))
                except ConsultationError as exc:
                    if isinstance(getattr(exc, "receipt", None), dict):
                        continue
                    raise
                if event.get("type") != "closed":
                    raise ConsultationError("Remote cleanup returned an unexpected event")
                if (
                    event.get("receipt_version") != 2
                    or event.get("discarded") is not True
                    or event.get("cleanup") != "complete"
                ):
                    error = ConsultationError(
                        "Remote side cleanup was not verified. Keep any received answer; "
                        f"inspect the consultation on {self.node.ssh_target}."
                    )
                    error.receipt = {
                        "stage": "cleanup",
                        "cleanup": "failed" if event.get("cleanup") == "failed" else "unknown",
                    }
                    raise error
                process.wait(timeout=max(0.1, min(2.0, deadline - time.monotonic())))
                self._cleanup_confirmed = True
                return
        except (OSError, ValueError, subprocess.TimeoutExpired, ConsultationError) as exc:
            self._abort()
            if isinstance(exc, ConsultationError):
                if not isinstance(getattr(exc, "receipt", None), dict):
                    exc.receipt = {"stage": "cleanup", "cleanup": "unknown"}
                raise
            error = ConsultationError(
                "Remote cleanup could not be confirmed; keep any received answer. "
                f"Inspect the consultation on {self.node.ssh_target}: {exc}"
            )
            error.receipt = {"stage": "cleanup", "cleanup": "unknown"}
            raise error from exc


def _exact_local_session(pika: Any, provider: object, session_id: object) -> Session:
    if provider not in {"codex", "claude", "opencode"} or not isinstance(session_id, str):
        raise FleetError("Invalid exact session identity", kind="invalid_request")
    session = pika.store.get_session(provider, session_id)
    if session is None:
        raise FleetError("Exact session is not tracked on this node", kind="not_found")
    current = next((item for item in pika.refresh() if item.key == session.key), None)
    if current is None:
        raise FleetError(
            "Exact session disappeared during reconciliation", kind="not_found"
        )
    return current


def handle_fleet_stdio(pika: Any, stdin: Any, stdout: Any) -> int:
    """Serve authenticated SSH callers; each input line receives one JSON line."""
    node_id = pika.store.local_node_id()

    def emit(payload: dict[str, Any]) -> None:
        stdout.write(
            json.dumps(payload, ensure_ascii=False, separators=(",", ":")) + "\n"
        )
        stdout.flush()

    for raw in stdin:
        if len(raw.encode()) > MAX_MESSAGE_BYTES:
            emit(
                {
                    "type": "error",
                    "kind": "invalid_request",
                    "message": "request too large",
                }
            )
            continue
        try:
            request = json.loads(raw)
            if not isinstance(request, dict):
                raise ValueError("request must be an object")
            if (
                request.get("protocol") != PROTOCOL_NAME
                or request.get("version") != PROTOCOL_VERSION
            ):
                raise FleetError(
                    f"Pika fleet protocol {PROTOCOL_VERSION} required",
                    kind="incompatible",
                )
            expected = request.get("expected_node_id")
            if expected is not None and expected != node_id:
                raise FleetError(
                    f"NODE IDENTITY CHANGED: expected {str(expected)[:8]}, received {node_id[:8]}",
                    kind="quarantined",
                )
            op = request.get("op")
            if op == "hello":
                emit(
                    {
                        "type": "hello",
                        "protocol": PROTOCOL_NAME,
                        "version": PROTOCOL_VERSION,
                        "node_id": node_id,
                        "machine": local_machine_name(),
                        "package_version": __version__,
                        "capabilities": list(CAPABILITIES),
                    }
                )
                continue
            if op == "snapshot":
                sessions = pika.refresh(usage=False)
                profiles = pika.store.list_expert_profiles()
                extended = request.get("expert_directory") is True
                unwatched = []
                if extended:
                    loader = getattr(pika.store, "list_untracked_sessions", None)
                    unwatched = loader() if callable(loader) else []
                    hidden = pika.hidden_session_keys(unwatched) if unwatched else set()
                    profile_keys = {item.key for item in profiles}
                    unwatched = [item for item in unwatched if item.key not in hidden and item.key in profile_keys]
                expert_sessions = [*sessions, *unwatched]
                visible_keys = {item.key for item in expert_sessions}
                profiles = [item for item in profiles if item.key in visible_keys]
                cards = pika.expert_card_states(expert_sessions)
                card_keys = {item.session.key for item in cards}
                unwatched = [item for item in unwatched if item.key in card_keys]
                visible_keys = {item.key for item in [*sessions, *unwatched]}
                profiles = [item for item in profiles if item.key in visible_keys]
                unwatched_keys = {item.key for item in unwatched}
                emit(
                    {
                        "type": "snapshot",
                        "protocol": PROTOCOL_NAME,
                        "version": PROTOCOL_VERSION,
                        "node_id": node_id,
                        "machine": local_machine_name(),
                        "captured_at": time.time(),
                        "sessions": [session_to_wire(item, extended=extended) for item in sessions],
                        "profiles": [_profile_to_wire(item, extended=extended) for item in profiles],
                        "cards": [
                            {
                                "provider": item.session.provider,
                                "session_id": item.session.session_id,
                                "status": item.status,
                                "detail": item.detail,
                                **({
                                    "watched": item.session.key not in unwatched_keys,
                                    "availability": item.availability or expert_availability(item.session),
                                    "current_state_status": profile_freshness(item.session, item.profile)["current_state_status"],
                                } if extended else {}),
                            }
                            for item in cards
                        ],
                        **({"expert_sessions": [session_to_wire(item, extended=True) for item in unwatched]} if extended else {}),
                    }
                )
                continue
            if op == "candidates":
                tracked = {item.key for item in pika.store.list_sessions()}
                untracked = pika.store.untracked_session_keys()
                candidates = [
                    item
                    for item in pika.discover_import_candidates()
                    if item.name or item.live
                    if (item.provider, item.session_id) not in tracked
                    and (item.provider, item.session_id) not in untracked
                ]
                emit(
                    {
                        "type": "candidates",
                        "node_id": node_id,
                        "candidates": [candidate_to_wire(item) for item in candidates],
                    }
                )
                continue
            if op == "adopt":
                request_id = str(request.get("request_id") or "")
                try:
                    request_id = str(uuid.UUID(request_id))
                except ValueError:
                    raise FleetError(
                        "Adoption requires a UUID request_id", kind="invalid_request"
                    ) from None
                receipt_key = f"fleet:adopt:{request_id}"
                saved = pika.store.get_meta(receipt_key)
                if saved:
                    emit(json.loads(saved))
                    continue
                provider = request.get("provider")
                session_id = request.get("session_id")
                session = pika.store.get_session(str(provider), str(session_id))
                if session is None:
                    matches = [
                        item
                        for item in pika.discover_import_candidates()
                        if (item.provider, item.session_id) == (provider, session_id)
                    ]
                    if len(matches) != 1:
                        raise FleetError(
                            "Exact adoption candidate is unavailable", kind="not_found"
                        )
                    session = pika.import_candidate(matches[0])
                receipt = {
                    "type": "adopted",
                    "node_id": node_id,
                    "request_id": request_id,
                    "session": session_to_wire(session),
                }
                pika.store.set_meta(
                    receipt_key, json.dumps(receipt, separators=(",", ":"))
                )
                emit(receipt)
                continue
            if op == "untrack":
                request_id = str(request.get("request_id") or "")
                try:
                    request_id = str(uuid.UUID(request_id))
                except ValueError:
                    raise FleetError(
                        "Untrack requires a UUID request_id", kind="invalid_request"
                    ) from None
                receipt_key = f"fleet:untrack:{request_id}"
                saved = pika.store.get_meta(receipt_key)
                if saved:
                    emit(json.loads(saved))
                    continue
                provider = request.get("provider")
                session_id = request.get("session_id")
                if (
                    provider in {"codex", "claude", "opencode"}
                    and isinstance(session_id, str)
                    and pika.store.is_untracked(provider, session_id)
                ):
                    receipt = {
                        "type": "untracked",
                        "node_id": node_id,
                        "request_id": request_id,
                        "panes_cleared": 0,
                        "reconciled": True,
                    }
                    pika.store.set_meta(
                        receipt_key, json.dumps(receipt, separators=(",", ":"))
                    )
                    emit(receipt)
                    continue
            session = _exact_local_session(
                pika, request.get("provider"), request.get("session_id")
            )
            if op == "peek":
                pane = pika.tmux.get_pane(
                    session.tmux_pane or session.tmux_session or ""
                )
                if pane is None:
                    raise FleetError(
                        "Exact session has no surviving pane", kind="not_found"
                    )
                lines = max(1, min(2000, int(request.get("lines") or 200)))
                emit(
                    {
                        "type": "peek",
                        "node_id": node_id,
                        "text": pika.capture(session, lines),
                    }
                )
            elif op == "acknowledge":
                emit(
                    {
                        "type": "acknowledged",
                        "node_id": node_id,
                        "acknowledged": pika.acknowledge(session),
                    }
                )
            elif op == "untrack":
                cleared = pika.untrack(session)
                receipt = {
                    "type": "untracked",
                    "node_id": node_id,
                    "request_id": request_id,
                    "panes_cleared": cleared,
                }
                pika.store.set_meta(
                    receipt_key, json.dumps(receipt, separators=(",", ":"))
                )
                emit(receipt)
            else:
                raise FleetError(
                    f"Unsupported fleet operation {op!r}", kind="invalid_request"
                )
        except (FleetError, TypeError, ValueError) as exc:
            emit(
                {
                    "type": "error",
                    "kind": getattr(exc, "kind", "invalid_request"),
                    "message": terminal_text(exc),
                }
            )
    return 0
