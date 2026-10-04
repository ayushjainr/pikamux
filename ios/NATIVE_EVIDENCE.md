# Native iOS evidence ledger

## Native composer controls — 2026-10-03

Workspace implementation only; these controls are not deployed to existing
0.6.40 server binaries. Older endpoints report controls unavailable; no phone
catalog or unsupported native command is fabricated.

Measured native-provider test: **1 passed in 1.59s**. Log:
`/tmp/pika-composer-native-controls.log`. This uses the explicitly
selected installed Codex executable, a fresh temporary provider/Pika/database
home and a loopback fake inference endpoint. It reads no personal transcript,
contacts no fleet machine and spends no model quota. Provider plugin downloads
and shell snapshots are explicitly disabled in this disposable probe:

```sh
PATH=/Users/ayushjain/.rustup/toolchains/1.88.0-aarch64-apple-darwin/bin:$PATH \
scripts/with-test-home.sh env PIKA_IOS_CODEX=/Users/ayushjain/.local/bin/codex \
cargo test --test ios_codex_socket_spike \
mobile_controls_change_exact_model_and_send_provider_skill_input \
-- --ignored --test-threads=1 --nocapture
```

The actual `_mobile` → installed Codex socket path verifies unopened controls
fail closed, current model comes from the selected provider, model selection
updates only that exact thread and survives reopening, and selection creates
no model turn. `thread/settings/update` can acknowledge before applying its
queued change: the endpoint re-observes for at most three seconds without
retrying the mutation, otherwise retaining an unknown outcome.

The provider's enabled, exact-cwd skill catalog supplies structured
`{type: "skill", name, path}` input. The fake inference request contains the
fixture skill's unique instruction. A nonexistent explicitly selected skill
is rejected before dispatch. Ordinary `$HOME` and `$PATH` text, without
selected-skill metadata, is accepted literally. Removing the fixture skill
after dispatch cannot hide the original accepted receipt on an identical
operation replay. Metadata is separately selected by the picker; arbitrary
dollar words do not cause skill discovery or activation.

Disposable native unit checks: **14 passed, 1 pre-existing isolated-SSH test
ignored**. `cargo fmt --all -- --check`, `git diff --check`, and disposable
`cargo clippy --locked --all-targets -- -D warnings` passed. The native model
picker exposes one bounded provider catalog page and labels additional pages
explicitly.

Separate native simulator result `/tmp/pika-composer-final-02.xcresult`
(log `/tmp/pika-composer-final-02.log`): **10 passed, 0 failed, 0 skipped**;
parsed `runtimeWarnings: []`; executable tests took **297.837s**. Six labeled
UI-fixture composer journeys cover current checked model selection without
a message, typed `/model` and `$` pickers, searchable skill insertion preserving
literal `$HOME`, ordinary `/mnt/...` text sent literally, unsupported/offline
controls, and an unknown model outcome never claiming a setting or permitting
another immediate change. Four existing fixture regressions also passed:
offline draft retention, machine/project paging plus creation receipt recovery,
assistant late approval/history and exact-project return, and unknown-message
receipt non-repetition. These are native UI fixture tests, not live provider,
phone-device or fleet-server claims.
Exported creation screenshot
`/tmp/pika-composer-final-02-attachments/4CE34EF3-3911-4C4A-A825-E5076D8ACE94.png`
was visually inspected: the required-name explanation remains readable above
the native keyboard with the selected machine/project visible.

The earlier combined `/tmp/pika-composer-final-01.xcresult` run was deliberately
cancelled before its SSH test when listener shell-startup HOME isolation required
hardening. It is not a completed test result. Final-02 selects only the labeled
UI fixtures; safe SSH retention/source isolation has its own separate ledger
entry and run. No server binary was published or deployed by these checks.

## 2026-10-03 three separately saved machines over ordinary SSH

`/tmp/pika-ios-transport.b4aP3S/three-machine-05.xcresult`: **1 passed,
0 failed, 0 skipped, runtimeWarnings []**; 143.293 seconds test operations,
not a latency benchmark. The actual Simulator app manually onboarded three
ordinary loopback OpenSSH endpoints with three independently checked host-key
fingerprints and different exact Pika node IDs. All expose the same provider
`codex`, thread ID `collision-thread` and display title `Shared work` to exercise
identity collisions. Actual scoped Keychain credentials and three saved
connections survived process termination/relaunch. All three rows remained,
the machine filter isolated Alpha, and Alpha/Gamma opens and sends reached their
exact endpoints rather than the last-connected Gamma. The Alpha nickname `rs6`
survived another process restart, and the Pika chooser selected all three named
machines explicitly. Taking Beta offline preserved its cached row and left
Alpha/Gamma connected; Alpha remained readable.

These endpoints are explicitly labeled protocol doubles served by
`scripts/ios-three-machine-fixture.py` through actual SSH exec. They are **not**
Rust `_mobile`, a provider continuation, a real model, Tailscale, live fleet,
or physical-phone proof. Independent endpoint-owned JSONL logs recorded exactly
one Alpha send, zero Beta sends and one Gamma send, all with the receiving
endpoint's exact node/provider/thread identity and distinct client operation IDs.
Alpha received `Exact destination Alpha`; Gamma received
`Exact destination Gamma`; no offline retry was sent. Evidence/config/logs:
`/var/folders/k9/s1xh63d93rq9bd97cngqvf4c0000gn/T/pika-three-ssh-ibr3x7pc`.
Keys are retained only in this generated fixture root; do not publish them.

