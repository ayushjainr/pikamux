# Changelog

## 0.6.27 — Files beside your agent, Muse on your board

- Open a read-only Files companion from the agent's return bar. Browse the
  project tree, navigate directories, and inspect workspace changes and diffs
  without interrupting the agent.
- Render Markdown and highlight scripts and source code. Toggle wrapping and
  the tree, scroll with the mouse, and resize the companion or its divider.
- Add native Muse Code discovery, exact UUID resume, lifecycle status, and the
  shared return bar and Files companion. Detect Muse during setup and preserve
  existing user settings when adding hooks.
- Keep Muse helper sessions out of parent ownership. Muse private consultation
  and quota reporting remain unavailable.

## 0.6.25 — Reliable returns, live status

- Reopen the verified running conversation even when an old shell retains
  duplicate Pika labels. Keep stale shells and agent processes untouched.
- Identify the conversation's actual owner instead of a nested provider helper;
  continue blocking genuine duplicates and incomplete identity evidence.
- Match the return bar's Pika label to the current conversation's status using
  the shared activity feed, with neutral styling when fresh status is unavailable.
- Show the newest ready conversation in the return bar when nothing needs
  attention, using the same filtering and selection rules.
- Distinguish uncertain ownership from multiple proven owners in diagnostics.

## 0.6.23 — Keep the consultation skill current

- Refresh the installed agent-convo skill after an approved macOS/Linux binary
  update when its bytes match a known shipped bundle, keeping a backup of the
  prior copy.
- Reconcile the skill on first board opening for managed installations updated
  by older updaters that predate skill refresh.
- Preserve absent, customized, symlinked, and externally managed copies. Report
  skill read or refresh failures separately without rolling back a successful
  binary activation.

## 0.6.22 — Expert cards with less upkeep

- Limit scheduled interviews to missing cards. New conversation activity no
  longer triggers another interview of already-published expertise.
- Guide participating agents to maintain their cards at meaningful milestones
  during existing work, keeping durable expertise separate from current work.
- Streamline the bundled consultation skill: direct named routing, reuse of
  relevant discovery results, focused questions, and related follow-up reuse.
- Preserve honest freshness, explicit refresh, quota guards, and saved cards
  when a refresh fails. Card age never blocks a named consultation.

## 0.6.21 — Faster expert consultations

- Use Luna-medium for Codex questions on the board and CLI; select Sol-medium
  explicitly with `pika ask --deep`. Keep Claude and OpenCode provider-native.
- Show provisional Codex answers while they arrive, locally and across supported
  fleet hosts. Add opt-in `--jsonl --stream` for agent callers.
- Wait for confirmed turn completion and report observed timing and token usage;
  partial output or an idle notification alone never counts as success.
- Route named consultations directly, without requiring an expert-card refresh.
  Keep related follow-ups in the same private side conversation.
- Deliver consultation instructions with the first question while preserving
  inherited history, read-only isolation and the original conversation.

## 0.6.20 — Keep the board in sight

- Restore live board counts in the agent return bar on tmux 3.2a, binding the
  feed to the exact attached client rather than guessing another client's pane.
- Show the newest attention item beside the counts, distinguishing a request
  for input from opening warnings such as duplicate clients or runtime errors.
- Keep warning labels compatible with older hosts, shorten them on narrow
  terminals, and clear them as the underlying attention state changes.

## 0.6.19 — A steadier board, cleaner previews

- Update changed rows without clearing the screen on routine refreshes, keeping
  the board steady even in terminals without synchronized-output support.
- Remove wrapped Codex suggestions and recognized Claude composer/status-line
  clutter from previews and expanded peek, including custom usage footers.
- Preserve submitted messages, approval questions, unread state, Unicode and
  colors. Native agent interfaces remain unchanged.

## 0.6.18 — Stay connected to your work

- Carry the board's live counts into the agent return bar, with the latest
  conversation waiting for your input. Share one feed across views and machines.
- Give setup the board's focused interface. Keep hook diffs and backup receipts
  under details, with explicit approval before settings change.
- Restore remote previews when the owning machine has a live pane. Hide known
  Codex and Claude composer/status-line clutter without changing agent output.
- Preserve exact handoff when a verified launcher starts its native client;
  continue blocking genuine duplicates and reused process identities.
- Keep Windows client builds separate from Unix-only feed receivers.

## 0.6.17 — See the work without leaving the board

- Show recent output from the selected live pane automatically, keeping unread
  state intact. Selection never asks a model or interrupts the agent.
- Give questions and recent output priority, with remembered expertise leading
  parked conversations. Adapt the rail to thread names and terminal width.
- Show genuine remote expertise instead of card-health diagnostics. Keep durable
  scope and current work separate, each with its own observation age.
