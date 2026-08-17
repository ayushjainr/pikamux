---
name: Pika Live Operations
---

# Overview

Pika is a dense terminal control surface for developers delegating durable work
to Codex and Claude. The primary outcome is to see where human attention is
needed, understand why, and enter the exact conversation without inspecting
processes or transcripts. The trust posture is fail-closed around identity and
honest about missing usage data.

The reference world is a night-shift railway signal board crossed with btop:
compact, continuously current, operationally calm, and organized around
exceptions. Borrow the signal board's attention hierarchy and btop's keyboard-
first density, not their surface ornament.

Anti-references: no Pikachu interaction gimmicks, neon-everywhere hacker
aesthetics, fabricated precision, decorative charts, or animation that competes
with state changes.

Signature decisions:

- A commander's brief says where intervention is required, how many results are
  waiting, and what to open next; its underlying categories remain disjoint.
- On wide terminals, a grouped workstream rail keeps the delegation portfolio
  visible while a persistent inspector explains the selected exact identity,
  expert card, live pane tail, and available actions.
- One stable selection band binds every action to a visible provider, name, and
  immutable UUID fingerprint.
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
inspector presents provider UUID proof before card-derived expertise, a
read-only selected-pane tail, and UUID-bound actions. Narrow terminals fall
back to the sortable table and selected-workstream detail band, removing
secondary columns before truncating identity or state. Below the supported
minimum, show the required dimensions and retain a visible quit path. A
persistent Pika Playbook strip rotates every five minutes through copyable,
high-leverage operating habits selected from current state; it never displaces
current state or keyboard recovery guidance. Usage is a deliberate secondary
view, not default visual spectacle.

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
An unbound live process cannot be opened as if managed. Refresh errors retain
the last good screen and identify the failure. Empty, loading, narrow, overflow,
unread, working, ready, parked, error, unbound, remote, and cached-offline states
are first-class.

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
it adds no runtime dependency or persistent background daemon.
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

Pika may run as a Linux node or as the local Windows client, but it remains one
package and one command. The server owns conversation truth. The client owns
only the ability to create a local terminal window.

The optional client bridge is intentionally narrower than fleet federation:

- it listens only on client loopback;
- a reverse SSH forward supplies transport, so Pika opens no server port;
- pairing verifies the immutable server node UUID before writing a random
  per-node secret on both ends;
- requests contain source node UUID, target node UUID, provider, and exact
  conversation UUID, never a display name or command string;
- the client maps the target UUID to its own trusted SSH target and constructs
  `wt.exe` plus `_fleet-open` arguments locally;
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

- `src/pikamux/monitor.py` is the canonical implementation for layout, styling,
  responsive columns, interaction, and refresh cadence.
- `src/pikamux/ui.py` owns shared formatting, ordering, and sanitization rules.
- `src/pikamux/models.py` owns state names and attention semantics.
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