Isolation caveat for 05: the protocol Python processes ran with an empty
environment and disposable HOME, and no provider or personal transcript was
opened. The initial sshd account shell startup lacked `SetEnv HOME`, so this run
does not establish full shell-startup isolation. The reusable harness now sets
sshd's startup HOME and explicit XDG/provider/Pika/database/temp/tmux roots.
The frozen-source retained-store follow-up must use corrected owned listener
configuration; later source changes are not credited to 05 without that rerun.

That separate final-source check passed:
`/tmp/pika-ios-transport.b4aP3S/three-machine-retained-02.xcresult`, **1 passed,
0 failed, 0 skipped, runtimeWarnings []**, 38.150 seconds test operations.
The retained owned Alpha/Gamma listeners were reloaded with verified disposable
HOME/ZDOTDIR and all XDG/provider/Pika/database/temp/tmux paths before this run;
`PermitUserRC no` and `PermitUserEnvironment no` were explicit. Their existing
host keys, node IDs, saved phone credentials and ports were unchanged.
Beta remained offline. This was saved-store regression, not another onboarding.

The 02 source retains `rs6` and restores Alpha/Gamma connections. The Dex
capture shows neutral `Cached · Ready` badges on all three rows despite the
connected status checks and green lens; it is not accepted as steady-state
proof that online card badges are accurate. The test captured immediately after
dismissing connections and checked only that a cached badge existed, not each
row's settled badge. Scoped raw source boards contain fresh non-stale Alpha/Gamma
observations; the rendering discrepancy was subsequently fixed and validated
separately below.
The fixture held the original Alpha open response,
closed only Gamma's owned exec channel, independently observed a newly
authenticated Gamma hello, then released the original Alpha response. Its exact
context rendered rather than being discarded by the unrelated reconnect.
A one-shot nil-node Gamma `connection/error` frame was independently marked
emitted; Alpha's original composer remained send-capable and its exact reply
was delivered. Endpoint log deltas were Alpha +1 send, Beta +0, Gamma +0;
after 02 the retained logs contained Alpha 2, Beta 0, Gamma 1 sends, all bound to the
receiving node's exact identity. The Gamma event is a protocol-fixture injection,
not a naturally occurring fleet failure or provider proof.

Final native images in `three-machine-retained-02-attachments`:
`1A9FDDB5-21CC-40DA-980B-CD3035894A41.png` (Dex capture with unresolved online-badge discrepancy),
`4EEF1075-103C-4A41-99D1-441F347525D1.png` (saved connections),
`497CA457-7DF0-4B28-8014-D87DC4E13E1F.png` (exact Alpha reply after overlap/error).
Owned harness session 92487 stopped cleanly, exit 0. `lsof` confirmed no
listeners remained on 59431, 59432 or 59433; earlier harness sessions 10576 and
69649 had also stopped cleanly. Generated keys/config/logs and result bundles
remain in disposable evidence roots; no installed Pika, user SSH policy, fleet
machine or phone was changed.

Final visual/routing check after flattening the board's state-grouped lazy row
identity: `three-machine-retained-03.xcresult`, **1 passed, 0 failed, 0 skipped,
runtimeWarnings []**, 41.241 seconds test operations. The same generated
credentials, exact node IDs and saved store were retained; only owned
Alpha/Gamma listeners were restarted, with the corrected startup and runtime
environment isolation preserved. The test now waits for each exact rendered
observation Text (`boardFreshness-<exact identity>`): Alpha/rs6 and Gamma must
say `Observed`, Beta must say `Cached`. All predicates passed, and native visual
inspection confirms green Ready cards for rs6/Gamma, a gray Cached · Ready card
only for Beta and the matching green lens. The nested group identity had allowed
stale lazy-card presentation; the final board uses one attention-ordered exact
identity list, rather than accepting a longer timing wait as its fix.

The same final run repeated the held Alpha response across Gamma reconnect,
one-shot unrelated Gamma error and exact Alpha reply with independent endpoint
delta Alpha +1, Beta +0, Gamma +0. Retained log totals after 03 are Alpha 3,
Beta 0 and Gamma 1 sends. Final native images in
`three-machine-retained-03-attachments`:
`896E8625-F3DF-4994-9DDC-A16E18D38982.png` (settled exact live/cached Dex),
`C355A229-C290-4EB1-96B4-CF56453EA1CC.png` (connections), and
`37CF921D-7CBD-44AD-96AA-E30E85ADF3B4.png` (exact Alpha reply).
Resumed owned-listener session 65182 stopped cleanly, exit 0; `lsof` again
confirmed no listeners on 59431, 59432 or 59433. No fixture listener is left live.

Native screenshot directory: `three-machine-05-attachments` under the result
root above. Dex with persisted `rs6`:
`1783E84C-96C7-4112-B1DE-A87A6496E800.png`; actual routed Alpha reply:
`E828AEF2-F011-422C-9BEF-0449D9882ED7.png`; saved connections showing Beta offline
and both other nodes connected: `156DF9A6-1503-4774-A184-2443323F0B42.png`;
explicit Pika chooser: `50644DCE-C16A-4730-BB38-97369B5F5695.png`.

Earlier attempts are preserved and not counted as passes:

- `three-machine-01.xcresult`: build failure because the new composer component
  was not registered in the Xcode project; no journey ran.
- `three-machine-02.xcresult`: build succeeded but the new test config was not
  forwarded by the shared scheme; one test skipped, zero passed.
