# Pika Native — phased Rust migration PRD

Status: native implementation complete through the local CP7.1 candidate gate;
publication, live cutover, and retirement remain separately authorized operations.
Owner: Ayush Jain. Prepared: 2026-09-11.
Baseline: public Python v0.5.0a4, commit `36de1b1eed1a182ed4e608d47f95d362f15d571a`.
Repository inspection: `c63d026792b9ce13f349c949e9143bb859fb6d17` (subsequent video-only update).

This is a private working document in a separate local repository, not public
README copy or a public roadmap. No remote is configured for this repository.
Approval of this PRD authorizes only the scope expressly approved by the owner;
writing it does not authorize a rewrite, live migration, release, or remote upgrade.

## 1. Decision and product promise

Rebuild Pika in Rust through small, independently verified behaviour checkpoints.
Preserve the product, not the Python module structure. Remove unnecessary runtime
and execution layers while keeping the hard-won identity, attention, consultation,
and recovery guarantees.

The user still types `pika`, selects a conversation, or runs `pika NAME`.
Their agents still discover experts and privately consult the relevant project.
The user should not need to learn Rust, re-adopt conversations, recreate cards,
repair tmux, choose implementation backends, or coordinate a fleet-wide upgrade.

The target is one native Pika executable per supported target, with the skill
embedded and provider-owned hook assets emitted only where needed. Fresh native
installs require no Python, uv, Node, or Rust toolchain for Pika itself. Agent
CLIs, tmux, and SSH retain their existing roles and prerequisites.

This is not a project to accelerate model inference, replace agent harnesses,
build a terminal emulator, or promise that Rust eliminates semantic bugs.

### Success in one sentence

The same Pika workflows work with the same existing conversations and metadata,
but installation is smaller, commands are cheaper, and the implementation has
fewer moving parts and less duplicated work.

## 2. Users and important journeys

| User/job | Required outcome | First believable success |
| --- | --- | --- |
| Developer or researcher with several coding agents | Find who needs input and return to the right conversation | `pika`, select, Enter: exact native agent UI opens |
| First-time single-machine user | Install without environment management | Reviewed setup, one conversation visible, open/detach/return works |
| Calling agent | Find relevant prior expertise and consult it | Exact expert identity and evidence-backed answer; parent keeps working |
| Laptop user with remote machines | Manage approved machines from one board | Local board responds even with an offline node; exact remote attach works |
| Existing Pika user | Upgrade without losing work or settings | Same names, cards, unread state, machine trust, and running agents |

The board and return-by-name work before expert cards are configured. Cards are
the next step for agent-to-agent consultation, not an onboarding prerequisite.
Routine setup must not interview all agents. Avoid adding daily commands.

## 3. Scope, exclusions, and simplification authority

### Must survive the migration

- Local hosting for Codex, Claude Code, and OpenCode on macOS and Linux.
- Exact open/create/resume, naming, setup discovery, and current collision policy.
- Needs You / Working / Ready / Parked board grouping, truthful row reasons,
  stable selection, read-only previews, inline side conversations, and recovery.
- Expert discovery, durable scope versus current work, publishing, freshness,
  quota-aware refresh, model/effort policy, and bundled agent-convo skill.
- Trusted SSH federation, remote consultation, stale-node behaviour, and exact
  node/provider/conversation routing. No deployment hostname is special.
- Scriptable CLI/JSON/JSONL contracts, hook callbacks, tmux callbacks, doctor,
  explain, activity, usage, acknowledgements, and untracking tombstones.
- Approved update checks, installation recovery, configuration preservation,
  and existing Windows client/bridge behaviour at its experimental maturity.

### Not part of this port

- Native Windows agent hosting, replacing tmux, moving live processes between
  terminals, transporting provider transcripts, or cross-host conversation migration.
- A hosted service, new telemetry service, automatic agent delegation, new model
  routing policy, background model calls, or a new public plugin framework.
- A new TUI product design, new public statuses, extra configuration modes,
  automatic remote installation, or automatic unapproved package activation.
- Supporting every historical Python release indefinitely.
- Deleting user histories, cards, configuration, unrelated source, or campaign assets.

### Delete / retain / decide ledger

| Surface | Decision | Evidence required before removal |
| --- | --- | --- |
| Python/uv bootstrap for a fresh native installation | Remove | Public clean-host install and update tests pass |
| Python process launches inside the new native Pika runtime | Remove | Process trace shows none on supported native paths |
| Provider-required JavaScript hook plugin | Retain minimal emitter | OpenCode consumes it; it must invoke native Pika, not duplicate policy |
| Multiple implementations of state precedence / identity routing | Consolidate | One rule implementation serves CLI, board, hooks, and fleet |
| Repeated whole-inventory scans in one refresh | Measure, then reduce | Counters demonstrate redundancy; fresh pre-action proof remains |
| Pass-through wrappers and speculative extension layers | Remove or avoid | Caller inventory shows no invariant or real variation served |
| Public legacy aliases and hidden callbacks | Retain cheaply at CLI boundary | Remove only after caller/config evidence and explicit approval |
| SQLite tables, leases, locks, tombstones, event watermark | Retain initially | Named consumers and real failure cases exist; no bulk schema redesign |
| Old Python runtime referenced by a still-running agent or scheduler | Temporarily retain | References, live process ownership, and rollback obligations have ended |
| Embedded authoring/review/performance machinery in runtime | Do not build | Verification tools belong in development/CI only |

Less code is a useful consequence, not a licence to remove safety. Every proposed
behaviour deletion needs a named consumer assessment and owner-approved change
record. Internal removals with preserved behaviour need evidence, not a new user
question for each function. Do not carry broken legacy behaviour forward merely
to achieve byte-for-byte parity: classify it separately and approve the correction.

## 4. Evidence baseline and measurable acceptance

The inspected runtime has 25,361 Python lines across 33 modules. This is a scale
indicator, not a Rust line-count target. The v0.5.0a4 release was verified with
764 tests passing, four skipped, and 36 passing subtests locally; release CI
passed on Linux Python 3.10/3.13, macOS, and the Windows client smoke job.
Existing tests are useful evidence, not proof of complete behavioural coverage.

