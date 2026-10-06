# Native iOS evidence ledger

Current scope amendment (2026-10-04): the user approved future shared-control
launches. The earlier pending-decision/saved-history-only statements below record
the prior checkpoint; see the later provider investigation and native OpenCode
results for subsequent work. Nothing in this ledger establishes a fleet release
or a physical-iPhone installation of these changes.

## Mobile history gaps — 2026-10-03 (unreleased work)

Approved outcomes: fix long-thread reopening/paging and support other providers'
existing mobile conversations. **The second outcome is not complete:** the new
Claude/OpenCode/Muse adapter is saved-history-only, not original-owner messaging.
No provider is launched, resumed, or forked by history reads. Replies remain
explicitly unavailable. Future launch changes require the pending user decision;
no current agent, phone installation or fleet server was changed by this work.

`mobile_long_history_pages_and_reopening_keep_latest_exchange` passed against
the installed Codex using disposable homes and loopback fake inference: 22
completed desktop exchanges, then an actual `_mobile` send and reply, latest ten
turns in chronological order, previous ten in chronological order, and a new
mobile process reopening the exact same thread with the last reply still last.
Final measured run: 1 passed in 1.84 seconds (not a performance benchmark).
This is real provider/endpoint proof, not a physical iPhone or live-model claim.

`mobile_history_journey` passed 3/3 through actual `_mobile` subprocesses with
synthetic native provider stores and denied provider executables. Each exercises
85 literal messages, older paging, exact identities, preserved unread state,
rejected unwatched access and rejected sends. It does **not** prove live provider
ingress or complete compatibility with every historical provider schema.
`ios_mobile_fleet` routing/revocation regression passed 1/1.

Native pages request Codex's descending traversal explicitly, reverse only the
turns (not their items), and mark chronological order while retaining its cursor.
The phone also recognizes legacy unmarked Codex descending pages. Streaming
without a snapshot watermark cannot safely concatenate an overlapping partial
item; the implementation preserves the snapshot and waits for the full completed
item, with a visible refresh explanation, instead of guessing text overlap.

Review caught that overlap race and missing Muse user-display/steer records;
both are being validated. Final simulator results and reviewer disposition must
be recorded below before claiming the history fix complete. Initial simulator
run at `/tmp/pika-history-ios.XU7zaf/history.xcresult` had 4 passes and 1 failure:
the latest retained text was behind the keyboard. It is not passing evidence.
The follow-up fixes keyboard-settled following without resuming intentional
older-history reading. SQLite history access is database-read-only; SQLite may
create/update its normal WAL shared-memory coordination sidecar.

Final native regression evidence: four installed-Codex/fake-inference journeys
passed in 3.84s, including the new long-history test, model/skills, lost-message
receipt recovery, and shared-owner/stale-steering checks. A prior combined run
failed its synthetic HTTP fixture because accepted sockets inherited nonblocking
mode; explicitly making that fixture connection blocking fixed it. The passing
repeat did not change provider production behavior to accommodate the test.

`/tmp/pika-mobile-gaps-native-final.log`: library 860 passed, 6 explicitly ignored;
fleet routing/revocation 1 passed; provider-history endpoint journeys 5 passed.
Earlier library validation failed two new SQLite fixtures using macOS `/tmp`
symlink paths with strict NOFOLLOW. Fixture roots were canonicalized; production
NOFOLLOW was retained. The final endpoint suite adds refusal of Claude rewind
and compaction: **only verified linear parent-linked Claude histories up to
16 MiB are supported in this interim adapter**. Missing/disconnected ancestry,
repeated IDs and native relinking cases stay unavailable. This is safe partial
support, not fulfillment of arbitrary Claude history or mobile sending.

Native UI evidence (explicit in-app endpoint doubles, not SSH/model journeys):
`/tmp/pika-history-ios.XU7zaf/history-refined.xcresult` was an intermediate
6/8 run: prefix/full-completion and read-only/following checks passed, strict
pixel anchors failed. `/tmp/pika-history-ios.XU7zaf/history-final.xcresult`
then passed five selected tests, with no skipped tests or runtime warnings.
The final anchor implementation was strengthened again to use the original
message's native rectangle, not total content height, and to fence restoration
by exact selection and route generation. This prevents concurrently arriving
output below the reader from shifting their position.

`/tmp/pika-history-ios.XU7zaf/history-anchor-concurrent-fixed.xcresult`
passed three selected journeys in 122.755s, zero failures/skips/runtime warnings:
legacy and chronological page orders retain the original visible message within
5pt while output arrives below during the history request; read-only explanation
and disabled controls also pass. Its first build failed on a RouteEpoch/UUID
type mismatch and is retained separately, not counted as passing evidence.

Root visual inspection found the newly added UI fixture initially emitted an
assistant completion before its user completion, despite correct persisted
order. The fixture now emits user then reply and asserts visible order before
and after reopening. The final two-test repeat is still required below.
Independent adversarial review scored the supported history slice 95/100
conditional on that final repeat, but the full original open-and-reply scope
70/100 because non-Codex sends and complex Claude histories remain unavailable.

Final repeat verified by root with `xcresulttool`: 2 passed, 0 failed/skipped,
runtimeWarnings empty in
`/tmp/pika-history-ios.XU7zaf/history-chronological-final.xcresult` (95.982s test
operations). Both legacy/current thirty-turn journeys assert user then reply
visible above the keyboard, correct order after reopening, and older-page anchor
within 5pt during concurrent arriving output. Root visually inspected the final
keyboard screenshot
`/tmp/pika-history-ios.XU7zaf/chronological-final-attachments/AE5FAE74-5FBF-49B7-9B55-A86B1F32FA9C.png`.
The review condition is satisfied for this history slice: self/reviewer 95/100.
Overall remains self/reviewer 70/100 and incomplete; future provider-control
launch changes need the outstanding authorization and original-owner protocol
verification. None of this work is published or installed on fleet/phone.

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

