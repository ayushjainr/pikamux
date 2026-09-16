# Feature inventory and validation

Audit: 2026-09-14, against `37d9b7c` / 0.6.12 plus the local repairs described below.
This is a contributor audit, not a fleet-installation certificate. The repairs
described here are included in 0.6.14 and the 0.6.15 shell-portability patch; installed machines
require separate version and workflow verification.

## Conclusion

### Unreleased: shared live feed and newest input request

An in-process activity service owns observation; the board and return strip
consume the same filtered snapshot. Independent cursors keep one view from
consuming another's updates. The last consumer releases owned workers; exiting
the Pika process expires its remote feed rather than leaving frozen live counts.

The strip names the newest non-stale NEEDS YOU conversation, with its remote
machine. READY, errors and pending launches are excluded. Identity breaks equal
event-time ties; resolving the request removes it or reveals the next pending
one. Names shorten with terminal width and disappear before crowding navigation.
The version-2 frame adds only a bounded, inert label; older hosts get counts only.

Executed on isolated macOS fixtures: nine feed tests (including both SSH frame
versions, label-injection rejection, independent subscribers and unchanged-count
name updates), 47 monitor tests, and the real-tmux open/live-update/F12/mouse-return
journey pass. The journey checks the label reaching the terminal, numeric width
branches and removal after resolution. The receiving endpoint also accepted a
version-2 frame in its executable contract. Formatting and warnings-as-errors
Clippy pass. Windows CI now includes feed tests; no real Windows/fleet deployment
or release is certified by these local checks.

### 0.6.17: selected-pane briefing

Automatic preview uses one cancellable worker, a selection-generation fence and
exact node/provider/UUID/pane identity. Only the selected eligible pane is read;
no model call, transcript scan, durable preview cache or acknowledgement is added.
Local refresh is bounded to two seconds, remote to fifteen seconds, and failures
to thirty seconds. Old output remains visible during refresh; unavailable output
is stated honestly. Control programs are removed without flattening terminal rows.

The inspector adapts its rail and budgets recent output before a full expert card.
Durable scope and current work have separate ages; remote card diagnostics cannot
become expertise. Old snapshots with no genuine profile do not invent one.

Targeted evidence: 46 monitor tests, six real-tmux board journeys, five Windows
client contracts, eight preview-worker tests, 41 fleet contracts and five expert
federation contracts. The open/return journey observes automatic pane output
without pressing `p` and checks unread preservation. Layout fixtures cover
120×24, 140×32 and 180×48 with full cards and recent output. This section records
isolated fake-provider evidence, not a live-provider or Windows-device certificate.

Release-candidate validation on macOS: all 567 native tests passed, with formatting,
warnings-as-errors Clippy, third-party notices and RustSec checks also passing.
Six exported terminal frames at 120×24 and 180×48 were rendered and inspected:
no clipping or overlapping actions; larger screens retain more recent output.
The deployment smoke fixture was corrected to publish a new authoritative Stop
event rather than flipping an acknowledged projection bit. Both the observation
and projected unread state remain intact after automatic capture. Independent
adversarial review closed the multiline and compact-layout findings; publication
and installed-machine validation remain separate gates.

### Unreleased: mouse-reporting cleanup at terminal boundaries

The reported `0;55;31M`/`m` fragments match SGR mouse reports leaking into the
shell after a terminal handoff. Termios restoration did not disable the separate
DEC mouse modes. Board entry/teardown and interactive PTY/SSH handoff guards now
disable mouse capture without discarding input or altering the terminal palette.
Noninteractive handoffs do not emit cleanup escapes. Board setup also establishes
its teardown guard before fallible terminal output.

Six executable board journeys and two terminal-signal contracts passed in the
disposable Mac environment. The PTY fixtures enable mouse reporting, then verify
the reset follows child output on successful exit, failed exit and forwarded
SIGTERM; canonical typing mode is restored. The fixture must consume PTY output
while waiting: macOS TCSADRAIN otherwise waits for its absent terminal reader.
This is not live Surface/SSH-disconnect certification, and cannot guarantee
cleanup after SIGKILL or for SSH processes launched outside Pika. Installed
Mac and rs6 executables remain unchanged.

