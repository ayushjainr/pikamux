# Pika iOS alpha

Native SwiftUI iPhone client, iOS 17+, Swift 6. Uses ordinary SSH over an
existing private network to execute `pika _mobile` without a PTY. Tailscale
connectivity alone does not grant SSH login. No new public listener is needed.

Generate the project with XcodeGen from this directory:

```sh
DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer xcodegen generate
```

Open `Pika.xcodeproj`, choose an iPhone simulator, and build the Pika scheme.
Simulator runs that exercise Keychain need ad-hoc code signing; unsigned builds
are compilation evidence only. A physical iPhone requires the release owner's
Apple signing team and provisioning. Development builds through 44 have been
installed on the owner's physical iPhone; this does not establish a complete
physical-device user journey. See the dated evidence ledger.
Select your Apple team under the Pika target's Signing & Capabilities, leave
automatic signing enabled, then select your paired iPhone. The generated
project does not globally disable signing. Installing on a physical device is
a separate owner-controlled step, not part of the disposable validation.

The validated simulator is the isolated **Pika Pro Max Test**, iPhone 18 Pro Max,
iOS 27.0 (24A434), UUID `98ECDE03-E8B5-4E7C-ABBE-DBC0C5935D27`:

```sh
DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer xcodebuild \
  -project Pika.xcodeproj -scheme Pika \
  -destination 'platform=iOS Simulator,id=98ECDE03-E8B5-4E7C-ABBE-DBC0C5935D27' \
  CODE_SIGNING_ALLOWED=YES CODE_SIGN_IDENTITY=- CODE_SIGNING_REQUIRED=NO build
```

Executable tests must additionally set disposable HOME/XDG and build/package
directories as required by repository contributor instructions.

Pairing is two steps: choose **Connect phone** on the machine, then **Scan pairing
code** on the iPhone, with Tailscale enabled on both. The compact QR pins a
short-lived TLS bootstrap endpoint and single-use capability; it contains no
private SSH key. The phone generates its own dedicated Ed25519 key and stages
it in device-only Keychain before claiming. Success still requires the QR-pinned
SSH host, exact Pika node and first validated board. Pairing's temporary listener
is foreground-only on the machine's private Tailscale address, not a public service.
Automated physical-camera and actual Tailscale journey evidence remains incomplete. A native
Simulator journey imported a disposable compact descriptor, completed real
pinned TLS enrollment and ordinary SSH into `_mobile`, verified the exact node
and seeded board, then reconnected after relaunch with the saved phone key.
Descriptor import is DEBUG-only, not a camera-scan claim. Invalid QR rejection
and the collapsed manual fallback also passed native checks.
Real disposable endpoint checks also passed wrong TLS pin, expired descriptor,
wrong Pika node and lost enrollment receipt with same-key SSH recovery. Exact
logs, fixture boundaries and remaining device checks are in
[PAIRING_EVIDENCE.md](PAIRING_EVIDENCE.md).

An uncertain claim is checked through SSH with the same saved phone key, never
automatically registered again. **Finish previous pairing** retains this recovery
without the original QR. Explicitly scanning a fresh code for the same exact
machine can reauthorize that same key. Discarding an unfinished pairing removes
the phone's key only; it does not revoke a possibly installed machine key.

Manual login remains available under **Manual login**: enter address, SSH username/port, password or
imported OpenSSH Ed25519 key. Compare the displayed SHA-256 host fingerprint
through an independent trusted channel. Credentials enter device-only Keychain
only after successful authentication and exact Pika node verification. Changing
a saved host key or node identity fails closed. Encrypted-key/passphrase and
password authentication need additional device-level validation.

The board retains exact node/provider/thread identities. Normal Return inserts
a newline; Send submits explicitly. Uncertain delivery stays saved and is never
automatically replayed. Provider capabilities come from the connected endpoint;
availability of another provider's icon does not imply supported control.

## Evidence and limitations

`TransportSpike/EVIDENCE.md` records the disposable OpenSSH proof and iOS arm64
dependency compilation; `NATIVE_EVIDENCE.md` records later actual saved-login,
background reconnect, original project question/reply and saved main-assistant
journeys, plus clearly labeled native UI-double regressions. The actual provider
tests use synthetic inference, not model quota. `PikaSSHIntegrationTests`
requires an explicitly scoped disposable configuration build setting; default
runs skip it. Clean actual native Start and one-time command approval journeys
passed through ordinary live SSH, with independently matched original desktop
identities, processes and continuation. Historical protected-SSH-child refusals
and test-only failures remain in the ledger; the reviewed exact OS-process
identity check preserves fail-closed handling of unknown processes. Complete physical-device journeys, actual file approval, Tailscale,
password/encrypted-key and multi-machine/provider validation remain separate
gates. Do not present this alpha as complete.

Citadel is pinned to 0.12.1 with its NIOSSH fork and transitive dependencies.
The client uses NIOSSH's event-loop bootstrap directly because Citadel's
`connect(on:settings:)` calls synchronous pipeline operations off-loop and
crashed in the actual simulator integration. Cryptography and SSH protocol
remain library-owned. Release packaging requires dependency notice/license and
security review, physical signing evidence, and the repository's release gates.

DEBUG-only fixture/import helpers are visually labeled and are not production
connection success paths. Tests must not use real credentials, model quota,
fleet machines, or the user's installed Pika/database/configuration.

## Distribution readiness

A paired, Developer Mode-enabled phone can receive development builds over the
local network; USB is not required for every update. Installation and automated
UI access still depend on the device's connection and lock state. Development
provisioning expires; a successful installation is not indefinite distribution.

TestFlight requires Apple Developer Program access and an App Store Connect app
record, distribution signing, a validated archive, and Apple's build processing.
The inspected Xcode account on 2026-10-09 lists only a Personal Team. Do not
claim TestFlight availability or enroll, accept agreements, or upload on the
owner's behalf without the required account access and publication approval.
See Apple's [distribution guide](https://developer.apple.com/documentation/xcode/distributing-your-app-for-beta-testing-and-releases/).
