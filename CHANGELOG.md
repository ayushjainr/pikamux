# Changelog

## 0.6.1 — cross-platform native release

- Correct platform-specific compilation for Linux hosts and the Windows client.
- Keep release validation aligned with the published version and dependency notices.

## 0.6.0 — Pika in Rust

- Ship a small native executable for macOS and Linux, with prebuilt downloads
  ready to run.
- Keep the live board, return by name, private consultations, expert discovery,
  and SSH/Tailscale federation in one familiar command.
- Reduce board startup, redraw latency, background CPU, and memory use. See
  [measured performance](docs/PERFORMANCE.md) for the benchmark details.
- Preserve existing conversation identities, expert cards, and local state.
- Verify native release downloads before activation and retain prior native
  releases for `pika update --rollback`.
- Include an experimental native Windows client for pairing with a Pika host.

## 0.5.0a4 — reliable returns and consultations

- Support private consultations from newer Codex conversations with paginated
  history, without changing the expert conversation's context.
- Keep failed board attachments visible with an exact recovery command, including
  on wide terminals, instead of silently returning to the board.
- Simplify the board to Needs You, Working, Ready, and Parked. Show recovery
  reasons and cached state on individual rows without losing exact identities.
- Make the default installer select the newest complete published release,
  including alpha releases, without requiring a version argument.
- Refine first-use documentation and update the illustrative walkthrough.

## 0.5.0a3 — approved updates from the board

- Add cached, background update notices for installer-managed boards. Press `U`
  to review the exact release and Enter to approve; installation runs separately
  without stopping agents. No automatic installation or remote upgrades.
- Make online `pika update` channel-aware: alpha/beta/RC installations can find
  newer prereleases or stable versions; stable installations stay stable. Support
  `--release VERSION` for an explicit, version-pinned update.

## 0.5.0a2 — skill included in setup

- Include the bundled agent-convo skill in setup for installed Codex, Claude,
  and OpenCode providers, using the existing preview and approval step. Back up
  replaced instructions, preserve other resources, and leave externally managed
  symlinks alone with a notice. No separate skill-install command is required
  when accepting setup. `--no-setup` and dry runs remain non-mutating.

## 0.5.0a1 — first public alpha

Distributed through GitHub Releases, not PyPI. Historical 0.4.4 tags do not
contain everything listed below; their artifacts have not been overwritten.

- Add an inspectable fresh-host installer, isolated runtime, staged `pika update`,
  private release bundle builder, and approved verified-package transfer over SSH.
  Keep live agents, provider configuration and existing development installs intact.
- Resolve same-name conversations using verified provider/directory/activity,
  preferring one exact live home. Exclude confirmed-missing stale choices while
  keeping unknown storage and genuine concurrency fail-closed.

- Separate intervention requests, unread results, and failures on the board;
  share state evidence with `pika explain`.
- Render cached inventory first and retain navigation through attach/detach.
- Preserve expert discoverability after unwatching; track durable expertise and
  current work separately.
- Report consultation stage, delivery certainty, and verified cleanup.
- Distinguish consultation isolation from measured transcript preservation:
  unmeasured parent-byte equality is `null`, never an automatic success claim.
- Package the agent-convo skill and introduce first-conversation onboarding.
- Prepare public documentation, synthetic examples, portable tests, locked
  development dependencies, and build verification.
- Add native macOS process identity through a macOS-only psutil dependency,
  quota-aware LaunchAgent scheduling, and Mac host discovery. Keep unsafe
  process takeover unavailable on runtimes without generation-pinned signaling.
- Exercise native Mac identity and tmux recovery; make PTY fixtures continuously
  consume terminal output instead of depending on Linux buffer sizes.
- Require explicit naming provenance for the first setup chooser. Follow it with
  a separate, bounded Recent chooser for unnamed and uncertain titles, including
  native Codex names whose authorship is not recorded; keep older inventory behind
  Browse all. Preserve exact lookup and
  existing watched homes; apply the same filter on updated remote nodes.

See Git history for earlier development; older versions are not claimed as
separately maintained release lines.
