# Independent iPhone pairing review

3 October 2026. Adversarial review of the approved **Connect phone → Scan QR →
existing board** amendment, not an expansion of Pika's product or account model.

The direction survives the review. A bounded foreground enrollment listener is
necessary setup machinery; the existing SSH `_mobile` command remains the only
durable transport. The repaired implementation preserves exact machine identity,
the phone's original key and uncertain-outcome recovery; no remaining critical
source defect is established here. Five native Simulator pairing journeys now
have independently inspected passing result summaries. Physical scanning and
real Tailscale routing are not inferred from those disposable endpoint checks.

## Agreement and implementation boundary

The baseline is the user-approved 3 October amendment in the private
`pika-planning/ios/PRD.md` and `SYSTEM_DESIGN.md`, reflected in `DESIGN.md:8`.
Normal setup is two user actions: choose Connect phone on the owning machine,
then scan on the phone and reach that machine's board. Manual login remains a
fallback. Pairing creates no assistant, thread, fleet scanner, permanent service,
cloud account or new authority over conversations.

The compact QR contains a private endpoint, ephemeral certificate hash and
256-bit capability. Exact SSH/Pika metadata is obtained over that pinned
connection, rather than putting a large identity document in the visible QR.
Only the phone-generated public key is submitted. Existing SSH access is not
replaced. The token expires; an installed restricted SSH key does not expire
automatically and remains authorized until removed through SSH access control.

## Source judgment

- `src/mobile_pairing.rs:129`, `:183`, `:256` and `:286`: the listener binds a
  validated private literal address for at most 300 seconds, authenticates the
  token, and stops after the first valid grant or **any** enrollment failure.
  Post-write receipt failure cannot leave the token available to enroll another
  key. Invalid unauthenticated requests create no grant.
- `src/mobile_pairing_io.rs:12`, `src/mobile_pairing_keys.rs:147` and `:201`:
  TLS reads/writes use an absolute exchange deadline capped by capability expiry.
  The file lock is nonblocking and bounded; cancellation/expiry is checked again
  immediately before append. Owned regular files, no-follow opening, hard-link
  rejection, ancestor trust, ACL validation and inode/device checks preserve the
  selected authorization target. Existing bytes are preserved; a changed target
  after writing is an unknown outcome, not a safe invitation to retry.
- `src/mobile_pairing_keys.rs` constructs `restrict,command="… _mobile"` with
  both shell and authorized-keys quoting. Canonical Ed25519 parsing excludes
  options, comments and control-character injection through the submitted key.
  A differently authorized existing key is not overwritten.
- `ios/Pika/Connection/Pairing.swift:116`, `:162` and `:191`: the dedicated key
  and descriptor are persisted provisionally before dispatch. TLS certificate
  pinning precedes HTTP body delivery; redirects and mismatched receipts are not
  trusted. A dispatched stage is retained after failure. Finish uses that same
  key on pinned SSH without resubmitting registration. Explicit fresh scanning
  can reauthorize the same key, not silently generate a replacement.
- `ios/Pika/Core/AppModel.swift:160` and `:207`: generation/cancellation and
  foreground fences prevent superseded pairing work. SSH host identity, exact
  Pika node and a valid first board are checked before completed connection
  storage. Cancellation after dispatch does not claim server-side revocation;
  discarding the provisional key explicitly warns that server authorization may
  remain. Scanner start/stop is serialized and late permission callbacks are
  fenced in `ios/Pika/Presentation/PairingScanner.swift`.

No transactional grant-rollback protocol or separate account service is needed
for this explicit user-authorized pairing action. The narrow same-key SSH check
handles a lost response without broadening the product or weakening identity.

## Repaired findings

- **P2, in-scope compatibility defect — default Mac host-key path.** The initial
  default `/etc/ssh/…` traversed macOS's `/etc → private/etc` symlink and was
  rejected by the otherwise intentional ancestor fence. `src/mobile_pairing.rs:50`
  now selects `/private/etc/ssh/…` on macOS and `/etc/ssh/…` elsewhere. This repairs
  the default without admitting arbitrary authorization-path symlinks. Explicit
  alias paths remain conservatively rejected.