The executed same-machine comparison and exact fixture boundaries are recorded in
`docs/PERFORMANCE.md`. The native candidate now has measured startup, cached-board,
input, hook, reconciliation, five-minute CPU/RSS, and artifact evidence against the
frozen Python reference; provider inference speed remains deliberately excluded.

### Release gates

These budgets were fixed before implementation. Achieved local macOS-arm64 results
are recorded separately in `docs/PERFORMANCE.md`; cross-platform publication still
depends on the protected CI/release workflow and a separately authorized release.

| ID | Metric | Proposed gate | Measurement boundary |
| --- | --- | --- | --- |
| N01 | Pika runtime prerequisites | No interpreter, package manager, or Rust compiler | Fresh supported host; agent tools remain prerequisites |
| N02 | Native artifact / active footprint | Compressed artifact <= 20 MiB; active Pika installation <= 50 MiB | Binary, skill, native libraries/resources; exclude provider tools, user state, and separately reported retained rollback runtimes |
| N03 | Command startup | `--version` p95 <= 30 ms | Release binary, process spawn to exit, 100 warm-cache samples; disk-cold runs reported separately |
| N04 | First usable cached board | p95 <= 200 ms and no regression versus pinned Python | 200 cached rows, 150x35 PTY, offline fleet member; includes spawn and complete usable frame |
| N05 | Input responsiveness | p95 <= 50 ms, p99 <= 100 ms | 1,000 key actions while fake provider/SSH operations stall; no dropped/rebound selection |
| N06 | Warm board memory | <= 40 MiB RSS and >= 30% below Python when Python exceeds 40 MiB | Same 200-row fixture and bounded expert/preview data after 60 s; count Pika-owned workers too |
| N07 | Idle board CPU | <= 1% of one core, averaged over 5 min | Same fixture after warm-up; report process tree and machine contention |
| N08 | Hook fast path | p95 <= 75 ms uncontended, no model or SSH calls | 100 fixture events, includes spawn, validation, durable commit; lock contention tested separately |
| N09 | Local reconciliation | p95 <= 500 ms for 200 fixture rows; no work overlap | Count DB operations, process enumeration, provider reads, and tmux spawns; fixture not live-provider SLA |
| N10 | Fleet responsiveness | Stalled node adds <= 20 ms to local key latency | Fault-injected transport; freshness retains current 45 s semantics |
| N11 | Zero silent behavioural regressions | All mandatory contract cases pass | Golden outputs plus side effects, error certainty, cleanup, and identity |
| N12 | Runtime simplicity | One authoritative policy path; no Python calls in native workflow; bounded workers/caches | Architecture review and process/resource traces |

Use 30 independent launches for board startup, five independent steady-state
runs, and 100 CLI/hook launches. Report p50/p95/max and sample counts; input tests
also report p99. Alternate Python/Rust order on the same machine, record hardware,
OS, terminal, build flags, dependency lockfile, fixture seed, and source SHA.
Measure macOS arm64 and Linux x86_64 on dedicated reference environments; timing
on shared CI is diagnostic, not a hardware-independent pass/fail oracle.

Scaling fixtures: empty inventory; 20 rows; 200 rows; 2,000 rows; 1/5/20 remote
nodes with slow/offline members; oversized cards and bursty hooks. At 2,000 rows,
first frame must remain <= 1 s and input p95 <= 100 ms; memory must plateau under
the documented bounded retention policy. The 20-node fixture need not make all
snapshots fresh: it must remain fair, bounded, responsive, and truthful.

Measure consultation stages separately: local prepare, provider startup/fork,
turn submission, first output, completion, cleanup. No inference-speed improvement
is promised. Compare simulated provider traces for overhead and authorized live
consultations for compatibility; do not subtract unmatched live model runs to
claim a language speedup. No quota is consumed by baseline or CI fixtures.

## 5. Functional requirements and acceptance contracts

| ID | Requirement | Observable acceptance |
| --- | --- | --- |
| F01 | `pika` remains the daily entrypoint | Interactive board; redirected bare command is finite; help/version never scan providers or connect remotely |
| F02 | Exact return by name or board | Resolve using current policy, then bind action to immutable node/provider/UUID; never follow a renamed selection to a different conversation |
| F03 | Safe creation and resume | New names create and register exactly once; pending launches appear immediately; archived/deleted candidates stay excluded |
| F04 | Honest ambiguity | Same-name cross-provider/directory/concurrency choices preserve current policy and safe cancellation; no global newest-title heuristic |
| F05 | Naming and discovery | Explicitly named choices precede bounded recent unnamed choices; provenance, not title prefix, classifies workers; rename/fork preserves distinct identities |
| F06 | Exact ownership | UUID evidence and process-generation evidence; expired shared-server leases can recover; genuine independent UUID owners remain OPEN TWICE |
| F07 | Truthful attention | Structured questions request input; matching answer clears wait; newer work supersedes stale completion; safety wins over lifecycle; late events cannot erase stronger evidence |
| F08 | Stable board | Four groups; cached/recovery reasons stay visible; sorting/refresh cannot change selected action target; narrow/empty/error layouts retain recovery and quit |
| F09 | Faithful terminal handoff | Preserve provider colours, user configuration, scrollback behaviour, key input, resize, Unicode, and raw-mode restoration across attach/detach/error |
| F10 | Peek and acknowledgement | Automatic/redirected inspection preserves unread; explicit acknowledgement binds atomically to the intended event and exact identity |
| F11 | Stop watching is not stop working | Untrack preserves process, history, and expert card; tombstone prevents hooks resurrecting observation; explicit restoration works |
| F12 | Expert directory | Search returns exact local/federated identity, source/freshness, durable mandate and current work; card search makes no model call |
| F13 | Consultations | Keep one separate multi-turn side per consultation; immutable model policy; caller gets answer; original remains working and receives no side prompt |
| F14 | Provider-specific isolation | Preserve Codex ephemeral fork, Claude non-persistent fork with tools disabled, and OpenCode temporary persisted side/read-only policy and verified cleanup |
| F15 | Honest failure receipts | Distinguish fork/turn/response/discard stages, unknown delivery, partial answer, and failed cleanup; no blind resend or invented parent-byte guarantee |
| F16 | Inline asks | Stay inside board; cancellation remains responsive during startup/turn; parent is never terminated as cleanup |
| F17 | Quota-aware expertise upkeep | Preserve reset-aware eligibility, current thresholds/model selection and bounded interviews; unknown quota fails to a skip, not speculative spend |
| F18 | Trust-preserving federation | Passive candidates do not cause SSH; approved nodes only; strict node IDs and capability checks; local inventory never synchronously waits on fleet |
| F19 | Protocol compatibility | Preserve CLI exit semantics and JSON/JSONL schemas for callers, installed skill, hook/plugin emitters, callbacks, and supported mixed-version nodes |
| F20 | Setup and installation | Preview, approval, backups, idempotence, skill inclusion, no interview by default; no personal paths or provider credentials in artifacts |
| F21 | Upgrades | Existing v0.5.0a4 user can reach native release through existing update/install surfaces; no manual re-adoption, DB export/import, or name cleanup |
| F22 | Existing Windows bridge | Preserve supported pairing and exact launch behaviour at experimental maturity; do not claim native hosting or pairing completeness from `--help` alone |
| F23 | Diagnostics and usage | Doctor/explain/activity remain evidence-based, sanitized and scoped; retain unknown values and dated cost estimates; no transcript indexing for ordinary status |
| F24 | Generic product | No internal host aliases, fixed deployment user, personal email, or workstation path embedded in functionality, tests, or published artifacts; official project distribution URLs and license attribution remain explicit |