### Unreleased: launcher certificates cannot manufacture duplicate clients

A read-only rs6 inspection of the reported `cf_perf` failure found a single
Node launcher/native Codex child chain. Direct argv scanning folded the launcher
away, but its valid saved recovery certificate added that PID back into the
combined ownership set, manufacturing `OPEN TWICE` and blocking attachment.
Ownership now canonicalizes the combined direct/lease/recovery evidence without
deleting a still-valid certificate. Only recognized runtime launchers with
matching forwarded arguments and plausible parent/child generations are folded;
an actual native client spawning another native client stays distinct.

Regression coverage reproduces the certificate-plus-hook-lease case, proves a
real additional client remains blocked, verifies automatic recovery after that
copy leaves, and rejects reused certificate generations. Core (28) and process
(11) unit tests pass. Genuine multiple-client pane failures now name the observed
PIDs, a read-only inspection command and the exact UUID reopen command; the board
no longer suggests blindly retrying before resolving the cause. Live rs6 state
and its installed 0.6.15 executable were not changed by this diagnosis or repair.

### Unreleased: concise briefings and visible return navigation

The heading is now `PIKA`, with status-coloured counts. The selected briefing
leads with recognizable context and state-specific recorded evidence; short
expert excerpts retain card age and normal text contrast. `d` exposes the full
UUID, path, recorded pane and unabridged card. Only otherwise colliding names
show short identity fingerprints by default. No card interview, model turn or
new transcript read is triggered by selection. Missing question/result content
is not fabricated. The SGR 21 bold-reset bug is corrected to SGR 22, and
`NO_COLOR` now strips colour from the whole composed board while retaining
selection/emphasis and cursor control.

Exact Pika homes get a default-background `← Pika` strip. F12 (or the displayed
free F11/F10 fallback) and a click return without stopping the agent. Existing
root bindings and catch-all bindings are retained; keys pass through outside
opted-in homes. If no control can be allocated, the strip states the limitation.

Executed on the Mac in disposable environments: 43 monitor unit tests, 23 tmux
unit tests, six executable board journeys, five identity-safety contracts and
four real-tmux contracts passed. The actual PTY journey exercises F12, a mouse
click, custom-F12 preservation with F11 fallback, same-pane reopening, retained
filter and return to an existing tmux client's previous session. Layout tests
cover widths 80–140 and heights 16–32. Formatting, diff whitespace checks and
warnings-as-errors Clippy pass. These are fake-provider/local-terminal results,
not screenshots or device certification for Surface, rs6 or native agent themes.
No publication, installation, live settings or user-agent cutover occurred.

### Unreleased: explicit board membership

Provider names and hook events are no longer sufficient to enter the watched
inventory. Explicit Pika selections and exact attachments record durable,
provider/UUID-qualified receipts. Legacy managed and historical launch/attach
evidence remain accepted; unconfirmed external records are retained separately
and offered by setup or exact lookup, without deletion or unwatch tombstones.
They cannot emit alerts or request pane tagging before selection. Independent
renamed forks require their own choice; existing continuations retain their home.
Existing expert cards remain discoverable independently of board membership.

Executed evidence: the real Unix board hides an unconfirmed generated title while
retaining its unread record; explicit lookup stays read-only and selected opening
records consent even if launch is blocked; hooks for all three providers cannot
subscribe through names or inherited panes; tracking survives renames and respects
unwatch tombstones. Store, setup, discovery, identity, fleet, CLI and expert
federation contracts pass. The concurrency-hook fixture now explicitly subscribes
before expecting an alert. An unrelated provider-version probe timeout passed its
isolated rerun without changing its production deadline. Formatting and
warnings-as-errors Clippy pass. Installed copies and fleet state were not changed
in this membership pass; deployment and hook callback updates remain separate.

