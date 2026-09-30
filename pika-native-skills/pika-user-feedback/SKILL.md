---
name: pika-user-feedback
description: Record explicitly submitted product feedback about Pika in its private user_feedback.md log.
---

When the user explicitly submits feedback, use the Pika backend feedback tool to append their exact note, UTC date and current scope. Preserve existing notes. Confirm only after a successful write; a save does not mean the issue is fixed. If no note is supplied, explain usage and location without writing. Use this skill by `$pika-user-feedback`; do not pretend `/feedback` is a registered native Codex command.

Call `pika_user_feedback` with the active trusted Human `source_id` for the exact `$pika-user-feedback NOTE` submission, obtained from native state. The backend owns parsing the exact user's wording; do not substitute a rewritten note or manufacture a Human source. If the submitted source is unavailable or untrusted, explain that the save could not be confirmed.

The log is product feedback, not a second memory/persona authority. Do not automatically capture transcripts, change guidance, implement a fix, or add a triage job. Backend logging remains usable without a provider; this skill adds no extra model call.
