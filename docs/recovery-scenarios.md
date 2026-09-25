# Recovery scenario checks

`scripts/check-recovery.sh` is the focused recovery runner. It builds each
Cargo test target once with `--locked`, validates every exact test name from a
single `--list` inventory, and runs each test with `--test-threads=1`. Every
build, listing, and test invocation goes through `scripts/with-test-home.sh`,
so the runner does not use installed Pika state, a user's provider
directories, real transcripts, fleet hosts, or real tmux sessions. It does
not start a model turn.

Use the repository's normal Cargo toolchain, or select one explicitly:

```sh
scripts/check-recovery.sh
scripts/check-recovery.sh --toolchain 1.88.0
PIKA_CARGO_TOOLCHAIN=1.88.0 scripts/check-recovery.sh
```

## Scenarios covered

### Core and tmux identity recovery

- A pending launch is completed only by an independently proven exact home.
- A vanished pane cannot create a replacement pending launch.
- Recovery refuses a session removed from fresh provider inventory.
- A genuine duplicate reaches `OPEN TWICE` and then recovers automatically.
- Exact attach records the opening while preserving a newer event.
- A newer process commit supersedes a stale board observation.
- Foreground reconciliation reobserves after another connection commits.
- An unverified binding accepts only one tagged live provider without UUID
  argv evidence.
- A tmux generation change blocks guarded attach before handoff.
- A failed attach does not run its callback.

These are the `core::tests::*` and `tmux::tests::*` unit scenarios named in
the runner.

### Hooks and durable store state

- Wrong launch identity fails closed, while an exact home can certify.
- A stale wrapper cannot demote or unbind a replacement owner.
- PID reuse cannot clear a newer generation.
- Ownership reservations and bindings preserve PID generations.
- Verified exit clears only a coalesced runtime PID.
- Pending launch phase and provider generation advance atomically.

### Board and isolated terminal journeys

- A failed board open returns to its filtered board without replaying the
  action.
- Hiding an overdue launch preserves recovery data and does not untrack the
  conversation.
- The unverified-terminal fallback requires a choice, never relaunches an agent,
  and preserves unread state.
- An isolated tmux reopen chooses a unique UUID owner beside an inert stale
  tag.
- An isolated tmux terminal with a competing live UUID pane remains blocked.
- A nested provider helper cannot steal the conversation owner's pane.
- Keyboard Ctrl+C and external SIGINT each reach a cancellation-capable child
  once without closing the terminal bridge; terminal modes restore after exit.

### Identity-safety failure paths

- Tag failure occurs before provider execution and retains the recovery record.
- A post-execution readback failure retains the exact pending generation.

## Remaining limitations

This is a targeted safety/recovery pass, not a release-wide validation. It
does not cover provider discovery, remote/fleet recovery, setup migration,
Windows client behavior, installer/update rollback, or every board and Files
journey. The isolated `real_tmux_contract` tests use disposable fake-provider
fixtures and an isolated tmux socket; they do not prove behavior against a
user's running tmux server or provider installation. Run the broader native,
Clippy, notice, and security checks required by the release process separately.
