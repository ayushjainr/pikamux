Pika 0.6.33 brings the persistent assistant onto the main board.

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