- `three-machine-03.xcresult`: actual onboards/restarts/filter/exact sends and
  Beta-offline/other-two-connected assertions were reached, then the app crashed
  during a refused reconnect. The crash was an unfulfilled authentication
  promise destroyed after bootstrap failure; SSHWire now completes that failure
  path explicitly. Exported crash: `three-machine-03-attachments/`
  `F601C49D-2D01-412C-843E-AA3165160F26.ips`. Overall result: failed.
- `three-machine-04.xcresult`: failed locating the native nickname alert field;
  UIKit dropped its SwiftUI accessibility identifier. The verified alert's exact
  placeholder now supplies the test locator. No app crash was recorded, and the
  failure is not credited as a complete journey.
- `three-machine-retained-01.xcresult`: failed a brittle assertion that an
  injected Gamma error's shared global notice must remain visible. It reached
  neutral cached Beta and the held Alpha response across an actual Gamma
  reconnect; an offline Beta retry's connection-failure notice was visible at
  failure. The final test instead requires the independent error-emitted marker,
  original Alpha Send capability and exact endpoint delivery; production source
  was unchanged between these two retained runs. The failed run sent nothing.

## 2026-10-03 Start and Pika release recheck

`/tmp/pika-start-assistant-01.xcresult`: four native UI journeys passed, zero
failures/skips: machine/project selection and uncertain creation reconciliation;
Pika older history/pending approval and return to exact project; reopening the
same assistant retains the original request; a delayed assistant response cannot
replace a newer selected project. These are labeled UI fixtures.

Separate installed Codex + synthetic inference checks passed on current backend:
creation/lost receipt/one exact UUID/first reply/same desktop TUI (1.77s), and
private assistant shared launch/original reply/retained MCP preference (9.02s).
Logs: `/tmp/pika-start-real-provider.log`, `/tmp/pika-assistant-real-provider.log`.
No fleet host, real model quota or user transcript was used. Previous full native
SSH journeys remain recorded below; this rerun does not claim a new phone-to-fleet
test or a model-invoked memory save.

## 2026-10-03 thread header spacing polish

Kept the 50pt header; reserved a fixed left signal bay, separated the identity
from the curved seam, softened metadata and increased the provider mark to 32pt.
`/tmp/pika-header-polish-01.xcresult`: native Dex/thread/keyboard/draft/send/swipe
fixture journey passed (1 passed, 0 failed). Inspected the native screenshot.
Signed Release build passed; this is a visual update, not new transport evidence.

## 2026-10-03 aligned casing and tail send

Native simulator journeys passed: `/tmp/pika-aligned-01.xcresult` (Dex →
thread → multiline keyboard → swipe back → separate drafts → tail send →
Pika tab), and `/tmp/pika-aligned-rotation.xcresult` (rotation preserves draft
and sending). Each reports 1 passed, 0 failed, 0 skipped. These are disposable
UI fixtures, not new proof of remote provider transport. Actual screenshots
were inspected for the joined red corner/rim, aligned main header, compact
curved thread header and enabled/disabled tail send control.

Signed Release build passed with the existing orientation warning. Installed
on the connected iPhone without uninstalling or resetting stored connections:
installation `40804A42-1322-43BD-93ED-55878485E03A`, database sequence 4500.
This install was not itself a new physical-device journey test. No changes to
thinking/progress rendering: the user clarified that comment was a typo.

The older evidence and limitations below describe their respective runs.

Latest clean evidence covers actual native saved login/background project
reattachment, original structured answer/reply, Main Pika, exact existing Add,
ordinary-SSH Start and original command-once approval. The final app is left
running in normal empty onboarding on the isolated Simulator, not a fixture.
Physical signing/install, real Tailscale, password/encrypted-key and actual file
approval remain unverified. Failed historical runs below are not passing tests.

Simulator: isolated Pika Pro Max Test, UUID
`98ECDE03-E8B5-4E7C-ABBE-DBC0C5935D27`, iPhone 18 Pro Max, iOS 27.0
(24A434). Xcode 27.0 (27A266a), explicit developer directory; no global
developer-directory change. Swift 6 strict concurrency.

## Actual app → SSH → Rust → original provider

2026-10-03 run session `11820`, exit 0. Result bundle:
`/tmp/pika-ios-transport.b4aP3S/ssh-integration-06.xcresult`.
Summary: **1 passed, 0 failed, 0 skipped, runtimeWarnings []**.
Test operation measured 34.545 seconds (not an app latency benchmark).

The installed native app manually entered loopback address/port/SSH username,
imported a generated disposable Ed25519 key, displayed a SHA-256 host key and
required explicit independent verification. Actual ad-hoc signed Simulator
Keychain saved credentials after authenticated SSH exec and exact node hello.
The app read original context, answered the original pending structured
question with Proceed, then explicitly sent:

```text
Exact synthetic mobile reply
second line
```

Independent fixture owner reported the original Codex app-server PID **12413**
unchanged, thread **01a10007-f1e6-7cc3-95cf-633c5df98d35**, original tool
continuation asserted Proceed, and:

```text
PIKA_IOS_FIXTURE_DELIVERED exact multiline input reached the original provider inference for 01a10007-f1e6-7cc3-95cf-633c5df98d35
```

The provider streamed synthetic assistant text: `Fixture received the exact
multiline reply.` No actual model account, quota, fleet machine, or user
credential was used. This app journey did **not** independently count native
history duplicates; the provider owner's separate production-handler test did.