Current provider policy and event mapping are frozen as fixtures in Phase 0, not
re-invented from this summary. Semantic status precedence is owned by one pure
projection; a visible group is not a replacement for underlying safety evidence.

## 6. Architecture constraints

### Native structure

Start with one Cargo package, one `pika` binary and a library target for tests.
Use cohesive modules for CLI, identities/state, provider I/O, store, terminal,
board, consultation, fleet, install/setup. Do not mirror every Python file or
create a crate for each module. Keep end-to-end operations readable from their
entrypoint. Introduce traits only at actual OS/provider/storage/transport seams
or where deterministic testing needs them.

Use a small, pinned dependency set. Candidate roles include clap for CLI,
serde/serde_json for interchange, rusqlite with bundled SQLite for storage,
Ratatui/Crossterm for the board, and an async runtime only if it simplifies the
measured I/O workflow. Select exact versions/MSRV in Phase 0. Do not combine
several runtimes, HTTP stacks, terminal stacks, or logging systems casually.
Review transitive dependencies, license notices, binary size and target support.
macOS/Linux-specific process inspection must preserve argv boundaries, ownership
and generation precision; a lossy `ps` parser is not an acceptable simplification.

### Execution model

- One input/render loop; blocking filesystem/process/network work cannot occupy it.
- One local reconciliation in flight. Reuse its discovery snapshot, then obtain
  fresh identity immediately before an attach, capture, acknowledgement, or signal.
- Preserve the current bounded, fair single-flight fleet scheduler initially.
  Change fan-out only with a separate measured checkpoint and explicit bound.
- Distinct cancellation/deadlines for selected preview, consultation, usage and
  update checks; bound pending messages, stderr, retained pane output and side text.
- Child cleanup is scoped to processes created by the operation. Revalidate
  generation before signaling; cancellation does not imply successful provider deletion.
- Graceful shutdown is bounded and leaves native provider/tmux processes alive.
  No task waits indefinitely on a blocked pipe. No persistent server daemon added.
- Reuse SSH/tmux executables and established provider protocols. Reimplementing
  their transport or process-persistence machinery is out of scope.

Keep policy in Rust; setup may emit the minimal provider-required JS plugin and
OS service definitions. This is an evidence-backed exception to a single-language
ideal: those files are consumed by existing harnesses/OS schedulers. They do not
justify a second Pika business-logic implementation.

### Source versus specification

Python is the reference for intended behaviour and compatibility, not an oracle
that makes every bug correct. Specification priority: approved user guarantees,
approved correction records, pinned external contracts, then reference behaviour.
Unexplained discrepancies block the checkpoint. A reviewer must distinguish an
intentional correction from accidental drift.

## 7. State, update and mixed-runtime transition

### Persisted state

Keep existing JSON config, XDG/PIKA path overrides, SQLite tables, event ordering,
node IDs, expert cards, leases and tombstones readable. Do not add a second
authoritative registry or import-and-rescan from provider titles. Capture schema
and semantic contracts directly from v0.5.0a4, including nullability, indexes,
transaction boundaries, lock paths, permissions and timestamp units.

The first native release should be schema-compatible. Existing Python callbacks
may continue writing the same database during transition; mixed-writer contention,
deduplication and status ordering therefore need explicit tests. Process start
times use platform-specific representations today: Rust must not reinterpret
Linux ticks as Darwin microseconds or silently change stored units.

No live migration during baseline collection. Use SQLite's consistent backup
mechanism for any authorized snapshot; copying only a live `.db` without its
transaction state is not a backup strategy. Real metadata and transcripts stay
outside source control, PRs and public artifacts. Synthetic fixtures are the default.

If a destructive schema change becomes necessary, stop and amend this PRD:
identify writers, obtain owner approval, define quiescence that does not kill
agents, verify backup/restore, and explain rollback's effect on post-cutover events.
Do not use an old DB restore to silently discard new hook events. Binary rollback
should require no DB restore for the schema-compatible migration.

### Installation compatibility is a first-class checkpoint

The current installer/updater expects a schema-1 `pika-release.json` naming a
universal Python wheel. Publishing only a native asset with a changed manifest
would strand existing users. Remote bootstrap also selects a version-specific
installer. Phase 0 must choose and freeze a transition design before implementation.

Required outcomes, regardless of bridge implementation:

1. The existing README installation command installs the appropriate native build
   on a fresh supported machine, with checksums and no toolchain bootstrap.