# Source-bound partial board coverage — 2026-10-03

Dex retains partial coverage per authenticated subscription source instead of
letting the last machine's snapshot replace a global warning. All machines
names the partial sources; a selected directly paired owner uses only its own
coverage, even offline. Coordinator-only selection follows the verified route,
with conservative cached/ambiguous-source warnings. Coverage is retained with
the disposable board cache across relaunch; failed/cancelled enrollment restores
the previous source's coverage. Invalid or incomplete pages do not clear it.

`/tmp/pika-coverage-01.xcresult`: 1 passed, 0 failed/skipped/runtime warnings,
167.882 seconds test operations. Actual native app → ordinary loopback SSH →
three explicitly labelled protocol doubles, no provider/model/fleet access.
The onboarding journey verifies Alpha partial followed by Gamma complete,
All machines/Alpha/Gamma filters, saved-login process restart and independent
offline Beta, while preserving exact Alpha/Gamma sends and server-owned logs.
Exported All machines screenshot was visually inspected: the Alpha warning is
readable beside all three independently observed machine cards.

`/tmp/pika-coverage-02.xcresult`: focused retained-store offline/recovery journey,
1 passed, 0 failed/skipped/runtime warnings, 37.519 seconds test operations.
Alpha's partial warning remains explicitly `(cached)` after its owned endpoint
disconnects; selecting offline Alpha retains it, selecting healthy Gamma hides
it, and All machines restores it. Only Alpha's own complete reconnect snapshot
clears the warning. The cached-warning screenshot was visually inspected.

Commands use the earlier isolated HOME/XDG invocation, simulator
`98ECDE03-E8B5-4E7C-ABBE-DBC0C5935D27`, disposable DerivedData
`/tmp/pika-ios-transport.b4aP3S/app-build` and sibling `xcode-packages`.
Only `PikaMultiMachineSSHTests/testThreeSavedMachinesCollisionRoutingAndIndependentOffline`
was selected, with `PIKA_MULTI_SSH_TEST_CONFIG` pointing to the freshly generated
`/var/folders/k9/s1xh63d93rq9bd97cngqvf4c0000gn/T/pika-three-ssh-a6b5lskn/configuration.json`.
No installed Pika, provider homes, real transcripts, tmux sessions or user SSH
configuration were changed. Existing Xcode debugger-version diagnostic remains;
this is not a zero-build-warning claim.
Fixture stop marker was applied after both tests; its owner exited 0 and all
three owned loopback listeners (59431–59433) were confirmed stopped. Generated
keys/config/logs remain only in that disposable fixture root. Whitespace check
passed. No commit, tag, publication or device/installed-command cutover here.
# Future provider shared-control investigation — 2026-10-04

User approved changes to future managed launches, not existing running agents.
No provider is considered mobile-writable merely because its saved history loads.

Muse native probe used installed signed `1.3.0-R3401.1`, an isolated canonical
temporary HOME/XDG tree, native interactive TUI with `--provider echo`, disabled
shell/write tools, and `MUSE_EXPERIMENTAL_EXTERNAL_AGENT_INGRESS=on`.
Native discovery returned the exact fresh session; a desktop prompt and its echo
reply appeared in that TUI without model quota. Native `session-message send`
returned `status: unavailable`, `error_code: sender_unverified`, `receipts: []`.
The documented display-context variant and a byte-identical signed binary named
`muse` returned the same failure. Signature verification passed. This is a failed
ingress probe, **not** a passing mobile journey. No production launch flag or
permission bypass was installed. Initial `/tmp` symlink registry rejection was
resolved in the fixture by using its canonical `/private/tmp` paths.

Claude Code native `2.1.272` channel probe is **blocked, not completed**.
Disposable proof root `/tmp/pika-claude-channel.t3WQxs/RESULT.md` records the
sanitized command, isolation, protocol and failed result. Native interactive
launch used explicit fresh `--session-id`, launch-local `--mcp-config`,
`--dangerously-load-development-channels server:pika`, and isolation-only
`--bare --strict-mcp-config`. Fake inference remained loopback with a fake key.
MCP child PID 90227, parent native PID 90203, received `initialize` from
`claude-code 2.1.272`, `notifications/initialized`, and `tools/list`; original
session was `7f31bb0f-30af-48bd-84c0-6a8d686ecaa2`.
Structured phone channel notification was written, explicitly not acknowledged.
Native diagnostic: `Channel notifications skipped: channels feature is not
currently available`; TUI: `Channels are not currently available`.
No correlated reply was received, so no original-conversation mobile send/reply
success is claimed. Owned native/MCP processes were confirmed stopped at
2026-10-04 10:27 UTC; existing agents were untouched. No gate/cache/policy bypass,
real credentials, quota, fleet or user transcripts were used.

Official contract: <https://code.claude.com/docs/en/channels> and
<https://code.claude.com/docs/en/channels-reference>. Development channels bypass
the preview plugin allowlist, not provider availability or managed
`channelsEnabled` policy; native consent must remain honored. Next supported
step is provider preview availability for an eligible runtime/account and, where
managed, administrator channel policy/plugin approval, then a fresh isolated
native consent/send/correlated-reply test. No production channel enablement or
provider source implementation was added. Production launch integration must
preserve existing MCP configuration rather than adopting isolation-only strict
configuration. Official Remote Control is not a documented Pika ingress API:
<https://code.claude.com/docs/en/remote-control>.

## OpenCode direct native protocol evidence — 2026-10-04

