# Installation and updates

Pika runs on macOS and Linux, on ARM64 and x86-64. You need tmux and at least
one agent CLI installed and signed in: Codex, Claude Code, or OpenCode.

```bash
curl -fsSL https://raw.githubusercontent.com/ayushjainr/pikamux/main/install.sh | bash
```

The installer downloads the native executable, verifies its checksum, installs
the included agent-convo skill, and offers setup. Setup previews changes to
agent settings and backs up existing files before applying them.

## Prerequisites

Install tmux if it is missing:

```bash
brew install tmux                 # macOS
sudo apt-get install tmux         # Debian / Ubuntu
```

Use your distribution's package manager on other Linux systems. Provider CLIs
must be installed separately. See [first consultation](first-consultation.md)
for consultation compatibility.

## Command location

The default command is `~/.local/bin/pika`. If your shell cannot find it,
add that directory to your PATH in your shell configuration:

```bash
export PATH="$HOME/.local/bin:$PATH"
```

Then open a new terminal. You can also run `~/.local/bin/pika` directly.

Releases are stored under `~/.local/share/pikamux`; your configuration and
conversation metadata remain in `~/.config/pika` and `~/.local/state/pika`.
The installer does not replace a command owned by another installation method.

For unattended installation, use `bash install.sh --no-setup`. This skips
skill installation and onboarding. Run `pika setup` when ready.

## Updates

```bash
pika update --check
pika update
```

The first command checks release metadata and caches an update notice for the
board. The second lets you review and install the available version. You can
also press **U** on the board to review a cached update.

Pika verifies the download and prepares the new release before switching the
command. Running agents continue; reopen Pika to use the new version.
Updates apply only to the local machine. To review newer hooks and bundled
skills after an update, run `pika setup`.

If needed, return to the previous retained native release:

```bash
pika update --rollback
```

## Other machines

Install Pika on each machine that hosts agents, then add those machines through
`pika setup`. SSH authentication must already work.

To upgrade a configured machine:

```bash
pika machine upgrade devbox
```

For offline or private distribution, supply a verified bundle explicitly:

```bash
pika machine upgrade devbox --bundle /path/to/release
pika setup --machine devbox --install-bundle /path/to/release
```

Only the installation bundle crosses SSH. Remote upgrades do not copy
conversation history or provider credentials.

## Build from source

With Rust 1.88 or newer:

```bash
cargo install --path . --locked
pika setup
```

Source installations are updated through Cargo, not `pika update`.

## Windows

Windows can host Pika inside a supported WSL Linux environment. The native
Windows download is an experimental client for opening conversations on a
macOS or Linux host; it does not host agents or tmux itself.

Run in a local PowerShell window, not inside SSH:

```powershell
irm https://raw.githubusercontent.com/ayushjainr/pikamux/main/install.ps1 | iex
pika setup YOUR_SSH_HOST
```

Replace `YOUR_SSH_HOST` with your existing SSH host name. Pika must already be
installed on that host. Pairing prints the reverse-forward setting needed to
open exact conversations in Windows Terminal.

The installer requires 64-bit Windows and PowerShell 5.1 or newer. It verifies
the release manifest, checksum and ZIP contents before running the client,
installs under `%LOCALAPPDATA%\Pika\Client`, and adds the command to your user
PATH and current PowerShell session. No administrator access is needed. It does
not change SSH configuration, pair machines, or start a bridge during installation.

Run the same install command to update. Previous releases remain on disk; an
already running bridge is left alone. Windows `pika update` is not available.
Offline installation accepts `install.ps1 -Bundle C:\path\to\release`; `-NoPath`
leaves PATH unchanged. See [client pairing](guide.md#open-a-local-window-from-a-remote-pika-board).

## Troubleshooting

- **Command not found:** check the command location and PATH above.
- **Existing launcher:** use the existing installation's update method, or
  remove that launcher through its original installer before changing methods.
- **Checksum or download failure:** retry the installer. Failed verification
  leaves the active installation unchanged.
- **macOS blocks a downloaded executable:** use the terminal installer.
  Release executables are not Apple Developer ID signed or notarized.
- **Missing conversations or hooks:** run `pika setup` to review integration,
  then `pika doctor` for evidence and recovery steps.
