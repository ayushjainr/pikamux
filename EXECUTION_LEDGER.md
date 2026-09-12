# Rust migration execution ledger

Goal: complete a native Rust Pika that preserves verified Python product behavior
and is measurably leaner and lighter. The installed Python product remains the
production baseline until a separately authorized cutover.

Authorization: on 2026-09-11 the owner requested end-to-end implementation under
Night's Watch. This is treated as a phase implementation envelope inside this
repository, including local commits and isolated tests. It does not authorize a
push, release, modification of installed Pika, live database migration, global
PATH change, provider spending, or deployment to another machine.

## Quality bar

- Behavior correctness and parity: 35 points.
- Identity, state integrity and operational safety: 20 points.
- Board and terminal experience: 15 points.
- Expert consultation and fleet behavior: 15 points.
- Startup, memory and artifact footprint: 10 points.
- Maintainability and deletion of unnecessary machinery: 5 points.

Completion requires self-score and an independent adversarial score of at least
95/100, with no unresolved high-severity finding.

## Current phase

- CP0.1 through CP6.3: implemented and fixture-verified.
- CP7.1 release candidate: implementation complete; independent review pending.
- CP7.2 through CP7.4: deliberately not executed. They require a separately
  authorized live pilot, publication, launcher cutover, and Python retirement.
- Rust MSRV: exactly 1.88.0, verified with formatting and warnings-as-errors Clippy.
- Rust toolchains and advisory database remain isolated under ignored `.toolchains/`.
- Frozen reference: Python v0.5.0a4 at `36de1b1...`, read-only and ignored.
- Installed Pika, real state, provider histories, fleet nodes, hooks, global PATH,
  and model quota were not touched.

## Decisions and evidence

- Separate repository avoids accidental edits to the shipping Python tree, but
  executable tests still require isolated HOME/XDG/Pika state and fake commands.
- Native runtime may not shell out to Python. Minimal provider-owned JavaScript is
  allowed only where a provider integration requires it.
- Performance claims are comparative measurements on the same machine, not assumed
  benefits of the implementation language.
- OpenCode does not expose trustworthy user-authored-title provenance in its
  current storage schema. It remains UUID-addressable and browseable, but its
  generated labels cannot flood setup's explicitly named screen.
- Provider/process reconciliation is the fallback safety net every ten seconds;
  lifecycle hooks publish attention immediately. This keeps five-minute idle CPU
  below one percent without weakening event responsiveness.
- Rust 1.88 is the honest build minimum because the implementation uses stabilized
  let chains. Released users need only the native executable and tmux.

## Candidate evidence

| Gate | Result |
| --- | --- |
| Native behavior suite | 206 tests passed on macOS arm64, including one isolated real-tmux journey for Codex, Claude, and OpenCode |
| Mixed-runtime transition | Real Python v0.5.0a4 and Rust alternated SQLite hook writes; each consumed the other's fleet v2 envelopes |
| Formatting/lint/MSRV | `cargo +1.88.0 fmt --check` and warnings-as-errors Clippy passed for all targets/features |
| Dependency integrity | Locked license inventory reproduced exactly; RustSec scanned 157 locked crates with zero advisories after updating `time` to 0.3.47 |
| Native packaging | Release archive, strict manifest, sidecar checksums, offline installer, installed version/help, and embedded skill verified in a disposable root |
| Startup | p95 6.12 ms native versus 135.20 ms Python, 100 launches each |
| Board | first-frame p95 11.36 ms versus 452.04 ms; input p95 0.115 ms versus 8.37 ms |
| Hook | p95 18.71 ms versus 467.57 ms, 100 events each |
| Reconciliation | 200-row public path p95 61.17 ms versus 2,245.45 ms, 30 runs each |
| Warm resources | 6.73 MiB RSS p95 and 0.833% one-core mean over a five-minute native board run |
| Artifact | 4.09 MB executable; 2.11 MB gzip; no Python/runtime/compiler prerequisite |

Full methodology and raw-boundary definitions are in `docs/PERFORMANCE.md`.

## Checkpoint history

| Checkpoint | Status | Strongest evidence | Next action |
|---|---|---|---|
| Workspace isolation | complete | separate Git root; no remote; frozen archive; toolchain local | Freeze contract inventory |
| CP0 contract and transition | complete | literal differential fixtures; negative mutants; schema-1 bridge and old updater tests | Keep frozen reference immutable |
| CP1 native core | complete | native command surface, SQLite/config compatibility, embedded skill, no Python runtime path | Maintain strict schemas |
| CP2 board and observation | complete | cached-first PTY tests, provider discovery provenance, performance gates | Preserve hook immediacy |
| CP3 local lifecycle | complete | exact ownership, stale lease/PID reuse/duplicate/fork recovery, terminal filtering, real isolated tmux | Live pilot remains separate |
| CP4 expert network | complete | cards, freshness, quota gates, multi-turn isolation and failure-stage receipts for all providers | No setup-time interviews |
| CP5 federation/client | complete | strict fleet v2, mixed runtimes, remote consults, Windows loopback bridge | Windows hosting remains out of scope |
| CP6 distribution/setup | complete | preview/backup/idempotence, launchd/systemd, native install/update and remote bundle contracts | External platform CI pending |
| CP7.1 candidate | review | all local gates above passed | Two independent adversarial reviews and repair loop |
| CP7.2–CP7.4 | not authorized | no live pilot, tag, upload, cutover, or runtime retirement performed | Owner decision after candidate review |

Update this ledger at checkpoint boundaries and when a long-running command changes
health. Do not use it to claim unverified completion.
