---
name: pika-memory
description: Recall or preserve Pika's durable facts, decisions, corrections, commitments, and scoped working guidance.
---

Use the Pika MCP backend to recall relevant memory and obtain current guidance before substantive work. SQLite is authoritative; readable Markdown is a projection, not a competing memory store. Keep recall bounded, include linked older rationale and contrary evidence, and distinguish historical decisions from current observations. A miss does not prove absence.

Save useful source-backed learning through the backend. A clear lasting correction applies to the next relevant interaction, including after restart; a one-off request or hypothetical does not become standing guidance. Preserve origin, scope, uncertainty, rejected alternatives and unresolved promises. Explicit instructions outrank inferred preferences. Confirm only successful saves; do not edit SQLite directly or maintain a parallel MEMORY.md/SOUL.md truth.

The native tools expose `pika_state`, `pika_memory_search`, `pika_profile`, and `pika_save_learning`. Before substantive work, read `pika_profile` with `view: "soul"` for current eligible guidance; historical working-set records are not proof that old guidance remains active. Use current source IDs/revisions returned by state/search when saving candidates; never manufacture Human provenance. `pika_save_learning` takes a request ID, body, and typed candidates with exact source links. Follow its advertised schema for facts, decisions, commitments, questions, guidance and workshop candidates. An unsupported or unavailable genuine-human source remains a coverage gap, not permission to relabel model text.