Installed native OpenCode `1.18.31` passed the disposable
`ios_opencode_shared_spike` test: 1 passed, 4.81 seconds (run 90382).
An actual terminal-typed prompt reached synthetic loopback inference. The native
TUI then selected a deliberately separate session; a direct second-client reply
addressed the original session, retained its initial prompt/reply context, left
the second session's message history unchanged, and appeared when the same TUI
reopened the original. Original server and TUI process generations were unchanged.
This establishes exact durable server-session authority, not a guarantee that
the desktop currently displays the phone's selected conversation.

The proven route is authenticated loopback native `serve` plus native `attach`,
using `/session/{id}/prompt_async` and exact native message IDs. Native bounded
older pages use the **opaque `X-Next-Cursor` header**, not a message ID: the latter
failed with HTTP 400 in two probes. A limit-one/older-cursor journey passed;
the greater-than-100-message Pika endpoint journey remains separate evidence.
The newer `/api/session` engine admitted synthetic input but did not produce
inference; default TUI `--port` accepted TCP but returned no HTTP response in
three isolated retries. Neither unproved route is enabled. Native duplicate
message IDs are not treated as inference-idempotent; Pika's durable operation
journal forbids replay after an unknown result.

New `mobile_opencode`, launch, transport, private-storage and Linux-proof modules
scope future supported launches to one owned server/TUI pair. Credentials use
private files and `Command.env`, not terminal command arguments. Both generations,
launch identity, native session, directory and connected kernel socket owner are
checked. Unsupported launch options retain ordinary native launch behavior.
Terminal exit/interrupt cleans and reaps only owned children; server exit is
peeked without reaping before owned-group cleanup. Existing running agents are
not migrated. Reverted native history fails closed, including the connected poll.

Focused tests: 5 passed, 0.61 seconds (run 38446), covering atomic session/owner
admission, explicit-untrack refusal, actual Darwin foreign-read ACL and hardlink
denial, and actual loopback socket ownership. An unrelated live PID received
**no authentication bytes**. Linux proof has source/tests but no Linux native
runtime result here. All probes used disposable HOME/XDG/Pika/database/tmux paths,
fake credentials and loopback inference; no quota, fleet, real transcripts or
installed configuration changes. This is not full Pika phone/UI completion.

## OpenCode Pika endpoint acceptance — 2026-10-04

The actual native OpenCode `1.18.31` plus compiled Pika `_mobile` journey
`future_native_launch_mobile_reopen_and_lost_ack_never_replay` passed: 1 passed,
15.76 seconds runtime, 0.79 seconds build; completed by 11:28:53 UTC.
The normal public future-launch command ran in an isolated tmux terminal,
with the real generated Pika lifecycle plugin and synthetic loopback inference.
Phone-protocol open did not launch a provider. Two consecutive sends produced
chronological replies in the original native TUI/session/process generations.
The plugin and public-list reconciliation preserved its exact original TUI PID
and pane, without classifying the certified local server as another owner.

Dropping the phone transport after dispatch, reopening, querying the exact
native message receipt, and retrying the same operation ID produced no repeated
inference. Altering the isolated owner generation rejected a fresh operation
before dispatch while retaining the prior delivered receipt. Unsupported native
model/skill/approval controls stayed explicit rather than claiming parity.

105 actual native `noReply` messages crossed the 100-message snapshot boundary
using the native opaque cursor. The older page was chronological and had no
duplicated native IDs; seeding and paging made no inference calls. A native
revert after connection caused a disconnect and denied open/new send without
inference. Closing only the owned terminal reaped the exact TUI, server and
supervisor generations.

After the negative revert test, the supported native `unrevert` restored the
original history. Following complete terminal/server/supervisor exit, public
`pika open` addressed the exact existing provider ID in the disposable terminal.
Cold resume retained the sole watched conversation and older delivered reply,
with new launch token, TUI and server generations. Launch made no inference
requests. A fresh phone send produced a chronological exact delivered reply,
visible in the resumed original native conversation's TUI. Closing that terminal
also reaped all its owned generations. No source change was required for this
additional acceptance proof.

The unchanged final assertions first exposed two real reconciliation defects:
pending-to-native pane tags were not handed off, then the certified sibling
server was treated as an unverified extra provider process. Failed isolated
fixtures remain at `/tmp/pika-opencode-mobile-Iy67XR` and
`/tmp/pika-opencode-mobile-JXkLDs`; the fixes retained full process-tree checks
and exempted only the exactly certified pair. The final successful fixture was
removed. The read-only saved-history regression also passed 1/1 in 0.81 seconds.

Reproduce with the repository's disposable-home wrapper, explicit installed
OpenCode/tmux paths, and `cargo test --test ios_opencode_mobile_spike
future_native_launch_mobile_reopen_and_lost_ack_never_replay -- --ignored
--nocapture`. Build profiles used debug=0 and Rust 1.88.0. All native provider,
configuration, database, tmp and tmux paths were disposable; no quota, user
transcripts, fleet machines, installed Pika/configuration, or existing running
agents were touched. This proves the actual provider/Pika protocol journey;
it does not claim a new native-provider iOS simulator journey or Linux runtime
proof.

## Actual OpenCode native iOS simulator journey — 2026-10-05 UTC

`/tmp/pika-opencode-simulator-9JOAgb/native-02.xcresult` passed 1/1, with
zero failures/skips/runtime warnings and 56.502 seconds of test operations.
This was the built native iOS app on simulator
`98ECDE03-E8B5-4E7C-ABBE-DBC0C5935D27`, ordinary loopback OpenSSH, the compiled
production `pika _mobile`, and actual installed OpenCode 1.18.31. It did not use
`--ui-fixture` or a protocol/provider double. Only model inference was synthetic
loopback; no account, quota, real transcript, fleet or phone install was used.