Installed 0.6.14 passed five isolated macOS board journeys. The same release's
rs6 verification exposed a shell-portability defect: `sh -c 'sleep 30'` retained
a shell parent and child, so the unchanged idle-tree safety check refused Pika's
own launch holder. Version 0.6.15 uses `exec sleep 30` and the real board regression
sets `SHELL=/bin/sh` explicitly. This is separate from a genuine second agent,
which must still be refused. Installed-host verification follows publication.

Pika's identity, storage, provider-protocol, and update contracts have substantial
automated coverage. That coverage did **not** establish that its everyday board
journeys worked. The Unix Peek command could leave the board; documented keys
had no handlers; and several documented information surfaces are absent.

The appropriate release gate is a user outcome demonstrated through the actual
executable, with lower-level contracts underneath it—not a total test count.

## Reading this inventory

- **Executed**: an isolated executable/PTY, real tmux, generated hook, or shell
  boundary was exercised. Providers and remote endpoints were fakes.
- **Contract**: source inspection plus passing deterministic automated coverage;
  this does not establish compatibility with every installed provider version.
- **Partial**: an implementation exists, but the documented outcome or platform
  coverage is incomplete. The limitation is stated in the row.
- **Gap**: a promised behaviour is absent or contradictory in the implementation.
- **External**: the actual device, account, network, or provider remains untested.

Rows describe capabilities, not one-to-one test counts. Related options are
grouped; internal transport endpoints are listed separately from user features.
macOS/Linux are agent hosts. Native Windows is a client, not a tmux host.

## Board and everyday operation

