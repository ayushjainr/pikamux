# Pika architecture and interaction design

Pika connects existing Codex, Claude Code, and OpenCode conversations. The board
shows where attention is needed and opens the exact selected conversation;
expert cards let agents discover and consult relevant project experience.
Provider harnesses remain responsible for their conversations and history.

Board membership is an explicit Pika choice, separate from provider naming.
An open, creation or adoption keeps watching that exact provider identity;
provider-generated names, hook events and inherited pane tags alone do not.
Explicit selection and exact attachment receipts persist independently of later
renames and lifecycle events. Existing managed records and historical exact
launch/attach evidence remain watched. Other legacy external observations remain
stored but are excluded from the board, attention queue and alerts. Setup offers
them for selection (named first, recent unnamed second); exact-name/UUID opening
also restores them. This is not archival, deletion or an explicit-unwatch tombstone.
Provider names are labels, not proof that a human renamed a conversation. Hooks
must not grant tracking to a new fork merely by inheriting its parent's pane.

## Design principles

Keep identity, attention, and freshness visible. Use keyboard-first navigation
and stable selection so background refreshes do not change the target of an
action. Do not present inferred usage as precise measurement or hide an identity
failure behind a successful-looking attachment.

- The board summary says where intervention is required, how many results are
  waiting, and what to open next; its underlying categories remain disjoint.
- On wide terminals, a grouped workstream rail keeps the delegation portfolio
  visible while a persistent inspector explains the selected exact identity,
  expert card, live pane tail, and available actions.
- One stable selection band binds every action to an exact provider and UUID.
  Names and machine labels are visible; otherwise colliding names also show a
  short UUID fingerprint. Full identity evidence is available through `d`.
- Exact/protected language requires current UUID-to-PID pane evidence. Tags,
  attachment, and liveness alone never earn it.
- Provider launcher/native-child aliases count as one logical client. Shared
  app-server hook leases are advisory when direct UUID-bearing client evidence
  exists; distinct UUID-bearing process trees remain `OPEN TWICE`.
- Freshness, partial provider discovery, and refresh failure remain visible;
  stale data is never silently presented as current or fully synchronized.
- Federation is location, not migration. A user-selected coordinator may show
  another Pika node, but the remote node remains authoritative and transcript
  content never enters the coordinator cache.

# Layout

The operational topology is status → anomaly → explanation → action. On wide
terminals, the live monitor uses a compact status header, a left rail grouped by
attention state, and a right inspector for the selected workstream. The
inspector leads with recognizable name, provider/model and a compact project
location, then a state-specific briefing. Questions/reasons lead when attention
is required; ready output is identified without inventing a result summary;
working detail uses recorded observations. Parked expertise stays readable.
Provider-authored work updates and durable scope retain their card age; ordinary
selection never generates a summary or interviews an agent. `d` opens full
identity, ownership metadata and the unabridged card. Missing pane bookkeeping
is not a permanent warning: actual blocked opening gets exact recovery steps.
The panel keeps UUID-bound actions and an explicit read-only preview. Narrow terminals fall
back to the sortable table and selected-workstream detail band, removing
secondary columns before truncating identity or state. Below the supported
minimum, show the required dimensions and retain a visible quit path. A
persistent Pika Playbook strip rotates every five minutes through copyable,
high-leverage operating habits selected from current state; it never displaces
current state or keyboard recovery guidance. Usage is a deliberate secondary
view, not default visual spectacle.

Subscription quota is a compact footer immediately above keyboard shortcuts,
separate from per-thread token accounting. Codex and Claude show weekly remaining
bars, reset dates in the viewing machine's local time, and individual observation
ages. Wide boards share one row; medium boards stack providers; short boards
retain percentages and stale labels without sacrificing the selected thread.
The strip stays on the machine running the board, labeled `this machine`, even
when a remote thread is selected. It never sums allowances or assumes the same
account is signed in across machines. Clients without a local quota source show
that limitation instead of substituting a remote account.

