---
name: agent-convo
description: Find and privately consult the user's existing Codex, Claude, and OpenCode conversations through Pika across local and trusted remote machines. Use for questions to the agent that did earlier work, project expertise, cross-project context, second opinions, expert panels, and publishing this conversation's expertise or current work. Keep the original agents working and their parent transcripts unchanged.
---

# Agent conversation network

Use Pika as a directory of context-bearing coding agents. Search first; route by
machine, provider, and immutable conversation ID; consult in a separate side
conversation. Never imitate an expert from a card alone.

## Find the right expert

Run `pika experts QUERY --json`. Prefer a current, exact card whose durable scope,
topics, working directory, and artifacts fit the question. If several experts are
materially relevant, tell the user which ones you chose and why. Use
`pika explain NAME --json` when identity, status, or freshness is ambiguous.

## Consult privately

For one question, run:

```sh
pika ask NAME --json -- QUESTION
```

For dependent follow-ups, keep one side conversation alive:

```sh
pika ask NAME --jsonl
```

Write one JSON object per line: `{"question":"..."}`. Read one receipt per line.
Close stdin after the last turn. Do not silently resend after an unknown-delivery
receipt. Report partial answers and cleanup failures accurately.

The parent conversation must remain untouched and keep working. A consultation is
not permission to edit its project, stop its process, acknowledge its unread work,
or copy its transcript. Use `--fast` only when latency matters more than retaining
the target thread's model profile.

For remote experts, use the qualified identity returned by search. Pika owns SSH
routing; do not reconstruct a remote command or guess a hostname.

## Publish this conversation

Publish a durable scope and a separate current-work note:

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