| ID | Capability / entry point | Validation and boundary |
| --- | --- | --- |
| B01 | Bare `pika`: cached interactive board | **Executed.** Actual Unix executable starts in a disposable PTY and accepts keys. Cache/slow-observer contracts cover first paint. No new fleet-scale startup benchmark. |
| B02 | Redirected `pika`; finite `pika list` | **Executed.** CLI smoke tests; no interactive board on redirected output. `--json`, `--no-usage`, `--all-machines` are supported. |
| B03 | Grouped attention, working, ready and parked rail | **Contract.** Status projection and ordering tests. A historical unresolved error can remain in attention; the label does not prove a present request for input. |
| B04 | Arrow/j/k, page, home/end selection; `/` filtering | **Contract.** Stable exact-identity selection and viewport tests. Real PTY coverage is presently limited to the journeys listed below. |
| B05 | Enter opens selected exact identity | **Executed, repaired.** Real isolated tmux tests prove exact resume/reuse and receipt delivery. Unix open failures now return to a board inspector notice without replaying the action. |
| B06 | Detach back into the same board, selection and filter preserved | **Executed, repaired.** Native handoffs return to the board with remembered UI selection/filter/offset, never cached identity proof. Real PTY test opens, detaches and reopens the same fixture pane. |
| B07 | `p`: preview without leaving the board | **Executed, repaired.** Unix now uses a background action worker and a scrollable inspector, like the Windows action path. A saved conversation without a pane gets an explanation, not a board exit. |
| B08 | `p`: exact pane capture and unread preservation | **Contract + Executed.** Identity checks surround capture; no transcript fallback. New PTY journey proves an unavailable preview does not acknowledge unread work. Actual Windows preview still needs device testing. |
| B09 | `a`: inline private multi-turn consultation | **Contract.** Monitor phase/input/cancellation tests and fake provider multi-turn tests. No live model turn was purchased in this audit. |
| B10 | `A`: fast inline consultation | **Gap.** Documented in DESIGN, but no board key handler. CLI `ask --fast` exists and has separate policy tests. |
| B11 | `x`: stop watching, with confirmation/cancel | **Executed, repaired for Unix inline use.** PTY tests prove cancellation leaves the row watched and confirmation unwatches without closing the board. Tombstone contracts prevent hook-driven reappearance. |
| B12 | `n` / `pika next`: oldest attention item | **Contract.** Shared local/fleet ordering. Same Unix open/return limitation as Enter. |
| B13 | `r`: refresh; cached rows cannot silently act | **Contract, repaired feedback.** Stale Enter/peek/ask/unwatch now explains that refresh is required. It sends no action. Background failures preserve last-good state. |
| B14 | `?`: keyboard help | **Executed, repaired.** Previously absent despite documentation; now scrollable and dismissible in the real board. |
| B15 | `u`: secondary cumulative usage view | **Executed, repaired.** Previously absent. Totals moved out of the primary inspector; accounting is explicitly not context-window size or a subscription bill. |
| B16 | `U`: update offer, explicit Y/N, resume updated board | **Contract.** Native/Windows policy and updater tests; no installed-command cutover performed. Unix inline actions retain native updater behaviour. |
| B17 | `q`, Escape, Ctrl-C, terminal restoration | **Executed/Contract.** PTY leave/escape and terminal-signal tests. Escape dismisses focused panels/filters; it is not universally a quit key. |
| B18 | Inspector: context, state-specific reason, identity details | **Contract, improved (unreleased).** Default briefing keeps metadata secondary; `d` exposes exact identity, full path/model/branch and recorded pane. It is not the full `explain --json` evidence view. |
| B19 | Inspector: expert scope/current work/topics | **Partial, improved (unreleased).** Bounded readable excerpts retain card age; `d` exposes the full card. Remote board annotations retain less structure than the expert-search directory. |
| B20 | Inspector: automatic selected live pane tail | **Gap.** No periodic selected-pane capture worker. Explicit `p` is implemented; automatic preview described in DESIGN/guide is not. |
| B21 | Inspector: useful result/current-work summary without a card | **Gap.** Lifecycle words such as `completed` cannot explain the work. No independent live-work/result summary fallback exists. The board now says when no summary is recorded. |
| B22 | Return-visit / since-last-visit briefing | **Gap.** Event history exists, but the documented visit-watermark briefing is not wired into the monitor. |
| B23 | Five-minute playbook tips | **Partial.** Timer-free rotation exists, but selects generic tips by time rather than current operational context. |
| B24 | Narrow terminal and mouse-wheel support | **Partial/Gap.** Consultation can use full width; ordinary selected detail disappears below the split threshold rather than becoming the documented detail band. Mouse events are not handled. |
| B25 | No-flicker rendering, resize and sanitization | **Contract.** Buffered synchronized frames, dirty-frame logic, resize and hostile-text tests. Surface flicker is not live-verified by these Mac tests. |
| B26 | `NO_COLOR` accessibility | **Contract, repaired (unreleased).** Whole-frame colour filtering preserves selection, emphasis, cursor movement and Unicode. |
| B27 | Account quota footer | **Contract.** Codex RPC and Claude status-line/cache parsing, expiry, reset and layout fixtures. Viewing-machine scope only; missing data is unavailable, not borrowed from another machine. |
| B28 | Cost of background observation | **Partial.** Cached-first board and bounded/single-flight workers exist. Full local discovery currently has a 20-second fallback plus store notifications; usage worker wakes every two seconds even when usage is hidden. DESIGN's visible-only 30-second accounting is not implemented. |

## Find, open and protect a conversation

