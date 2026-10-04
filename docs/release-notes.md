Pika 0.6.42 keeps multiple paired machines together on the iPhone board and adds
exact-thread provider controls. Saved machines retain independent subscriptions,
including cached offline rows. Optional nicknames change presentation only;
conversation actions remain bound to the verified owning machine.

Version 0.6.41 was not published after macOS CI exposed a disposable terminal
recorder issue. This release keeps the same production changes and corrects the
test recorder's input lifetime and Files frame-completion wait without weakening
the receipt or file-preservation assertions.

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
