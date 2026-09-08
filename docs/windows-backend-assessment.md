# Native Windows terminal backend assessment

Assessed 2026-09-07. Read-only source/documentation comparison; **neither backend
was installed or exercised on Windows**. No remote machines were contacted.
Maintainer-reported support below is not a Pika compatibility certification.

## Recommendation

Evaluate **psmux first in an isolated Windows prototype**, while retaining
macOS/Linux as the supported agent-hosting platforms. Do not advertise native
Windows hosting, silently substitute a `tmux.exe` alias, or change the current
experimental Windows client-bridge contract on this evidence alone.

Neither candidate removes Pika's own Windows porting work. A terminal multiplexer
can preserve a process without proving which conversation it belongs to. Pika
must retain UUID plus process-generation verification and safe ambiguity handling.

The alternative, **arndawg/tmux-windows**, preserves more upstream tmux source and
is useful as a compatibility reference. Its documented unresolved security
findings are a release gate, not grounds for claiming a tested exploit or making
a blanket judgment about every release.

## Evidence and scope

Sources inspected at these revisions:

- psmux: `dda59c65f6905ea35e56a6e9b069f33300c66d88`.
- tmux-windows: `9be15fb340f2e4a897b6bb17374f114e16fe501c`.

psmux describes a native Rust/ConPTY implementation, Windows shells, tmux command
aliases, and session persistence. These make it a plausible candidate, not an
upstream tmux binary. [Project README](https://github.com/psmux/psmux/blob/dda59c65f6905ea35e56a6e9b069f33300c66d88/README.md)

tmux-windows describes a ConPTY port of upstream tmux with named-pipe discovery,
authenticated TCP transport, and process-handle exit monitoring. Its maintainer
reports successful basic creation, listing, capture, split, attach, and SSH
disconnect persistence. Pika has not independently reproduced those results.
[Windows port notes](https://github.com/arndawg/tmux-windows/blob/9be15fb340f2e4a897b6bb17374f114e16fe501c/README_WIN32.MD)

## Pika contract versus candidate evidence

| Pika requirement | psmux evidence / gap | tmux-windows evidence / gap |
| --- | --- | --- |
| Inventory: `list-panes -a -F` with 16 fields, stable `%pane`, numeric PID, active/attached/dead state and four `@pika_*` tags | Command is documented; format source returns the child PID. Exact output framing and pane-local tag isolation need tests. | Upstream-style command source exists; basic `list-panes` is maintainer-tested, not the full Pika format. |
| Pane-local tagging: `set-option -p -t`, unset with `-u` | Inspected option writer and format lookup use an `AppState.user_options` map. Do **not** assume `-p` isolates tags between panes. | Upstream-style options are promising; verify isolation under the Windows build. |
| Recovery: dead state, exit code, activity, respawn | Format source maps `session_activity` to creation time and emits `0` for `pane_dead_status`. These are semantic mismatches, even though fields exist. | Exit monitoring is documented; exercise nonzero exit codes and respawn in a real build. |
| Exact-pane attach and receipts | Attach/switch/select are documented. The flag reference limits `list-clients -F` to server/control layers and omits Pika's `display-message -c/-l`; client-specific receipts need an adapter or proven support. | Attach is maintainer-tested; exact-pane targeting and client-specific receipt routing remain untested by Pika. |
| Peek/capture: `capture-pane -p -e -J -S <negative-line-count> -t %id`; popup preview | Capture flags and popup are documented. Verify ANSI, wrapped history, UTF-8, target isolation and no acknowledgement side effects. | Basic capture is maintainer-tested; expanded flags/history need the same Pika tests. |

Evidence: [psmux flag reference](https://github.com/psmux/psmux/blob/dda59c65f6905ea35e56a6e9b069f33300c66d88/docs/tmux_args_reference.md),
[format implementation](https://github.com/psmux/psmux/blob/dda59c65f6905ea35e56a6e9b069f33300c66d88/src/format.rs#L1474),
[user-option lookup](https://github.com/psmux/psmux/blob/dda59c65f6905ea35e56a6e9b069f33300c66d88/src/format.rs#L954),
[option storage](https://github.com/psmux/psmux/blob/dda59c65f6905ea35e56a6e9b069f33300c66d88/src/server/options.rs#L735),
[upstream-style list-panes command](https://github.com/arndawg/tmux-windows/blob/9be15fb340f2e4a897b6bb17374f114e16fe501c/cmd-list-panes.c).

psmux also documents two lifecycle differences: unqualified `kill-server` affects
all namespaces, and pane-shell exit normally cleans up descendant processes.
Explicit kill operations terminate the pane process tree. Any prototype must use
a dedicated namespace and scoped cleanup; Unix background-process assumptions do
not transfer automatically. The same documentation calls out UTF-8 decoding for
Windows subprocess clients. [Compatibility notes](https://github.com/psmux/psmux/blob/dda59c65f6905ea35e56a6e9b069f33300c66d88/docs/compatibility.md#behavioral-differences-from-tmux)

tmux-windows' TODO lists unresolved peer-identity and socketpair race findings.
The inspected compatibility source still implements `getpeereid` with zero
UID/GID. That confirms the stub exists; this assessment does not establish which
current authentication paths expose it. Require an upstream resolution or a
focused security review before distribution. [Maintainer TODO](https://github.com/arndawg/tmux-windows/blob/9be15fb340f2e4a897b6bb17374f114e16fe501c/TODO.md#security-open-findings-from-2026-02-22-audit),
[compatibility stub](https://github.com/arndawg/tmux-windows/blob/9be15fb340f2e4a897b6bb17374f114e16fe501c/win32/win32-compat.c#L345).

## Work Pika still owns

Source-observed in this repository:

- [tmux.py](../src/pikamux/tmux.py) launches a `sleep 30` holding pane, then a POSIX
  wrapper using `env -u`, shell quoting, `$?`, and `exec`. PowerShell/cmd require a
  native launch implementation, not string substitutions. Preserve environment
  removal, exit receipts, argument boundaries and the protected fallback shell.
- [processes.py](../src/pikamux/processes.py) has Linux `/proc` and macOS readers,
  not Windows process-generation/ancestry proof. A pane PID or tag alone must
  never become sufficient identity evidence.
- In particular, its `process_alive()` uses `os.kill(pid, 0)`. This must **not**
  be reused as a Windows liveness check: CPython routes Windows control-event
  values through `GenerateConsoleCtrlEvent`, and other values through
  `TerminateProcess`; zero is Windows' `CTRL_C_EVENT`, not the POSIX read-only
  probe. Replace it with non-signalling process-handle checks before enabling
  hosting. [Python implementation](https://github.com/python/cpython/blob/3.13/Modules/posixmodule.c#L9621),
  [Windows control-event API](https://learn.microsoft.com/en-us/windows/console/generateconsolectrlevent).
- [terminal_bridge.py](../src/pikamux/terminal_bridge.py) uses POSIX PTYs, termios
  and fcntl. Windows needs an appropriate console path, including resize, palette
  queries, terminal replies and Ctrl+C behavior.
- `tmux-direct`, RGB options, mouse/100,000-line history, user-key reply guards,
  and exact-client receipt delivery require behavioral testing. An accepted
  option name is not evidence that it takes effect.

## Validation gates before native Windows hosting

1. **Pinned, isolated backend:** record backend version/hash; use a random explicit
   namespace; leave existing tmux aliases and sessions alone. Test cleanup cannot
   affect a second namespace. Never use an unqualified `kill-server`.
2. **Inventory and identity:** round-trip all 16 fields with Unicode and spaced
   paths. Two panes must retain different provider/UUID/name/launch-token tags;
   clearing one cannot affect the other. Verify actual child PID, creation time,
   ancestry, access-denied behavior, stale hooks, PID reuse and duplicate UUID
   processes. Missing proof must fail closed.
3. **Launch/environment:** fake providers receive exact argv and cwd, with spaces,
   quotes and metacharacters preserved as data. Remove stale inherited identity
   and automation variables. A new launch binds once to its exact UUID; no
   duplicate launch when provider state arrives late.
4. **Recovery:** hold a fake agent through detach, terminal close and SSH
   disconnect; reattach the same PID generation/UUID. Exercise normal exit,
   nonzero exit, Ctrl+C, shell fallback and exact respawn. Do not certify survival
   across logout, reboot or backend-server death without separate evidence.
5. **Daily TUI:** test Windows Terminal/PowerShell with each supported provider:
   foreground/background contrast, scrolling, history, resize, arrow keys,
   bracketed paste, mouse, no leaked terminal replies, and exact-pane attach.
   Capture/peek must not redirect output or acknowledge unread work accidentally.
6. **Trust and installation:** same-user IPC isolation and release provenance,
   then clean-machine install/update/rollback-failure tests. No silent privilege
   escalation, agent settings edits, process termination, or backend replacement.

Start with fake providers and capability probes. Only after these gates pass
should real-provider testing spend quota or the installer offer a native Windows
backend. This is a bounded feasibility plan, not a commitment to build another
terminal multiplexer.
