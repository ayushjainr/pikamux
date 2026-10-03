# Installed-provider mobile evidence — 3 October 2026

All provider execution below used Codex 0.160.0 with disposable HOME, CODEX_HOME,
XDG/database/temporary roots and a synthetic localhost Responses service. No
real model, user transcript, fleet machine or installed Pika state was used.
These are implementation evidence, not production deployment or Tailscale proof.

## Original running conversation

`tests/ios_codex_socket_spike.rs` verifies two independently connected clients
on one original app-server/thread. A native question emitted before late attach
is replayed unchanged; answering its actual JSON-RPC request changes the original
function-call output. Exact active-turn steering enters subsequent inference
and native history; stale expected-turn steering is rejected.

The production handler test
`mobile_handler_resolves_question_and_recovers_lost_multiline_receipt` passed
in 1.58 seconds after exact kernel-peer process verification was introduced.
It kills the mobile handler after admission without consuming its receipt,
reconnects, reconciles delivery using native user-item client ID, resends the
same operation ID without another user item, and rejects conflicting reuse.

Run:

```sh
PATH=/Users/ayushjain/.rustup/toolchains/1.88.0-aarch64-apple-darwin/bin:/usr/bin:/bin \
scripts/with-test-home.sh env PIKA_IOS_CODEX=/Users/ayushjain/.local/bin/codex \
cargo test --test ios_codex_socket_spike -- --ignored --test-threads=1 --nocapture
```

## Signed simulator → SSH → handler → original provider

The native app executor's signed simulator test `11820` passed one test, zero
failures, with no runtime warnings (`ssh-integration-06.xcresult`). Its separate
temporary sshd accepted the manually configured connection and pinned key; the
app read original context and a native pending question, answered `Proceed`,
and sent exactly `Exact synthetic mobile reply\nsecond line` (a real newline).

Independent provider fixture `79130` retained original server PID `12413` and
thread `01a10007-f1e6-7cc3-95cf-633c5df98d35`. The original inference verified
the actual question's function output contained `Proceed`, then verified exact
multiline text in continuation and printed:

```text
PIKA_IOS_FIXTURE_DELIVERED exact multiline input reached the original provider inference for 01a10007-f1e6-7cc3-95cf-633c5df98d35
```

The fake model returned `Fixture received the exact multiline reply.`.
The fixture stopped cleanly after 266.87 seconds, killing/waiting its original
provider and deleting its temporary roots. That held app fixture did not itself
count duplicate native user items; the separate handler fault-injection test
above proves duplicate prevention. A real device/network/model was not tested.

## New conversation and first reply, including native desktop

`tests/ios_mobile_creation.rs` passed in 1.52 seconds after finding and correcting
a real ordering defect: setting provider name before native home certification
made the core's existing-name guard discover its own just-created thread.
Naming now follows exact UUID certification; the guard remains unchanged.

The installed-provider test verifies initial loaded thread count zero, explicit
mobile creation of one UUID, its actual managed native TUI, and no initialization
model turn. It terminates the handler without reading the successful creation
receipt, reopens, reconciles the original creation ID, and repeats that same ID
without another UUID. The first exact user message enters synthetic inference
and native user-item history. Its synthetic response renders in the same live
desktop TUI; native PID generation and sole loaded UUID remain unchanged.
Only the disposable tmux server/provider are then stopped.

```sh
PATH=/Users/ayushjain/.rustup/toolchains/1.88.0-aarch64-apple-darwin/bin:/usr/bin:/bin \
scripts/with-test-home.sh env PIKA_IOS_CODEX=/Users/ayushjain/.local/bin/codex \
cargo test --test ios_mobile_creation -- --ignored --test-threads=1 --nocapture
```

## Remaining gates

Actual typed file approval execution, original no-daemon TUI adoption,
unverified providers, app-level main assistant and creation/adoption journeys,
real device/Tailscale operation, and long-history receipt cursor continuation
still require their own evidence. The source supports resumable read-only receipt
pages, all-row atomic board pages and stable-ID selector pages; source support
alone is not end-to-end proof.

## Final backend reruns and one-time command approval