| ID | Capability / entry point | Validation and boundary |
| --- | --- | --- |
| O01 | `pika NAME` / `pika open NAME` | **Contract + Executed.** Name resolver and CLI tests; real isolated tmux resume/reuse. `open` disambiguates names that are also commands. |
| O02 | New name creates, names and tracks a conversation | **Contract.** Launch reservation, pending identity and provider-event binding tests. Actual first-run naming behaviour of current provider releases is not live-tested. |
| O03 | `pika .` / `pika -` | **Contract.** Current-project and previous-open resolution; exact identity remains the action target. |
| O04 | Provider-qualified name, UUID, name@machine | **Contract.** Resolution, ambiguity and immutable remote target tests. No fallback from a rejected UUID to a similarly named conversation. |
| O05 | Same-name chooser / same-project recency / near matches | **Contract.** Cross-provider choices, safe cancellation, independent fork distinction and close-match protection. Not based solely on a title or UUID prefix. |
| O06 | Resume versus reuse versus already live elsewhere | **Executed/Contract.** Real isolated provider stand-ins cover reuse; duplicate/ownership tests cover refusal. No attempt to move an arbitrary live process into tmux. |
| O07 | Exact identity, PID reuse, shared app-server leases | **Contract + Executed.** UUID-bearing process evidence, pane identity, expiry and genuine-duplicate tests; real tmux framing now works in a C-locale client. |
| O08 | Recovery and continuity receipts | **Executed.** Receipt commit/attachment ordering exercised through real isolated tmux. Numeric process existence alone cannot earn a receipt. |
| O09 | Automatic recovery after stale observations | **Contract, repaired.** Foreground operations, exact fleet lookups and fleet snapshots retry only the typed discarded-observation conflict, with at most three fresh scans. Identity failures and closed-board fences are not retried; no attach/model delivery is repeated. |
| O10 | Persisted provider-native renames and independent forks | **Contract.** Discovery tests cover renamed roots/leaves and distinct independent forks. Card identity is not guessed from shared titles. |
| O11 | Native terminal colours, input and scrollback | **Contract/External.** Terminal bridge, palette, DA-response and signal tests exist. Exhaustive native Codex/Claude/OpenCode visual parity, scroll gestures and Surface keyboard behaviour remain live-platform work. |
| O12 | `peek NAME --lines N --ack` | **Contract.** Bounded/sanitized capture, event-specific acknowledgment and unread preservation. Human terminal peek may acknowledge; redirected output does not unless explicitly requested. Board `p` preserves unread. |
| O13 | `wait NAME --for ... --timeout ... --json` | **Contract.** Finite state/wait behaviour; accepts any, needs-you, ready and error. Not a general remote orchestration daemon. |
| O14 | `untrack NAME` and explicit restoration | **Executed/Contract.** Watch tombstones, exact tag clearing, no agent termination/archive; explicit open restores eligibility. |
| O15 | Compatibility `new`, `adopt`, `recover-closed` | **Contract.** Hidden command surfaces still parse. They are compatibility paths, not prerequisites for everyday `pika NAME`. |

## Lifecycle, attention and diagnostics

| ID | Capability / entry point | Validation and boundary |
| --- | --- | --- |
| S01 | Codex, Claude and OpenCode hooks | **Contract; OpenCode adapter executed.** 24 hook contracts plus generated JavaScript execution with fake host APIs. Installed integrations were not rewritten. |
| S02 | Structured question/permission attention | **Contract.** Pre-question/pre-permission transitions and matching completion events. No live provider UI was used to prove a current thread's question state. |
| S03 | Completion/unread; working/parked; errors/OPEN TWICE | **Contract.** Separate lifecycle/runtime/safety projections and precedence. Old events alone do not establish what a real agent is doing now. |
| S04 | Automation worker exclusion and descendant grouping | **Contract.** Provenance, archived/untracked and root/child fixtures. Unsupported/missing provenance is not silently treated as proof of automation. |
| S05 | Event deduplication, stale owners and process exits | **Contract.** Store, hooks, ownership and runtime event tests. UUID and process generation checks retained. |
| S06 | `explain NAME --json` | **Contract.** Diagnostic evidence/reason surface exists separately from the board's shorter inspector. |
| S07 | `activity --limit N --json` | **Contract.** Transcript-free event history. It is not the missing return-visit briefing. |
| S08 | `doctor --json --verbose --repair-stale` | **Contract.** Scoped certificate and conservative repairs; no full live fleet recovery certificate issued in this audit. |
| S09 | OpenCode error messages | **Executed, repaired.** Generated hook handles string/message/nested data.message and safe fallback. Unknown objects no longer become `[object Object]`; arbitrary object fields are not dumped. Existing opaque records cannot recover their original message. |
| S10 | Concurrent reconciliation | **Contract, repaired foreground path.** Real SQLite second-connection conflict test proves fresh re-observation. Background board retains its fence; other one-shot internal callers still exist and need separate conflict UX review. |

## Expert discovery, consultation and the bundled skill