The existing public-launch/cold-resume test held its original certified native
TUI/server/session for a bounded opt-in window. The app manually entered its
disposable SSH login/key, verified the disposable host fingerprint, saved login,
restarted, opened the exact existing OpenCode thread, sent one distinct prompt,
and showed the full latest reply above the still-visible native keyboard.
Leaving and reopening retained that same latest reply. The held fixture then
independently verified the exact prompt/reply in chronological native history,
the original TUI visibly containing the reply, unchanged owner generation and
exactly one new inference request. It passed 1/1 in 433.60 seconds including the
deliberate simulator/build hold. Terminal exit reaped its owned TUI/server/
supervisor; fixture cleanup removed the disposable provider root. Owned loopback
sshd PID22534 stopped, and the test credentials remain only in the evidence root.

Visually inspected attachments:

- Keyboard: `/tmp/pika-opencode-simulator-9JOAgb/attachments-02/F525E9FD-F5C9-404C-B691-CBD55E40931C.png`.
- Reopened: `/tmp/pika-opencode-simulator-9JOAgb/attachments-02/912AFCB1-EE97-44EA-B879-2F7C3A59C304.png`.

Both show the unique user message before the complete OpenCode reply, with that
latest reply visible. The first attachment includes the native keyboard.
The first run remains `native-01.xcresult`: failure after 45.940 seconds because
the test precreated Pika state mode0755 and real board subscription correctly
rejected it as not owner-only. The fixture now creates its disposable directories
mode0700; no application privacy check was weakened.

Reproduction uses `PIKA_IOS_OPENCODE_READY=<evidence>/ready.json` with the existing
ignored native OpenCode mobile test. It emits only fixture identity/environment,
holds at most600 seconds and releases on `<evidence>/ready.stop`, with independent
native reply and cleanup assertions. Configure a new disposable loopback sshd
ForceCommand with exactly that environment and compiled Pika `_mobile`, then
run the existing `PikaSSHIntegrationTests` with configuration `mode=native`,
the emitted thread/context/reply, a fresh store UUID and new test keys. Use the
earlier isolated Xcode invocation with `PIKA_SSH_TEST_CONFIG=<config>` and
`-only-testing:PikaUITests/PikaSSHIntegrationTests`. No production iOS transport
or application behavior change was required. Linux runtime remains separately
reported; this simulator result does not imply unsupported model/skill/approval
control parity or Claude/Muse send capability.

## Concurrent native OpenCode future launches — 2026-10-05 UTC

Actual installed OpenCode 1.18.31 on macOS passed
`concurrent_managed_native_launches_keep_exact_ports_owners_and_messages`: 1/1,
18.43 seconds runtime, 8.70 seconds build, complete by 03:40:54 UTC. Two public
managed `pika new` commands shared one disposable Pika/provider home but retained
distinct native conversation IDs, server ports/PIDs, native TUI generations and
launch tokens. Public-list reconciliation before and after replies retained each
exact TUI PID/pane without OpenTwice. Independent `_mobile` clients sent distinct
prompts: snapshots, inference contexts and original native TUI replies did not
cross conversations. Exact receipts were delivered; same-ID retries left exactly
two total synthetic inference calls. Closing terminal A reaped its owned group
while native B remained alive, then closing B reaped its group.

Unchanged assertions first exposed native concurrent cold migration contention:
40.17/40.21-second failures, with actual native panes reporting `database is
locked` and `Native shared server exited before readiness`. Retained failure
roots are `/tmp/pika-opencode-mobile-jW4wYW` and
`/tmp/pika-opencode-mobile-B0HDWi`. The supported launcher now selects explicit
ephemeral native ports and serializes only startup through native bootstrap,
session creation and TUI certification; it releases the startup guard before
the terminal lifetime. No inference retry was introduced. The final acceptance
assertions were not weakened, and the successful disposable provider root was
cleaned up. Formatting and diff checks passed. Linux concurrent evidence is
reported separately by its runtime owner.

Reproduce with the disposable-home wrapper and explicit OpenCode/tmux binaries:
`cargo test --test ios_opencode_mobile_spike
concurrent_managed_native_launches_keep_exact_ports_owners_and_messages --
--ignored --nocapture`. This proves concurrent native launch/control isolation,
not additional iOS control parity. No user transcript, fleet, configuration,
installed Pika, real model account or quota was used.

### Authorized disposable Linux bootstrap/socket diagnosis (2026-10-05 UTC)

On rs8, native Bun's TCP_DEFER_ACCEPT left a connected zero-data socket in
SYN_RECV with inode zero. A constant leading CRLF closed the connection; a fixed
`GET ` method token caused acceptance and yielded the exact server PID's owned
FD/inode, UID and established four-tuple. Linux transport now permits only the
fixed method token before that unchanged proof; path, credentials and content
remain withheld. macOS still sends no bytes before proof. An unrelated-PID
negative test permits only that fixed token, never authentication.

The subsequent generated-plugin cold-start failure was independently reproduced
without Pika launch state: a fresh disposable native home completed its first
session request in 12.334 seconds; a restart of the same home took 0.286 seconds.
The native config log paused during dependency materialization, with its native
installation lock and incomplete node_modules. Embedded native code joins
Config.waitForDependencies on Npm.install/Arborist reify. Both probe children were
terminated and reaped; no model requests or real transcripts were used.

Startup now issues one read-only GET /session?limit=1 with a separate bounded 30-second
bootstrap budget and cancellation/exact-process-generation checks before its
single session-create POST. Ordinary RPCs retain their 10-second budget; there
is no create/send retry. Full Linux integrated-journey acceptance remains a
separate test result, not a claim from these probes.

The same disposable native server returned two sessions for GET /session and
one for GET /session?limit=1, proving that the readiness query is bounded by the
actual provider. Local adapter tests passed 6/6, including cancellation before
connection and the actual socket-owner negative test.

