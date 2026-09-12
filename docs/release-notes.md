Open your whole Pika board from Windows with one command.

- Run `pika`, choose your existing Mac or Linux board once, and see its local
  conversations and configured servers together.
- Pika remembers your choice and handles pairing, local window routing, and
  the SSH connection. No manual SSH configuration edits.
- Open fleet conversations through the board host without pairing each
  destination separately on Windows. Machine and conversation identities are
  checked before attachment.
- Keep the board open after launching a conversation in a new window.
- Existing Mac and Linux workflows remain unchanged.

In local PowerShell:

```powershell
irm https://raw.githubusercontent.com/ayushjainr/pikamux/main/install.ps1 | iex
pika
```

The native Windows client is experimental; agents remain on Mac or Linux hosts.
Keep the board host updated with `pika update`. macOS and Linux installation:

```bash
curl -fsSL https://raw.githubusercontent.com/ayushjainr/pikamux/main/install.sh | bash
```
