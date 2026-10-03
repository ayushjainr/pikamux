# Native pairing evidence — 2026-10-03

This is disposable endpoint validation, not physical camera or real Tailscale
routing evidence. No user keys, credentials, transcripts or fleet machines were
used. The DEBUG descriptor-import button is not shipped in Release.

## Passed native journeys

- `qr-ui-03.xcresult`: invalid QR and manual address/cancellation, 2/0;
  `qr-screen-04.xcresult`: QR-primary/collapsed-manual screenshot, 1/0.
- `qr-actual-01.xcresult`: compact bootstrap → pinned TLS descriptor/claim →
  phone-generated Ed25519 key → real ordinary SSH → real Rust `_mobile` → exact
  node `de393633-1ab4-48d8-bd1e-b8c9f80e7bfd` and `Paired Synthetic Board` → saved
  relaunch, 1/0. No host-key confirmation fallback was shown. Runtime warnings
  were empty in all three bundles.
- `qr-lost-receipt-01.xcresult`: server durably enrolled the key then dropped the
  `/pair` receipt and closed the pairing listener. The phone recovered through
  exact-pinned SSH with that same key and reconnected after relaunch, 1/0,
  runtime warnings empty. This tests automatic read-only possession checking;
  it does not yet test the explicit Finish button after an SSH outage.
- `qr-wrong-tls-01.xcresult`: only the QR TLS digest was changed; actual
  URLSession rejected the server identity, no staged key or saved machine,
  1/0, runtime warnings empty.
- `qr-expired-01.xcresult`: genuine pinned descriptor returned an expired
  authorization window; rejected before staging/claim, no saved machine, 1/0,
  runtime warnings empty.
- `qr-wrong-node-01.xcresult`: production claim and SSH succeeded, but `_mobile`
  returned a different node. No machine was saved; the provisional intent stayed
  available until explicit discard with a no-revocation warning, 1/0, runtime
  warnings empty. Backend independently observed exactly one descriptor/claim,
  one added key, and unchanged original authorization-file sentinel.

Bundles and matching raw logs are under `/tmp/pika-ios-transport.b4aP3S/`.
Actual board attachment:
`qr-actual-01-attachments/73C086DC-D1E8-4C24-8FBE-802EC8820330.png`.

Device: owned iPhone 18 Pro Max Simulator
`98ECDE03-E8B5-4E7C-ABBE-DBC0C5935D27`, iOS 27.0 (24A434), Xcode 27.0.
Explicit `DEVELOPER_DIR` and disposable HOME/XDG/build/package directories were
used; the user's signing-team settings were preserved.

## Fixture boundaries

Owned endpoint `/private/tmp/pika-qr-native.ztVKSY`, SSH loopback port 59383.
The temporary sshd uses `StrictModes no` only because OpenSSH refuses the
world-writable `/private/tmp` ancestor. Production authorization-file safety
checks remain enabled. There is no global ForceCommand. The pairing fixture's
per-key test wrapper accepts only `_mobile` and restores disposable TMPDIR before
executing real Pika: macOS overwrote sshd's SetEnv TMPDIR during the environment
probe. HOME, provider, database, Pika, XDG and tmux paths were isolated. The probe
key was removed before phone enrollment, preserving the original comment sentinel.

## Historical failures and remaining checks

`qr-build-01` failed a Swift continuation type diagnostic (fixed);
`qr-ui-02` failed manual-button visibility after the new disclosure (bounded
native test scrolling fixed; `qr-ui-03` passed). Backend held server 95218 stopped
before QR issuance on ACL-absence handling; no phone key was installed by it.
Backend integration diagnostics also retained: check 51170 (`getrandom::Error`
context conversion), seed 51154/90259 (test metadata/environment), keys 81623
(`/tmp` fixture alias; canonicalized without guard changes), and held 95218/93923
(Darwin ACL absence returns ENOENT; repaired with positive filesec query, not
relaxed permission checks).

Backend owner reported clean gates: keys 17756 3/0; pairing 7 passed/1 ignored
(0.10s); managed grant first→next executable cutover (0.14s); formatting passed;
Clippy 49721 passed (9.89s); complexity 69901 passed (4572/137/0 failures).
Independent reviewer 50159 checked expiry/slow-client/single-use fences; 82153
proved per-key forwarding denied even with global sshd forwarding allowed.
These are backend proofs, not physical camera, Tailscale-network or deployed
system-sshd evidence.
All requested local positive/negative protocol journeys passed. Explicit Finish
after an SSH outage is a distinct unverified recovery interaction; lost-receipt
same-key SSH recovery itself passed. All foreground pairing servers exited, and
owned sshd listener 67208 was stopped; port 59383 had no remaining sockets.
Physical camera scanning, physical-device installation and actual Tailscale
connectivity remain unverified.