| ID | Capability / entry point | Validation and boundary |
| --- | --- | --- |
| E01 | `experts QUERY --json --with-notices` | **Contract.** Exact identity, relevance ranking, freshness and omission notices over local/trusted-remote card metadata. |
| E02 | `expert publish` scope/now/topics/artifacts | **Contract.** Provider-bound durable expertise/current-work schema and input validation. Publishing is not automatic evidence that the card represents the entire project correctly. |
| E03 | `expert update`, `clear`, `status` | **Contract.** Exact card state and source availability tests. Clearing a card is separate from untracking a conversation. |
| E04 | Card freshness and continuation identity | **Partial.** Card state exists, but the local board mainly shows an update age rather than the documented `+NEW CONTEXT` distinction. A card on a different UUID is not automatically inherited. |
| E05 | `expert refresh NAME`, `--all`, `--provider`, `--due` | **Contract.** Explicit/manual and guarded refresh fixtures. No actual interviews run during this audit. |
| E06 | Quota-aware scheduled refresh | **Contract.** Due policy requires fresh telemetry, final six hours before weekly reset and more than 10% remaining. Unknown/exhausted telemetry skips model calls; scheduler templates tested, no real scheduled service activated. |
| E07 | `ask NAME QUESTION --json / --jsonl` | **Contract.** Delivery receipts, stdin follow-ups and finite/streaming outputs. Actual installed-provider compatibility remains External. |
| E08 | Codex private consultation | **Contract.** Fake RPC proves one confirmed ephemeral child, pinned model/effort, multi-turn continuity and no parent-ID delivery. |
| E09 | Claude private consultation | **Contract.** Fake process proves one nonpersistent tool-less multi-turn side. No live account tested. |
| E10 | OpenCode private consultation | **Contract.** Fake HTTP provider proves exact read-only child and verified deletion; failed cleanup preserves the answer with an explicit caveat. |
| E11 | `ask --fast` | **Contract.** Codex fast profile policy is pinned; unsupported provider profiles reject before launch. This does not implement missing board `A`. |
| E12 | Cancellation / timeout / uncertain delivery | **Contract.** Owned-child cleanup, bounded output, no automatic re-send, and unknown-delivery handling. Zero user model quota used. |
| E13 | Remote expert search and private multi-turn ask | **Contract.** Five expert-federation and associated fleet/protocol tests. No new live cross-server model consultation in this audit. |
| E14 | `skill show`, `skill install [PATH] --json` | **Executed/Contract.** Embedded content, safe installation and bootstrap fixtures. Ordinary native updates deliberately do not rewrite installed skills/hooks. |

## Setup, machines and Windows

| ID | Capability / entry point | Validation and boundary |
| --- | --- | --- |
| F01 | `setup` preview, approval, backups and idempotence | **Contract.** Provider options, aliases, dry-run/yes and configuration preservation. No live setup applied. |
| F02 | Personally named first; optional recent unnamed screen | **Contract.** `--import-all`, `--browse-all`, `--no-import`, archived/automation/untracked exclusions; bounded provider discovery fixtures. |
| F03 | Setup rename reconciliation without agent interviews | **Contract.** Name updates and deferred cards are separate. Actual current-provider rename provenance still needs periodic compatibility verification. |
| F04 | Installed versus observed integrations | **Contract.** Commissioning checks are distinct; Codex hook trust remains a user action. A green historical observation is not a promise that every future event will arrive. |
| F05 | SSH-config / Tailscale passive discovery | **Contract/Partial.** Discovery is passive until selected, and SSH aliases are available. The `all` journey can still select many irrelevant or inaccessible candidates. |
| F06 | `machines list/discover/add/remove/ignore` | **Contract.** Exact node binding, explicit trust and persisted choice. Alias `machine` also parses. |
| F07 | `sync MACHINE`, bounded background inventory | **Contract.** Versioned snapshots, fair single-flight refresh, cached-offline labels, partial failure and identity rejection. No transcript copied to coordinator cache. |
| F08 | Exact remote open/peek/untrack | **Contract.** Node/provider/conversation identity and acknowledgments validated. No name-based fallback after a routing rejection. |
| F09 | `setup --machine`, `--install-bundle`, remote adoption; `machines upgrade` | **Contract/Partial.** Explicit version-pinned native bundle paths tested. Selecting a host is not an unconditional installation path when no usable bundle/runtime is present. |
| F10 | SSH alias RemoteCommand and noninteractive PATH | **Executed, repaired.** Fake SSH rejects the original conflicting call; corrected call uses `RemoteCommand=none` and safely resolves Pika via PATH or `$HOME/.local/bin/pika`. Applied to fleet, pairing and new-window commands. |
| F11 | Unknown/changed host keys, auth and offline hosts | **External/Partial UX.** No host-key verification bypass. Actual keys/accounts/connectivity not changed; contextual recovery steps for mass-selection failures remain incomplete. |
| F12 | Windows first-use pairing and combined local fleet | **Contract.** Mac-hosted Windows runtime fakes prove multi-selection and retained pairings; not a real Windows run. |
| F13 | Windows Enter opens an exact Windows Terminal session | **Contract/External.** Locally constructed argument and identity tests pass. Actual PowerShell/OpenSSH/wt quoting, process launch and attach require Surface validation. |
| F14 | Windows inline preview/error/untrack | **Contract/External.** Action-driver tests cover worker behaviour. Device interaction and flicker remain unverified. |
| F15 | Windows `setup`, `status`, `update`, `bridge` | **Contract.** Windows CLI is a smaller surface; it does not expose all host CLI commands or host local agents. |
| F16 | Optional bridge/reverse-forward remote-console opening | **Contract.** Loopback scope, pairing secret, exact-node requests, deduplication and bounded outcomes. Normal Windows board opening does not require this bridge. |
| F17 | Windows local quota | **Partial.** Without local provider telemetry the footer cannot show an allowance; it intentionally does not substitute an arbitrary remote account. |

