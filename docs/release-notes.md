Ask an expert and see its answer arrive.

- **Faster Codex questions.** The board and CLI use Luna-medium by default.
  Choose `pika ask NAME --deep` for Sol-medium. Claude and OpenCode keep their
  native profiles.
- **Answers as they arrive.** Codex consultations show provisional text on the
  board. Agent callers can use `--jsonl --stream`; supported remote hosts stream
  too, while older hosts continue returning completed answers.
- **Less unnecessary work.** Named experts are consulted directly without a
  card refresh. Related follow-ups reuse the same private conversation.
- **Clear completion.** Pika waits for a confirmed completed turn and reports
  observed timing and token usage. Partial text is never presented as success.

Update from the board's **Update now? [y/N]** offer, or run `pika update`.
Reopen the board after updating. Existing agents keep running.

Windows packages remain unsigned in this release. Managed endpoint security may
still block installation or launch; this release does not resolve that limitation.