After operation-handler refactoring, the isolated actual-provider rerun `9488`
passed: socket/handler two tests in 1.33 seconds, creation/first desktop reply
in 0.79 seconds, and extended assistant shared-launch in 8.09 seconds. The
assistant test now calls the actual `_mobile assistant/open` path against a
normally saved default private profile; it verifies exact profile, scope,
memory epoch and UUID, one loaded thread, unchanged binding, and original
synthetic preference recall through the real Pika MCP after mobile selection.
This is backend assistant evidence, not phone assistant evidence.

Actual command approval run `21111` passed in 2.01 seconds. Installed Codex
offered `exec_command`, emitted `item/commandExecution/requestApproval`, and
replayed the complete original pending request to a late mobile connection.
The mobile one-time acceptance changed the original native tool output to
`synthetic-approved`. Repeating the same stable operation ID did not resubmit;
the original provider PID, native TUI generation and sole thread UUID remained
unchanged. All execution used a disposable root and synthetic loopback inference.

The strengthened rerun `79546` passed in 1.96 seconds, also exercising normal
desktop `untrack codex:<UUID>`, native-backed mobile candidate selection and
`conversation/adopt` of that exact original UUID. The saved unread value was
unchanged by Add, and provider PID, native TUI generation and sole loaded UUID
were unchanged. Previously watched identities now remain eligible for explicit
Add only when their exact native provider record still exists; no guessed rename
or replacement conversation is required.

```sh
PATH=/Users/ayushjain/.rustup/toolchains/1.88.0-aarch64-apple-darwin/bin:/usr/bin:/bin \
scripts/with-test-home.sh env PIKA_IOS_CODEX=/Users/ayushjain/.local/bin/codex \
cargo test --test ios_mobile_creation actual_command_once_approval -- --ignored --test-threads=1 --nocapture
```

A bounded file-change probe stopped after 26.18 seconds: this installed provider
did not offer `apply_patch` for the synthetic model configuration. No file
approval was fabricated or accepted. Typed file schema and native presentation
remain separately tested, but actual provider file-decision execution remains
unverified. The unsupported probe is not part of the passing opt-in test set.

Two held-assistant synthetic-inference preparation runs failed closed (`84503`,
98.35 seconds; `30630`, 84.11 seconds): the first actual native HTTP request
contained the user input but no top-level `tools` array. Subsequent actual run
`30297` proved this was the fixture's old-schema assumption: normal
`POST /v1/responses`, `tool_choice:auto`, and native `input.additional_tools`,
not compaction or proof tools were unavailable. This agrees with the installed
version's [Responses Lite regression source](https://github.com/openai/codex/blob/rust-v0.160.0/codex-rs/core/tests/suite/responses_lite.rs).
No model-invoked memory output was fabricated. Failed roots and processes were
cleaned. Run `30297` passed in 8.82 seconds: exact role=user mobile text entered
the original private inference, loaded UUID/native generation were unchanged,
and real Pika MCP still recalled the original preference afterward. Native
inference delivery and direct real MCP retention are separate evidence; this
does not claim a model-invoked memory tool call.

Later app run `29109` (`ssh-integration-07`) passed its original-context,
saved-Keychain relaunch, background/reconnect, original question answer, exact
multiline reply and original final-response assertions, then failed its added
desktop-removal synchronization. Provider fixture `52497` independently proved
delivery, but normal desktop `untrack` correctly refused complete ownership
reconciliation while some process command lines were unreadable. The initial
Simulator attribution was incomplete: native app `08` subsequently passed
one test in 54.491 seconds, but fixture `99023` still refused removal after
Simulator shutdown. Read-only exact PID inspection identified `50173` as our
owned, unprivileged temporary sshd listener, not an unrelated user process.
Both failed fixtures cleaned their original owners/roots; later recreated
empty databases are not continuation evidence. No global guard was weakened.
The native-app Add round trip requires stopping our owned Simulator and test
sshd before the real desktop command, then restarting the identical SSH setup.
This is not proven to be only a test-environment limitation. A root-owned
listener is excluded by the existing real/effective UID filter; an authenticated
SSH child whose real and effective UIDs both match the user remains included.
If macOS denies that child's command line, ordinary Mac SSH can encounter the
same complete-inventory refusal. That deployment and Tailscale remain unverified.

