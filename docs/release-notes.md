Pika 0.6.30 fixes two misleading states on the board.

- **A conversation can reopen after a duplicate exits.** Pika no longer keeps
  an old `OPEN TWICE` warning alive after exact live evidence proves that only
  one owner remains. Genuine duplicate owners still block an unsafe open.
- **Claude usage remains useful between replies.** The last weekly reading
  stays visible with a stale label until its reset. If Claude has not supplied
  a reading for the current week, Pika says so instead of showing “awaiting
  usage.” Expert upkeep still needs a fresh reading before spending quota.

This release also includes a test-oracle audit. Running agents are not stopped
automatically. Update each host separately to get the new board behavior.
Windows artifacts remain unsigned unless publisher signing is enrolled;
enterprise policy may still block them.
