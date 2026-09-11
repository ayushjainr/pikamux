# Your first consultation

Give your current agent access to relevant experience from another conversation.
This guide covers setup, preparing one expert card, and checking the resulting
consultation.

Start with one local project, an existing conversation that did relevant work,
and a small next task. No second server is required. Use a project you are allowed
to share with the selected model provider; avoid sensitive work for a first trial.

## Release compatibility

Use Pika 0.5.0a4 or newer for Codex conversations with paginated history. If you
installed an earlier release, run `pika update` before consulting those conversations.
Claude consultation requires CLI 2.1.228 or newer; OpenCode has temporary
persisted-side cleanup limits. Read [consultation boundaries](../SECURITY.md#consultation-boundaries).

## 1. Install the agent workflow

Use the [Mac/Linux installer](installing.md#fresh-mac-or-linux-machine), with tmux
and an authenticated supported provider CLI available. Run `pika setup` if you
skipped onboarding. Review the changes and the bundled `agent-convo` skill in the
same approval step. Codex users must review and trust hooks through `/hooks`.
If your agent cannot see the newly installed skill, reload its skill list or
start a fresh caller conversation; do not restart the expert solely to consult it.

Choose one existing conversation during setup. This registers it with Pika;
**setup makes no interview calls and does not fill missing cards**. You do not
need to attach to or interrupt the expert for a side consultation.

## 2. Make one conversation discoverable

Inspect metadata first:

```bash
pika expert status --json
```

If the selected conversation already has a useful profile, skip the interview.
Check durable scope, topics, artifacts, source availability, and publication age;
a recently updated current-work field does not make every historical claim current.

For a missing card, explicitly interview **that one exact conversation**, using
the `session_id` from the status output, not the literal placeholder below:

```text
pika expert refresh <native-id>
```

This spends provider quota once for the card interview, separately from the
later consultation. Do not start with `--all`. If an answer or delivery status is
unknown, inspect the reported outcome before retrying; do not repeatedly send it.

An alternative for an agent already working inside its verified Pika pane is
to publish its own concise profile using the installed skill:

```text
pika expert publish --scope "Durable project responsibility" --now "Current objective" --topic "specific topic" --artifact "path/to/relevant-artifact"
```

Replace these descriptions with firsthand facts. Publication is restricted to
the calling conversation's exact Pika pane; it cannot edit another expert's card.
Do not move a live external agent into a new home merely to publish a card—use
the bounded interview instead. Exclude secrets and transcript excerpts.

The scheduled refresher is quota-gated, not an immediate onboarding service.
Missing or stale quota telemetry can mean no scheduled interview happens at all.

## 3. Give your current agent the task—not an expert's name

In the caller conversation, use a prompt like this with your real change:

> Before implementing this change, use the agent-convo skill to search Pika's
> expert cards for relevant prior work. Select one firsthand expert, ask which
> earlier decision constrains this change, and request a concrete artifact or
> test I can verify. Use one question initially. Do not modify or resume the
> original conversation. Check the evidence, close the side, and tell me what
> this changes in your plan. If no relevant expert is discoverable, say so.

The skill starts with metadata, for example:

```bash
pika experts "order endpoint" --json
```

Use a short project topic from the real task. Search is lexical, not an LLM
judgment: `matched_on` explains matching fields, and `score` measures relevance
under those rules, not intelligence or answer confidence. If a narrow search
misses, try a shorter relevant topic; do not scan transcripts for discovery.

The agent should select by provider and immutable `session_id`, checking scope
and artifacts rather than taking the first name match. For a remote result it
also retains the machine route and pinned node identity. Unwatched experts can
remain discoverable; archived ones are excluded. **Never unarchive or re-adopt
a conversation just to ask it a question.**

## 4. Check the exchange, not just the answer

The agent holds one JSONL process open. This is a protocol illustration, not a
shell block to paste as one command:

```text
pika ask <native-id> --jsonl
{"question":"Which earlier decision constrains this change? Cite the artifact and distinguish firsthand knowledge from inference."}
{"close":true}
```

It waits for the opened receipt and answer, then sends `close`. If needed, it
can ask a targeted follow-up before closing the same process. Remote experts use
`<native-id>@<machine>` throughout; there is no fallback to a similarly named
local conversation. The skill uses Pika's default consultation profile.

Judge the trial on three things:

- **Useful discovery:** the agent explains why this card fits the task without
  you supplying the expert's name.
- **Substantive evidence:** a cited artifact, test, or recorded decision supports
  the expert's answer, and the caller says what it changed or confirmed in the
  plan. If the artifact is unavailable, the claim stays unverified/last known.
- **Honest lifecycle:** the opened receipt identifies the intended parent and
  a terminal `closed` receipt with `discarded: true` confirms side cleanup.
  An answer or EOF alone is not cleanup proof. Ordinary receipts do not hash the
  parent transcript; do not report measured byte-for-byte preservation without
  a separate authorized audit.

A peer answer is evidence, not authorization to change files or systems. The
caller must still follow your task's permissions. Retain a useful answer if
cleanup fails, but report that failure separately; never blindly resend an
uncertain question.

## What stays private—and what costs quota

Discovery and ordinary board/status views make no model calls. Explicit card
interviews and consultations consume provider quota; one card and one question
are enough for this trial. Existing scheduled refresh remains subject to its
separate quota gate.

“Private” means a separate side, not offline processing: the selected provider
processes the inherited context and question. Pika stores card metadata locally,
which can include sensitive names, paths, and summaries. Trusted remote machines
can share card metadata; their parent histories remain authoritative there.
Do not put consultation contents, raw receipts, private paths, or credentials
in a public issue. If sharing feedback, describe the useful decision or failure
with a synthetic or sanitized reproduction.
