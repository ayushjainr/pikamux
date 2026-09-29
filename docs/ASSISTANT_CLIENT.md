# One assistant, Windows as a client

For host commands, memory, sharing, spending, and recovery, see the
[assistant guide](ASSISTANT.md).

Windows attaches to the existing assistant on one explicitly chosen paired
macOS/Linux host. It does not run a second assistant, open a network listener,
or select another paired machine when the chosen host is unavailable.

## Connect

1. Pair the intended host using `pika setup SSH_HOST` on Windows.
2. Use `pika status` on Windows for its full node ID. On the intended host,
   `pika pika --json` reports the assistant's immutable `profile_id`.
3. On Windows, bind those exact identities:

   ```text
   pika pika --bind-node NODE_UUID --profile PROFILE_UUID
   ```

4. Run `pika pika`, or press `P` on the Windows board. A Windows Terminal window
   opens the same host-owned assistant over authenticated SSH. The selected
   board row does not select an authority or authorize sharing its contents.

Binding probes only read the host's existing presentation cache; they do not
start its assistant or call a model. Opening the interactive view attaches to
the existing host authority under its normal lifecycle rules. The host checks
both identities again before accepting interaction. A launcher receipt means
only that the window opened; inspect that window for successful attachment.

## Offline and drafts

`pika pika --json` refreshes cached presentation when available. If unavailable,
it returns an explicit offline state and a nonzero exit code. `cached_at` is when
the client last fetched the copy, not the source's freshness; the presentation
retains its own date. No alternate host or local assistant is started.

```text
pika pika --draft "Text to keep locally, not send"
pika pika --clear-draft
```

Drafts are never submitted automatically, including after reconnection. Copy a
draft into the connected assistant yourself when ready. Copy any draft you want
to keep, then clear it before rebinding. Approval/action flags are not accepted
by the offline client. Its private cache contains only the exact binding,
bounded presentation, and optional draft—not pairing tokens or provider state.

Changed SSH routes require explicit rebinding; removed pairings fail closed.
Concurrent windows cannot overwrite a newer binding or draft with stale state.
The initial client entry uses the personal scope. iOS, public transport, and
native Windows provider hosting remain out of scope.

## Validation boundary

Hermetic tests exercise command routing, exact identities, cache/draft behavior,
and concurrency using fake hosts, plus the actual binary's read-only identity
probe against disposable databases. A real Windows Terminal/SSH attachment to an
approved host remains a separate manual compatibility check; these tests do
not measure model usefulness or live reliability.