- **P2, in-scope trust defect — ACL writes outside mode bits.** Initial checks
  covered POSIX permissions but not Darwin ACL grants. `src/mobile_pairing_acl.rs`
  now establishes positive ACL absence through public filesec APIs, permits
  harmless read/deny entries (including default home deny-delete), and rejects
  mutation grants or lookup errors. Both native allocations are freed. The
  public Intel `lstatx_np$INODE64` ABI annotation is present. This was source
  reviewed; Intel execution is not claimed.
- **P2, in-scope durable-reconnect defect — inactive managed release.** Active
  installation discovery intentionally rejects a still-running old release
  after an update changes `current`. The initial pairing helper treated that
  rejection as genuinely unmanaged and could enroll the retired binary path.
  `src/mobile_pairing.rs:381` and `:397` now refuse fallback when an adjacent
  release receipt or the exact releases-root marker exists, and propagate
  metadata errors. A verified active install still enrolls the stable launcher;
  a genuine unmanaged development binary retains its canonical path. The
  regression now checks both an existing grant following launcher cutover and
  a stale running release refusing new pairing after cutover. Source repair is
  independently confirmed. The provider reports latest-source behavioral
  test32588 passed1/0 in0.11s, Clippy95647 passed in8.96s, fmt passed and
  complexity27213 passed4573/137-existing/0fail. These backend gate results are
  reported rather than independently executed again by this reviewer.

## Independent measured evidence

All execution used disposable HOME/XDG/Pika/provider/database/temporary state
through `scripts/with-test-home.sh`. SSH hosts, keys and listeners were newly
generated and loopback-only. No real transcript, fleet machine, model inference,
installed configuration or user's tmux session was used. The reviewer edited no
implementation source; this report and temporary diagnostic harnesses were the
authorized review artifacts.

| Check | Observed result | What it does not prove |
| --- | --- | --- |
| Production QR renderer through a real 100×40 PTY, reconstructed from its emitted half-block/color cells and decoded by native CoreImage | PASS, exact bootstrap URI; 3,380 captured bytes and 1,225 black-on-white cells | Physical terminal font pixels, camera permission or physical scanning |
| Independent current-backend enrollment, run50159 | PASS: wrong token returned403; exact descriptor; dribbling TLS client bounded by3s and listener remained usable; receipt deliberately unread after valid claim still produced one restricted grant; second distinct key could not reuse token | Native URLSession behavior or the phone's recovery presentation |
| Actual capability expiry, run50159 | PASS:30-second listener expired and closed without an extra grant | Physical background/camera interruptions |
| Per-key scope, run82153 | PASS: newly enrolled generated key authenticated, then `ssh -W` direct-tcpip failed administratively prohibited with server forwarding and PTY globally enabled and no global ForceCommand | Actual PTY/agent/X11 requests; those are source-reviewed restrictions |
| Existing authorization preservation | PASS: unrelated sentinel bytes preserved; exactly one `restrict`/forced-command grant | Every filesystem or concurrent noncooperating SSH-key editor |

The test TLS client checked the exact leaf certificate hash **before sending
HTTP**, including deliberate wrong-pin rejection. This is independent client
proof, not a substitute for an actual iOS wrong-pin test. The forwarding test
used `StrictModes=no` for owned temporary ancestors; no SSH session or `_mobile`
command was executed, so no provider could be reached.

Reproduction records:

```text
scripts/with-test-home.sh /tmp/pika-qr-review.NKDKzO/target/debug/enrollment /Users/ayushjain/.codex/worktrees/test-audit/pikamux/target/debug/pika
run50159: PASS wrong-token/exact-descriptor/absolute-slowloris/lost-receipt/second-key refusal; PASS preserved single restricted grant; PASS actual30s expiry

scripts/with-test-home.sh /tmp/pika-qr-review.NKDKzO/target/debug/enrollment /Users/ayushjain/.codex/worktrees/test-audit/pikamux/target/debug/pika --scope-only
run82153: PASS authenticated enrolled key; direct-tcpip administratively prohibited despite global forwarding enabled
```