Main-Pika native app run `18244` (`ssh-assistant-02`) passed one test in
48.918 seconds with no runtime warnings: real SSH host-key confirmation,
saved Keychain relaunch, Main Pika open, exact original reply and rendered
original response. The same private UUID `01a10051-526c-79f2-b991-49ac2a6a8de6`
and native PID `52386` were preserved. Earlier app `2015` failed before sending:
the genuine desktop TUI was waiting for review of its six generated hooks.
Only the disposable fixture's verified generated hooks were accepted through
the normal native trust screen; no production auto-trust or launch change was
introduced. The original inference independently captured the exact phone text.
Held fixture `39688` later failed after 968.42 seconds because its test worker
incorrectly required a subsequent background-maintenance inference to contain
the original phone input. This failed fixture cleaned its owner and root;
it did not complete its post-phone MCP assertion.

Corrected independent backend run `17555` passed one test in 8.83 seconds.
It required the first inference's exact mobile input and then verified the
original real Pika MCP still recalled the synthetic preference after the
mobile interaction, with one loaded UUID and unchanged native PID/generation.
This complements the native phone proof; it is not a model-invoked memory
claim or a retroactive passing result for fixture `39688`.

Creation-fixture readiness run `89824` passed in 1.57 seconds: a genuine
configured seed project, actual mobile creation, first message delivered to
the newly created native UUID, and final response rendered in its original
desktop TUI. The held native-phone creation/Add journey remains separately
pending and is not implied by that readiness check.

Initial actual Start invocation `35252` was skipped because the test config
was passed as an environment variable rather than the required build setting.
Corrected app run `1597` failed its saved-board assertion before Start.
Retry `15326` waited for affirmative connection and also failed before Start.
Direct exact-environment diagnostics established the cause: the disposable
fixture had created its state directory with mode 0755, whereas assistant
observation requires owner-only state. After correcting only that fixture
directory to 0700, the real endpoint returned subscribed:true and one complete
board row. No production guard was changed. Neither failed app run created a
native thread or submitted a message; the retry retains the original provider.

Native Start `38364` failed before authentication because nested trust-sheet
presentation incorrectly triggered cancellation. The scoped sheet-dismissal
fix was applied. Actual retry `71424` reached saved login and real project
selection, then Start was definitively rejected before provider dispatch.
Receipt `B96543D9-80B2-412C-8C1F-9F9AEB0D6682` recorded rejected with null
identity: complete ownership observation could not read PIDs 63150/63152.
The UI owner independently identified those as its temporary SSH session
children using the established loopback connection. Stopping only the listener
preserved that channel, so the existing global launch guard still correctly
refused. No guard was relaxed and no new UUID was created. Held fixture
`85629` was stopped and failed at 871.68 seconds because no phone-created
delivery proof existed. Full real-phone Start remains unverified on a normal
system SSH deployment and may face the same protected same-user SSH-child
limitation; backend creation is independently proven. Global complete inventory
is currently required to establish absence of conflicting owners before launch,
not merely to identify the selected existing shared provider.

Fresh backend-origin fixture `63240` created UUID
`01a10074-2359-70c3-b109-a42f2ada0dc1`, delivered its first message through
the actual mobile endpoint, and rendered the response in native TUI PID
63943 under original provider PID 63847. With the owned Simulator and test
SSH stopped, normal desktop `untrack` removed that exact UUID. Native app
`13490` (`ssh-adopt-01`) then passed one test in 73.318 seconds, with no skips
or runtime warnings: manual SSH trust, saved Keychain relaunch, exact genuine
candidate Add, original first reply and final response. Independent database
and process checks confirmed the same UUID, saved name, unread=0 unchanged
by Add, and the same original provider/TUI PIDs; no new inference was sent.
Held fixture `63240` passed and cleaned up in 226.04 seconds. This proves
the native Add-back journey, not phone-origin Start.

