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

If a conversation is literally named like a Pika command, `pika open NAME`
provides the exact escape without renaming it.

## Install from source

Pika supports macOS and Linux on Apple Silicon/ARM64 and x86-64. From this
checkout:

```sh
cargo install --path . --locked
pika setup
```

Rust 1.88 or newer and tmux are required to build and host conversations. Setup
previews every change, preserves existing Codex and Claude settings, installs
the `agent-convo` skill, and configures lifecycle hooks only after approval.

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
  boards, exact remote actions, and a fail-closed Python/native transition.
- Previewable setup with preserved settings, atomic writes, backups, lifecycle
  hooks, the bundled `agent-convo` skill, and launchd/systemd expert refresh.
- Locally verified native installation, updates, rollback-safe release roots,
  remote upgrade bundles, and checksums.
- Evidence-based doctor, explain, activity, usage, and stale-bookkeeping repair.

### Make conversations discoverable as experts

`pika setup` installs the agent-facing skill and a quota-aware card refresher.
Cards describe durable expertise separately from current work, so another agent
can find the right conversation and consult it without interrupting or modifying
the parent. Inspect progress or refresh one eligible card with:

```sh
pika expert status
pika expert refresh --due
```

An agent can also publish its own exact card explicitly with `pika expert
publish`; run `pika expert publish --help` for the scope, current-work, topic,
and artifact fields.

### Windows client preview

Native Windows hosting is out of scope because tmux is not native to Windows.
The repository contains an experimental Windows client/bridge artifact for
pairing with a macOS/Linux Pika host; clean-host installation and self-update
remain release gates.

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

Rust 1.88 or newer is required to build. A future published native release will
need only the compiled binary and tmux on macOS/Linux.

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
| CLI startup, p95 (100 launches) | 4.00 ms | 136.21 ms |
| Cached board first frame, p95 (30 launches) | 11.63 ms | 454.79 ms |
| Board input redraw, p95 (1,000 keys) | 0.117 ms | 8.26 ms |
| Hook fast path, p95 (100 events) | 13.50 ms | 470.97 ms |
| 200-row local reconciliation, p95 (30 runs) | 81.29 ms | 2,333.46 ms |
| Five-minute idle CPU, mean | 0.978% | 19.157% |
| Warm board RSS, p95 | 11.63 MiB | 36.48 MiB |
| Binary / gzip | 4.45 MB / 2.29 MB | interpreter environment required |

Reproduce the measurements with:

```sh
scripts/benchmark-native.sh target/release/pika dev/python-v050a4
python3 dev/measure_board.py target/release/pika \
  --python-reference dev/python-v050a4 --samples 30 --input-samples 1000
python3 dev/measure_hook.py target/release/pika dev/python-v050a4 --samples 100
python3 dev/measure_reconcile.py target/release/pika dev/python-v050a4 --samples 30
python3 dev/measure_board_steady.py target/release/pika dev/python-v050a4 \
  --seconds 300 --program both
```

Timing is hardware-specific; the behavioural gates are not. See the full
[performance methodology](docs/PERFORMANCE.md) and
[mixed-runtime evidence](docs/MIXED_RUNTIME_COMPATIBILITY.md) for the frozen
contracts and acceptance method.

## Release boundary

The native release workflow keeps publication, installed-command cutover, live
machine migration, and rollback-runtime retirement as separate, reversible
decisions after target-platform CI passes.

MIT licensed. See [LICENSE](LICENSE) and [THIRD_PARTY.md](THIRD_PARTY.md).