A separate cancellable worker reads base-machine quota at most every two minutes
while the board is open. Moving between threads never triggers quota SSH calls.
Codex uses only its account rate-limit RPC;
Claude prefers its documented status-line rate-limit feed, captured by a silent
native adapter installed through setup. Existing status-line commands and their
options are retained; the adapter forwards the original JSON and display output.
Only the weekly percentage, reset time, observation time, and source are saved
in a private bounded cache; the older utilization cache remains a fallback.
No model turn, credential export, or transcript read is needed. Missing
readings remain unavailable. Readings older than five minutes are visibly stale;
crossing a reset never manufactures a refill. The optional `quota-v1` exact-node-bound
read-only endpoint remains available to protocol clients, but the board does not
use remote quota. Older fleet hosts therefore need no upgrade for the base-machine
strip. Quota never writes to conversation state or blocks rendering/input.

Stopping observation is distinct from detaching a tmux client. `x` opens a
UUID-bound confirmation in the inspector: Pika removes the workstream from Live
Operations while leaving the agent process, provider conversation, and expert
card intact. The action is reversible through explicit opening or adoption,
and a durable tombstone prevents lifecycle hooks from silently adding the
workstream back before then.

Pressing a replaces only the inspector with an inline side conversation; the
workstream rail and live operations header remain visible. On narrow terminals,
the same side conversation becomes a focused full-width panel rather than
forcing a compressed split.

# Interaction & States

`pika` opens the live monitor only on an interactive terminal. `pika list` and
redirected bare output remain finite and stable. Arrow keys or j/k move the
selection; Enter opens the exact selected identity; `a` focuses the default
Sol-medium ephemeral multi-turn side consultation inside the inspector; `A`
uses the faster Luna-medium Codex profile; n opens the oldest attention item
using the same ordering as `pika next`; p opens a sanitized pane peek; u
toggles the usage view; r reconciles; ? explains controls; q or Esc recovers or
exits. `x` asks to stop watching the selected UUID; x or Enter confirms and Esc
or q cancels. Inside the side panel, ordinary keys type, Enter sends, Ctrl+J
inserts a newline, Ctrl+U clears the draft, arrows scroll conversation history,
and Esc closes and discards the side before returning focus to operations.

Local selection persists by `(provider, UUID)`; federated selection persists by
`(node UUID, provider, UUID)` across refresh and resort. The human spelling is
`thread@machine`, while every action is freshly routed by immutable identities.

Pika-owned native-agent homes show a single bottom navigation row: `← Pika`
and the available return key, normally F12. Clicking the label or pressing the
displayed key detaches a fresh terminal attachment; an existing tmux client
switches back to its previous session. Neither action sends an exit command to
the agent. The normal board loop retains selection, filter and viewport, while
freshly checking identity on the next opening. Separate Windows agent windows
close their attachment and leave the original board running; native window
focus remains the terminal/OS's responsibility.

Only unallocated root bindings are used (F12, then F11/F10; unused left-status
mouse bindings). An existing catch-all or conflicting custom binding is never
replaced. If no control is available, the strip reports that limitation rather
than advertising a dead shortcut. Outside opted-in Pika homes, the keyboard
input is passed through. The strip is session-local and default-background;
it does not alter provider configuration, window styles or the agent status line.

Board entry and teardown disable inherited mouse reporting, independently of
raw typing mode. Interactive PTY and Pika-owned SSH handoffs likewise reset
mouse reporting on entry and return, including ordinary error returns. Cleanup
does not flush queued keyboard input or impose a palette. Uncatchable process
termination and SSH sessions launched outside Pika are not cleanup guarantees.

An unbound live process cannot be opened as if managed. Refresh errors retain
the last good screen and identify the failure. Empty, loading, narrow, overflow,
unread, working, ready, parked, error, unbound, remote, and cached-offline states
are first-class.

