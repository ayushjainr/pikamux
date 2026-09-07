from __future__ import annotations

import os
import subprocess
from pathlib import Path


def process_alive(pid: int | None) -> bool:
    if not pid or pid <= 0:
        return False
    try:
        os.kill(pid, 0)
        return True
    except (OSError, ProcessLookupError):
        return False


def cmdline(pid: int) -> list[str]:
    try:
        raw = Path(f"/proc/{pid}/cmdline").read_bytes()
    except OSError:
        return []
    return [part.decode(errors="replace") for part in raw.split(b"\0") if part]


def process_environment(pid: int) -> dict[str, str]:
    """Read one same-user process environment without invoking a shell."""
    try:
        raw = Path(f"/proc/{pid}/environ").read_bytes()
    except OSError:
        return {}
    result: dict[str, str] = {}
    for entry in raw.split(b"\0"):
        if b"=" not in entry:
            continue
        key, value = entry.split(b"=", 1)
        result[key.decode(errors="replace")] = value.decode(errors="replace")
    return result


def process_tty(pid: int) -> str | None:
    """Return the process's terminal device without inspecting its content."""
    for descriptor in (0, 1, 2):
        try:
            target = os.readlink(f"/proc/{pid}/fd/{descriptor}")
        except OSError:
            continue
        if target.startswith("/dev/pts/") or target.startswith("/dev/tty"):
            return target
    return None


def _process_kind(argv: list[str]) -> str | None:
    """Classify agent executables without matching unrelated path fragments."""
    names = {Path(value).name.lower() for value in argv[:4]}
    if names & {"claude", "claude-code"}:
        return "claude"
    if names & {"codex", "codex.js"}:
        return "codex"
    if names & {"opencode", "opencode.js"}:
        return "opencode"
    return None


def child_pids(pid: int) -> list[int]:
    try:
        raw = Path(f"/proc/{pid}/task/{pid}/children").read_text()
    except OSError:
        return []
    result: list[int] = []
    for value in raw.split():
        try:
            result.append(int(value))
        except ValueError:
            continue
    return result


def parent_pid(pid: int) -> int | None:
    try:
        raw = Path(f"/proc/{pid}/stat").read_text()
        # comm is parenthesized and may itself contain spaces. Fields after the
        # final ')' begin with stat field 3 (state).
        fields = raw[raw.rfind(")") + 2 :].split()
        value = int(fields[1])
    except (OSError, ValueError, IndexError):
        return None
    return value if value > 0 else None


def process_state(pid: int) -> str | None:
    """Return the one-letter Linux process state for race-safe tree checks."""
    try:
        raw = Path(f"/proc/{pid}/stat").read_text()
        # comm is parenthesized and may contain spaces. The first field after
        # the final ')' is stat field 3, the process state.
        fields = raw[raw.rfind(")") + 2 :].split()
        state = fields[0]
    except (OSError, IndexError):
        return None
    return state if len(state) == 1 else None


def process_start_time(pid: int | None) -> int | None:
    """Return Linux /proc start ticks, which disambiguate reused PIDs."""
    if not pid or pid <= 0:
        return None
    try:
        raw = Path(f"/proc/{pid}/stat").read_text()
        fields = raw[raw.rfind(")") + 2 :].split()
        # starttime is stat field 22; fields[0] is field 3.
        return int(fields[19])
    except (OSError, ValueError, IndexError):
        return None


def provider_ancestor(pid: int | None, provider: str) -> int | None:
    """Find the nearest provider process above a hook subprocess."""
    if not pid:
        return None
    seen: set[int] = set()
    current: int | None = pid
    while current and current not in seen:
        seen.add(current)
        if _process_kind(cmdline(current)) == provider:
            return current
        current = parent_pid(current)
    return None


def process_tree(root_pid: int | None) -> list[int]:
    if not process_alive(root_pid):
        return []
    assert root_pid is not None
    result: list[int] = []
    seen: set[int] = set()
    stack = [root_pid]
    while stack:
        pid = stack.pop()
        if pid in seen or not process_alive(pid):
            continue
        seen.add(pid)
        result.append(pid)
        stack.extend(child_pids(pid))
    return result