Read-only owned-SSH diagnostics subsequently found a safe collector route.
Kernel `pidpath` succeeded for both protected children even when argv was
unreadable: the actual modern image was `/usr/libexec/sshd-session`, not the
initial diagnostic's expected `/usr/sbin/sshd`. The initial assertion failures
were retained as that expected-path mistake; the corrected diagnostic passed.
The reviewed macOS fallback now excludes only either exact trusted OS image
after argv retries fail, with root-owner/non-writable/protected/no-symlink
metadata and positive ACL-absence proof for the image and ancestors. Kernel
path and PID/start/real/effective UID are rechecked; root callers, denied reads,
unknown images and wrappers retain the original partial result. Children are
still independently enumerated. Post-fix owned-child run `37481` passed in
0.04 seconds and retained a separate readable fake-provider process. Process
regressions passed 25 tests with one installed-OS check ignored; the core
partial-observation/no-launch regression passed in 0.03 seconds. This corrects
the collector defect without changing provider-ownership absence requirements.
Actual phone Start `45570` (`ssh-create-06`) subsequently passed one test in
82.450 seconds with no skips or runtime warnings, over the live SSH listener
without the earlier stop-listener workaround. It covered manual host-key
trust, saved-login relaunch, native Start, new identity, exact first reply and
rendered final response. The actual provider fixture `33189` independently
verified the new identity differed from the seed and captured the exact first
inference, but failed at 156.94 seconds on an incorrect test assumption that
the unused empty seed must remain loaded forever. It had not yet checked the
new native TUI PID and rendering when that assertion stopped it. The owner and
root were cleaned. The fixture now verifies persistent seed identity rather
than pinning its idle runtime; independent corrected native-home validation
is recorded separately, not retroactively claimed for this failed fixture.

Corrected whole-chain native Start `16197` (`ssh-create-07`) passed one test
in 81.308 seconds with no skips or runtime warnings. The ordinary test SSH
listener stayed live throughout. Original fixture `36111` independently proved
new UUID `01a10094-b71a-7860-a5d7-22442d2bc94b`, original provider PID 90631,
new native TUI PID 91640 with unchanged generation, exact first reply in native
history and inference, and the final response rendered in that same desktop
TUI. The persisted seed identity remained unchanged, with no unexpected loaded
UUIDs. The fixture waited for completed phone assertions/screenshots before
cleanup and passed in 220.21 seconds; both exact PIDs were then absent.
This closes actual phone-origin Start over the exercised macOS SSH transport;
Tailscale and other operating-system/server deployments remain unverified.

Actual native command approval app `33390` (`ssh-approval-01`) showed the exact
original command and reason, then sent Allow once. The original actual provider
fixture `18244` verified `synthetic-approved` in the next native tool output,
the final response rendered in its original TUI, one loaded UUID, and unchanged
provider/TUI generations. The app test nevertheless failed because it queried
isEnabled on the correctly removed, resolved button before its final assertion;
its failure hierarchy already contained the final response. This remains a
failed UI test, not a clean app pass. Its closed request was not replayed;
cleanup preceded a fresh test-only approval chain.

Clean actual native command approval `62797` (`ssh-approval-02`) passed one
test in 77.994 seconds with no skips or runtime warnings. It presented the
exact original command and reason, sent Allow once, observed the terminal
request card, and asserted the final original response. Original fixture
`45593` independently captured the native tool output `synthetic-approved`,
the original desktop TUI final response, exactly one loaded UUID
`01a1009b-005f-72b1-b82c-d231e7f6f56c`, unchanged provider PID 93442 and
TUI PID 93506/generation. Cleanup waited for completed app assertions and
screenshots. This proves command-once approval through the full actual phone
chain; file approval remains schema/UI-only, not actual-provider execution.

The bounded new-creation board check `25239` failed in 47.18 seconds after its
original native creation/reply proof passed. It exposed a real freshness bug:
fractional observation times were compared against a current clock truncated
to integer seconds. The endpoint now retains fractional time; missing, truly
future, expired and non-finite observations still fail stale, as do producer
stale flags. The new regression and board paging tests passed 2/0; current
warnings-as-errors Clippy passed in 8.42 seconds. The initial decimal exact-60s
test used a non-exact binary floating-point boundary and was corrected to an
exactly representable boundary, without introducing a semantic tolerance.

Actual bounded rerun `27910` passed in 21.85 seconds. It reused the existing
subscribed shared feed, created UUID `01a100a6-2377-7e91-9d5e-ce9884ef4b02`,
and independently verified its original first inference/history, native TUI
PID 96660/generation and final rendering under provider PID 96561. Feed revision
2 showed STARTING with unknown/stale observation; revision 4 showed READY,
stale=false and actual observedAt=1791012270.898703 after 19.87 seconds.
No hooks, second observer, inferred app state or extra scan were added. This
proves the existing 20-second fallback resolves the created thread's Starting
state and freshness. Both exact PIDs and the temporary root were cleaned.
