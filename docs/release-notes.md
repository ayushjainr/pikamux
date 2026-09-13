Quota at a glance. A steadier board.

- See weekly Codex and Claude allowance, reset times, and reading freshness above
  the board shortcuts on macOS and Linux. The strip stays tied to the machine
  running Pika, even when you select a thread on another server.
- Reduce flicker with buffered, synchronized frames, including in Windows Terminal.
  Unchanged frames no longer trigger a repaint.
- Get background update notices without interrupting agents. Installation remains
  your choice; Windows shows the PowerShell update command inside the board.

Claude quota uses its signed-in CLI's status-line feed during normal activity.
Run `pika setup` to add the collector; existing status-line commands are preserved.
Pika does not send model prompts or extract login tokens to populate the meter.
The native Windows client has no local quota source yet and labels that explicitly.

Update on macOS or Linux:

```sh
pika update
```

Update in Windows PowerShell:

```powershell
irm https://raw.githubusercontent.com/ayushjainr/pikamux/main/install.ps1 | iex
```

Close and reopen the board after updating. Your running agents are left intact.