The harness and QR diagnostic image remain under `/tmp/pika-qr-review.NKDKzO`;
the service/key homes `/private/tmp/pq.NaWfIY` and `/private/tmp/pq.MAnjlw` were
removed by the disposable wrapper. The concise measured record above survives
that cleanup. Later extraction-only source factoring was independently reread;
it preserves these critical fences and does not justify repeating expiry.

Earlier diagnostic failures are retained, not converted to passes: run81623 had
one key-parser pass and two fixture setup failures because `/tmp` is a macOS
symlink; owned test paths were subsequently canonicalized without loosening
production symlink rejection. Run30681 failed before HTTP because the system
Python's old LibreSSL could not negotiate backend TLS1.3. The corrected network
check used bundled Python/OpenSSL3.5.8; the backend protocol was not weakened.

## Native app evidence inspected afterward

This reviewer independently read all five saved xcresult summaries with
`xcresulttool get test-results summary`. Every bundle reports **1 passed, 0
failed, 0 skipped, runtimeWarnings[]** on the owned iPhone18 Pro Max Simulator.
No native test or protocol proof was rerun for this evidence-only update.

| Bundle under `/tmp/pika-ios-transport.b4aP3S/` | Exercised journey recorded in `ios/PAIRING_EVIDENCE.md` | Bundle elapsed |
| --- | --- | --- |
| `qr-actual-01.xcresult` / run13638 | Compact bootstrap, native pinned TLS claim, phone-generated key, ordinary SSH and real `_mobile`, exact-node board and saved-key relaunch |46.433s |
| `qr-lost-receipt-01.xcresult` / run24678 | Durable grant with receipt dropped, automatic exact-pinned SSH possession check using the same key, saved relaunch |44.887s |
| `qr-wrong-tls-01.xcresult` / run44635 | Wrong QR TLS digest rejected by actual URLSession before staging or saving a connection |27.816s |
| `qr-expired-01.xcresult` / run42007 | Genuine pinned descriptor with expired authorization rejected before staging/claim |18.705s |
| `qr-wrong-node-01.xcresult` / run69804 | Grant and SSH succeed but different Pika node prevents connection storage; provisional key remains until explicit discard |21.283s |

The mechanism and server-observation descriptions are the executor's recorded
evidence, cross-checked against the reviewed native test assertions; this
reviewer did not recreate the endpoints or independently replay their server
logs. Result status/counts/warnings above are independently inspected, not merely
relayed. The exported actual-board screenshot was also independently viewed and
shows `Paired Synthetic Board` with an observed connection and disposable-test
banner. No host-key confirmation fallback is asserted in the successful native
test. Fixture isolation, no global ForceCommand and the per-key wrapper are
documented in `ios/PAIRING_EVIDENCE.md`; independent per-key forwarding proof is
separate above.

The parent reports the full library's parallel run had838 passes,2 timing
failures and6 ignored; both failed tests passed individually. The subsequent
sequential run30783 passed840/0 with6 ignored in65.36s. That broader result is
reported, not independently inspected here, and precedes the last narrow
inactive-managed-release fence. It is not substituted for the separate latest
targeted fence test reported above.

## Reviewed, not proven

Explicit **Finish previous pairing** after an actual SSH outage remains a
distinct unexecuted native interaction; successful automatic same-key recovery
after a lost receipt is now evidenced and must not be called pending. Native
post-dispatch cancellation/outage behavior is source-reviewed, not inferred from
the five journeys. This is an evidence qualification, not a request for a new
grant-rollback protocol or broader credential-management product.

There is still no verified signed physical iPhone camera/Tailscale journey in
this review. A physical device must scan the actual displayed terminal code and
reach the existing board over Tailscale. Prior keyboard/IME/dictation/VoiceOver
and physical interruption acceptance remains separate and is not waived by QR
setup. No completion score or full-product acceptance claim is made here.
