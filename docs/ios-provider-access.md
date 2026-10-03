# iOS provider-access feasibility — 3 October 2026

## Result

Installed Codex 0.160.0 has a viable **already-running shared app-server** path.
The isolated installed-provider spike verifies a second client can read/resume
the same running thread, receive a pending `request_user_input` after attaching,
answer it, and steer the same active turn. This is actual provider execution with
a synthetic localhost Responses endpoint, not a real model or phone integration.

An existing embedded/no-daemon TUI has no verified externally attachable socket.
Do not resume its UUID on another server: that creates another owner, even when
the saved thread identity matches. Claude, OpenCode and Muse remain unverified.

## Evidence

`tests/ios_codex_socket_spike.rs` runs opt-in, serially, with disposable HOME,
CODEX_HOME, XDG roots, workspace, socket and database. Authentication/key variables
are absent; only the synthetic loopback model is configured. The test drives
installed Codex rather than a fake app-server. Two independent clients use
WebSocket over one Unix-domain socket. It verifies:

- One loaded thread/active turn, unchanged across second-client read/resume.
- A provider-generated structured question received before the second client
  attaches; the second client receives the identical pending request.
- The phone-side answer produces `serverRequest/resolved` on the original
  client, and the original tool call's answer appears in the next synthetic
  provider inference request.
- Exact-turn steering is accepted; stale-turn steering is rejected.

The production `_mobile` executable passed the installed-provider journey in
1.40 seconds: late question replay/answer, exact multiline steering, endpoint
termination after acceptance without reading its receipt, reconnect, read-only
delivery reconciliation using the original native user item's client ID,
same-ID retry with zero additional user items, and conflicting-ID reuse refusal.
This timing is not product latency. This is not a real model test.

Run from the checkout with an explicitly selected installed provider:

```sh
PATH=/Users/ayushjain/.rustup/toolchains/1.88.0-aarch64-apple-darwin/bin:/usr/bin:/bin \
scripts/with-test-home.sh env PIKA_IOS_CODEX=/Users/ayushjain/.local/bin/codex \
cargo test --test ios_codex_socket_spike -- --ignored --test-threads=1 --nocapture
```

## Narrow access contract

The SSH connection runs one foreground `_mobile` handler. JSONL envelopes are
version 1. Requests are `{v:1,id:UUID,method,params}`; responses contain `result`
or `error` under the same ID; unsolicited events contain `event` and `params`.
An immutable identity is `{nodeId,provider,threadId}`. Human names never route.

Implemented methods: `hello`, `nodes/list`, `board/subscribe`, `projects/list`,
`conversation/open`, `conversation/history`, `conversation/send`,
`conversation/answer`, `conversation/approve`, `conversation/receipt`,
`conversation/candidates`, `conversation/adopt`, `conversation/create`, and
`assistant/open`. Capability gaps
are explicit results/errors, not an invitation to launch a substitute. Create,
adopt and fleet forwarding must reuse their existing exact owners, not a generic
provider-command endpoint.

Selected Codex reads first prove the exact thread is **already loaded** by the
selected running server. Only then may `thread/resume` subscribe the connection.
Reads use metadata-only `thread/read`, `thread/turns/list` and
`thread/items/list`, bounded by selected identity and page size. Do not hydrate
all histories or use a disk thread listing as running-owner proof.

Running replies use `turn/steer` with exact `expectedTurnId` and stable
`clientUserMessageId`; an explicit queued reply uses `thread/queue/add`.
Idle replies use `turn/start` on the same loaded thread without config overrides.
Provider acceptance is not completion. Disconnect after submission is unknown,
never automatic replay. Pika durably journals the stable operation ID before
dispatch and returns prior receipts without replay. Accepted/unknown sends
become delivered only when native history contains the original client ID.
Post-dispatch relay/receipt-write failures return unknown, not rejection.
Creation receipt queries use `{nodeId,clientOperationId}` without an identity.

Question answers freeze node/thread/turn/item/server-request IDs and question IDs.
Respond to the actual JSON-RPC server request with `answers`, not free text or
terminal keystrokes. `serverRequest/resolved` can also mean cleanup, so confirm
the offered request/answer outcome without inventing a success after unrelated
interruption. Native verification-owner elicitations are not ordinary approvals
and must remain unavailable unless separately supported.

## Shared board producer

Use `activity_observer::start` once per connection and independently subscribe
through `activity_feed::Source::subscribe`. Its existing OS observation lease
coordinates with boards: followers consume committed caches rather than scan.
The initial snapshot is cached, not freshly verified. Keep row freshness and
health; do not acknowledge unread work. Closing the last consumer ends owned
workers while provider processes continue. Do not use the return-strip frame as
a transcript API or build a separate mobile observer.

## Main assistant: forward-only shared launch

`assistant_native.rs` persists profile ID, scope, memory epoch, launch token and
the exact native thread in `native-binding.json`. These are the main identity,
not a mobile-created provider thread. The legacy embedded launch uses
`--profile pika-assistant` plus scoped `--config` overrides.
The v0.160 TUI source excludes non-default loader/CLI configuration from implicit
shared-daemon reuse; it chooses an embedded app-server. No supported flag has
been verified for exposing that existing embedded process as a Unix listener.

A separately implemented, explicitly approved forward-only path uses one
Pika-owned private app-server with the same
private provider home, explicit equivalent scoped security/MCP configuration
and launch generation, followed by the native TUI using `--remote unix://PATH`
and the exact durably bound thread. A separate installed-provider test verifies
fresh empty thread creation without an initialization model turn, actual scoped
Pika MCP preference recall, all seven skills, reuse of the same live TUI PID,
and relaunch of the same UUID after killing only the disposable native TUI.
That test measured 8.42 seconds. Mobile attachment still needs its own journey.
The shared launch cannot be
applied by silently restarting an already-running assistant. `daemon start`
does not support `--profile`; flattening/replaying configuration is not assumed
equivalent, especially launch-token-bound MCP environment. No new assistant,
copied credentials or mobile memory authority is permitted.

## Source references

- [Official app-server contract](https://learn.chatgpt.com/docs/app-server)
- [Exact-tag Unix WebSocket transport](https://github.com/openai/codex/blob/rust-v0.160.0/codex-rs/app-server-transport/src/transport/unix_socket.rs)
- [Exact-tag outgoing pending-request replay](https://github.com/openai/codex/blob/rust-v0.160.0/codex-rs/app-server/src/outgoing_message.rs)
- [Exact-tag TUI target/configuration selection](https://github.com/openai/codex/blob/rust-v0.160.0/codex-rs/tui/src/lib.rs)

Generated schemas from the installed 0.160 binary confirm typed question
payloads, turn preconditions, queue identifiers and paginated reads. There is no
`serverRequest/list` method or pending-request array in `thread/read`; pending
requests arrive through subscribed server-request replay.

## Not proved

Actual iPhone/SSH/Tailscale paths; a real model turn; real existing user/fleet
conversation access; desktop UI visibility; no-daemon adoption; ordinary command
approval racing; actual app private main assistant attachment; other providers;
mobile start/adopt/automatic-admission journeys. Lost-send receipt recovery is
proved above; real network failure permutations remain completion gates.