- Preserve indentation and line breaks in previews, remove hidden terminal
  control payloads, and discard late replies after selection changes.

## 0.6.16 — A clearer board, an easier return

- Keep automatically discovered conversations off the board until selected;
  retain their records and expert cards without generating false attention.
- Simplify selected-thread briefings, colour-code status counts, and move full
  identity and diagnostics behind **d**. Fix unintended double underlining.
- Return from a Pika agent home using its **← Pika** control or displayed
  function key, normally **F12**, without stopping the agent.
- Stop counting a verified Codex launcher and its native child as two clients.
  Genuine duplicate conversations still block opening and show exact next steps.
- Reset mouse reporting at board and interactive handoff boundaries so clicks
  do not keep sending terminal-control fragments into the shell.

## 0.6.15 — Reliable opening across shells

- Open exact conversations on hosts whose default shell keeps a child process
  for commands. Pika's private launch holder now explicitly replaces its shell;
  protections against replacing another process remain unchanged.

## 0.6.14 — Return to the same board

- Return to your selected conversation and filter after detaching from a native
  agent. Keep failed opens in the board without automatically repeating a launch.
- Recover from transient session-observation conflicts during local and remote
  opens, preserving exact identity checks and duplicate-process protection.
- Keep pane previews and stop-watching confirmations inside the Unix board;
  add keyboard help and a separate cumulative-usage view.
- Explain missing expert cards, show recorded activity and pane availability,
  and retain readable structured OpenCode error messages.
- Support SSH aliases with a configured remote command and Pika installations
  outside the remote shell's default PATH.
- Preserve tmux identity framing when the invoking terminal uses a non-UTF-8 locale.

## 0.6.12 — Update without leaving Pika behind

- Offer new releases with an explicit Y/N confirmation on every platform.
  Defer the prompt during typing, consultations, and active actions.
- Install verified Windows client updates directly from Pika, without copying
  an installer command. Preserve running agents, pairings, and remote machines.
- Reopen the board after a successful update. Hand off old Windows PATH entries
  to the verified active release so existing PowerShell sessions stay current.
- Stop board observation without waiting on an unfinished local reader.

## 0.6.10 — Quota at a glance, steadier boards

- Reduce board flicker with buffered, synchronized frame presentation and skip
  terminal writes when the visible frame is unchanged.
- Show Codex and Claude weekly quota bars above the board shortcuts, with local
  reset times and reading freshness. Keep quota tied to the machine running the
  board regardless of thread selection; keep unavailable readings distinct from zero.
- Collect Claude's documented status-line quota feed during normal use, preserving
  existing status-line commands and saving no conversation content.
- Check for updates automatically in the background on macOS, Linux, and Windows.
  Show newer compatible releases on the board without delaying startup or
  interrupting agents. Updates remain explicitly initiated by the user.
- Cache release checks across board sessions and back off quietly when offline.
- Show Windows update instructions inside the board without an installation prompt.

## 0.6.7 — One local Windows board, all your machines

- Combine all paired machines in a native Windows board, with cached-first
  startup and independent machine health. No mandatory remote board host.
- Select multiple SSH or Tailscale machines; retain existing pairings.
- Open exact conversations directly in Windows Terminal without a reverse
  tunnel. Launch failures stay visible inside the board, without automatic retries.
- Keep remote peeks and private multi-turn consultations inside the board.
- Allow delayed launch receipts on optional bridges and keep the listener alive
  when a caller disconnects. Cancel blocked Windows pipe reads and writes.

## 0.6.6 — One Windows command, your whole fleet

- Open the remote board directly from `pika` on Windows. Choose an SSH host
  once; Pika remembers it and manages pairing, local window routing, and the
  board connection without SSH configuration edits.
- Revalidate the selected machine on the actual board connection. Preserve
  existing connections and fail visibly on forwarding conflicts or changed
  identities.
- Keep the Windows board's full fleet visible and open destinations through
  its trusted board host without requiring separate Windows pairing per server.
- Keep the Windows board connection open after launching a conversation.

## 0.6.5 — Pika in Rust

- Ship a small native executable for macOS and Linux, with prebuilt downloads
  ready to run.
- Keep the live board, return by name, private consultations, expert discovery,
  and SSH/Tailscale federation in one familiar command.
- Reduce board startup, redraw latency, background CPU, and memory use. See
  [measured performance](docs/PERFORMANCE.md) for the benchmark details.
- Preserve existing conversation identities, expert cards, and local state.
- Preserve timestamp precision in fleet snapshots and recognize exited Linux
  processes without false recovery errors.
- Verify attached clients across tmux versions that encode terminal fields differently.
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
