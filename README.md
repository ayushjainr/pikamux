# Pika Native

Pika gives coding-agent conversations one durable home—and lets agents privately
consult the exact conversations that already know the work. This repository is
the native Rust implementation of Pika for Codex, Claude, and OpenCode.

The everyday interface stays small:

```sh
pika                         # live board
pika strategy_dashboard      # attach, resume, adopt, or create safely
pika next                    # open the oldest item needing attention
pika ask returns_tracker "Which timestamp convention did you use?"
```

`pika NAME` resolves the provider's immutable conversation identity before it
acts. A live exact home attaches. A saved conversation resumes into tmux. A
discoverable conversation is adopted. A genuinely new name is created and
named. Ambiguity is shown; Pika does not guess.

## What is implemented

- A responsive four-group board: **Needs You**, **Working**, **Ready**, and
  **Parked**, with stable selection, filtering, peeks, usage, explanations, and
  inline multi-turn expert questions.
- Exact local lifecycle management with UUID/process-generation evidence,
  expiring shared-server leases, PID-reuse protection, honest `OPEN TWICE`
  detection, and one reusable tmux home per conversation.
- Provider-native private consultations. The expert keeps working, the parent
  transcript receives no side prompt, and the caller gets explicit
  fork/turn/response/cleanup receipts.
- Durable expert cards and quota-aware refresh for agent-to-agent discovery.
- Trusted SSH/Tailscale federation with immutable node identity, cached remote
  boards, exact remote actions, and mixed Python/native protocol compatibility.
- Previewable setup with preserved settings, atomic writes, backups, lifecycle
  hooks, the bundled `agent-convo` skill, and launchd/systemd expert refresh.
- Verified native installation, updates, rollback-safe release roots, remote
  upgrade bundles, checksums, and a portable Windows client/bridge artifact.
- Evidence-based doctor, explain, activity, usage, and stale-bookkeeping repair.

Native Windows agent hosting is intentionally out of scope because tmux is not
native to Windows. The Windows build is a client: it pairs with a macOS/Linux
Pika host and opens exact remote conversations in Windows Terminal.

## Safety model

Pika treats identity as a product promise, not a pane label.

- A provider UUID and current process generation must agree before a live pane
  is trusted.
- Shared app-server ownership expires and is revalidated; genuine independent
  owners remain fail-closed.
- Automatic peeks preserve unread work. Acknowledgement is explicit and pinned
  to the observed event.
- Stopping watch does not stop, archive, or delete the agent.
- Fleet actions bind node, provider, and conversation identity. Stale remote
  cache cannot claim that somebody needs attention.
- Setup and upgrades preview their targets, require approval, preserve unrelated
  settings, and verify bytes before activation.

## Build and verify

Rust 1.88 or newer is required to build; released users need only the compiled
binary and tmux on macOS/Linux.

```sh
cargo build --release --locked
cargo fmt --all --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-targets --all-features
scripts/test-mixed-runtime.sh
```

Tests use disposable HOME/XDG/provider state, fake provider and SSH boundaries,
and a dedicated tmux socket. The real-tmux contract launches inert local fake
providers only. It never touches the installed Pika database, real transcripts,
fleet nodes, or model quota.

## Measured native advantage

Same-machine measurements on macOS arm64 compare the optimized native binary
with the frozen Python v0.5.0a4 reference. Board runs use 200 synthetic rows in
the same terminal geometry; hook runs alternate Python/native order.

| Contract | Native Rust | Python v0.5.0a4 |
| --- | ---: | ---: |
| CLI startup, p95 (100 launches) | 6.12 ms | 135.20 ms |
| Cached board first frame, p95 (30 launches) | 11.36 ms | 452.04 ms |
| Board input redraw, p95 (1,000 keys) | 0.115 ms | 8.37 ms |
| Hook fast path, p95 (100 events) | 18.71 ms | 467.57 ms |
| 200-row local reconciliation, p95 (30 runs) | 61.17 ms | 2,245.45 ms |
| Warm board RSS, p95 | 6.73 MiB | 36.42 MiB |
| Binary / gzip | 4.41 MB / 2.07 MB | interpreter environment required |

Reproduce the measurements with:

```sh
scripts/benchmark-native.sh target/release/pika dev/python-v050a4
python3 dev/measure_board.py target/release/pika \
  --python-reference dev/python-v050a4 --samples 30 --input-samples 1000
python3 dev/measure_hook.py target/release/pika dev/python-v050a4 --samples 100
python3 dev/measure_reconcile.py target/release/pika dev/python-v050a4 --samples 30
```

Timing is hardware-specific; the behavioural gates are not. See the full
[performance methodology](docs/PERFORMANCE.md),
[PRD.md](PRD.md), [the mixed-runtime evidence](docs/MIXED_RUNTIME_COMPATIBILITY.md),
and [the execution ledger](EXECUTION_LEDGER.md) for the frozen contracts and
acceptance record.

## Release boundary

This workspace does not replace an installed Python Pika merely because it
builds successfully. Publishing, changing the global `pika` command, migrating
live machines, and retiring rollback runtimes are separate, reversible release
decisions after native artifacts pass their target-platform CI gates.

MIT licensed. See [LICENSE](LICENSE) and [THIRD_PARTY.md](THIRD_PARTY.md).