Repeated Linux full-journey diagnostics then measured exact owned acceptance at
752ms, while individual bounded /proc scans took at most 49ms. The former 500ms
ownership observation cutoff therefore rejected the correct native server.
Linux now observes for at most two seconds (plus a final bounded exact check);
macOS retains 500ms. This changes only when ownership can be proven: all original
PID-birth, UID, accepted FD/inode and four-tuple requirements still hold, and no
path, authentication or content is sent before that proof. Temporary latency
telemetry was removed; bounded non-secret tuple diagnostics remain on failure.

With that source, the full actual Linux native/Pika journey passed 1/1 in 50.68
seconds, including generated hooks, consecutive exact-session replies, opaque
history pagination, lost acknowledgement/no replay, revert refusal, lifecycle
cleanup and cold resume. The focused Linux native security/adapter tests passed
7/7 in 2.57 seconds. Logs are in the authorized disposable validation directory
as journey-owned-final.log and security-owned-final.log. This result predates
the separate explicit ephemeral-port change required for concurrent launches.

The explicit-port original/cold-resume Linux journey subsequently passed 1/1
in 53.09 seconds (journey-ephemeral-final.log). Concurrent cold native launches
then exposed native SQLite migration contention: one server failed CREATE TABLE
workspace before readiness, matching the independently captured macOS database
locked error. No assertion was weakened and no native session-create was retried.

Managed native startup now holds an OS-owned, private pinned-file lock per Pika
state through server initialization, read-only bootstrap, exact session creation,
TUI generation proof and certification, releasing it before terminal lifetime.
Acquisition is bounded to 30 seconds and cancellation-aware; closing the owner
releases it, without unlinking/stealing an inode. Existing running agents are
untouched. Explicit OS-reserved loopback ports are released immediately before
native spawn and the reported port must match; a bind race fails closed.

With that source, the actual two-concurrent-launch Linux test passed 1/1 in
31.65 seconds (concurrent-locked-final.log): distinct native ports, exact owners
and sessions, isolated phone/TUI messages and inference contexts, delivered
receipts, same-ID no replay, public reconciliation, A shutdown preserving B,
and cleanup of both owned groups. Local adapter/security tests passed 7/7 in
0.64 seconds, including bounded/cancelled OS-lock acquisition and close release.

Final Linux regressions with the startup lock passed: focused native
security/adapter tests 8/8 in 2.58 seconds (security-locked-final.log), and the
complete original-conversation/cold-resume journey 1/1 in 50.71 seconds
(journey-locked-final.log). All validation retained isolated provider/Pika/tmux
homes and fake inference. Successful fixture cleanup reaped owned processes;
failed diagnostic fixture artifacts were retained, not used as acceptance.

## Claude native channel eligibility correction — 2026-10-05

The user completed isolated native authentication and enabled organization
Channels. Our disposable launcher had introduced `DISABLE_TELEMETRY=1`;
unavailable-channel results from that environment do not establish an external
provider blocker. Removing only that test-added flag produced effective managed
`channelsEnabled=true` in native Claude Code 2.1.274. User privacy settings,
production configuration, policy and feature caches were not changed.

After genuine development-channel consent, one benign structured notification
received the exact correlated `CHANNEL_OK` reply in disposable native session
`e08190ae-2d22-4c44-aad1-c7683647485d`. Original native PID 1095387 and MCP child
1095852 remained alive and unchanged. The owning native transcript independently
confirmed the session; terminal typing, a fork or duplicate resume did not
substitute for incoming channel delivery. Root inspected the sanitized proof at
`/tmp/pika-claude-normal-proof.json` and remote `RESULT.md` under the isolated
`/tmp/pika-claude-user-auth-AzOp0t` root. One incoming model turn was used.

This proves the native protocol only, not phone E2E or production readiness.
The test preauthorized only the reply tool and used explicit development-channel
consent. Normal permission handling and approved distribution remain unproved.
Native tool-call metadata lacks a current conversation UUID: model-echoed chat
metadata and unchanged PID are insufficient for safe dispatch after an in-TUI
session switch. Current-session attestation remains a required acceptance gate.
No Claude production adapter, installed-command change or release is claimed.

After the user approved explicit experimental channel opt-in, a further native
probe matched `PreToolUse.session_id/tool_use_id` to the MCP call's native tool
ID before releasing content. The first `/clear` attempt queued behind the held
tool, so it did not prove a switch boundary. A fresh uniquely named tool probe
then interrupted the turn with Escape and cleared the conversation before
releasing a held synthetic response. The original transcript contained the
native cancelled-tool result; both original and new transcripts existed and
contained zero sentinel records. Root inspected
`/tmp/pika-claude-attested-cancel-proof.json`. This is ordinary cancellation
evidence, not background-tool, normal-permission or phone E2E evidence. Probe
tools were explicitly preallowed only in the disposable test environment.

Independent review still found a material contract gap: a content-free channel
wake is queued in whichever native conversation is current, without a provider
UUID precondition. Attested fetching can refuse user content to the wrong UUID,
but cannot prevent that wake from causing model work, quota use or native records
in the wrong conversation. Experimental consent does not silently relax exact
thread routing. No supported exact-target alternative was established; Claude
mobile sending remains unimplemented pending either a native dispatch guarantee
or explicit approval of that narrower behavior. No claim is made that the
wrong-context wake race was reproduced by the cancellation test itself.
## 2026-10-05 — continuation-only Claude implementation checkpoints

The user clarified that the phone continues one selected conversation: mobile
clear and in-place resume are not requested. Native terminal controls remain
available; a native context change revokes the old connection. An opaque wake
is not evidence of sensitive-content delivery or a reply, and zero model work
in an external-switch race is not claimed.

Local implementation now has explicit future-launch experimental opt-in,
native permission prompts, and one-use hook/MCP tool-call correlation under
the existing trusted-account boundary. It does not protect against arbitrary
same-UID code that can already read/write the account's Pika data.