Public state is a projection, not a stored assertion. Pika persists provider
lifecycle, identity-safety, and runtime observations as separate latest facts.
Reconciliation derives the display label with fail-closed precedence: active
identity safety, unresolved runtime failure, provider lifecycle, then current
ownership/liveness. The sessions row is only a compatibility cache. This keeps
a stale process launch from masquerading as `WORKING`, prevents a newer provider
event from erasing `OPEN TWICE`, and lets an exact recovered process shed an old
runtime error without manual cleanup.

The side panel explicitly renders opening, ready, thinking, and error states.
One provider process owns the side for its whole lifetime, so follow-ups retain
side context. A visible block cursor and persistent control legend make input
focus unambiguous; q is ordinary text while the side has focus. The panel names
the parent UUID and repeats that the side is ephemeral and the parent transcript
is unchanged. Closing is always available through Esc, including during a slow
or failed provider turn.

The consultation policy is explicit and immutable for the side's lifetime.
Codex defaults to `gpt-5.6-sol` at medium effort; the narrow fast path uses
`gpt-5.6-luna` at medium effort. Both the ephemeral fork and every turn pin that
selection, and visible receipts report it. Claude stays provider-native and
rejects the fast path until separately benchmarked. Pika's fast path is a model
profile, not Codex Fast mode or a service-tier setting.

Codex automation provenance is classified before lifecycle or tmux identity.
A UUID-matching `session_meta.originator` on the configured worker-origin list
keeps that short-lived execution unit outside setup, ownership leases, pane
tags, and attention; missing, malformed, or mismatched metadata fails open and
preserves the conversation. The parent run remains the user-facing workstream.
This is a provenance decision, never a title or UUID-prefix heuristic.

Provider question tools cross the attention boundary before their UI blocks:
Codex `request_user_input` and Claude `AskUserQuestion` set `NEEDS YOU` from
structured pre-tool lifecycle events without reading the question. Their
matching post-tool event clears the wait and returns the workstream to
`WORKING` after an answer.

The wide inspector describes a stale expert card as `+NEW CONTEXT`: the exact
machine state remains `STALE`, but the visible framing makes clear that the
conversation advanced rather than the card failing. Card claims remain
provider-authored evidence, not Pika's inference. Every current card has two
visible horizons: `scope` is the durable thread mandate synthesized across the
full inherited history, and `now` is the current objective, stage, blocker,
decision, or next step. A recent-work receipt cannot satisfy the card contract;
legacy cards without `now` are stale until refreshed. The selected live-pane
tail is read-only and never acknowledges unread work; expanding it with p
retains the existing explicit acknowledgement contract.

The first successful scan claims a committed event-ledger watermark and monitor
visit in one transaction. First use briefly frames the current handoff; a visit
after a six-hour gap briefly counts finished, decision, and error events committed
since the prior watermark. Rendering never acknowledges those events. Opening
an unread READY result earns `RESULT COLLECTED` only when exact identity proof
and an event-specific atomic acknowledgement both succeed.

# Accessibility

Every action is keyboard-operable. State and unread meaning never depend on
color alone. ANSI styling respects `NO_COLOR`; selection uses a full-row band,
and control instructions remain persistent. Provider-controlled strings are
sanitized before rendering. Mouse-wheel navigation is supplementary.

# Performance

Operational reconciliation runs off the input/render loop and never overlaps
itself. Live state refreshes every two seconds without routine animation. Initial,
manual, or demonstrably slow refreshes animate; fast background refreshes remain
calm. Slower usage accounting starts independently every thirty seconds only
while visible and carries forward its last known values. Monitor workers are
daemon-scoped so optional accounting or reconciliation can never hold open a quit
or delay an attach. The monitor uses the alternate screen and redraws text only;
it adds no runtime dependency or persistent background daemon. Each frame is built
in memory before any terminal output, bracketed by synchronized-update markers,
and written only when its bytes or terminal dimensions change. This prevents
line-buffered stdout from exposing the cleared screen and partially drawn rows.
Terminals without synchronized-output support still receive a prebuilt frame.
Resize forces repaint; write failures and terminal teardown end synchronization.
Playbook rotation is derived from wall-clock five-minute buckets, so it requires
no additional timer, task, or persistent state.
Expert cards are loaded locally on a separate five-minute cadence; this display
refresh never interviews a provider. A read-only tail captures only the selected
tmux pane every two seconds in a daemon-scoped worker, does not read provider
transcripts, and never changes unread state.
Inline consultation construction and turns run on a dedicated daemon worker so
provider latency cannot freeze dashboard refresh, typing, or Esc recovery. Esc
requests cancellation immediately and terminates the transient provider process;
the monitor itself remains open.