Keyboard-visible attachment:
`/tmp/pika-ios-transport.b4aP3S/integration-06-attachments/C5EDCEA4-9710-4B5F-B8ED-56C58DD25474.png`.
It displays exact multiline text, native iOS keyboard, explicit Send, original
conversation identity, and visibly labeled synthetic integration context.

Invocation from `ios/` used disposable HOME and XDG paths:

```sh
env DEVELOPER_DIR=/Applications/Xcode.app/Contents/Developer \
  HOME=/tmp/pika-ios-transport.b4aP3S \
  XDG_CONFIG_HOME=/tmp/pika-ios-transport.b4aP3S/config \
  XDG_CACHE_HOME=/tmp/pika-ios-transport.b4aP3S/cache \
  xcodebuild -quiet -project Pika.xcodeproj -scheme Pika \
  -destination 'platform=iOS Simulator,id=98ECDE03-E8B5-4E7C-ABBE-DBC0C5935D27' \
  -derivedDataPath /tmp/pika-ios-transport.b4aP3S/app-build \
  -clonedSourcePackagesDirPath /tmp/pika-ios-transport.b4aP3S/xcode-packages \
  -resultBundlePath /tmp/pika-ios-transport.b4aP3S/ssh-integration-06.xcresult \
  -only-testing:PikaUITests/PikaSSHIntegrationTests \
  PIKA_SSH_TEST_CONFIG=/tmp/pika-ios-transport.b4aP3S/integration.json \
  CODE_SIGNING_ALLOWED=YES CODE_SIGN_IDENTITY=- CODE_SIGNING_REQUIRED=NO test
```

sshd session `99927` stopped, exit 130; `lsof` confirmed no listener on 59273.
Held provider fixture session `79130` stopped via marker, exit 0; its owner
confirmed child provider killed/waited and temporary root removed. Generated
SSH keys and build/evidence files remain only in the disposable evidence root.

## Current native regression evidence

`ssh-integration-08.xcresult` passed the current actual app → ordinary OpenSSH →
Rust `_mobile` → installed original Codex/synthetic inference chain: one passed,
zero failures/skips/runtime warnings, 54.491 seconds test operations. It includes
manual independent pin verification, actual Keychain saved-login process
relaunch, foreground reattachment after background (three independently logged
authenticated SSH connections), original Proceed question, exact multiline
reply and original final assistant response. Original thread:
`01a10048-7c3c-78d0-83a9-badcd5d70149`, provider PID `49925`, fixture `99023`.
No actual model quota was accessed. Keyboard image:
`integration08-attachments/3EA293E9-13AD-46EF-88F2-C946F4555070.png`.

The later host-side split adoption attempt was not part of that passing test.
Even after Simulator shutdown, normal desktop untrack failed closed: PID 50173
was our disposable OpenSSH listener with protected/partial process arguments.
The fixture assertion cleaned the provider/root, so no native Add pass is
claimed. Listener session `32462` was subsequently stopped (exit 130), with no
59273 listener remaining. The safety guard was not bypassed.

On the same isolated iPhone 18 Pro Max Simulator, `ui-smoke-05.xcresult`
passed 11 fixture-native journeys, zero failures/skips/runtime warnings
(287.682 seconds test operations). These cover multiline drafts and explicit
Send, offline preservation, unknown non-replay and paginated read-only receipt,
machine/project paging and uncertain creation receipt, older context, original
command/file approval details, resolution-before-response, assistant/project
return isolation and a superseded assistant open. These use a clearly labeled
DEBUG UI double, not an actual provider.

`ui-followup-01.xcresult` passed two focused journeys, zero failures/skips/runtime
warnings (43.327 seconds): the same existing assistant reopened through Back →
Open Pika retains the original pending file request, and native rotation retains
multiline draft/caret typing and enabled Send. The first landscape attachment
captured a transition and is not accepted as steady-state visual proof.

`ui-visual-dark-largest-02.xcresult` passed one native journey (22.723 seconds)
with dark appearance and maximum accessibility Dynamic Type. Editor/message
text retains native sizing; only informational fixture chrome is capped.
Keyboard attachment: `dark-largest02-attachments/189C9E4D-463B-4CD7-91C3-66B4FB616D6A.png`.
Simulator appearance/content size restored to light/large after this run.

`ui-rotation-02.xcresult` passed the focused steady-state native rotation check
(22.782 seconds). `ui-scroll-01.xcresult` passed two focused journeys, no
failures/skips/runtime warnings (63.155 seconds): long history opens at recent
context (including preloaded assistant history), incoming output follows at the
bottom but does not steal older reading position, and older-history loading
preserves the original anchor. Compact landscape removes redundant composer
labels without shrinking editor text. Its keyboard remains physically tall;
the visible conversation region is narrow, not claimed ample context.
Latest landscape attachment:
`scroll01-attachments/EC657CDB-52D9-45A7-94B8-5939547507F1.png`.

All result bundles and attachments above are under
`/tmp/pika-ios-transport.b4aP3S`. These results do not establish physical-device
signing, actual Tailscale connectivity, encrypted-key/password interoperability,
or IME composition behavior on a real keyboard/device.

## Failures preserved, not counted as success