2. A v0.5.0a4 managed install using `pika update` can discover and install a supported
   migration path. Old clients must never pick an incompatible manifest and fail
   without an exact supported next command. Prefer a one-approval migration; any
   unavoidable additional step must be explicitly approved, not hidden.
3. A temporary compatibility release/envelope is allowed only for this old-updater
   constraint. Record which old client reads each manifest, which bytes it runs,
   and when the launcher switches. Validate it with the unmodified old executable.
4. Verification subcommands (`--version`, `--help`, skill show) must be side-effect
   free: do not smuggle installation activation into validation calls.
5. Stage and verify exact target/version/hash before atomically switching the
   managed launcher. Partial downloads, unsupported architectures, missing runtime
   symbols, denied writes and concurrent upgrades leave the old launcher usable.
6. Never overwrite an existing released version with different bytes. Do not
   mislabel an OS-specific binary as a universal wheel. Unsupported targets get a
   precise message, not a source build or an unexpected runtime download.
7. New native installs have no Python dependency. Upgraded installs may retain a
   legacy runtime for old callbacks and rollback; report its disk cost separately.
   The new native runtime must not use it for normal operation.

The manifest/bridge design is an implementation discovery decision, not an
owner-owned product question. CP0.3 cannot pass until the old-to-new update path
is executable in an isolated fixture. Do not announce native availability early.

### Hooks, wrappers, and schedulers

Current hook/plugin commands, tmux wrapper/exit callbacks, systemd and launchd
jobs can embed the old Python executable. Enumerate all of them, including
already-running processes whose command definitions are cached.

New setup emits a stable native executable path with structured arguments and
correct quoting. Preview and back up configuration edits. Declining setup edits
must not break existing callbacks. Retain old referenced runtimes until consumers
exit or reload normally; never restart an expert simply to complete the port.

Maintain one-time compatibility at the boundary rather than duplicating policy
throughout the runtime. Before retiring Python distributions, prove that supported
old nodes can still attach/ask and that old callbacks remain valid for their
documented transition window. Existing personal development checkouts are not
silently converted into managed installations.

### Mixed fleet and experimental clients

Test Python coordinator → Rust node and Rust coordinator → Python node, as well
as homogeneous pairs. Pin protocol 2, its capabilities and strict envelope limits
from `fleet.py`; additive fields are accepted only where old parsers allow them.
The v0.5 acknowledgement wire has no selected-event timestamp. Native v0.6 must
therefore reject acknowledgement in both mixed-version directions rather than
weaken F10's event binding; hello, snapshot, peek and untrack remain supported
during that transition, and the incompatibility must be explicit in the pairing
matrix and receipts.

Inspection found `client_cli.py` declares fleet version 1 while `fleet.py` declares
version 2. Treat this as an unresolved compatibility discrepancy requiring a
reproduction, not a claim that Windows currently works or a request to weaken
handshakes. Correct it in an explicit checkpoint if confirmed; preserve strict
node identity and document the actually supported pairing matrix.

## 8. Helix-style execution contract

