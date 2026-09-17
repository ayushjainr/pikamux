---
name: agent-convo
description: Discover and privately consult existing Codex, Claude, and OpenCode conversations through Pika across trusted machines. Use for prior project expertise, cross-project questions, or maintaining a participating conversation's expert card at work milestones. Not a prerequisite for ordinary coding tasks.
---

# Consult existing expertise

Pika finds the conversation that did earlier work and asks it privately. A card
helps choose whom to ask; it is not the expert's answer.

## Choose the shortest route

- **Named conversation:** go directly to `pika ask NAME --json -- "QUESTION"`.
  Preserve supplied UUIDs and `NAME@machine` qualifiers. Do not search, inspect,
  publish, or refresh cards first. Missing or stale cards do not block asking.
- **Unknown expert:** run `pika experts "QUERY" --json` once with specific task
  terms. Choose a relevant `source-available` match and use its `qualified_name`
  verbatim. Reuse a suitable match already returned during this task; Pika
  revalidates identity when opening. Do not interview multiple candidates merely
  to choose one or automatically commission missing cards.
- **Unresolved target:** use returned candidates or, if needed,
  `pika list --json --no-usage --all-machines`. Use `pika explain NAME --json`
  only to diagnose identity/availability. Ask one focused question if still
  ambiguous; do not guess a machine or substitute a same-name conversation.

Pika owns SSH routing. Never reconstruct remote commands or copy transcripts.
Stop on unproven identity, stale routing evidence, or duplicate ownership; that
is different from an old card. No repeated searches or inventory polling while
a consultation is pending.

## Ask only what unblocks the task

Consult when relevant context is missing, not by default for every task. Prefer
one focused question; combine closely related questions and request a concise
answer. Use panels only when independently different views are needed. Ask before
expanding beyond the user's scoped projects, machines, or approved spending.

Codex questions default to Luna-medium; `--deep` explicitly selects Sol-medium.
Neither inherits the parent's model. Claude and OpenCode stay provider-native.
Do not automatically escalate, retry, or reopen because a reply is slow or uncertain.

For related follow-ups, keep one bounded side open:

```sh
pika ask NAME --jsonl
```

Send `{"question":"..."}` per line; read JSONL receipts until that turn's final
answer or error. Add `--stream` only if consuming provisional Codex answer
fragments helps; otherwise avoid their extra output. Partial text is not success;
the final `answer` replaces it. Never resend after an unknown-delivery receipt.
Report incomplete answers and cleanup failures accurately.

Reuse the side only for related questions: it retains its inherited snapshot,
not later parent changes. If a new request needs later parent context, close the
old side and open a fresh one for the same exact target; this is not a retry of
an uncertain turn. Close stdin after the exchange. Do not keep idle sides
alive to warm caches. The parent must keep working unchanged: no project edits,
process stops, unread acknowledgement, or parent messages. Distinguish the
expert's claims from your own verification; quote only what the task needs.

## Maintain your own card without a separate interview

Only for this conversation already participating in Pika, use an existing work
turn to publish at a meaningful milestone—not every turn or consultation. If a
card is missing or durable expertise materially changed:

```sh
pika expert publish --scope "..." --now "..." --topic "..." --artifact "..."
```

`scope` describes firsthand expertise across the conversation; `now` describes
the current objective, stage, blocker, or next step. Publication replaces the
card: retain still-valid scope, topics, and artifacts, not just recent achievements.
If only current work changed, use:

```sh
pika expert update --now "..."
```

Skip unchanged updates. Use context already available; do not reread the entire
history, start a model turn, or create a side solely for card maintenance.
Publishing is a local write with no model call, though composing it uses tokens.
Exact caller proof is required. If unavailable, skip publication and continue the
task; do not move agents, bypass checks, or enroll unrelated conversations.
Never publish secrets, credentials, private user data, or raw transcript excerpts.

Scheduled interviews fill missing cards only. Existing expertise remains useful
when current work is stale; re-interview it only on the user's request, not to
make the freshness label look better.