- `ssh-assistant-01.xcresult`: failed at composer existence after real pin/login,
  Keychain saved-login relaunch and Main Pika → Open. No user reply was sent.
  Fixture owner reproduced the exact same-environment read-only backend error:
  declared original assistant UUID was not loaded on its exact private shared
  server, while original native TUI PID 52386 was alive. This is not claimed
  mobile assistant integration success and is not attributed to SSH without
  evidence. Scoped sshd `84229` stopped (exit 130); no 59273 listener remained.

- `ssh-integration-07.xcresult`: all earlier assertions reached original
  onboarding, saved-Keychain relaunch, background reconnect (three actual
  authenticated SSH connections), Proceed answer, exact multiline reply and
  final original assistant response. The later adoption marker timed out:
  fixture-owned normal desktop untrack failed closed on protected partial
  process arguments; the later split attempt identified our own disposable
  OpenSSH listener PID 50173 as the blocker, not a Simulator safety bypass.
  Its assertion cleaned the original provider/root
  before `adopt-removed`. Therefore the overall test is failed, not a native Add
  pass. Fixture session `52497` exited 101; its owner confirmed cleanup.
  sshd session `1078` stopped, exit 130; no 59273 listener remained.

- `ssh-integration-01.xcresult`: Citadel `connect(on:settings:)` crashed at
  off-event-loop synchronous pipeline initialization, before credentials.
  Replaced only its bootstrap/session path with library-owned NIOSSH bootstrap
  on the actual event loop and explicit bounded authentication completion.
- `ssh-integration-02.xcresult`: unsigned Simulator Keychain write failed;
  no insecure storage bypass. Ad-hoc Simulator signing fixed actual storage.
- `ssh-integration-03.xcresult`: signed UI-test target needed generated Info.plist.
- `ssh-integration-04.xcresult`: original conversation access failed closed
  because backend incorrectly required unrelated global process inventory.
  Backend owner changed to exact kernel-peer PID validation and re-tested.
- `ssh-integration-05.xcresult`: original context and pending question rendered;
  test option lookup mismatched combined accessibility label. Explicit original
  question/option identifier fixed the test without changing provider behavior.

Initial fixture-native suite: `ui-smoke-01.xcresult`, four passed; not provider
proof. Its composer update warning was corrected. `ui-smoke-02.xcresult` failed
compilation before tests; not a pass.

Physical iPhone signing/install, actual Tailscale path, password/encrypted-key
interoperability, actual file approval (not offered by the installed fixture),
non-Codex providers and remote fleet routing remain unverified. Reconnect and
saved login have actual SSH/provider evidence above; older history, paging and
permission presentation additionally have labeled native UI-double evidence.
Creation/admission and final updated-source regression are recorded separately
as their bounded runs finish; later changes require proportional revalidation.

## Actual saved assistant and unsigned device build

`ssh-assistant-02.xcresult` passed one actual native journey with zero failures,
skips or runtime warnings (48.918 seconds): manual SSH/pin, saved Keychain
relaunch, Main Pika, exact original reply `Keep my synthetic preference`, and
rendered `Original assistant received the exact mobile reply.`. The legitimate
saved profile `2750a8ce-301d-42b0-bbe6-109833116fb8`, original conversation
`01a10051-526c-79f2-b991-49ac2a6a8de6` and native PID 52386 stayed unchanged.
The first failed run was recovered only after completing the original desktop
fixture's verified generated-hook trust prompt; no replacement conversation or
resume fallback was used. Screenshot:
`assistant02-attachments/CCA5440D-0466-4A6B-96F0-0EB5D7ED5B71.png`.
The held fixture's later maintenance-inference postcheck failed; it is not
credited as post-phone memory proof. A separate corrected backend run `17555`
passed 8.83 seconds with actual MCP preference retention before/after mobile
interaction; it is distinct evidence, not a model-invoked memory-tool claim.
Scoped listener 27310 stopped, and port 59273 was verified absent afterward.

Unsigned generic iOS arm64 Release build `54306` completed exit 0. Its app bundle
occupies 16,320 KiB on disk; executable is 16,454,600 bytes. These are local
uncompressed build measurements, not App Store/download size or signing/install
evidence. Xcode emitted an orientation advisory; this iPhone-only target supports
portrait and both landscapes and makes no iPad multitasking claim.
After lifecycle, typed-rejection wording and live-header updates, incremental
unsigned Release build `4226` passed: 16,304 KiB bundle and 16,440,152-byte
executable. Debug generic Simulator compile `55295` also passed without running
the Simulator or provider. Final-source unsigned arm64 Release build `86569`
also passed, including the header accessibility identifier: 16,308 KiB bundle,
16,442,872-byte executable. No signing or physical installation is claimed.

Creation preflight failures retained: `ssh-create-01.xcresult` skipped because
the explicit fixture configuration was passed as shell environment rather than
the scheme's build setting; no app mutation. `ssh-create-02.xcresult` and
`ssh-create-03.xcresult` failed before creation at validated board/login: the
fixture's state directory was 0755 rather than required owner-only 0700, and
`board/subscribe` correctly rejected it. The second test additionally requires
affirmative validated connection before process death; button disappearance
alone is not accepted as saved-login proof. No Start, new UUID or model input
occurred in those failed runs. No production safety guard was relaxed.

