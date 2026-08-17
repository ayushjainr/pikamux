# Pikamux

Pikamux provides the `pika` command: a persistent identity keeper and attention
router for Codex and Claude conversations running in tmux.

Pika does not do the agent's work. It remembers exactly where that work lives
and brings it back when called.

## Why Pika

Agent conversations already persist, and tmux sessions already persist. The
missing piece is a trustworthy bridge between a human name, the provider's
immutable conversation UUID, and the pane where that conversation is running.
Pika owns that bridge.

- It resumes exact UUIDs. It never falls back to `--last` or `--continue`.
- It refuses to open a second copy of a conversation already running elsewhere.
- It routes attention using `NEEDS YOU`, `WORKING`, `READY`, `PARKED`,
  `UNBOUND`, `OPEN TWICE`, and `ERROR` states.
- It stores metadata only. Ordinary views never index or display transcript
  contents; `peek` explicitly captures the visible tmux pane.
- Linux nodes use hooks plus command-time reconciliation, with no background
  daemon. The optional Windows client bridge is described separately below.

## Install a Pika node

Pikamux requires Linux, Python 3.10 or newer, and tmux. Codex and/or Claude must
already be installed.

```bash
uv tool install --editable /path/to/pikamux
pika setup
```

`pika setup` is presented as a commissioning flow, not a blind installer. It
states the integration contract, shows a diff before changing anything, backs up
existing files, preserves existing JSON key order, and merges lifecycle hooks
into Codex and Claude user settings. On first setup it offers to add existing
resumable conversations with explicit names plus any conversation that is
currently live before asking for a default provider. Later setup runs configure
and verify the integration without rescanning conversation inventory or machines;
use `--import-all`, `--machine`, or the daily `pika NAME` entry point when that
work is intentional. Archived sessions, missing histories, and
AI-generated Claude summaries stay out of this commissioning choice. For Codex,
setup uses the effective saved name exposed by Codex: names created with
`/rename` are eligible even when the internal SQLite `threads.name` column is
empty. Subordinate Codex threads (`thread_source=subagent`, including native
side work) and short-lived workers from known automation harness origins are
also excluded. Pika reads only immutable `session_meta` provenance and requires
its UUID to match the candidate; it never guesses from a generated name. The
built-in automation origins are `agentic_fund` and `quant_agent_autonomy`.
Installations can extend `codex_worker_originators` in Pika's `config.json` for
their own runners. The parent conversation or run remains the visible
workstream. Nothing is imported silently. The final
commissioning ledger distinguishes active hook definitions from observed live
events. Codex requires one extra trust step: open
`/hooks`, approve the Pika definitions, and use Codex once so `pika doctor` can
observe a real lifecycle event. Pika uses “commissioned” only when both provider
executables pass a version probe, both integrations are currently active, and
both have delivered the current hook definition; otherwise it names every
remaining executable, activation, or observation proof.
Ordinary monitor/list/name reconciliation keeps provider-native renames current;
setup itself does not use conversation refresh as a hidden side effect. It never
interviews agents.
Conversations you explicitly untracked stay out of later setup import choices;
first setup and explicit import runs report that suppression and give the simple
`pika <conversation-name>` restore path. An explicit Claude title (including
`/rename` and `customTitle` history)
outranks a later generated summary.

Setup also pins the exact Codex and Claude executables it commissions, and uses
the same stable runtime PATH for the expert-refresh timer. A later shell, NVM, or
PATH change therefore cannot silently make Pika launch a different provider
binary. Override a choice explicitly with `--codex-executable PATH` or
`--claude-executable PATH`.

For automation, review a dry run and then apply it explicitly:

```bash
pika setup --dry-run --default-provider codex
pika setup --yes --default-provider codex --no-import
pika setup --yes --default-provider codex --import-all
```

Backups are written beside each changed file with a
`.pika-backup-YYYYMMDDTHHMMSSZ` suffix. Restore one by copying it back over its
original file; Pika never deletes backups.

## Open a local window from a remote Pika board

Pika uses the same package and `pika` command on the laptop and server. Linux
nodes own provider processes, exact UUID reconciliation, and tmux homes. The
Windows client owns only local window creation; a remote process is never given
permission to execute an arbitrary client-side command.