def provider_process(root_pid: int | None, provider: str | None = None) -> int | None:
    matches: list[int] = []
    for pid in process_tree(root_pid):
        argv = cmdline(pid)
        if not argv:
            continue
        kind = _process_kind(argv)
        if kind and (provider is None or provider == kind):
            matches.append(pid)
    return matches[-1] if matches else None


def shared_provider_process(pid: int | None, provider: str) -> bool:
    """Return whether ``pid`` is shared provider infrastructure.

    Codex hooks may execute beneath a shared app-server, and one OpenCode
    process may emit events for several roots. These processes are useful as
    short-lived ownership hints, but their continued existence cannot
    permanently prove that any one conversation is open.
    """
    if not pid or provider not in {"codex", "opencode"}:
        return False
    argv = cmdline(pid)
    if _process_kind(argv) != provider:
        return False
    # One OpenCode process can visit/create several roots. Its hook ancestry is
    # therefore a renewable routing hint, not permanent ownership of each root.
    if provider == "opencode":
        return True
    return "app-server" in argv


def process_stats(root_pid: int | None) -> tuple[float | None, int | None]:
    pids = process_tree(root_pid)
    if not pids:
        return None, None
    try:
        proc = subprocess.run(
            ["ps", "-o", "pid=,pcpu=,rss=", "-p", ",".join(str(pid) for pid in pids)],
            text=True,
            capture_output=True,
            timeout=2,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired):
        return None, None
    cpu = 0.0
    rss = 0
    found = False
    for line in proc.stdout.splitlines():
        parts = line.split()
        if len(parts) < 3:
            continue
        try:
            cpu += float(parts[1])
            rss += int(parts[2])
            found = True
        except ValueError:
            continue
    return (cpu, rss) if found else (None, None)


def find_processes_with_session_id(session_id: str, provider: str) -> list[int]:
    matches: list[int] = []
    proc_root = Path("/proc")
    for entry in proc_root.iterdir():
        if not entry.name.isdigit():
            continue
        pid = int(entry.name)
        argv = cmdline(pid)
        if not argv or session_id not in argv:
            continue
        if _process_kind(argv) == provider:
            matches.append(pid)
    return _canonical_identity_pids(matches)


def opencode_session_processes(
    proc_root: Path = Path("/proc"),
) -> dict[str, list[int]]:
    """Map every explicit OpenCode ``--session`` argument in one /proc pass."""
    matches: dict[str, list[int]] = {}
    try:
        entries = list(proc_root.iterdir())
    except OSError:
        return matches
    for entry in entries:
        if not entry.name.isdigit():
            continue
        try:
            raw = (entry / "cmdline").read_bytes()
        except OSError:
            continue
        argv = [
            part.decode(errors="replace")
            for part in raw.split(b"\0")
            if part
        ]
        if _process_kind(argv) != "opencode":
            continue
        session_ids: set[str] = set()
        for index, value in enumerate(argv):
            if value in {"--session", "-s"} and index + 1 < len(argv):
                session_ids.add(argv[index + 1])
            elif value.startswith("--session="):
                session_ids.add(value.partition("=")[2])
        for session_id in session_ids:
            if (
                session_id.startswith("ses_")
                and 8 <= len(session_id) <= 128
                and session_id[4:].isalnum()
            ):
                matches.setdefault(session_id, []).append(int(entry.name))
    return {
        session_id: _canonical_identity_pids(pids)
        for session_id, pids in matches.items()
    }


def _canonical_identity_pids(pids: list[int]) -> list[int]:
    """Collapse a provider launcher and its direct native child into one owner.

    The npm Codex launcher and the native Codex binary both retain the resumed
    thread UUID in argv. They are one client process tree, not two concurrent
    opens. Separate trees remain separate so genuine concurrency stays visible.
    """
    matches = set(pids)
    launcher_aliases = {
        parent
        for pid in matches
        if (parent := parent_pid(pid)) is not None and parent in matches
    }
    return sorted(matches - launcher_aliases)