`ssh-create-04.xcresult` failed before authentication at host-prompt existence.
Connection-form disappearance cancellation was moved to the actual parent
sheet dismissal, retaining explicit active-attempt Cancel/background cleanup.
`ssh-create-05.xcresult` subsequently passed real host verification, validated
board/save and saved-login relaunch. The host terminated only verified listener
60551; authenticated app 63137 and SSH children 63150/63152 remained connected,
and native machine/project selectors succeeded before Start. Actual Start was
definitively rejected: durable operation
`B96543D9-80B2-412C-8C1F-9F9AEB0D6682` had state `rejected`, identity null, because
the existing desktop global process-inventory guard could not read those exact
protected SSH children's arguments. No provider was launched or ownership
cleared. Overall native test failed (77.458 seconds); no full-chain creation
pass is claimed. App presentation now distinguishes this definitive rejection
from uncertain delivery and exposes original error only in Details. No guard
bypass or repeat operation was attempted. Simulator was shut down and port
59273 verified completely unused after teardown.

`ssh-adopt-01.xcresult` passed one actual native Add journey, zero failures,
skips/runtime warnings (73.318 seconds). A separate actual backend created and
delivered first reply to `01a10074-2359-70c3-b109-a42f2ada0dc1`, then normal
desktop CLI untracked it before starting SSH/Simulator. Phone manual pin,
validated login, saved Keychain relaunch and Add existing admitted that exact
candidate and rendered both original `First actual phone-created reply` and
`Actual phone-created reply received.`. Fixture owner independently observed
watched managed state restored, same provider PID 63847 and native TUI PID 63943;
the phone did not send another message or launch a replacement. This proves
native admission, not native Start. Listener 44338 stopped (exit 130), and port
59273 was completely unused afterward.

`ui-final-focused-01.xcresult` passed three current-source native UI-double
journeys, zero failures/skips/runtime warnings (66.360 seconds): inspecting and
canceling machine settings keeps the established event feed alive; original
request resolution before its RPC response preserves truthful closed wording
and permits unrelated new text; observed resolution wins a later unknown
read-only request-status response. These are fixtures, not provider proof.
The isolated Simulator was shut down after the run. A later presentation-only
live-header change follows the same exact board identity; its narrow native
revalidation is recorded after the parent release gate completes.

`ui-live-header-01.xcresult` passed the final focused native UI-double journey,
zero failures/skips/runtime warnings (67.895 seconds). After inspecting and
canceling machine settings, the existing event feed still delivers a same-exact-
identity board update. The open conversation's header name/status, composer
description and Send accessibility label update without changing identity,
draft key or action routing. The isolated Simulator was shut down afterward.

`ssh-create-06.xcresult` passed the native ordinary-SSH Start assertions, zero
failures/skips/runtime warnings (82.450 seconds): manual pin, validated save,
Keychain relaunch, real machine/project selectors, Start, new identity marker,
exact first reply and final synthetic response. Listener stayed running; no
pause/workaround was used. The reviewed backend collector independently uses
exact stable kernel process path/UID/start identity for trusted OS SSH
supervisors; unknown processes still fail closed. The held fixture's later
postcheck failed because it incorrectly required the empty unowned seed thread
to remain loaded alongside the new thread. It cleaned the root before the
final screenshot; the app correctly shows cached provider-ended state there.
The scoped phone journal records created operation
`B14D748E-264C-4AA6-B008-E473462C141B` with original new identity
`bf81b0e2-7b81-431d-993b-9cc4ad2b388a/codex/01a1008f-c92c-78c1-8869-9cb57af0ce7a`.
This failed fixture invariant is not credited as independent desktop-owner
proof. Listener 55398 stopped (exit 130), port 59273 unused, isolated Simulator
shut down afterward. Later corrected fixture evidence is recorded separately.

`ssh-create-07.xcresult` passed the corrected final whole native Start journey,
zero failures/skips/runtime warnings (81.308 seconds). Normal SSH listener
remained running throughout. New conversation
`01a10094-b71a-7860-a5d7-22442d2bc94b` received exact original first reply and
returned the final synthetic text in the app. Fixture owner independently
verified native TUI PID 91640, original provider PID 90631, matching native
history/inference, original desktop pane final response, unchanged PID generation,
persisted seed identity and no unexpected loaded UUIDs. Screenshot completed
before cleanup: `create07-attachments/3BB1AB71-E307-4720-95B6-042FF5FCBB61.png`.
Listener 30692 stopped (exit 130), port 59273 unused, owned Simulator shut down.

`ssh-approval-01.xcresult` is a failed automated run (80.659 seconds), not a
passing test: after verifying exact original context, command
`/bin/zsh -lc 'printf synthetic-approved'`, reason and enabled Allow once, the
test tapped it and then queried `isEnabled` on a request button already removed
by real native resolution. XCTest rejected the nonexistent-element query.
Its full failure hierarchy independently contains both truthful original-
request-closed wording and final `Approval journey finished.`. The test now
short-circuits absent-versus-disabled terminal UI; no app behavior was changed
and the original decision was not repeated. Listener 44287 stopped (exit 130),
port 59273 unused, owned Simulator shut down. Corrected final evidence follows
separately.

`ssh-approval-02.xcresult` passed the corrected fresh original command approval
journey, zero failures/skips/runtime warnings (77.994 seconds). It verifies
original context, exact command/reason, enabled offered Allow once, one tap,
terminal disabled-or-removed control and final `Approval journey finished.`.
No extra user message or replacement conversation was launched. Fixture owner
independently verified native function output `synthetic-approved`, final
original desktop pane, exactly one loaded UUID
`01a1009b-005f-72b1-b82c-d231e7f6f56c`, provider PID 93442 and TUI PID 93506 with
unchanged generation. Listener 95726 stopped (exit 130), port 59273 unused,
owned Simulator shut down; fixture 45593 then passed cleanup, exact children
gone and temporary root removed. Actual file approval remains unverified.