Two native Simulator UI-double checks passed independently:

- Experimental notice can open/dismiss without losing a typed draft or hiding
  the native keyboard/composer: 1/1, 25.393 seconds. Result bundle
  `/tmp/pika-history-ios.XU7zaf/claude-experimental-ui.xcresult`; screenshot
  `claude-experimental-attachments/20120018-01BB-4889-B5ED-1F54B581F2A4.png`
  inspected by the lead agent.
- An arriving correlated Claude reply triggers a read-only original receipt
  check, clears only the matching original draft, and permits the next draft
  without replay: 1/1, 24.561 seconds. Result bundle
  `/tmp/pika-history-ios.XU7zaf/claude-receipt-ui.xcresult`.

These are **UI fixture evidence, not native Claude delivery evidence**.
The disposable actual `_mobile` history journey also passed six checks,
including fresh-process reopen of source-native fetch/reply records correlated
with their journal tool IDs; uncorrelated lookalike tools were not shown.

The first real rs8 run opened the certified fresh native Claude conversation
`18a60a1f-7877-4245-8532-a7debc973c43` (PID 1189085), admitted one uppercase
phone operation as unknown, and returned unknown without replay for a repeated
operation ID. It did **not** prove delivery: native Claude canonicalized argv0
to its `claude/versions/2.1.274` executable while keeping PID and UUID, which
the old process-kind classifier failed to recognize. The hook then timed out.
No fetch or reply tool record was present. Controller logs that guessed a
permission prompt from scrollback are not permission evidence; those automatic
inputs were stopped. The original uncertain operation is not being retried.
The narrow argv0-only native-version classifier fix retains exact UUID,
process-generation, launch and watched-state checks. A new owned native run
must establish actual delivery before this feature is called verified.

The next fresh owned run (`/tmp/pika-claude-mobile-krFkn5`, conversation
`795b895f-23a7-4714-aa60-4056666a370f`, native PID 1203231) delivered an
opaque operation notification but still did not fetch the literal request or
reply. Native `/mcp` showed the exact generated server connected with two
tools; an earlier startup warning was not reliable evidence of missing tools.
The model asked what to do with the bare UUID. The wake was sent about 60
seconds after readiness, so an immediate-startup race is not established.
Initialize instructions alone did not produce the intended native behavior.
The pending correction adds fixed protocol routing instructions and exact
tool names to the opaque notification, never the user's literal text. This
operation remains unknown and will not be re-woken. No native end-to-end
success is claimed for this run.

The third owned run (`/tmp/pika-claude-mobile-VUQqvu`) delivered an actual
attested fetch/reply in 8.64 seconds in the original conversation
`780ca0ba-7d1b-440b-85f8-7e3b56eac251`, native PID 1211409/birth 4148039592.
Native permissions were preserved; no approval keys or allowed-tools injection
were used. This first successful probe explicitly asked for a test reply via
the reply tool, so a separate plain-language request is still required before
generalizing the result to ordinary phone messages.

Reopen initially exposed native attachment-only leaf siblings, which the
history verifier incorrectly treated as alternate conversation branches. It now
prunes only terminal attachment chains; message ancestors and all actual
conversation branches retain their checks. Native `isMeta` protocol text is not
rendered as a human message. Independent review found no material regression.
After the owned native process was reaped at 16:03:23 UTC, a validated expired
binding also needed to permit saved-history access without control. That narrow
fallback leaves malformed descriptors and live identity mismatches as errors.

At 16:54:41 UTC, a fresh real `_mobile` process reopened this same native
source successfully, displayed the literal request and attested reply under
their original native tool IDs, advertised `send:false`, and returned the
original uppercase operation receipt as delivered. No native restart or new
model call was used for this check. Lead inspected
`/tmp/pika-claude-native-reopen-passed.json` independently. Latest Linux binary
SHA256: `11bb08694879932a293ab18fc95c9d0a5298727d08079352e8cf514d8a62cb1f`.
Simulator SSH presentation and plain-message live delivery are separate pending
checks, not implied by this backend result.

Plain-message native check passed at 16:57 UTC: the literal question
`What is 19 plus 23? Answer with the number only.` produced an attested `42`
in 8.633 seconds. The exact original session
`ca814c25-6fae-435f-8b17-0691ac7cb8d4`, native PID 1244952/birth 4148448566,
and one fetch/reply tool pair were independently checked. A fresh endpoint
reopened the same source with `send:true`; the same operation ID was not
re-woken. Lead inspected `/tmp/pika-claude-plain-prompt-proof.json` and
`/tmp/pika-claude-plain-native-tools.json`. Owned native processes were reaped
at 16:57:41 UTC.

The native iOS Simulator also passed a real SSH read-only journey against the
earlier preserved rs8 source: saved-login reconnect, exact tracked thread,
literal request and reply, sending disabled after native exit, back/reopen.
`/tmp/pika-claude-ios-gateway.BH8YKw/readonly-01.xcresult` reports 1 passed,
0 failed, 0 skipped, 81.676 seconds elapsed. Lead independently read its summary
and inspected `attachments/CEEAF2A8-BF07-4AF4-87D5-CB7BD3CE8971.png`.
The gateway bound only localhost, imported only a disposable key into the
simulator, and was reaped with its port closed. This read-only run caused no
model inference. A final live simulator-send journey is pending; combining
these separate checks is not presented as that full-path proof.

The final **live simulator → SSH → native Claude** journey also passed:
`/tmp/pika-claude-ios-send.YOJJFQ/native-01.xcresult`, 1 passed, 0 failed,
0 skipped, 78.017 seconds elapsed. The app alone sent
`What is 17 plus 25? Answer with the number only.` and displayed `42` above
the keyboard. An unsent next draft verified receipt-driven composer readiness;
it was deleted without sending. Back/reopen retained the original reply.
Lead independently read the Xcode summary and both screenshots.