Install the same release on Windows, then pair each server whose agents may be
opened locally:

```powershell
uv tool install "git+ssh://git@github.com/ajainwolfe/pikamux.git@v0.4.4"
pika setup rstudio-6
```

Pairing performs two SSH identity receipts, stores a different random secret for
that exact Pika node on each side, and starts the loopback-only client bridge.
It prints one line to add to the matching `Host rstudio-6` block in the local
OpenSSH configuration:

```sshconfig
RemoteForward 127.0.0.1:47654 127.0.0.1:47653
```

Reconnect that SSH session after adding the forward. Thereafter, Enter on an
exact row in the remote `pika` monitor asks the paired client to launch:

```text
wt.exe -w new ... ssh.exe -tt -o ClearAllForwardings=yes rstudio-6 pika _fleet-open \
  --expected-node-id NODE_UUID --provider codex --session-id CONVERSATION_UUID
```

The request contains only a client ID, source node UUID, target node UUID,
provider, conversation UUID, one-time request ID, and pairing secret. The
Windows side chooses the SSH target from its own trusted mapping and constructs
the argument vector itself. Names and shell command strings never cross this
boundary. The remote `_fleet-open` endpoint revalidates the target node and
tracked conversation before attaching. The launched attach disables inherited
SSH forwards so it cannot compete with the dashboard connection for the reverse
bridge port.

The monitor stays open after a confirmed `WINDOW LAUNCHED` receipt. If the
reverse forward or client bridge is absent, Enter preserves the original
behavior and attaches in the current terminal. A reachable bridge that rejects
identity fails closed and leaves the monitor open with the exact reason.

Useful client commands:

```text
pika setup SSH_HOST       pair one exact Pika node and start the bridge
pika                      show paired client nodes
pika bridge status        show paired client nodes
pika bridge start         start the loopback bridge in the background
pika bridge serve         run it in the foreground for diagnosis
```

Pair every target machine you want a fleet board to open directly. A board on
one paired server may then request a window for another paired node using only
that target's immutable node UUID. The bridge cannot route to an unpaired node.

## Multiple machines

Any Pika installation can be a **coordinator** for other Pika machines. The
role is selected by the user; no hostname, cloud, Tailscale account, or machine
such as `rstudio-6` is built into the product. Each remote Pika remains the sole
authority for its own provider UUIDs, processes, transcripts, tmux panes, and
expert cards. The coordinator stores only an explicitly trusted node UUID and a
sanitized last-good metadata snapshot. It never copies transcript content.

Pika uses the SSH access you already have. It does not copy private keys, edit
`known_hosts`, weaken host-key checking, change Tailscale ACLs, scan subnets, or
open a Pika server network port. A hidden versioned JSONL protocol runs over an
ordinary non-interactive SSH process; human attaches use an SSH PTY. Network
visibility is not treated as authorization.

Interactive `pika setup` has a two-stage federation step:

1. It passively reads concrete aliases from `~/.ssh/config` and peers already
   visible in `tailscale status --json`, then asks which machines to contact.
2. After a selected machine proves its immutable Pika node UUID and protocol,
   Pika asks which eligible conversations to add there.

Unselected candidates receive no network traffic. Selecting a remote
conversation updates Pika only on that remote machine; it does not start, move,
resume, or interview the agent. If Pika is missing remotely, setup displays the
exact version-tagged install command and requires a separate confirmation before
running it. Handshakes report the remote package version, and `pika machines
upgrade MACHINE` is the explicit rolling-upgrade path; Pika never follows a
mutable branch during remote installation.
`pika setup --yes` never selects machines or installs remotely by itself.

For an explicit non-interactive rollout:

```bash
pika setup --yes --machine rstudio-5 --remote-import-all
```

Useful fleet commands:

```text
pika machines discover            passive candidates; zero SSH connections
pika machines add HOST --alias A  verify and trust one existing Pika node
pika machines list                node UUID, address, health, and last error
pika machines remove A            delete local trust/cache; remote untouched
pika machines upgrade A           install the pinned matching release, then reverify
pika machines ignore HOST         stop offering one discovery candidate
pika sync A                       refresh one node in the foreground
pika list --all-machines          local truth plus cached remote snapshots
pika list --all-machines --json   versioned pikamux-fleet-list/v1 envelope
```