## Stable visual handoff

## Native rich replies — October 3

Provider text now renders with pinned MarkdownUI 2.4.1 (native SwiftUI/cmark,
no WebView); user text remains literal. Code copies its displayed body, excluding
the fence-separating final newline, without executing it. Provider icons use
neutral adaptive foregrounds. Remote images are placeholders, not network loads.

`/tmp/pika-rich-03.xcresult` passed the actual Simulator rich-reading/code-copy
journey and the existing multiline-send/draft-isolation journey (2 passed,
zero failures/skips/runtime warnings). The copy journey taps Copy, pastes using
the native edit menu and compares the exact two-line code in the composer.
`/tmp/pika-rich-04.xcresult` reran the rich journey after correcting the adaptive
Markdown background; it passed. Exported screenshots in `/tmp/pika-rich-04-images`
were visually inspected. These use explicit disposable UI fixtures; they do not
prove provider/network delivery or a physical-phone rendering run. No installed
phone application or server binary was changed by this presentation pass.

`/tmp/pika-rich-dark-01.xcresult` also passed rich reading/code copy and the
existing recent-history follow/reading-anchor journey in dark mode (2 passed,
zero failures/skips/runtime warnings). Its rich screenshot was visually inspected;
the owned Simulator appearance was restored to light afterward.

Earlier failed runs are retained: `pika-rich-01` exposed theme override ordering;
`pika-rich-02` exposed a test expectation for the fence separator newline that
the library deliberately excludes from the displayed code body. The final test
instead verifies preservation of internal line breaks and literal backslashes.

## Large conversation transport regression — October 3

The reported physical-phone failure was `2819641 > 1048576`. The provider
WebSocket, outgoing mobile JSONL, remote relay, and iOS message parser now share
a 16 MiB message bound. Ten-turn full-history paging is unchanged. The SSH byte
queue remains bounded at 1 MiB; the phone parses complete lines before checking
the remaining partial frame. No transcript truncation or summary substitution.

`large-history-endpoint-final.log` in `/tmp/pika-ios-transport.b4aP3S` records the
original 1 MiB provider failure reproduced against the same synthetic history,
then a 3,147,781-byte successful production mobile open response. The real Codex
app-server used an isolated fake model, not user transcripts or model quota.
The journey answered the original question and reconciled a lost multiline
receipt without duplicate messages.

`large-history-ui-01.xcresult` in the same directory passed the native Simulator
SSH integration journey: manual onboarding, saved-login relaunch, original large
conversation, foreground/draft recovery, pending question, multiline send and
final provider reply. One passed, zero failed/skipped, zero runtime warnings.
This proves the native app through real localhost SSH and the production mobile
endpoint, not a physical iPhone/Tailscale replay of the user's exact thread.
Formatting and warnings-as-errors all-target Clippy passed.

rs8 preview 0.6.39-rc.4 was packaged, checksum-verified and activated through the
installed updater; running agents were not restarted. The signed Release app
was installed on the user's iPhone without uninstalling or clearing pairing.
Physical reopening of the originally failing thread remains user verification.

Selected original PNGs are copied unchanged, without overwriting user files, to
the private planning evidence directory outside Git. Result bundles remain in
`/tmp/pika-ios-transport.b4aP3S`:

- [Actual Start keyboard](/Users/ayushjain/pika-planning/ios/evidence/2026-10-03-native-validation/actual-create-07-3BB1AB71.png)
- [Original Main Pika reply](/Users/ayushjain/pika-planning/ios/evidence/2026-10-03-native-validation/actual-main-02-CCA5440D.png)
- [Original command approval](/Users/ayushjain/pika-planning/ios/evidence/2026-10-03-native-validation/actual-approval-card-02-67CFC3D5.png)
- [Original command result](/Users/ayushjain/pika-planning/ios/evidence/2026-10-03-native-validation/actual-approval-result-02-C43255AD.png)
- [Normal onboarding, no fixture](/Users/ayushjain/pika-planning/ios/evidence/2026-10-03-native-validation/normal-onboarding-01.png)

After backend feed gate 27910 passed and all fixtures were cleaned, the isolated
Simulator booted and normal Pika PID 97473 launched without test arguments or
fixture environment. The screenshot verifies empty normal onboarding, no saved
board/test banner/dead endpoint; no user state was reset. App remains running,
and no port 59273 connections remain. This host's Xcode distribution lacks the
Simulator.app GUI bundle, so a desktop Simulator window is not claimed; the
native CoreSimulator process and actual rendered screenshot are verified.
# Pokédex presentation — 2026-10-03

Native SwiftUI build, not the browser mockup. Dex has the red shoulder/blue lens,
flat cream cards, aligned miniature-Dex/tail navigation and shared-feed status
lights. Lens opens existing connections; no new transport, scanner or inline
approval path. Thread uses compact native navigation, without the large brand
header or bottom tabs. Composer retains native safe-area/keyboard docking.

Verified UI journeys on iPhone 18 Pro Max Simulator, iOS 27:

- `/tmp/pika-dex-journey-05.xcresult`: full Dex/add-sheet cancel/open/keyboard/
  distinct-thread draft isolation/send/back/Pika-tab/Dex journey passed.
- `/tmp/pika-dex-dark-01.xcresult`: same journey passed in dark appearance
  on the final tail-animation revision. Simulator restored to light afterward.
