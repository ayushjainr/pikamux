Keep the installed consultation skill current after upgrades.

- **Safe skill refresh.** An approved macOS/Linux `pika update` refreshes an
  installed agent-convo skill only when its bytes match a known shipped bundle,
  and backs up the prior copy first.
- **Older-updater compatibility.** A managed board reconciles the bundled skill
  on opening the new board when an older updater installed the runtime; the
  check is bounded and idempotent.
- **User content stays yours.** Absent, customized, symlinked, and externally
  managed copies are left untouched. A skill failure is reported separately;
  it does not undo a successful binary activation.

Update from the board's **Update now? [y/N]** offer, or run `pika update`.
Reopen the board after updating. Existing agents keep running.
`pika update --check` never writes skills. Use `pika skill install` to explicitly
install the bundled skill with a backup.

Windows packages remain unsigned in this release. Managed endpoint security may
still block installation or launch; this release does not resolve that limitation.