The human locator is `thread@machine`:

```bash
pika master_quant@rstudio-5
pika ask master_quant@rstudio-5 "What is the current blocker?"
pika peek master_quant@rstudio-5
pika untrack master_quant@rstudio-5
```

Names are routing conveniences. Before every remote action, the coordinator
refreshes that one node, resolves the name there, and sends only the exact
provider plus conversation UUID. `UUID@machine` therefore survives renames.
Same-name Codex/Claude sessions retain Pika's explicit chooser. If a literal
local name such as `build@atlas` also parses as a configured route, Pika asks
instead of guessing; non-interactive callers must use an exact UUID.

The live monitor merges fresh local sessions with cached remote snapshots.
Local reconciliation remains independent every two seconds. At most one remote
inventory refresh is in flight, selected oldest-attempt-first from due machines;
there is no artificial gap while more machines are due, and selection affects
only an explicit manual refresh. A machine whose SSH latency pushes the serial
fleet past the 45-second freshness budget is honestly marked `CACHED`; it never
blocks typing, local state, or quitting. Remote
pane tails are fetched only after explicit `p`, and remote side questions keep
one ephemeral SSH/JSONL process for all follow-ups. Stale rows move to a
`CACHED` group, lose actionable attention status, retain their last-success age,
and never disappear merely because a machine is offline.

Remote errors remain distinct: `UNREACHABLE`, `SSH TRUST OR AUTH FAILED`,
`INCOMPATIBLE`, and `NODE IDENTITY CHANGED`. An identity change quarantines
actions and preserves the last-good cache until the operator verifies and
re-adds that machine. `pika doctor` remains a local recovery certificate;
optional remote availability cannot make a locally recoverable session unsafe.
Remote mutation receipts are separate from follow-up cache reconciliation: a
proven adoption or untrack remains successful if the next snapshot fails, with
the node left visibly stale for later reconciliation. Remote peek displays its
captured output before starting any separate acknowledgement.

## Daily commands

```text
pika                       open the live operations monitor
pika NAME                  find, protect, attach, resume, or safely create by name
pika ask NAME "QUESTION"   ephemeral multi-turn consultation with that parent
pika ask NAME --fast ...   faster Codex consultation using Luna medium
pika ask NAME --jsonl      persistent JSON-lines side channel for agents/apps
pika experts QUERY         find provenance-bound experts across projects
pika experts QUERY --json  stable machine-readable expert matches
pika expert status         show current, stale, missing, and unknown cards
pika expert refresh NAME   interview one exact conversation now
pika expert refresh --all  build/update the tracked expert directory now
pika expert refresh --due  enforce the weekly quota-aware refresh policy
pika expert publish ...    publish this exact pane's own expert card
pika expert clear          remove this exact pane's expert card
pika .                     open the relevant conversation for this repository
pika -                     return to the previously attached Pika conversation
pika list                  show all tracked live and parked conversations
pika list --json           stable machine-readable inventory
pika list --all-machines   include cache-only remote Pika snapshots
pika machines discover     passively find SSH/Tailscale candidates
pika machines list         show trusted nodes and connection health
pika sync MACHINE          refresh one remote node now
pika next                  open the oldest conversation needing attention
pika peek NAME             inspect recent pane output without attaching
pika peek NAME --ack       explicitly acknowledge READY in a script
pika wait NAME             wait for NEEDS YOU, unread READY, OPEN TWICE, or ERROR
pika untrack NAME           stop watching without stopping or archiving the agent
pika doctor                print a recoverability receipt
pika doctor --verbose      show every receipt check
pika doctor --repair-stale remove confirmed stale launch locks (5m+)
```