## Installation, updates and operational integrity

| ID | Capability / entry point | Validation and boundary |
| --- | --- | --- |
| R01 | Versionless Unix installer, native platform bundle | **Executed/Contract.** Offline generated bundles and real shell bootstrap tests; no public release downloaded/installed for this audit. |
| R02 | Windows PowerShell installer | **Contract/External.** Manifest, target, checksum and packaging checks on Mac; actual Windows activation requires its native runner/device. |
| R03 | `update --check`, latest/channel selection, exact `--release` | **Executed/Contract.** Fake release metadata, CLI and managed-install fixtures; no public update issued. |
| R04 | Offline `update --bundle` | **Executed.** Generated release archive through public native CLI in a disposable installation. |
| R05 | Automatic update checker and explicit approval | **Contract.** Separate cache, lease, six-hour success/one-hour failure cache, decline/default-No and `PIKA_UPDATE_CHECK=0`. No silent update of agents or other machines. |
| R06 | Rollback, atomic activation and retained releases | **Executed/Contract.** Receipt/archive/launcher ownership validation and prior-release activation fixtures. |
| R07 | Tamper, oversized artifacts, symlinks and interrupted install | **Contract.** Bounded downloads/extraction, path/ownership rejection and exact owned-child signal cleanup. |
| R08 | Skill on initial install; hooks after approved setup | **Executed/Contract.** Bootstrap and setup are deliberately separate; updates alone do not refresh those integrations. The OpenCode hook repair therefore needs approved setup after deployment. |
| R09 | SQLite schema, event ledger, locks and tombstones | **Contract.** Store migration, transactional updates, observation fences and untracking contracts. No user database rewritten by tests. |
| R10 | Third-party notices and release packaging | **Contract.** Notice consistency and distribution safety suites pass. No dependency changes, new release artifacts, release security audit or cross-platform publication in this turn. |

## Internal surfaces (not additional advertised features)

The executable also contains `hook`, `_process-exit`, `_terminal-bridge`,
`_install-native`, `_fleet`, `_fleet-open`, `_fleet-ask`, `_peek-popup`,
`_client-pair`, `_client-board`, `_client-fleet-open`, `_enter`, and
`_claude-statusline`. Their validations belong to the hook, terminal, installer,
fleet, client and quota contract suites above. They are not arbitrary remote
execution APIs, and should not be taught as everyday commands.

## Evidence from this pass

