Your machines, together in a local Windows board.

- Run `pika` to see all paired machines together, without choosing a remote
  board host. Existing pairings are retained.
- Add several SSH or Tailscale machines through `pika setup`.
- Open the exact conversation directly in a new Windows Terminal window.
  Launch failures remain visible in the board; no automatic retry is made.
- Keep private consultations and read-only peeks inside the panel.
- See cached rows immediately, including when one machine is offline.

In local PowerShell:

```powershell
irm https://raw.githubusercontent.com/ayushjainr/pikamux/main/install.ps1 | iex
pika
```

The native Windows client is experimental; agents remain on Mac or Linux hosts.
No reverse SSH tunnel or single hub is needed. Existing Mac and Linux workflows
remain unchanged.