Server federation adds no daemon and no listener. The optional laptop
new-window bridge is a separate client-side process bound only to loopback; it
is reached from a paired server solely through the user's reverse SSH forward.
On Windows, interactive bare `pika` renders the same monitor locally. First run
offers multi-selection from passive SSH-config and Tailscale discovery; only
selected machines receive pairing requests. Existing client pairings are all
selected automatically, regardless of the legacy default-board field. A dedicated
client-fleet cache beside the pairing file stores validated remote snapshots, not
local provider history. It never imports a paired server's fleet topology.
The cached board paints before any SSH request. One fair, bounded background
observer refreshes selected machines and preserves cached-offline visibility.
Enter dispatches a single-flight action worker: fresh fleet identity validation,
then locally constructed Windows Terminal arguments for the exact destination.
No reverse tunnel, bridge startup, or mandatory remote coordinator is involved.
Peeks and action failures stay in a scrollable inspector;
untrack is explicitly confirmed. A window receipt confirms process launch only,
not eventual attachment. Unknown outcomes are never automatically retried.
Windows pipe cancellation targets only the exact owned pipe's outstanding I/O;
it cannot cancel another conversation's or process's I/O.
Boards on all platforms check public release metadata in a separate, cancellable
background worker. Successful checks are cached for six hours; failures are quiet
and retried after an hour. A short cross-process lease prevents concurrent boards
from duplicating requests. A separate update-cache database never writes agent
state or invalidates reconciliation. Only newer, complete releases compatible
with the running channel and platform produce a notice. `PIKA_UPDATE_CHECK=0`
disables these checks. A new release offers `Update now? [y/N]` once per version
per board visit, deferred while a consultation, filter, or action has focus.
Only an explicit Y approves; Enter, N, and Esc decline. U reopens the offer.
Approval restores the terminal and stops board observers before updating only
this installation, then reopens the verified new board. Windows executes its
embedded, reviewed installer with the approved version pinned, not a script from
a moving branch. Retained Windows paths forward to the newer active executable
only after its bounded receipt and executable digest validate; this avoids stale
PowerShell PATH entries reopening the old client. Staged installer probes never
forward. No agent, pairing, or remote installation is
changed. Closing the board cancels and reaps its update check.
The legacy bridge remains optional for openings initiated on a remote console;
its short connection budget is separate from its bounded launch-receipt budget,
and a disconnected caller cannot terminate the listener.
Passive discovery reads existing SSH
configuration and Tailscale's local status document without probing candidates.
The monitor's local two-second reconciliation never performs SSH. It keeps at
most one bounded remote inventory request in flight and replaces a node's cache
only after receiving and validating a complete versioned snapshot. Failed,
truncated, incompatible, or identity-mismatched responses preserve the last-good
snapshot and make its age/error visible. Remote pane previews are never polled;
`p` launches one explicit background fetch. Remote side questions use one SSH
JSONL process for the consultation lifetime, and remote mutations use exact node,
provider, and conversation UUIDs with no name fallback.

Remote inventory is a fair single-flight queue, ordered by oldest attempted
node. Ready nodes become due after 15 seconds and failed nodes after 30 seconds;
completion immediately admits the next due node. The 45-second snapshot limit is
a truth threshold, not an availability promise: if aggregate SSH tail latency
exceeds that budget, affected rows become visibly `CACHED` and non-actionable
instead of increasing concurrency or pretending freshness. Selection can jump
the queue only for an explicit manual refresh. Mutation receipts are durable
operation outcomes; the follow-up snapshot is a separate best-effort cache
reconciliation. A result fetched by remote peek is rendered before any separate
acknowledgement begins, so acknowledgement uncertainty cannot hide the result.