Independent server evidence `/tmp/pika-claude-simulator-native-proof.json`
contains exactly one durable phone operation and one native fetch/reply pair,
both under original UUID `8ff3dafc-3e72-4439-ac41-007f6dbc3822`, PID
1247435/birth 4148468328. The receipt reports delivered with matching node,
thread, and operation. The native source hash is
`98f5baff7e66ea1ce8bfa32c2abc53027646baa3ba2e155176b8f12e743582ea`.
No controller sent a message on the app's behalf. The localhost gateway was
reaped and its port checked closed; the native guard reaped its owned processes
at 17:04:38 UTC, with separate process-health confirmation at 17:04:42 UTC.

Evidence caveat: the reusable SSH test banner incorrectly said “synthetic
model, no quota” on this real-provider run. The native records, not that stale
label, establish actual Claude inference. The original artifact is retained
unchanged. The test-only banner has been corrected to the neutral “DISPOSABLE
SSH INTEGRATION”; a no-inference saved-history UI repeat verifies that correction.
No physical-phone installation or production-server cutover occurred.

The corrected-label saved-history repeat passed 1/1 with 0 failures/skips in
62.414 seconds: `/tmp/pika-claude-ios-send.YOJJFQ/readonly-label-02.xcresult`.
Lead independently read the summary and inspected the current screenshot
`attachments-label-02/BABDFF87-10D3-42DC-9787-EB8A871EA8A7.png`. It shows the
same plain request and `42` after native exit, with neutral test labeling and
sending disabled. No new message, model call, or native launch occurred; the
temporary gateway was again reaped and its port closed.

Scoped self/adversarial scores for the experimental native Claude
continuation/history/receipt slice are 95/100. Overall provider work remains
incomplete (90/100): Muse's native sender-admission boundary is unresolved,
nonlinear/compacted Claude history is explicitly refused, and unsupported native
controls are not represented as working. These are not release approval scores.

## 2026-10-05 — owner-authorized physical phone update

After the user connected the phone for installation, signed Release build 42
completed successfully from this worktree. The existing orientation warning
remains. Code-signature verification passed. The app was installed in place on
AJ's iPhone 15 Pro Max, bundle `dev.pika.mobile.alpha`, without uninstalling or
resetting saved state. Device inventory independently reports version 1.0,
build 42; normal app launch succeeded. Installation database sequence: 4692.
Build output: `/tmp/pika-phone-update.wuZg0I/build/Build/Products/Release-iphoneos/Pika.app`.

This is installation and launch evidence, not a physical-phone send/reopen
journey. No production server binary was updated by this action; experimental
Claude shared control still requires the matching server implementation and an
explicitly opted-in future native launch.

## 2026-10-05 — release review and deployment compatibility

The owner approved publication after release checks and version-pinned updates
to rs6, rs2a and rs8 without restarting live conversations. Read-only checks
confirmed the invoking account is `ajain` on all three. At this checkpoint rs6
and rs2a run Pika 0.6.44; rs8 runs 0.6.39-rc.4. No cutover has occurred yet.

Independent review found that a valid OpenCode shared certificate could outlive
its native owner and prevent saved-history fallback. After certificate validation,
an absent exact native PID/birth now permits read-only saved history. Live-owner
or malformed-certificate failures remain errors. An actual disposable native
journey passed (17.46 seconds), covering terminal exit, fresh mobile endpoint,
saved-history reopen, disabled/rejected sending, and explicit native cold resume.
Inference was a loopback fixture; no quota was spent.

rs6's installed OpenCode 1.18.33 also passed a disposable native serve/attach
journey (1/1, 8.40 seconds): terminal-typed context, the original owner generation,
a native view switch, phone delivery to the original session only, and visible
reopen. Lead read `rs6:/tmp/pika-rs6-opencode-compat.u96Bl0/native.log` directly.
The selected native executable SHA256 is
`0abbb7c32ab0294c0a7bfa2705f9ff0df5dce5ab721d1f00cccfe393f2a11427`.
Only disposable private state, a private tmux socket and loopback fake inference
were used. Owned processes were reaped; installed providers were not changed.

First full local release-check run failed distribution fixtures while parallel
builds replaced their shared `target/debug/pika`, producing byte/size mismatches.
The ordinary-launch fake OpenCode journey also failed because argv recognition
alone selected the shared launcher. These are recorded failures, not passing
release evidence; stable-binary reruns and runtime compatibility fallback are
required before publication. Dependency notice regeneration and RustSec audit
with warnings denied passed at this checkpoint.

### Claude compacted history — native reconstruction subset (2026-10-05)

Read-only inspection of the installed Claude 2.1.274 executable on rs8
(`15e2d05148f801b5774032faad87e624ecd172e9903288bda448b892eb58fa07`)
verified the provider's `iOs` / `aOs` compaction relinker. Its semantics match
the local 2.1.272 loader: exact `preservedMessages.uuids` take precedence over
legacy `preservedSegment` ancestry; preserved records are relinked after the
explicit anchor, superseded pre-boundary records are removed, and continuation
parents are redirected to the preserved tail. The native buffered reader also
discards the prefix at a full boundary without preservation. This is native
source evidence, not an inference from append order or a third-party schema.

Pika now reconstructs that strict native chain before projecting channel
fetch/reply records and paging. A logical cursor remains tied to the exact
source inode, frozen byte snapshot and prefix hash, plus a hash of canonical
display items. If later journal settlement changes that projection, an older
page fails explicitly and requests reopening; it cannot silently shift an
item-index boundary. Later transcript appends do not enter an older snapshot.