All executable tests used disposable HOME/XDG/Pika/provider/database/temp paths
and isolated tmux sockets. Default SSH/providers/download/service commands were
blocked or replaced with fixtures. The limited read-only deployment check was
separate from tests: installed version and selected registry/card metadata only;
no transcript content or model consultation was requested.

- 34 native public/compatibility command and subcommand help paths executed.
- **532 distinct Rust test cases have passing evidence** across the baseline
  and affected-suite reruns. This includes helper tests; it is not 532 user
  journeys or proof of 532 features. Unchanged evidence was reused.
- Three new executable PTY journeys: saved-row Peek/Escape/quit without
  acknowledgment; help/usage/Escape/quit; cancel/confirm unwatch while staying
  on the board. The first two reproduced failures before repair.
- Real isolated tmux: all three provider stand-ins resume/reuse exact panes;
  current-client/pane proof and post-proof receipt delivery pass.
- Fake SSH shell boundary: configured RemoteCommand conflict and a remote
  binary installed outside default noninteractive PATH reproduced and fixed.
- Actual Node execution of the generated OpenCode plugin: error forms and
  unknown-object redaction verified without provider calls.
- Typed reconciliation retry: a second SQLite connection commits during the
  first observation; a fresh observation succeeds. Retry bounds and nonretryable
  safety errors are separately checked.
- Formatting and warnings-as-errors Clippy checked. No dependency changes.

Initial failures also exposed harness problems: inherited fixture DB/update
settings, overlong Unix socket paths, and an assertion comparing terminal ACS
rendering to literal UTF-8. Those are distinguished from the actual tmux
control-separator compatibility defect. The output-flood test now isolates its
byte-limit check from scheduler speed using a test-only longer deadline; the
production deadline and separate timeout/owned-child tests remain unchanged.

The audit did **not** perform real Windows interaction, paid provider turns,
fleet-wide installs, live migration, release publication, or exhaustive
long-session visual/performance tests. Automated evidence must not be reported
as those outcomes.

## Opening-journey follow-up

The next pass closed the ordinary Unix board handoff gap. Both bare and legacy
bridged boards return after a native open, cancellation or failure. Selection,
filter and viewport survive; all session evidence is loaded again. An uncertain
launch produces an in-panel notice and is never automatically repeated.

Two additional executable PTY journeys pass: blocked open returns to a filtered
board; real isolated tmux opens, detaches and reopens the same fake-provider pane.
A new UI-state test proves restored selection remains node-qualified while the
new snapshot's status/staleness wins. Fleet snapshot and exact-lookup paths now
use the same bounded fresh-observation recovery as foreground opens. These
checks validate the candidate, not any particular installed machine.

## Repair acceptance criteria still open

1. **Extend handoff acceptance.** Open/blocked-open/detach now return to the
   board in executable tests. Still validate real-provider slow startup,
   cancellation and remote failure recovery on supported platforms.
2. **Make the inspector useful.** A selected row should show an attributable
   current-work/result summary or an explicit absence, with freshness and exact
   source identity. Add bounded selected local-pane preview without unread
   mutation. Do not copy another UUID's card or interview all agents on startup.
3. **Prove attention, not just labels.** Exercise working → question/permission
   → answer → working → result → collected, runtime exit and genuine duplicate
   sequences against supported provider versions. Distinguish old unresolved
   failures from current input requests. A live PID is insufficient.
4. **Close remaining board promises.** Implement or explicitly defer fast `A`,
   narrow detail, visit briefing, context-sensitive playbook, global NO_COLOR,
   mouse-wheel handling and visible-only accounting. Resolve the conflicting
   DESIGN/guide/README claims rather than marking them complete through tests.
5. **Verify Windows as Windows.** Test selected SSH aliases with RemoteCommand,
   untrusted keys, missing remote installation, repeated pairing, direct open,
   peek, close and update on Surface. Verify first-run `all` is understandable
   and bounded; never solve host-key failures by disabling verification.

Before deployment, run the applicable release gates for the exact candidate.
Publishing, local installation, remote upgrades and approved hook rewrites are
separate decisions. Until those occur, installed Pika retains the reported bugs.
