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
- Freshness, partial provider discovery, and refresh failure remain visible;
  stale data is never silently presented as current or fully synchronized.

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

Pressing a replaces only the inspector with an inline side conversation; the
workstream rail and live operations header remain visible. On narrow terminals,
the same side conversation becomes a focused full-width panel rather than
forcing a compressed split.

# Interaction & States

`pika` opens the live monitor only on an interactive terminal. `pika list` and
redirected bare output remain finite and stable. Arrow keys or j/k move the
selection; Enter opens the exact selected identity; `a` focuses an ephemeral
multi-turn side consultation inside the inspector; n opens the oldest attention
item using the same ordering as `pika next`; p opens a sanitized pane peek; u
toggles the usage view; r reconciles; ? explains controls; q or Esc recovers or
exits. Inside the side panel, ordinary keys type, Enter sends, Ctrl+J inserts a
newline, Ctrl+U clears the draft, arrows scroll conversation history, and Esc
closes and discards the side before returning focus to operations.

Selection persists by `(provider, UUID)` across refresh and resort. An unbound
live process cannot be opened as if managed. Refresh errors retain the last good
screen and identify the failure. Empty, loading, narrow, overflow, unread,
working, ready, parked, error, and unbound states are first-class.

The side panel explicitly renders opening, ready, thinking, and error states.
One provider process owns the side for its whole lifetime, so follow-ups retain
side context. A visible block cursor and persistent control legend make input
focus unambiguous; q is ordinary text while the side has focus. The panel names
the parent UUID and repeats that the side is ephemeral and the parent transcript
is unchanged. Closing is always available through Esc, including during a slow
or failed provider turn.

The wide inspector describes a stale expert card as `+NEW CONTEXT`: the exact
machine state remains `STALE`, but the visible framing makes clear that the
conversation advanced rather than the card failing. Card claims remain
provider-authored evidence, not Pika's inference. The selected live-pane tail
is read-only and never acknowledges unread work; expanding it with p retains
the existing explicit acknowledgement contract.

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
- Do remove secondary columns before compressing names into ambiguity.
- Do label API-equivalent estimates with their pricing date and unavailable data
  with an em dash.
- Don't make the default monitor the only scriptable inventory surface.
- Don't infer transcript meaning or invent token/cost values for visual fullness.
- Don't impose a terminal background palette; ANSI roles inherit the user's
  terminal theme and `NO_COLOR` remains authoritative.