`pika NAME` is the normal lifecycle command. If that exact name is already
tracked, Pika attaches or resumes its immutable provider UUID. If it is a native
Codex/Claude conversation Pika has not tracked yet, Pika protects it and opens it.
If it is already running in an untagged tmux pane, Pika can tag that pane and
attach. A process that started in an ordinary terminal cannot be moved safely;
Pika names the live PID and asks you to exit normally, after which the same
`pika NAME` command resumes it inside Pika. Genuine duplicate UUID processes are
reported as `OPEN TWICE`. Same-name providers or machines produce an explicit
chooser. Only a genuinely unknown, non-UUID, non-near-match name creates a new
conversation using the configured default provider and native provider name.
Implicit creation requires an interactive terminal. Scripts must use the
advanced `pika new NAME` command explicitly. When any trusted machine has a
missing or stale inventory, Pika also pauses implicit creation and names the
`pika sync MACHINE` command needed before retrying; explicit `pika new NAME`
remains the deliberate local escape hatch.

`pika open`, `pika new`, and `pika adopt` remain advanced diagnostic controls;
daily use should not require knowing which mechanism applies.

If Ctrl+C exits a Pika-managed Codex or Claude client, the wrapper immediately
releases only that client's live-owner lease and leaves the conversation
`PARKED`, not in a false error state. Run the same `pika NAME` command again and
Pika resumes the exact UUID in its existing idle pane or a new home. Modern
Codex CLI, IDE, and desktop clients can all use the same app-server, so Pika
does not label that shared PID as a desktop app. If it also sees a live
`codex resume NAME` process, the receipt says `ACTIVE IN CODEX CLI` and gives
the exact `/exit` then `pika NAME` sequence. Otherwise it reports ambiguous
shared client state without guessing which client owns the lease. The same
`pika NAME` invocation asks once whether every provider client has exited; a
confirmed answer revokes only shared-infrastructure leases and resumes the exact
UUID. An exact UUID PID, dedicated process, or running tagged pane remains
fail-closed. Non-interactive calls never assume confirmation. The receipt also
gives the UTC lease deadline as a wait-only alternative. Shared app-server PIDs
are explicitly marked as infrastructure that must not be killed. No recovery
subcommand is part of the user model.

`pika ask master_quant "What assumption is weakest here?"` opens a temporary
side conversation based on that exact provider UUID. In a terminal, ask
follow-ups at the `side>` prompt and type `/close` when finished. Codex defaults
to `gpt-5.6-sol` with medium reasoning; `--fast` selects the benchmarked
`gpt-5.6-luna` medium profile. Pika verifies the provider-confirmed model and
effort before showing them in terminal and JSONL open/close receipts. The inline
monitor and expert-card interviews use the same
Sol-medium default. Claude remains provider-native because it has not been
benchmarked for this override; `--fast` therefore fails closed for Claude.
Pika's `--fast` means the Luna-medium profile; it is not Codex Fast mode and
does not select a service tier.
Codex uses an in-memory ephemeral fork. Claude uses one streamed,
non-persistent fork with its tool surface disabled. The parent can keep working;
the side is read-only, does not append to the parent transcript, and is
discarded on close. Claude support is capability-gated to the tested CLI
version, and Pika fails closed if it cannot verify support.

Pika normally builds expert cards itself, but `pika setup` never interviews
agents. Commissioning and bulk adoption therefore finish without hidden model
calls, even when many tracked conversations have missing or stale cards.
`pika adopt` interviews only the one exact UUID it just adopted. Interviews use
the same read-only, ephemeral provider fork as `pika ask`, so a live parent keeps
working and its transcript is unchanged. The interview is asked for only
firsthand work, specific topics, and concrete artifacts. The saved card includes
its provider UUID, source, and transcript fingerprint. Each interview separates
two horizons: `scope` describes the durable mandate across the whole thread,
while `current_state` says what is actually happening now—the active objective,
stage, blocker, decision, or next step. The prompt explicitly rejects a recap of
the latest turn and asks the agent to weight early, recurring, and recent work
against the entire inherited conversation.

`pika expert status` reports `CURRENT`, `STALE`, `MISSING`, or `UNKNOWN` without
reading transcript contents. `pika expert refresh NAME` and `--all` are explicit
ways to spend quota now. Setup leaves missing and stale cards to the quota-aware
policy. A user-level one-shot timer checks every ten minutes,
but calls at most one changed conversation per provider only during the final
six hours before that provider's weekly reset and only while more than 10%
remains. It reads provider-native reset telemetry, never hard-codes reset times,
and makes no model call when telemetry is missing or stale. An attempted card is
not retried in the same reset cycle.