- `/tmp/pika-dex-journey-02.xcresult`: keyboard rotation, Markdown copy/paste
  and recent-history reading-anchor journeys passed; two earlier checks failed.
- `/tmp/pika-dex-journey-03.xcresult`: machine-settings cancel/feed/header
  update journey passed after replacing an obsolete removed-caption assertion
  with the actual composer's accessible destination label.

Initial review caught and removed an empty system tab bar beneath the custom
bar. Earlier new-journey failures preserved in 01–04: platform confirmation
popover cancellation assumptions, then a keyboard-boundary assertion that
incorrectly included the separately exposed 44pt Typing Predictions strip.
The final check measures its actual top: composer text view ends 18pt above
the prediction strip (8pt container padding + 10pt outer spacing), with no
overlap. It also asserts short-history bottom docking, native header <80pt,
no tab bar on threads, tab alignment, exact draft retention and explicit send.

Screenshots inspected for light/dark, open/closed keyboard and rich output.
These are native UI tests with disposable provider fixtures, not fresh live-SSH
or physical-phone message delivery evidence. Prior transport evidence below
remains separate. Original light screenshots copied unchanged to
`/Users/ayushjain/pika-planning/ios/evidence/2026-10-03-pokedex/`.

Signed Release build succeeded and was installed on the paired iPhone under
the same bundle ID, without uninstall/reset. No GitHub release or server
upgrade was performed. Xcode emitted its existing interface-orientations build
warning; no zero-warning release claim is made.
# Send resumes reply following — 2026-10-03

Explicit Send now resumes latest-output following even after reading older
history. Geometry-driven follow also accounts for Markdown layout growth.
Dragging up pauses follow; passive output does not override the reading anchor.
No message routing, receipt, retry or persistence behavior changed.

Native iPhone Pro Max simulator evidence (disposable in-app endpoint, not live
SSH/model delivery):
- `/tmp/pika-send-follow-03.xcresult`: recent-history journey passed, including
  opening at latest context, loading older history, sending with keyboard open,
  seeing the reply, and opening the assistant at latest context.
- `/tmp/pika-send-follow-04.xcresult`: delayed 12-paragraph streaming journey
  passed. Final paragraph remained above the composer; scrolling away during a
  subsequent stream retained the older message's screen position within 5pt.
- Screenshots retained in those result bundles. Signed device build passed.

Earlier attempts are not passing evidence: 01 had a test query compilation
error; 02 tried to tap offscreen history without scrolling; 03's separate
streaming test crashed on a forced unwrap after scrolling past its narrow
candidate range. The final test finds any visible older message and fails
explicitly if none exists. Production code was unchanged after the first build.
# Fixed cream appearance — 2026-10-03

The app root requests light appearance, per user preference. No theme switch
or system-setting change is introduced. `/tmp/pika-fixed-cream-01.xcresult`
passed the native Dex/thread/keyboard/draft/send journey with the simulator
system appearance set to dark. Exported board and keyboard screenshots were
visually inspected: both retain cream surfaces and readable dark text; the
keyboard is light. Simulator appearance was restored afterward. Signed device
build passed; this is UI fixture evidence, not a live provider delivery test.
# Thread casing and edge navigation — 2026-10-03

Compact red thread header, exact-thread status dots, full-width red lower rim on
Dex and threads. No visible Back control; rightward drag starting within the
left 24pt of the conversation dismisses after 80pt when predominantly horizontal.
Accessibility escape is also available. This uses the existing navigation stack,
not a new navigation owner or a provider action.

Native simulator UI fixture evidence:
- `/tmp/pika-casing-04.xcresult`: 2 passed, 0 failed. Dex/thread journey verifies
  edge return with keyboard open, exact draft isolation/restoration, sending,
  no visible navigation bar, compact header and keyboard docking. Recent-history
  journey verifies auto-follow after sending and returning to assistant history.
- `/tmp/pika-casing-02.xcresult`: status/header journey passed, including Needs
  you to Ready from the existing feed and Inactive after disconnect.
- `/tmp/pika-casing-03.xcresult`: keyboard/rotation journey passed.
- Board and open/closed keyboard screenshots from 04 visually inspected.
- Signed device build and whitespace check passed. No real model/SSH test here.

Early runs caught an accessibility identifier inherited by children (fixed by
explicit containment), a test wrongly counting the status safe area as header
content, and a non-working hidden-navigation UIKit swipe bridge. The bridge was
removed; final edge navigation is exercised by the actual UI journeys above.
# Slimmer casing — 2026-10-03

Bottom red no longer fills the 34pt home-indicator area: visible band goes from
about 42pt to 8pt (81% reduction on the tested Pro Max). Thread header content
goes from 54pt to 42pt; signals lead on the left and the provider mark trails.
`/tmp/pika-slim-01.xcresult` passed the native Dex/thread/keyboard/draft/send/swipe
journey, including an assertion that signals are left of the thread identity.
Closed-keyboard screenshot visually checked; signed build passed. Disposable
UI fixture only, no live provider call.
# Flush lower edge correction — 2026-10-03

The 8pt red strip is now a non-interactive root overlay reaching the physical
bottom, not an inset above the home-indicator area. No cream band remains
under it. `/tmp/pika-flush-01.xcresult` passed the native Dex/thread/keyboard,
draft/send and edge-return journey. Exported Dex and thread screenshots were
visually inspected at the actual bottom edge. Signed build passed. This is
disposable UI fixture evidence, not a new live-network claim.
