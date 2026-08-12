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

- An attention rail always exposes NEEDS YOU, unread, working, parked, error,
  and unbound counts in text as well as color.
- One stable selection band binds every action to a visible provider, name, and
  immutable UUID fingerprint.
- Freshness and refresh failure remain visible; stale data is never silently
  presented as current.

# Layout

The operational topology is status → anomaly → explanation → action. The live
monitor uses a header/freshness line, attention rail, sortable workstream table,
selected-workstream detail band, and persistent keyboard legend. Narrow
terminals remove secondary columns before truncating identity or state. Below
the supported minimum, show the required dimensions and retain a visible quit
path. A persistent Pika Playbook strip rotates every five minutes through
copyable, high-leverage operating habits; it never displaces current state or
keyboard recovery guidance.

# Interaction & States

`pika` opens the live monitor only on an interactive terminal. `pika list` and
redirected bare output remain finite and stable. Arrow keys or j/k move the
selection; Enter opens the exact selected identity; n opens the oldest attention
item using the same ordering as `pika next`; p opens a sanitized pane peek; r
reconciles; ? explains controls; q or Esc recovers or exits.

Selection persists by `(provider, UUID)` across refresh and resort. An unbound
live process cannot be opened as if managed. Refresh errors retain the last good
screen and identify the failure. Empty, loading, narrow, overflow, unread,
working, ready, parked, error, and unbound states are first-class.

# Accessibility

Every action is keyboard-operable. State and unread meaning never depend on
color alone. ANSI styling respects `NO_COLOR`; selection uses a full-row band,
and control instructions remain persistent. Provider-controlled strings are
sanitized before rendering. Mouse-wheel navigation is supplementary.

# Performance

Operational reconciliation runs off the input/render loop and never overlaps
itself. Live state refreshes every two seconds; slower usage accounting refreshes
every thirty seconds and carries forward its last known values. The monitor uses
the alternate screen and redraws text only; it adds no runtime dependency or
background daemon. Playbook rotation is derived from wall-clock five-minute
buckets, so it requires no additional timer, task, or state.

# Sources

- `src/pikamux/monitor.py` is the canonical implementation for layout, styling,
  responsive columns, interaction, and refresh cadence.
- `src/pikamux/ui.py` owns shared formatting, ordering, and sanitization rules.
- `src/pikamux/models.py` owns state names and attention semantics.
- This document owns durable rationale, exclusions, and interaction intent; the
  executable files own exact values.

# Do's and Don'ts

- Do prioritize intervention and exact identity over resource spectacle.
- Do preserve the last trustworthy snapshot when reconciliation fails.
- Do remove secondary columns before compressing names into ambiguity.
- Don't make the default monitor the only scriptable inventory surface.
- Don't infer transcript meaning or invent token/cost values for visual fullness.