Every fleet envelope is versioned and bounded while streaming. Sessions declare
whether their identity is an exact provider conversation or an unbound pane
placeholder; placeholder rows may be inventoried but remain blocked from exact
UUID actions. Expert profiles, card states, mutation receipts, booleans, numeric
fields, node IDs, and request IDs are validated before a last-good cache changes.
Human expert lookup labels stale remote evidence `CACHED` and ranks fresh evidence
first. Handshakes expose the package version, and remote installation or upgrade
uses the coordinator release's version tag rather than a moving branch.

Commissioning and expert interviews are separate latency boundaries. `pika
setup` may discover and adopt conversations, but never opens provider-side
consultations. Missing and stale cards are left to the quota-aware refresh policy
unless the user explicitly runs an expert refresh command. `pika adopt` may
interview only the single exact UUID it just adopted.

Commissioning finishes with a transcript-free operational reconciliation so
provider-native renames update tracked rows and pane recovery tags. This name
sync never opens an expert consultation.

# Client-window launch boundary

Pika may run as a Linux/macOS node or as the local Windows client, but it remains one
package and one command. The server owns conversation truth. The client owns
only the ability to create a local terminal window.

The optional client bridge is intentionally narrower than fleet federation:

- it listens only on client loopback;
- a reverse SSH forward supplies transport, so Pika opens no server port;
- pairing verifies the immutable server node UUID before writing a random
  per-node secret on both ends;
- requests contain source node UUID, target node UUID, provider, and exact
  conversation UUID, never a display name or command string;
- the client maps a paired target UUID to its own trusted SSH target, or uses
  its locally selected coordinator's fleet relay, and constructs `wt.exe` and
  fixed internal endpoint arguments locally;
- spawned attaches clear inherited SSH forwards, leaving the dashboard
  connection as the sole owner of its reverse bridge port;
- `_fleet-open` revalidates the target node UUID and tracked conversation;
- request IDs are deduplicated briefly so a retried receipt cannot create a
  burst of windows;
- the long-running client reloads each atomically replaced pairing file before
  handling a request, so pairing or rotating a node does not require a restart;
- a confirmed receipt means the local window process was launched, not that the
  provider completed its later remote attach. The new window prints the normal
  exact recovery receipt;
- no live reverse tunnel is ordinary absence and falls back to attaching in the
  current terminal; a reachable identity rejection fails closed.

This client listener is an explicit exception to Pika's no-daemon server model.
It cannot read transcripts, mutate the server registry, choose a conversation
by name, or execute caller-supplied shell text.

# Sources

- `src/monitor.rs` is the canonical implementation for layout, styling,
  responsive columns, interaction, and refresh cadence.
- `src/attention.rs` and `src/status.rs` own attention ordering and state projection.
- `src/model.rs` owns shared state and identity types.
- This document owns durable rationale, exclusions, and interaction intent; the
  executable files own exact values.

# Do's and Don'ts

- Do prioritize intervention and exact identity over resource spectacle.
- Do use the right inspector to explain one selected workstream rather than
  widening the left rail with metadata columns.
- Do preserve the last trustworthy snapshot when reconciliation fails.
- Do call the coordinating role generic; a deployment hostname is never a
  product assumption.
- Do keep remote cache rows out of the local session ledger, tmux tags, hook
  leases, resume locks, usage readers, and recovery certificate.
- Do remove secondary columns before compressing names into ambiguity.
- Do label API-equivalent estimates with their pricing date and unavailable data
  with an em dash.
- Don't make the default monitor the only scriptable inventory surface.
- Don't infer transcript meaning or invent token/cost values for visual fullness.
- Don't impose a terminal background palette; ANSI roles inherit the user's
  terminal theme and `NO_COLOR` remains authoritative.
- Don't treat Tailscale visibility as SSH authorization or silently install,
  trust, acknowledge, or retry work across machines.
