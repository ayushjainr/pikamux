Open your agent. Return to your place.

Pika now returns to the same board selection and filter after you detach from a
native agent. If opening fails, the explanation stays in the board. An uncertain
launch is never automatically repeated.

- Recover from transient verification conflicts during local and remote opens,
  while retaining exact-conversation checks.
- Preview pane output with **p** without leaving the board or marking work read.
  Saved conversations without a live pane get a clear explanation.
- Use **?** for keyboard help, **u** for cumulative usage, and **x** to stop
  watching a conversation without stopping its agent.
- See recorded activity, pane availability and explicit missing-card information
  in the inspector.
- Connect through SSH aliases that define their own remote command, including
  hosts where Pika is installed outside the noninteractive shell's PATH.
- Read structured OpenCode errors instead of an object placeholder.

Update from the board's **Update now? [y/N]** offer, or run `pika update`.
Each machine updates independently; running conversations remain intact.

The OpenCode hook improvement takes effect after reviewing and applying
`pika setup` on the host. Updating the executable alone does not rewrite hooks.
