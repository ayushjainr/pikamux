Update from Pika. Keep your agents running.

When a new version is available, Pika asks **Update now? [y/N]**. Press Y to
download and verify it, then return to the updated board. Enter, N, or Esc leaves
your installation unchanged; U opens the offer again.

- Works on macOS, Linux, and the native Windows client.
- Waits until you're out of a private consultation or text-entry field before
  offering an update.
- Updates only this machine. Running agents, paired machines, and conversations
  are left intact.
- Windows verifies the new executable before handing off, including when an
  already-open PowerShell still points at the older release.

To get this update from an older Windows version, run the installer once more:

```powershell
irm https://raw.githubusercontent.com/ayushjainr/pikamux/main/install.ps1 | iex
```

On macOS and Linux, run `pika update`. Future board updates need only your
confirmation.