Final isolated mobile endpoint journeys passed 7/7 in 0.94 seconds, including
67 displayed compacted-history messages across pages, physically old preserved
messages relocated after their summary anchor, append-during-paging, fresh
reopen, source-backed channel projection and honest read-only sending denial.
Focused history tests passed 14/14 in 0.06 seconds, including native list and
segment preservation, multiple boundaries, malformed/missing/duplicate
identities, real conversation branching refusal, native progress metadata,
and late projection change refusal. The complexity gate reported 4,891
functions and zero new failures at this checkpoint.

The exact extracted native relinker was also executed against three disposable
fixture graphs; all reconstructed orders matched the Rust fixtures. Artifacts:
`/tmp/pika-claude-native-compaction-functions.json`,
`/tmp/pika-claude-compaction-differential.js`, and
`/tmp/pika-claude-compaction-differential-result.json`.
These are model-free reducer comparisons, not a claim that a native CLI
compaction turn or physical-phone compaction journey was run. No quota, real
transcripts, provider configuration or live conversations were accessed.

Unknown compaction metadata, broken preservation walks, missing ancestors,
cycles, repeated native identities and unresolved user/assistant branches still
fail closed. Native timestamp fallback and the broader parallel-tool sibling
recovery reducer are deliberately not implemented. The existing 16 MiB verified
snapshot bound remains explicit. No phone Clear or Resume control was added.

### Final launch compatibility and release-source gates

OpenCode shared launch now requires a successful, bounded, private-state version
probe for the verified 1.18.31 or 1.18.33 runtime. Unknown, malformed, failed or
stalled probes preserve the original ordinary launch arguments before any shared
session is created. The provider is never upgraded automatically. Focused probe
checks passed 2/2 in 0.59 seconds.

The gate-enabled actual native OpenCode mobile journey passed 1/1 in 18.85 seconds:
`/tmp/pika-opencode-gate-evidence.Tn8000/native-journey-final.log`, independently
read by the lead. It covers consecutive replies, exact original TUI/session,
lost acknowledgement without replay, 105-message provider pagination, native
view changes, dead-owner saved-history reopen and send refusal, owned-process
cleanup, and explicit cold native resume. Inference was a loopback fixture.
An earlier compile attempt exposed a test-only dependency accidentally used in
production; it was fixed without adding a dependency and its failed log retained.

Final source formatting, all-target warnings-as-errors Clippy (30.65 seconds),
dependency-notice regeneration, RustSec audit with warnings denied, and complexity
(4,893 functions, 137 pre-existing hotspots, zero failures) pass. Full native
release checks and exact-commit CI remain separately required before publication.

### Large Claude history — streamed verification (2026-10-05)

The released 0.6.45 endpoint refused the same 21,120,496-byte synthetic source
that the corrected endpoint now reads as 120 exact messages across three
chronological pages. An actual iPhone Simulator -> loopback OpenSSH -> native
`_mobile` journey passed (1 test, 96.784 seconds): latest message 119 is visible,
older context crosses 079 -> 080 without duplication, and leaving/reopening
returns to 119. Sending remains disabled for this saved-history-only fixture.
The source SHA256 stayed unchanged and the private listener was reaped with its
port closed. No model, real transcript, fleet host, or native provider was used.
This is not a physical-phone or native-Claude-generation claim.

Evidence: `/private/var/folders/k9/s1xh63d93rq9bd97cngqvf4c0000gn/T/pika-claude-large-ui-hbyqsg1a/large-ui-01.xcresult`,
the adjacent endpoint JSON evidence, screenshots and `cleanup.json`.
`scripts/ios-claude-large-history-fixture.py` reproduces the isolated source and
SSH fixture; the existing SSH integration UI test has a `largeHistory` mode.

The optimized native endpoint was independently measured with the same 376-node
graph and 120 literal messages, increasing only non-displayed source padding:

| Source bytes | Initial open | Older-page requests | Reopen | Peak process RSS |
| --- | --- | --- | --- | --- |
| 42,103,216 | 717 ms | 709 / 712 ms | 708 ms | 13,139,968 bytes |
| 167,868,394 | 2,222 ms | 2,210 / 2,215 ms | 2,207 ms | 13,221,888 bytes |

Each run checked every returned literal, chronological order, exact reopen page,
send capability, clean process exit and unchanged source hash. These local
measurements are not fleet latency guarantees. Full-source scanning repeats per
page, so CPU and I/O scale with source size even though body retention does not.
Artifacts: `/tmp/pika-large-history-release-40.json` and
`/tmp/pika-large-history-release-160.json`.

Actual endpoint regressions passed 10/10, including >20 MiB native-compaction
fixtures, 5,000 small nodes, append/paging/reopen, same-inode nonprefix rewrite
refusal, and late receipt settlement invalidating an older cursor. Production
reader checks include changed-record refusal even if original bytes are restored
before the final digest. No obsolete test-only reader stands in for production.

This removes the 16 MiB whole-transcript cutoff. Individual durable records
retain their explicit 256 KiB bound; the separate conservative ancestry-memory
charge is bounded at 128 MiB (not a promise about total RSS). Unsupported native
ancestry remains a visible refusal rather than truncation or branch guessing.
No persistent index, provider state change, dependency, or phone runtime change
is introduced by this fix.

Final local gates: 54 native test targets completed, 1,327 passed, zero failed,
31 explicitly ignored; formatting and diff checks pass; warnings-as-errors
all-target Clippy passes (22.58 seconds); complexity passes (4,904 functions,
137 existing hotspots, zero failures). The dependency lockfile and notices are
unchanged, so the same-day passing release audit and notice verification remain
applicable. Independent adversarial review scored the scoped fix 95/100 with
no unresolved material defect. Publication and fleet deployment are not covered
by these local results and remain separate approval-gated actions.
