# Security and privacy

Pika is an alpha local tool. Treat the current release as the supported bug-fix
target; there is no security SLA or promise of compatibility with every provider
release. Exact-identity checks reduce specific risks, not all possible races or
provider failures.

## Reporting a vulnerability

Do not post credentials, raw transcripts, database files, or an exploitable
reproduction containing private information in a public issue. Use GitHub's
private vulnerability reporting on this repository **if enabled**. If that
option is unavailable, open an issue asking for a private contact without
including vulnerability details. Private reporting must be verified by the
maintainer at public launch (GitHub offers it for public repositories only);
it is not assumed active by this document.

## What is stored and where

On Linux and macOS, defaults are `~/.config/pika` for configuration and
`~/.local/state/pika/pika.db` for state (XDG and PIKA overrides are supported).
Provider transcripts remain owned by the provider. Pika metadata includes names,
paths, identities, lifecycle events, usage counters, machine mappings, and expert
profiles. Profiles and current-work summaries are agent-authored content and may
be sensitive. Client pairing configuration also contains secrets.

Ordinary inventory does not index full transcripts or send model prompts.
Provider integrations can read local history/structured records to discover
identity, lifecycle, and usage; metadata-only does not mean no local file reads.
An explicit peek, including a selected pane's board preview, captures terminal
output. Inline questions and answers are displayed in the active board; copied
output, shell redirection, and provider logs may retain content outside Pika.

## Consultation boundaries

"Private" means a side consultation separate from the parent conversation, not
offline processing, an encrypted vault, or invisibility to the provider. Your
provider receives the consultation and its inherited context under its own
account terms and retention controls. Pika does not proxy it through a Pika
hosted service.

- Codex uses an ephemeral in-memory fork and restricted consultation execution.
- Claude uses a non-persistent streamed fork with tools disabled.
- OpenCode uses a temporary persisted fork and a read-only permission profile;
  local reads are allowed. The fork is deleted only after verified cleanup.

Pika intends to leave parent transcripts unchanged. Unknown delivery and failed
cleanup are reported as such; interruption can leave provider-owned temporary
state requiring recovery. Do not interpret a requested discard as proof that
every provider log or external copy has been erased.

Expert interviews and consultations consume provider quota. Routine inventory
does not. Setup proposes a quota-aware refresher: its periodic check is cheap,
but an eligible interview sends a model request. Review that configuration if
background provider use is inappropriate for your environment.

## Machine trust and recovery

Remote machines are explicitly selected and contacted over SSH. Discovery of
SSH/Tailscale candidates is not authorization to connect. Selected remote nodes
can return metadata and expert summaries; explicit peeks and consultations can
return content. No full transcript is copied by ordinary fleet inventory.

Pika revalidates native identity before attach and pane capture, and refuses
unproven concurrency rather than choosing a copy. This is not a backup system:
host loss, deleted histories, compromised local accounts, and malicious provider
or SSH binaries are outside that guarantee. The Windows bridge trusts paired
node identities and loopback secrets; never expose its listener publicly.

Back up configuration before setup changes and inspect its diff. Never upload
Pika's state directory to report a bug. Read logs and diagnostics before sharing
them, even if a particular summary is designed to be shareable.
