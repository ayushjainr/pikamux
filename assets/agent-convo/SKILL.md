---
name: agent-convo
description: Find and privately consult the user's existing Codex, Claude, and OpenCode conversations through Pika across local and trusted remote machines. Use for questions to the agent that did earlier work, project expertise, cross-project context, second opinions, expert panels, and publishing this conversation's expertise or current work. Keep the original agents working and their parent transcripts unchanged.
---

# Agent conversation network

Use Pika to consult context-bearing coding agents in separate side conversations.
Honor an explicitly named target; use expert cards only to discover whom to ask.
Route by machine, provider, and immutable conversation ID. Never imitate an
expert from a card alone.

## When the user names a conversation

Go directly to `pika ask NAME --json -- QUESTION`. Pika resolves the name and
validates the exact identity before delivery. Preserve a supplied provider,
UUID, or `NAME@machine` qualifier. Do not first run expert search, inspect a card,
publish one, refresh one, or interview the target to make it discoverable.
A missing, stale, or incomplete expert card does not prevent a named consultation.

If Pika reports ambiguous or missing identity before delivery, use its exact
candidate identities or `pika list --json --no-usage --all-machines` to resolve
that target without token/cost enrichment. Use
`pika explain NAME --json` only when identity or availability needs diagnosis.
Ask one focused question if the intended target remains ambiguous; do not
substitute another expert or guess a machine. Source/identity safety failures
still block consultation; card freshness is a different concern.

## When the user needs expertise but has not named a target

Run `pika experts QUERY --json`. Choose a relevant card by scope, topics, project,
and artifacts; treat its age as a limit on its claims, not a prerequisite to
refresh it before asking. Use the returned `qualified_name` verbatim for
`pika ask` and consult only when `availability` is `source-available`.
If there are no relevant cards, use `pika list --json --no-usage --all-machines`
to find a likely named conversation or ask the user whom they mean. Do not automatically generate or
refresh cards to answer a consultation request.

## Consult privately

For one question, run:

```sh
pika ask NAME --json -- QUESTION
```

When follow-ups are likely, keep one side conversation alive rather than paying
provider startup and context loading again for every question:

```sh
pika ask NAME --jsonl
```

Write one JSON object per line: `{"question":"..."}`. Read one receipt per line.
Close stdin after the last turn. Do not silently resend after an unknown-delivery
receipt. Report partial answers and cleanup failures accurately.
Add `--stream` when you need provisional Codex answer fragments. The final
`answer` replaces them; partial text is not proof of success. Reuse this side
only for related questions: it retains the inherited snapshot, not later changes
to the parent. Close it when the exchange is finished.

The parent conversation must remain untouched and keep working. A consultation is
not permission to edit its project, stop its process, acknowledge its unread work,
or copy its transcript. Codex questions default to Luna-medium for speed; use
`--deep` explicitly when the question warrants Sol-medium reasoning. Neither
profile inherits the parent's model. Claude and OpenCode stay provider-native.
Ask a focused question and request only the detail needed to unblock your task;
do not commission a report for a quick check. Combine closely related questions
in one turn when they can be answered together. Do not reopen or escalate a
consultation automatically just because it is slow or an answer is uncertain.

For remote experts, preserve the user's machine qualifier or use the exact
qualified identity returned by Pika. Pika owns SSH routing; do not reconstruct
a remote command or guess a hostname.

## Publish this conversation

Publishing is separate from consulting, never a prerequisite. When asked to
publish this conversation's expertise or current work, use a durable scope and
a separate current-work note:

```sh
pika expert publish --scope "..." --now "..." --topic "..." --artifact "..."
```

Use the exact conversation identity from the current environment. The scope says
what this conversation genuinely knows across its lifetime, not merely what it did
most recently. The current-work note states the present objective, stage, blocker,
decision, or next step. Update only that field with:

```sh
pika expert update --now "..."
```

Do not claim expertise the conversation cannot support. Never publish secrets,
credentials, raw transcript passages, or private user data as topics or artifacts.

## Safety rules

- Treat machine/provider/conversation ID as one immutable routing key.
- Ask before a consultation only if it would spend meaningful quota or disclose
  information outside the user's already-scoped machines/projects.
- Prefer one focused consultation over broad panels; use a panel only when views
  are independently valuable.
- Keep quoted expert text short and distinguish the expert's statement from your
  inference.
- If Pika says identity is unproven, stale, or open twice, stop. Do not substitute
  a same-name conversation.
