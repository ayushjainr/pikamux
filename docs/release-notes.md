Keep the board in sight while working with an agent.

- **Live counts on older tmux.** The return bar now receives the board's live
  counts on tmux 3.2a, without borrowing another attached client's feed.
- **Know what needs attention.** The bar names the newest attention item and
  distinguishes an agent asking for input from an opening warning. Labels
  update as attention changes and shorten on narrow terminals.
- **Compatible across machines.** Older hosts keep supported counts and request
  labels without displaying opening warnings as agent-authored questions.

Update from the board's **Update now? [y/N]** offer, or run `pika update`.
Reopen the board after updating. Existing agents keep running.

Windows packages remain unsigned in this release. Managed endpoint security may
still block installation or launch; this release does not resolve that limitation.
