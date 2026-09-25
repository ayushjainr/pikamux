Pika 0.6.29 makes the board better at finding and returning to your work.

- **Named conversations appear automatically.** Claude custom names and Codex
  conversations with a later saved-name change join the board on refresh.
  Generated titles and helper conversations stay out. `+` now offers a focused
  named-conversation picker instead of every thread you have opened.
- **Safer recovery.** Pika can reconnect a stranded startup when exact evidence
  matches. If it cannot prove a live terminal's conversation, it offers an
  explicitly unverified terminal handoff without launching another agent or
  marking output read. `x` can hide an unconfirmed start without killing its
  terminal or deleting recovery information.
- **Claude usage returns on remote boards.** A stale machine-capability record
  no longer prevents a read-only quota request to a trusted, upgraded host.
  The host still proves its identity in the response.

This release also strengthens Ctrl+C and rename handling, adds recovery and
complexity checks, and prepares the Windows signing path. Windows artifacts
remain unsigned unless publisher signing is enrolled; enterprise policy may
still block them. Update each host where you want the new board behavior;
running agents are not stopped automatically.
