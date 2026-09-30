Pika 0.6.35 improves native navigation, terminal colors and connection UX.

- **F12 once** opens a compact thread list beside the agent. Arrows and Enter
  select an exact conversation; Escape closes the list. **F12 again** returns
  to the original full board with its selection and filter preserved.
- Native launches advertise tmux RGB capability even when SSH drops the color
  hint. Existing provider processes retain their original environment.
- Assistant connection, help and settings preserve drafts and profile identity.
  Startup reconnect is explicit; accepted or uncertain turns are never replayed.
  Installed npm Codex uses its validated native payload.
- Machine settings, Files reading and named discovery gain clearer behavior,
  backed by expanded isolated terminal and onboarding checks.

This ships the current assistant implementation, not a new provider-thread design.
Updates do not migrate fleet machines or live tmux sessions. Keep the tmux client
compatible with its running server: a 3.7c client with a 3.2a server can fail
attachment with a misleading “not a terminal” message. Plan migration separately.

The existing assistant capabilities and boundaries remain:

- Press **P** to talk to Pika. Wide terminals retain the live workstream rail;
  narrow terminals focus the conversation beneath the board header. Esc/F12
  returns to board navigation. The existing **a** expert consultation stays separate.
- Normal **pika pika** and board entry share a saved startup selection. Your
  existing assistant identity, scope, memory and provider conversation stay in
  place. Missing or mismatched profiles fail visibly rather than opening an
  empty replacement. No credentials are copied.
- The assistant keeps scoped memory and decisions with BM25 recall, supports
  separately enabled consolidation and Reflection, and carries its own bundled
  skills. **/feedback TEXT** saves your exact feedback locally without a model call.
- Background spending, board metadata sharing and private consultations retain
  their explicit boundaries. Opening the board does not itself send a model turn,
  grant transcript access, or stop project agents.

Existing assistant users can select their authenticated profile once with
`--set-default`; see the assistant guide in the repository.
Long-term recall quality and evolving collaboration are being evaluated through
daily use; this release does not promise perfect memory or unrestricted autonomy.
Native Windows provider hosting remains deferred. Windows signing is not yet
enrolled, and enterprise policy may still block unsigned artifacts.