Signed generic iOS Release build `qr-device-release-01.log` stopped at signing:
Xcode under disposable HOME reported no iOS Development certificate/private key
for the preserved team `9N2YHJBG86`. A subsequent normal-user read-only
`security find-identity` reported one valid Apple Development identity. Thus this
is a build-context failure, not proof the user's certificate is missing.
Normal-HOME signed retry `qr-device-signed-normal-01.log` then passed with the
same team, no provisioning flags or keychain edits. `codesign --verify --deep
--strict` passed; arm64 Release app is 17 MiB on disk (not App Store download
size). The Release executable has no DEBUG descriptor-import label/environment
key. Camera usage description is included. Existing nonfatal orientation and
unused AppIntents metadata warnings remain. No device installation was attempted.
Fresh signed device artifact:
`/tmp/pika-ios-transport.b4aP3S/qr-device-release/Build/Products/Release-iphoneos/Pika.app`.
Current QR Simulator app is
`/tmp/pika-ios-transport.b4aP3S/app-build/Build/Products/Debug-iphonesimulator/Pika.app`.

## Physical scan follow-up, October 3

The user's camera screenshot contained separated half-block rows; CoreImage and
Vision both found zero decoded payloads. Earlier reconstructed half-block image
checks did not cover real terminal font spacing and were insufficient evidence.
The renderer now uses full background-filled cells with a four-module quiet zone,
and refreshes only the countdown unless resized. It needs a taller terminal
(the fixture needs 98 columns by 53 rows). The production renderer's output in a
120×65 PTY, rasterized as background cells, decoded to the exact fixture payload
with CoreImage. This is not a physical camera or Terminal.app screenshot test.
The helper required NO_COLOR unset; arbitrary no-color environments remain unverified.

Formatting and all-target warnings-as-errors Clippy passed. The signed Release
iOS build passed and was installed over the existing app without uninstalling.
It adds local-network ATS intent and shows connection progress after scanner
dismissal. A launch attempt was blocked by the phone lock, not a build failure.
rs8 now runs managed preview 0.6.39-rc.2; archive checksums passed and existing
agents were not restarted. Physical scan-through-board acceptance remains pending.

### rs8 handshake isolation

A read-only Mac URLSession probe started an owned 30-second `pika pair` listener
on rs8, verified the exact ephemeral certificate SHA-256 pin, and fetched
`/descriptor`: HTTP 200, 389 bytes. It never sent `/pair` or installed a key;
the listener expired normally. rs8 reports `ShieldsUp: false` and `RunSSH: false`.
This proves Mac-to-rs8 pairing reachability, not the iPhone route or iOS policy.
Administrator-only firewall inspection was unavailable. The phone was locked,
so launching the installed diagnostic app was refused by iOS. Its next failure
will distinguish URLSession error codes and HTTP failures without exposing tokens.

### TLS policy reproduction and correction

The phone reported URLSession error -1200. The same read-only rs8 probe compiled
with the application's original embedded Info.plist reproduced -1200 **after**
the certificate pin matched. Without app ATS metadata it had returned HTTP 200.
A scoped `NSExceptionAllowsInsecureHTTPLoads` exception for 100.64.0.0/10 made
the pinned request return HTTP 200; deliberately supplying the wrong pin still
cancelled it (-999), with no descriptor returned. No /pair requests or key grants
were made in these probes.

Apple documents that this exception is also required to customize HTTPS server
trust, despite its HTTP-oriented name. The app now lists only the private IPv4
and ULA ranges already accepted by PairingBootstrap. HTTPS is still hard-coded,
the exact QR certificate digest remains mandatory, and redirects are rejected.
No global arbitrary-load, weaker TLS version, or forward-secrecy exception was
added. Future URLSession consumers must not assume private-network HTTP is
blocked by ATS. App Store submission needs justification for the exception.

Signed Release build and plist validation passed. Physical iPhone acceptance is
still pending; the Mac reproduction is not a substitute for it.
Reference: https://developer.apple.com/documentation/bundleresources/information-property-list/nsexceptionallowsinsecurehttploads

### Compact terminal display

The full-cell rc.2 renderer was too large in actual use. The next renderer packs
two QR rows into each terminal row. Equal halves are solid background spaces;
mixed halves are white half-glyphs on black, avoiding full-block font bearings.
Explicit grayscale values 16/231 prevent theme tinting and NO_COLOR from erasing
the machine-readable image. Four-module quiet zones and high error correction
are retained. Countdown-only refresh remains unchanged.

A production-renderer PTY capture at 80×40 was rasterized using AppKit's actual
Menlo, Monaco and Courier fonts at 18 points and decoded with CoreImage. A mixed
case 86-character fixture payload passed all three fonts at default line height.
Menlo also passed +2 and +4 pixels of line spacing; Monaco passed +2 but not +4;
Courier failed +2/+4 with this payload. Therefore unusual font/line spacing is
still a known limitation, not a universally solved camera-rendering claim.
The earlier all-uppercase fixture was less representative and is not the final
evidence. Physical camera scanning of the compact version remains pending.
An additional 64-byte nonrepeating deterministic payload encoded to base64url
required 69 columns × 35 QR rows (39 rows with instructions), fitting 80×40.
It passed Menlo/Courier at 0/+2/+4 spacing and Monaco at 0/+2; Monaco +4 failed.
Formatting, all-target Clippy with warnings denied, and the disposable-home
size regression passed. Font-space failures remain recorded rather than hidden.
