# Python-to-native compatibility evidence

Pika's native transition shares durable state and fleet protocol version 2
with Python v0.5.0a4. The compatibility laboratory runs the real frozen Python
store, hook handler, fleet server and fleet client against their Rust peers.
It uses copied reference code, disposable databases and fake transports only.

## Covered transitions

| Direction | Contract exercised |
| --- | --- |
| Python database → Rust | Python creates the full schema and writes a Claude lifecycle hook; Rust opens it, reads it and writes the next real hook event. |
| Rust database → Python | Rust creates the full schema and writes a hook; Python opens it, reads it and writes the next real hook event. |
| Rust client → Python server | Real Rust requests and real Python responses for hello, expert-directory snapshot, peek, acknowledge, untrack and the post-mutation snapshot. |
| Python client → Rust server | Real Rust response envelopes are consumed by Python's FleetManager for hello, snapshot, peek, acknowledge, untrack and refresh. |

The alternating hook assertions cover session projection, unread state and the
single-provider hook-observation row. The fleet assertions cover immutable node
identity, protocol/capability validation, exact provider/session routing,
mutation request IDs and post-untrack inventory.

Run the evidence locally with:

```sh
scripts/test-mixed-runtime.sh
```

The reference is copied from tag `v0.5.0a4`, commit
`36de1b1eed1a182ed4e608d47f95d362f15d571a`, source tree
`7c6144590f23624cf83edf92706b6911bbf714ee`. Each test copies it again into a
new temporary directory. Environment variables point HOME, config, state,
database and temporary files at that directory; Python bytecode writes are
disabled. No installed Pika executable or live user state is read.

## Evidence and finding

On 2026-09-12 the three mixed-runtime contracts passed on macOS arm64. The
first run caught one real incompatibility: Python includes the optional
`active_thread_id` field on ordinary sessions in an expert-directory snapshot,
while Rust originally allowed it only on `expert_sessions`. Rust now accepts
that single allowlisted extension on either session list while retaining strict
field validation.

## Boundaries

- The harness intentionally does not run SSH, tmux, provider CLIs, hooks from a
  live agent, or any installer.
- Adoption, attach and consultation streaming are tested elsewhere; this
  transition harness covers the requested hello/snapshot/peek/ack/untrack set.
- It proves the frozen v0.5.0a4 schema, not arbitrary older or partially
  migrated databases.
- Python 3.10+ runs the frozen code directly. Apple's system Python 3.9 lacks
  `dataclass(slots=True)`, so the disposable adapter ignores only that memory
  layout flag; no reference file is modified and the tested store/wire behavior
  is unchanged.