This is inspired by Shopify's published process, not an integration with a
released Helix package. Work is rebuilt in small behaviour slices with evidence
and review before advancing. Visual parity, two adversarial reviewers and human
approval are described in [Shopify's Helix account](https://shopify.engineering/back-to-native).
Their [Shop migration account](https://shopify.engineering/shop-app-migration)
also describes plan-hash approval and structured live-state/event comparison.

### Checkpoint loop

`observe → freeze acceptance → implement → compare → two reviews → approval → commit`

1. Observe one existing journey and its reachable failures. Identify what can be
   removed before specifying or porting it. No whole-file translation assignments.
2. Write a small checkpoint plan: allowed files, scope, invariants, fixture IDs,
   intended differences, benchmark effects, rollback and estimated cost.
3. Canonicalize that plan and hash it. Record owner acceptance of that exact hash.
   A changed scope, invariant or acceptance test invalidates acceptance. Routine
   implementation repairs inside the plan do not require re-approving the plan.
4. Implement in the separate `pikamux-rust` repository, using disposable test
   state with no credentials, real DB or live tmux socket. Use minimal stable
   seams; no speculative refactor in a checkpoint. Directory separation alone
   is not a sandbox: enforce environment and external-command isolation in tests.
5. Run targeted Rust checks and old/new fixture comparisons; for terminal changes,
   also compare PTY traces and representative rendered layouts. Compilation alone
   is never evidence of preserved behaviour.
6. Two independent reviewers inspect the final candidate: one challenges behaviour,
   identity/security/compatibility; the other challenges simplicity, resource
   bounds and tests that might conceal drift. Neither is the implementer. Both
   receive the approved plan and relevant source/evidence, not the other's verdict.
7. Resolve findings with minimal changes and rerun affected checks. Review verdicts
   bind to the candidate diff hash; changes invalidate affected verdicts. Unresolved
   high-severity findings or unknown safety behaviour block progression.
8. Present a short receipt. Human approval is required before committing the accepted
   checkpoint and starting its dependent work unless explicitly delegated below.
9. Save only actionable lessons and new regression fixtures. Do not accumulate
   full conversations as a permanent prompt or grow a migration orchestration service.

### Approval without exhausting the owner

Default: a concise approval request per checkpoint, as in the published Helix loop.
An owner may instead approve a phase envelope and explicitly delegate acceptance
of low-risk checkpoints whose tests and both reviews pass unchanged. That is a
Pika-specific adaptation, not a claim about Shopify's process. Scope changes,
safety exceptions, destructive state changes, real provider spending, user-machine
cutover and publication still require their own explicit authority. Never infer
that writing this PRD or invoking a skill delegates those decisions.

No reviewer quorum can override a failed test. No numerical review score replaces
concrete findings. Two reviewers discovering no problem is supporting evidence,
not a guarantee. A reviewer must say what was not exercised.

### Token and execution discipline

One checkpoint implementer and two bounded reviewer passes are the default.
Parallelize the independent reviews when authorized; do not fan out implementations
that edit shared state modules. Review diffs and targeted fixtures, not the entire
repo every time. Use deterministic tests before paid review. After two unsuccessful
repair/review cycles, stop expanding the attempt: diagnose the boundary, split or
amend the checkpoint, and show why. Set a token/time budget at phase approval and
report consumption; do not interview real expert agents merely to build this port.

## 9. Phases and checkpoints

Phase exits require all listed acceptance evidence and no unresolved blockers.
The phase order is dependency-driven; early experimental builds are not a claim
of product parity. Work happens beside Python; default public Pika remains Python
until final cutover approval. A temporary `pika-native` developer binary is not a
new daily command or a permanent selectable backend.

### Independent workspace and surgical integration

This repository is the native development workspace. The existing sibling Python
repository remains the maintained product baseline and is
not a shared worktree or dependency of native development. Neither repository's
uncommitted work is imported into the other.

`reference/python/` contains an ignored, frozen export of the baseline release.
Its provenance is recorded in `REFERENCE.md`. Treat it as read-only evidence,
not a second maintained Python implementation. Run reference tests from disposable
copies with isolated state; never install that export over the working Pika.

Python development may continue independently. Before accepting each phase, review
changes since the recorded baseline and identify affected contracts and regression
fixtures. Record any deliberate reference refresh by commit and rerun affected
comparisons. Do not silently update the baseline or demand byte parity with bugs
that the maintained product has subsequently fixed.

Port behaviour slices inside the native repository first. This does not authorize
incremental replacement of production Python modules with Rust subprocesses or
FFI: those would introduce an additional runtime boundary. After release gates
pass, prepare a small, separately approved distribution/launcher cutover; retain
the prior executable and compatible state for rollback. If an earlier surgical
component integration is proposed, justify its boundary cost and obtain approval
as an explicit plan change. No automatic copying back, shared live database,
global PATH modification, hook installation or remote deployment.

### Phase 0 — freeze the contract and remove unnecessary scope

- **CP0.1: Behaviour inventory.** Map every public/hidden command, config key,
  environment override, stored object and external callback to a consumer and
  test. Mark retain/consolidate/remove/unknown; no public deletion without approval.
- **CP0.2: Differential fixtures and baseline.** Capture synthetic reference outputs,
  state deltas and process/network effects for the corpus in section 10. Measure
  resource budgets; demonstrate that deliberately wrong status/UUID/ack outputs fail.
- **CP0.3: Transition feasibility.** Reproduce old wheel-based update selection,
  hook/runtime references, fleet versions, package targets and signing requirements.
  Approve a concrete old-to-native transition plan with a testable fixture driver.

Exit: approved inventory, budget baseline, checkpoint acceptance hash and transition
design. Rust implementation beyond a disposable feasibility stub has not begun.
Pause if preserving current users demands a broader product decision.

### Phase 1 — native core and read-only CLI

- **CP1.1: Native shell.** Single package; pinned dependencies; side-effect-free
  help/version; embedded skill show/install to an isolated destination; release builds.
- **CP1.2: State and pure rules.** Read existing fixture DB/config; preserve exact IDs,
  timestamp units and status/ordering. Expose offline snapshot/list/explain/expert
  lookup only through the internal contract runner. Current public `list`, `activity`
  and local `explain` reconcile provider and process state before returning, so they
  cannot claim parity until Phase 2 supplies those observations; only cached expert
  lookup is genuinely read-only in the public surface.
- **CP1.3: Contract runner.** Run Python and Rust on independent copies, compare JSON,
  stdout/stderr/exit codes and requested effects. Unknown/invalid schemas fail safely.

Exit: useful native read-only commands; startup/artifact measurements recorded;
no user data writes or background calls. Unsupported action fails explicitly,
not by silently launching Python.

### Phase 2 — cached board and local process evidence

- **CP2.1: Read-only board.** Four groups, navigation, resize, filter, NO_COLOR,
  narrow/empty/error layouts, first-cache-frame and finite redirected output.
- **CP2.2: Observation.** Provider metadata discovery, archive/worker/naming rules,
  process-generation evidence, tmux inventory and bounded reconciliation. Reuse
  snapshots without treating cached PID evidence as permission for an action.
- **CP2.3: Responsiveness.** Inject slow reads and huge outputs; preserve selection
  under reorder; bound queues and redraws; prove preview cannot acknowledge work.

Exit: actual same-machine metadata can be compared read-only only after authorized
inspection; fixture performance gates pass. Board action buttons remain disabled
until Phase 3 gives them verified execution semantics.

### Phase 3 — local lifecycle, open and faithful terminal handoff

- **CP3.1: One vertical journey.** Create/register/open/detach/resume a disposable
  Codex-like fixture conversation, including failed attachment with exact recovery.
- **CP3.2: Identity and lifecycle.** Lease expiry, genuine duplicates, PID reuse,
  generation-pinned cleanup, structured questions, late events, naming/forks and
  crash-interrupted launch reservations. Extend to all three provider fixtures.
- **CP3.3: Terminal fidelity.** Colours, fragmented terminal replies, input filtering,
  mouse/arrow keys, scrollback, resize, suspend/signals and mode restoration on
  success, failure and cancellation. Do not delete bridges before these pass.
- **CP3.4: Observation ownership.** Peek/ack/untrack/restore, doctor receipts and
  installed hook/plugin/exit callback contracts, including mixed Python/Rust writers.

Exit: complete local journeys for all three harnesses in fake-provider PTYs; isolated
real tmux tests on macOS/Linux; targeted authorized live provider spot checks for
behaviour that simulation cannot establish. Zero wrong-target actions.

### Phase 4 — expert network and private multi-turn consultations

- **CP4.1: Expert lifecycle.** Existing cards remain useful; scope/now/freshness,
  publish permissions, refresh eligibility, quota skips and one bounded interview.
- **CP4.2: Protocol parity.** Codex paginated forks, Claude stream/non-persistence,
  OpenCode persisted-side permissions/deletion; fixed model policy across turns;
  explicit uncertain delivery and cleanup receipts. Never resend unknown turns.
- **CP4.3: Caller and board use.** Bundled skill drives exact discovery/ask JSONL;
  inline side stays in panel; parent remains working; cancellation and retries
  are tested at every protocol stage without terminating the parent.

Exit: complete fake-provider failure matrix plus owner-approved disposable live
consultations. Parent-byte checks isolate legitimate parent progress; an advancing
parent is not proof of pollution. Record the actual guarantee tested, not a blanket
"nothing changed". Expert searches and setup emit no model prompts.

### Phase 5 — federation and client compatibility

- **CP5.1: Fleet inventory.** Passive discovery, selected-node trust, versioned strict
  envelopes, cache age and fair bounded scheduler; stalled SSH never stalls local UI.
- **CP5.2: Exact remote actions.** Attach/peek/ack/untrack/ask and expertise directory
  on capability-compatible pairs. During the v0.5/v0.6 transition, hello, snapshot,
  peek and untrack interoperate while event-unbound acknowledgement fails closed;
  changed node ID and malformed/truncated messages fail closed, and action outcome
  remains separate from later snapshot refresh.
- **CP5.3: Client bridge.** Reproduce/version-resolve the experimental client path;
  loopback-only listener, exact IDs, deduplication, tunnel absence versus rejection,
  pairing secret rotation, and confirmed-window-launch versus confirmed-attach.

Exit: two isolated nodes exercise cross-host consultation and exact remote open;
owner-approved live SSH smoke where required. Tests never probe an entire tailnet.
Windows bridge retains only its verified experimental claims.

### Phase 6 — native install, update and setup transition

- **CP6.1: Fresh native install.** Correct target selection, verified downloads,
  no interpreter/compiler bootstrap, correct PATH instructions, rollback on failure,
  embedded skill and guarded configuration edits.
- **CP6.2: Existing users.** Execute CP0.3's transition using unmodified v0.5.0a4;
  prove names/cards/unread/node IDs unchanged; old board and callbacks keep working;
  setup declined, delayed plugin reload and referenced legacy runtime all covered.
- **CP6.3: Fleet deployment and schedulers.** Version-pinned remote install/update,
  offline/private bundle, noninteractive rejection, systemd/launchd migration and
  targeted rollback. No automatic remote upgrade or unapproved scheduler restart.

Exit: fresh native installs meet dependency/footprint gates; legacy migration is
truthful about retained disk usage; user cannot select an incompatible release
through the old default updater. Rollback preserves post-update events.

### Phase 7 — controlled release and retirement

- **CP7.1: Release candidate.** All retained contracts covered or individually
  approved out of scope; Rust fmt/clippy/tests, differential suite, full clean-host
  matrix, installed skill, dependency/license scan, benchmarks and restore checks.
- **CP7.2: Authorized pilot.** One named owner-approved machine first; preserve
  agents; observe two real work cycles including detach/reopen, waiting/answer,
  hooks and consultation. Pilot failures produce fixtures, not broad cleanup.
- **CP7.3: Publish and verify.** Owner approves exact artifact hashes; tag tested
  commit; upload immutable assets; anonymously test public fresh install and old
  updater migration. Documentation claims reflect the published artifact.
- **CP7.4: Retire transition machinery.** Remove Python from active development
  once fallback obligations are satisfied. Retain tagged reference/fixtures without
  maintaining dual business logic. Never automatically delete user-side runtimes
  still referenced by active processes/config or needed for approved rollback.

Exit: native release is the default supported implementation; supported mixed
fleet/version window and remaining legacy references are explicit. Product is
leaner in measured dependencies, footprint and execution paths—not merely Rust.

## 10. Verification corpus and anti-cheating rules

### Mandatory scenario families

| Family | Required cases | Existing evidence to reuse |
| --- | --- | --- |
| Identity | Shared-server stale lease, true duplicate trees, launcher alias, PID reuse, permission denied, exit during proof, wrong pane tags, exact retry | test_processes, test_processes_macos, test_capture_identity, test_core, test_adversarial |
| Discovery | Archived/deleted, automation provenance, malformed provenance, explicit/uncertain names, same-name directories/providers, fork and rename, OpenCode duplicate metadata | test_providers, test_thread_resolution, test_setup_names, test_onboarding_contract |
| Attention | Working after READY, pre-question/post-answer, stale runtime errors, safety precedence, missing process, delayed/out-of-order/duplicate events | test_lifecycle, test_hooks, test_status_projection, test_board_contract |
| Board/terminal | 72x20/104x20/140x30/180x45, partial key sequences, arrow and mouse input, rapid refresh, vanished selection, failed attach/retry, colours and scrollback | test_monitor, test_monitor_open, test_tmux_integration, test_terminal_bridge, test_terminal_palette |
| Observation/state | Redirected peek, explicit ack race, event watermark, untrack tombstone, concurrent commits, lock timeout, backup/restore | test_store, test_ui, test_explain, test_doctor_cli |
| Consultations | Fork response versions, no-side confirmation, unknown submit, timeout, partial stream, malformed record, cancel, tools/permission restrictions, cleanup failure, multiple turns | test_consult, test_consult_outcomes, test_remote_consult_outcomes |
| Experts/usage | Existing profile, scope/now freshness, exact publisher, quota unknown/reset windows, no interview in setup, dated/unknown usage | test_experts, test_expert_freshness, test_expert_refresh, test_expert_schedule, test_quota, test_usage |
| Federation | Host/node mismatch, stale cache, slow node fairness, capability mismatch, oversize/truncated messages, SSH trust failure, mixed runtimes, window bridge | test_fleet, test_fleet_expert_directory, test_client_bridge |
| Distribution | Old updater, target mismatch, checksum, denied writes, interrupted switch, foreign/symlink roots, old running callbacks, setup backup/decline, skill files | test_bootstrap, test_installation, test_release_bundle, test_release_contract, test_setup_skill, test_ssh_bundle |

Phase 0 maps these module names to exact test IDs and fixture paths. A passing
count alone cannot close a family. Add missing failure coverage before rewriting
the relevant boundary, but do not characterize removable dead internals.

### Comparison format

Use small JSON fixtures plus scripted fake provider/tmux/SSH processes. Represent
inputs, initial logical state, observation sequence, requested effects, expected
output/exit and final state. Keep the same fixture independent of implementation.
The runner may be Python during development; end-user native builds may not depend
on it. No new network service or always-on test daemon is needed.

Compare ordered effects as well as final output: command argv, spawned process
identity, calls not made, files not changed, event acknowledgement and cleanup
scope. Use monotonic virtual time for deadlines and controlled wall time for
provider timestamps. Normalize only named volatile fields; preserve relationships
between generated IDs. Never normalize away target UUID, node ID, event identity,
status, permission, delivery certainty, cleanup outcome, or stdout/stderr channel.

Keep ANSI-byte tests for protocol filters and terminal mode restoration; compare
rendered cells for layout semantics. Human visual review covers actual terminal
themes and input feel. Screenshots cannot prove identity or parent isolation.
Do not run both implementations' mutating paths against the same live provider
conversation. Use independent fixtures/disposable parents and explicit quotas.

Inject failures before/after each irreversible boundary: launch, reservation,
state commit, provider submission, ack, rename, launcher activation and cleanup.
Every fixture has a hard timeout and cleanup limited to its own processes/socket.
Tests must fail if an agent takes a shortcut by weakening exact identity, faking
READY, disabling a wait, dropping a failing scenario, or swallowing nonzero exit.

## 11. Target platforms and deployment gates

Initial native hosting targets: macOS arm64/x86_64 and Linux x86_64/aarch64,
subject to Phase 0 confirming the existing supported target matrix. Pin minimum
OS/libc and Rust MSRV explicitly; cross-compilation alone is not runtime proof.
For any target unavailable for execution testing, block its native artifact or
retain the supported Python path until a deliberate support decision is approved.
Never quietly shrink current support to meet the deadline.

Windows x86_64 remains client/bridge-only and experimental. A future native TUI
backend does not imply tmux hosting, filesystem identity or signal parity.
Mac signing/notarization and actual Gatekeeper/quarantine behaviour are evaluated
in CP0.3; do not ask users to disable security or silently remove quarantine.
Signing credentials/paid developer-account decisions, if required, are owner inputs.

Release integrity: platform-target manifest, checksums over exact uploaded bytes,
bounded HTTPS downloads, vetted dependency licenses and source distribution.
Checksums detect mismatches; they are not an independent trust anchor if the
release account is compromised. Keep existing trust claims honest.

## 12. Risks, stop conditions and rollback

| Risk | Detection | Required response |
| --- | --- | --- |
| Rust reproduces old semantic bugs | Spec/reference disagreement or regression fixture | Approve explicit correction; never silently broaden migration scope |
| State identity changes under attach | Generation/UUID mismatch or concurrent rename | Fail closed, retain actionable recovery; no name fallback |
| Old hooks break after runtime removal | Callback inventory and delayed-reload test | Retain referenced runtime; preview native configuration; no forced agent restart |
| Old updater cannot consume native release | Unmodified old-client download test | Block publication until bridge path works |
| Lower source count but larger runtime | Footprint/CPU/dependency report | Remove duplication or reconsider dependency choice; do not game exclusions |
| SQLite or protocol drift strands fleet | Mixed-writer/mixed-node matrix | Keep schema/protocol compatible or amend plan with migration approval |
| Review/test loops burn quota | Checkpoint cost receipt, repeated failed cycles | Stop, diagnose and split; no unbounded reviewer fan-out |
| Local tests pass but terminals regress | Real PTY and authorized Mac/Linux visual checks | Block affected hosting target; add minimal regression fixture |
| Agent output contaminates public artifacts | Artifact allowlist and sanitized test fixtures | Remove private data before commit; keep local evidence ignored |

Default rollback switches only the validated managed launcher back to the prior
immutable release and restores configuration only when necessary and approved.
Keep data format compatible so new events survive. Record which schedulers and
callbacks use which runtime. Do not kill agents, restore stale provider histories,
or mass-delete tmux sessions. The old release remains available until retention
obligations are met; cleanup is not bundled into the first successful cutover.

## 13. Definition of done and handoff receipt

The port is done only when:

- All retained F01–F24 requirements and mandatory scenario families have evidence.
- N01–N12 gates pass or an owner-approved, specifically measured exception exists.
- Both reviewers approve each final checkpoint hash; required owner approvals exist.
- Existing users and supported mixed fleets retain identities, cards and history.
- Fresh installs and native operations do not require Python; legacy disk retention
  is explicitly separated from active native footprint.
- Actual public installer/update paths deliver the verified native release.
- README/skill/help match shipped behaviour and hide migration machinery from
  everyday use without hiding material limitations.
- There is one active implementation of Pika policy, a reproducible test boundary,
  a proven rollback, and no remaining unexplained high-severity finding.

Checkpoint receipt template:

```text
Checkpoint / phase:
Plan SHA256 / approved by / approval scope:
Reference SHA / candidate diff SHA:
Outcome preserved:
Code or execution work removed:
Permitted differences / requirement IDs:
Fixtures and commands / results / evidence paths:
Performance delta / sample counts / environment:
Reviewer A findings and verdict / reviewed diff SHA:
Reviewer B findings and verdict / reviewed diff SHA:
Live behaviour not tested:
Rollback boundary:
Token/time usage and remaining phase budget:
Owner acceptance or recorded delegation:
Commit / next dependent checkpoint:
```

No checkpoint is marked accepted in this PRD. Begin at CP0.1 after explicit
execution approval. Re-estimate after Phase 1 using completed checkpoint time,
review/repair cost and uncovered compatibility work. The earlier 2–4 week estimate
is provisional; no calendar deadline overrides identity or migration gates.

## Appendix A — product reasoning

### Product Thesis

Pika makes existing agent work findable, recoverable and useful across projects.
Rust should make that product easier to install and cheaper to run, not redefine it.

### User End State

One familiar command, correct attention, exact return, and reusable expertise.

### Current Workflow Cost

Environment bootstrapping, uncertain identity, fragmented machine state, and
re-explaining work cost more attention than choosing a programming language.

### Time-to-Value Risks

Missing prerequisites, noisy setup and blocking remote reads can delay the first
opened conversation. Expert interviews must remain optional after the board works.

### Hard Constraints

Provider-owned histories, live processes, OS identity semantics, SSH trust,
existing callbacks, finite quota, and multi-version installations are real.

### Fake Constraints to Ignore

Rust need not reproduce the module tree. A native binary need not replace tmux.
Shipping the port need not require migrating every machine simultaneously.

### Commodity Layer

Terminal rendering, argument parsing, JSON, SQLite, SSH and packaging utilities.
Reuse mature implementations rather than building differentiation here.

### Value-Creating Process

Correctly route an action or question to the exact context-bearing conversation,
preserve its work, and explain when certainty is insufficient.

### Recommended Product Shape

Keep the existing board, return-by-name and skill-first consultation flow.
Acquire context from provider metadata and existing expert cards; automate safe
reconciliation, expose only meaningful ambiguity and approvals. The first payoff
is opening the right conversation without manual tmux work.

### Trust Model

Observable identity, bounded effects, truthful freshness, safe cancellation and
receipts backed by tested action outcomes—not implementation-language branding.

### Wedge

One agent can use expertise from another project without interrupting its expert,
while the user retains a dependable home for all active work.

### Defensibility

Provider integration quality, accumulated failure knowledge and a reliable expert
network are the advantage. Rust itself is neither the moat nor the user outcome.

### Build First

Pinned behaviour corpus and one native read-only slice that measures packaging,
startup and existing-state compatibility before migration breadth expands.

### Do Not Build

A new agent harness, hosted memory platform, terminal backend, migration daemon,
or generic framework for rewriting arbitrary applications.

### Asymmetric Ideas

- Make upgrade continuity a fixture: old board and old hooks keep working while
  a native client starts. A small test protects a large user fear.
- Reuse every historical incident as a compact regression rather than retaining
  a giant narrative of the bug.
- Keep one stable `pika` entrypoint and embedded skill: deployment machinery
  disappears from daily use.
- Treat "no request sent" and "unread unchanged" as testable outputs alongside
  successful results; avoided work is part of correctness.

### Positioning Hook

One home for your coding agents. A way for their work to connect.
Same conversations, lighter installation. No new workflow to learn.

## Appendix B — systems review

### System Read

An I/O-heavy coordinator over local persistence, process evidence and provider
protocols. It is not a model-inference engine.

### Workload Reality

Two-second local refresh, short-lived hooks, user-triggered attaches/consultations,
bounded fleet polling and intermittent setup/update. Burstiness matters more
than treating every operation as a uniform request.

### Main Bottlenecks

Likely candidates are repeated discovery/process work, subprocess startup,
SQLite contention and external-provider waits. Only Phase 0 profiling can rank
their actual contribution. Rust removes interpreter startup, not network waits.

### Latency and Scaling Risks

Serialized remote work can exceed snapshot freshness budgets as node count grows.
Hook bursts can contend with reconciliation. Large provider records and unbounded
UI queues can dominate tail latency even in a fast compiled implementation.

### State and Boundaries

Provider histories stay provider-owned; local SQLite owns Pika facts; remote
nodes own their truth. Cards, lifecycle, safety evidence and render projections
remain distinct concepts even when their implementation is consolidated.

### Reliability Risks

PID reuse, stale leases, unknown submission, delayed hooks and half-applied updates
are the difficult boundaries. No language substitutes for these protocols.

### Simplify First

Remove redundant scans and wrappers; use one projection and one exact-action
route. Preserve consumer-serving state and pre-action proof. Avoid new servers.

### Architecture Recommendations

| Title | Problem | Change | Why it helps | Impact | Effort |
| --- | --- | --- | --- | --- | --- |
| Native deployment | Runtime bootstrap and environment coupling | One target-specific executable with embedded skill | Fewer installation steps and activation dependencies | High | High |
| Shared behaviour rules | Divergence between surfaces | Pure domain rules reused by CLI/board/hooks/fleet | One place to verify state and identity policy | High | Medium |
| Bounded I/O | Slow dependencies and bursts | Explicit queues, deadlines and operation cancellation | Stable input latency and finite resource use | High | Medium |
| Common reference corpus | Translation can agree with translated tests yet be wrong | Independent Python/Rust fixtures and effect traces | Detects semantic drift across implementations | High | Medium |
| Snapshot reuse | Repeated inventory effort | Reuse per-refresh reads; retain fresh pre-action evidence | Removes work without weakening action safety | Medium | Medium |

### Build Now vs Later

During the approved port: contract harness, native core, exact workflow slices,
compatibility and distribution. Later: alternate terminal backends, novel caching
architectures and expanded Windows hosting, each justified separately.

### What to Avoid

Unbounded concurrency, a daemon introduced for convenience, multiple authoritative
registries, automatic uncertain retries, dual-language business logic, and claims
that model latency improved because local code became Rust.

## Sources and implementation map

- [Shopify: Helix and the native migration](https://shopify.engineering/back-to-native)
- [Shopify: Shop migration, approval hashes and Tardis](https://shopify.engineering/shop-app-migration)
- [Ratatui backend comparison](https://ratatui.rs/concepts/backends/comparison/):
  candidate terminal infrastructure; platform backends do not establish Pika parity.
- Local baseline: `DESIGN.md`, `SECURITY.md`, `docs/releasing.md`, `pyproject.toml`.
- Semantics: `src/pikamux/core.py`, `status_projection.py`, `models.py`, `store.py`.
- Boundaries: `providers.py`, `processes.py`, `processes_macos.py`, `tmux.py`,
  `terminal_bridge.py`, `terminal_palette.py`, `consult.py`, `fleet.py`.
- Installed consumers: `setup_hooks.py`, `hooks.py`, `expert_schedule.py`,
  `installation.py`, `client_cli.py`, `client_bridge.py`, bundled skill files.
- Existing measurements/test entrypoints: `tests/benchmark_board.py`,
  `tests/test_monitor_open.py`, `tests/test_release_contract.py`,
  `tests/test_bootstrap.py`, `tests/test_installation.py`.

This document was checked against those surfaces and self-reviewed for conflicting
requirements. The owner approved the implementation campaign on 2026-09-11.
Publication, installation, live cutover and Python retirement remain separate,
unapproved execution gates.
