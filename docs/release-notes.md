Pika 0.6.46 fixes opening large Claude histories on your phone.

The reader no longer rejects an entire conversation because its file exceeds
16 MiB. It verifies ancestry and reads pages without retaining every message body
in memory. Chronological order, supported native compaction, exact conversation
identity and source-backed phone exchanges are preserved. Source changes or late
receipt settlement invalidate an older cursor rather than silently shifting it.

The same 21 MiB fixture rejected by 0.6.45 now passes the actual iPhone Simulator
to SSH journey: open, load older context, leave and reopen at the latest reply.
Local optimized measurements opened 40 MiB in 0.7 seconds and 160 MiB in 2.2
seconds, with about 13 MB peak process memory for both fixed-graph fixtures.
These are local measurements, not fleet latency guarantees.

Individual records retain their 256 KiB limit; ancestry metadata has a separate
bounded budget. Unsupported or ambiguous native histories remain explicit errors.
No transcript is truncated or modified. No new phone build, USB connection,
provider upgrade or running-thread restart is required.

Retained features from 0.6.45:

Pika adds native mobile continuation and repairs long-thread history ordering.

- Codex history pages retain chronological order across reopen and older-context
  loading; the iPhone follows new replies after sending.
- Compatible future Pika-managed OpenCode launches share their original native
  server/session with the phone. Existing live terminals are not migrated.
  After the terminal exits, saved history remains readable and send is disabled.
  Verified native versions are 1.18.31 and 1.18.33; other versions retain ordinary
  terminal launch without shared mobile control.
- Future Claude launches can explicitly opt in with `pika --claude-mobile`.
  This uses experimental native development Channels and retains Claude's account
  policy, warnings and tool permissions. The original native conversation must
  attest its tool calls before message text is released or a reply is accepted.
  Uncertain delivery is not retried automatically. No mobile Clear/Resume is added.
- Claude phone exchanges remain in their source-verified conversation history
  after exit. An external native conversation switch revokes the old phone route.
  Supported normal compaction records are reconstructed in native ancestry order
  before paging, including preserved messages written earlier in the source.

The separate iPhone alpha build is 42. Native release archives contain Pika host
and client binaries, not an iPhone installation. Verification includes actual
Simulator → SSH → native Claude send/reply/reopen and native OpenCode lifecycle
journeys; evidence and limitations are in `ios/NATIVE_EVIDENCE.md`.
Muse shared mobile control remains unavailable. Claude/OpenCode model and skill
menus are not claimed equivalent to Codex's verified live controls. Large or
ambiguous histories that cannot be verified remain explicit errors rather than
silently guessed branches. Updating Pika never upgrades the providers themselves.

Retained fixes from 0.6.44:

Pika fixes the assistant's first terminal attachment after its launch
becomes a certified exact conversation. The original launch receipt can recover
that same proven live home without creating or resuming another provider and
without undoing an explicit unwatch. Normal explicit thread selection is unchanged.

The assistant also locates Codex's bundled runtime shell in the standard npm
native-package layout. Existing capability checks, private credentials, state,
account ownership and permissions remain unchanged. Running providers are not
restarted. The iPhone app source and build identity remain unchanged.

Retained features from 0.6.43:

Pika keeps multiple paired machines together on the iPhone board and adds
exact-thread provider controls. Saved machines retain independent subscriptions,
including cached offline rows. Optional nicknames change presentation only;
conversation actions remain bound to the verified owning machine.

Version 0.6.41 was not published after macOS CI exposed a disposable terminal
recorder issue. This release keeps the same production changes and corrects the
test recorder's input lifetime and Files frame-completion wait without weakening
the receipt or file-preservation assertions.

Version 0.6.42 was also withheld after recovery instrumentation exposed concurrent
SQLite sidecar cleanup during assistant storage validation. Optional sidecars may
disappear safely; mandatory state files and unsafe ACLs, permissions, ownership or
links remain rejected. The phone source and build identity are unchanged.

Compatible Codex threads expose live model and skill catalogs. Model changes
require provider confirmation without sending a chat turn; skill references are
validated before dispatch. Unsupported or uncertain outcomes remain visible.
The private assistant also supports safe account homes beneath administrator-owned
mounts without moving credentials or changing directory permissions.

Retained pairing support from 0.6.40 covers existing SSH accounts whose home is under
an administrator-owned mount, such as `/mnt/ebs1/USER`.

Run `pika update`, then `pika pair`. Pika stays under the invoking account and
does not change mount ownership, relocate keys or restart conversations. The
account's home and SSH files retain their ownership, permissions, ACL and link
checks. Mount administrators are trusted to preserve the account's home path,
as they already control that path; Pika does not protect against their tampering.
SSH host-key validation and custom key paths outside home remain unchanged.

Retained iPhone server support from 0.6.39:

- Run **pika pair**, or choose **Connect phone** in board settings, then scan
  from Pika on the phone. A short-lived QR pins enrollment; the phone creates
  its own restricted SSH key. Tailscale/private routing and SSH must work first.
- Read the machine's board and connect to exact existing compatible Codex
  conversations. Start a thread explicitly, reply, and respond to supported
  original provider requests without creating a separate mobile conversation.
- The next normal Pika assistant launch supports private shared attachment.
  Existing running assistants are not restarted by an update.

The iPhone app is an alpha, not distributed through these binary release assets.
Other provider icons do not imply mobile control support. No other machines,
live provider sessions, or existing pairings are automatically migrated.

Retained desktop features from 0.6.38:

- **F12 once** opens the compact thread list; **F12 again** returns to the full
  board. Numbered recent choices make switching faster without new global key
  bindings. Choices remain fixed while the list is open.
- The companion shows cached weekly provider usage using the existing feed,
  without extra quota requests. Shared-terminal switching preserves the exact
  calling client, and remove/re-add recovery can find the existing live terminal.
- **P** on the board and **pika pika** open the persistent assistant in native
  Codex/Luna. It has private Pika instructions and skills, with required backend
  tools for scoped memory, board context, feedback and maintenance. Internal
  workspace paths are omitted from its native footer and window title.
- Transient local database contention no longer leaves pane previews waiting
  through the longer general error backoff. Recovery still rechecks exact identity.

The assistant's provider authentication remains separate: credentials are not
copied from coding sessions. Board sharing, private consultation and background
work retain their explicit controls. This release does not promise perfect recall
or unrestricted autonomy. Native Claude assistant hosting is not included.

Updating the binary does not upgrade other machines, migrate live tmux sessions,
or replace an existing provider conversation. Windows remains a client/bridge,
not a native agent host. Apple notarization and Windows signing enrollment remain
unconfigured.