`pika expert publish --scope "..." --now "..." --topic "..." --artifact "..."`
remains an exact-pane correction surface; one agent cannot manually write
another agent's card. `--summary` remains an alias for `--scope`. `pika experts
"factor attribution" --json` ranks cards deterministically from topics, durable
scope, current state, project, and artifacts, exposes `matched_on` rather than
pretending the score measures intelligence, and includes card freshness and
source. Archived conversations disappear from lookup with the rest of Pika's
daily surface. Cards created before the two-horizon contract are marked stale
until a scheduled or explicit refresh supplies their current state.

The advanced `pika adopt NAME` command resolves the exact UUID and finds an
untagged tmux pane that contains its live provider process. It is mainly useful
for diagnosis because `pika NAME` performs the safe lifecycle decision itself.

Agent and dashboard clients can keep a genuine multi-turn side open with
`pika ask UUID --jsonl`. Send one `{"question":"..."}` object per line and end
with `{"close":true}`. Responses identify the exact parent UUID and explicitly
confirm the discarded ephemeral lifecycle. Open and close receipts also report
`consultation_mode`, `model`, and `effort`; Claude reports null model/effort and
provider-native mode. The same provider process handles all questions until
close, so follow-ups retain side context without replaying answers or modifying
the parent.

The interactive `pika` monitor refreshes operational state every two seconds.
On wide terminals it uses a grouped workstream rail and a selected-workstream
inspector inspired by a live operations board: exact identity, signal, expert
card, read-only pane tail, and actions remain visible together. Narrow terminals
retain the compact table view. Use arrows or j/k to select, Enter to open the
selected identity, and `a` to focus an ephemeral multi-turn side consultation
inside the Pika panel for that exact UUID. Press `A` instead for the faster
Luna-medium Codex profile. The left operations rail remains visible on wide
terminals; narrow terminals use a focused full-width side panel.
Inside the side, type normally, press Enter to send, Ctrl+J for a newline,
Ctrl+U to clear the draft, arrows to scroll, and Esc to close and discard the
side without leaving Pika. Use `n` for the oldest attention item, `p` for a full
sanitized pane peek, `u` to reveal or hide provider usage, `r` to reconcile
immediately, `?` for keys, and `q` to leave from the operations view.
Press `x` to stop watching the selected workstream. Pika asks for confirmation
inside the panel, then removes that UUID from Live Operations while leaving its
agent process, provider conversation, and expert card intact. This differs from
tmux's `Ctrl-b d`, which only detaches your current view and keeps the workstream
on Pika. An untracked live hook cannot silently add the row back; explicitly
open its UUID or adopt the conversation when you want Pika to watch it again.
An `unbound:%pane` placeholder cannot be removed until Pika learns the immutable
provider UUID; the panel explains that boundary rather than creating a weak
pane-only ignore rule.

The inline side keeps one provider process for all follow-ups and shows opening,
thinking, ready, and error states without blocking dashboard refresh or input.
Its header identifies the immutable parent UUID and confirms that the parent
transcript remains unchanged. Usage collection starts separately every thirty
seconds only while its view is visible, so it cannot delay operational updates,
opening a workstream, or leaving the monitor. Expert cards are loaded locally
every five minutes without spending provider quota. The selected-pane tail is
read-only, transcript-free, and never acknowledges unread work. Selection
remains stable by provider UUID even when a status change reorders rows.

The monitor opens with a decision briefing rather than raw process totals. A
first handoff describes current state; after six hours away, a temporary
`SINCE YOUR LAST VISIT` strip counts actionable lifecycle events from Pika's
local transcript-free event ledger using an atomic committed-event watermark.
Quiet screens say `NO ATTENTION PENDING`;
partial or failed reconciliation says `PARTIAL` or `STALE` rather than claiming
complete synchronization. State time is semantic: waiting/results/failures use
their lifecycle event, while working and parked rows report last activity.

The `PIKA PLAYBOOK` strip rotates every five minutes through actions relevant to
the current screen. Exact/protected language appears only when Pika has current
provider UUID-to-PID evidence for that tmux pane. Opening an exact unread result
atomically clears that event and emits a one-shot `RESULT COLLECTED` receipt with
the remaining result count; losing a race to a newer event leaves it unread.
When stdout is redirected, bare `pika` falls back to the finite static briefing;
`pika list --json` remains the preferred automation contract.

When multiple conversations share a name, Pika displays a numbered chooser with
provider, short immutable UUID fingerprint, repository, branch, recency, and
state. Cross-provider collisions explicitly say that both Codex and Claude have
the name; `q`, Esc, or EOF cancels without opening anything. A missing name
prints close matches and never creates a near-duplicate accidentally. A name
with no exact or close match creates a new default-provider conversation.

`pika list` opens with a compact delegation briefing. Its `WHY` field is a
closed, transcript-free event reason such as `permission`, `question`,
`completed`, `failed`, or `exited`; `VIEW` means that exact pane is currently
visible, not merely that its tmux session has a client. The wide view includes
process-tree CPU/RAM and provider usage where structured counters are available.
Codex `request_user_input` and Claude `AskUserQuestion` enter `NEEDS YOU` at the
provider's pre-tool lifecycle boundary and return to `WORKING` after the answer
completes; Pika does not inspect the question text.
Dollar values are visibly labeled `~API$`: a dated, best-effort API-equivalent
list-price estimate, never a subscription bill. Unknown models or unavailable
usage render as `—` rather than zero. Applicable action hints teach `pika next`,
`pika .`, and `pika -` without adding new workflow concepts.

Inside tmux, `pika peek NAME` uses a popup: Enter attaches, while Esc or `q`
returns. A human terminal view acknowledges an unread `READY`; redirected or
scripted output preserves unread state unless `--ack` is explicit. In scripts,
`pika wait NAME --for needs-you --timeout 600 --json` provides a daemon-free
synchronization primitive.

Pika-created homes enable tmux mouse handling only for that session, so a wheel
or trackpad scroll enters pane history without a prefix key. Scroll back down to
the bottom or press `q` to return to the live agent; hold Shift while dragging
when the outer terminal should select text itself. Newly created and exact-
respawned Pika panes retain up to 100,000 lines. A live pane created by an older
Pika release gains wheel scrolling immediately and the deeper allocation on its
next exact respawn. Global tmux options and adopted user-owned sessions are not
changed.

## Data and recovery

Pika stores configuration in `~/.config/pika/config.json` and owner-only state
in `~/.local/state/pika/pika.db`. The durable key is `(provider, UUID)`; Pika
refreshes provider-native names and may therefore show collisions. A collision
always produces a chooser (or an error in a non-interactive process), never an
implicit provider choice.

Codex can continue work in a newly forked thread while the original client and
Pika pane remain alive. Pika treats that as one conversation: the original UUID
stays the stable Pika home, while provider operations and exact-process checks
follow the current immutable child UUID. One fresh, working, direct child with
the same saved name and working directory is reconciled automatically, so the
inventory shows the real current state instead of a stale `READY` parent plus a
second setup candidate. If the parent and child—or two children—are genuinely
working at once, Pika refuses to guess and marks the single home `OPEN TWICE`.
Receipts show the current exact UUID as well as the stable home fingerprint when
they differ.

Renaming an adopted conversation remains provider-owned. The next ordinary
reconciliation, including `pika`, `pika NAME`, or `pika list`, updates the
tracked display name and the pane's recovery metadata when the provider exposes
the new name.

Live hook ownership is bound to both a PID and its Linux process start time, so
a recycled PID cannot counterfeit exact-UUID identity. A shared Codex app-server
owner is advisory whenever a client process carries direct UUID evidence and is
a five-minute, hook-renewed lease only when stronger evidence is absent; an
expired claim is removed automatically. A Node launcher and its direct native
Codex child count as one logical client even though both argv values contain the
UUID. Separate UUID-bearing process trees remain fail-closed and are displayed
as `OPEN TWICE`. Legacy owner rows without start-time evidence remain
fail-closed.

Each Pika-managed conversation gets one UUID-derived tmux session. A new Claude
conversation is named natively with `--name`; a new Codex conversation is named
through Codex's local app-server thread API after its exact UUID is proven.
If a launch hook is delayed or missed, the launch remains visible as `STARTING`
on the board instead of disappearing. Command-time reconciliation can recover
it only from one token-bearing pane, one matching provider process and PID start
time, a successful pre-launch provider snapshot, a settling interval, one
UUID-matched provider record created in the exact directory and launch window,
and no competing exact UUID process. A missing baseline, subordinate Codex
thread, or ambiguous evidence remains
`IDENTITY PENDING`; after the grace period it becomes an actionable error.
Pika disables the tmux status bar only for sessions it creates, keeping the
Codex or Claude interface visually unchanged while leaving global tmux settings
and explicitly adopted sessions untouched. It also launches the agent with the
caller's `PATH`, a 24-bit RGB tmux terminal contract, and without stale
automation-only `NO_COLOR` state inherited from an older tmux server. Explicit
interactive `NO_COLOR` preferences remain respected. Pika declares RGB support
to tmux and uses the `tmux-direct` terminfo contract so both agents retain their
24-bit color palettes. Codex additionally derives its adaptive user-message and
composer fills from OSC 10/11 terminal queries, which tmux consumes without
answering. Before starting Codex, Pika queries the directly attached terminal
once and passes the result to a transparent private-PTY bridge. The bridge
answers only those two Codex probes and forwards every other terminal byte; it
does not hard-code a theme or alter Claude's launch path. If the outer terminal
does not report a palette, Pika leaves Codex's conservative fallback unchanged.

The provider process may exit while the tmux session remains as an idle shell.
Opening that conversation later respawns the exact UUID in the same pane. If
that pane contains any foreground or background work, Pika preserves it, clears
its Pika ownership tags, and creates a fresh UUID-derived home instead. If the
tmux session itself disappeared, Pika creates another and resumes the UUID there.
Every successful attach displays a short threshold receipt in tmux: provider,
name, UUID fingerprint, and a proof-scoped outcome such as `ATTACHED EXACT`,
`RESUMED EXACT`, `NEW HOME · EXACT`, or `IDENTITY PENDING`. When work was
preserved, the receipt identifies the old pane and command rather than hiding
the safety decision.

`pika doctor` is deliberately strict. “Safe to close this terminal” requires
valid provider UUIDs, durable provider history (or an exact live tagged pane),
existing saved directories, owner-only state, one owner per identity, installed
hooks, and a current structured hook observation for each provider in use. A
worker session cannot satisfy that commissioning proof. A warning is never
reported as safe.

A successful human receipt is deliberately scoped: `Recovery verified` includes
the exact conversation count and UTC time, followed by `Safe to disconnect this
terminal. Keep the tmux server running.` Inside tmux it also reminds the user how
to detach. Warnings and errors suppress all verified/safe language.

Interrupted launches and resume locks remain visible in verbose and JSON doctor
receipts, including their token, age, pane, and lock-owner PID. `pika doctor
--repair-stale` removes only state older than five minutes: a resume lock whose
recorded PID and process start time prove its owner is gone, or a pending launch
with no matching provider process found by pane ID, tmux session, or launch-token
tag. Active and unprovable legacy state remains fail-closed.

Pika-created and explicitly adopted sessions have the strongest recovery
guarantee. Historical discovery is intentionally best-effort because provider
local indexes and transcript formats may change.

Pika stores identity and operational metadata, not prompts or responses.
Attention reasons come only from closed provider lifecycle event types, never
transcript inference. Usage statistics read only structured provider counters.
`peek` is the explicit exception: it shows already-visible terminal output from
the selected tmux pane.

## Development

The package uses only the Python standard library at runtime.

```bash
PYTHONPATH=src python3 -m unittest discover -s tests -v
PYTHONPATH=src python3 -m pikamux --help
```

The integration suite uses an isolated tmux socket and fake provider processes;
it does not touch a developer's normal tmux server or provider configuration.
