# Installation and updates

Pika is distributed through GitHub Releases.

Installing Pika also offers the bundled agent workflow: your current agent can
discover expert cards and privately consult a relevant earlier conversation.
After installation, follow [one useful consultation](first-consultation.md),
not a fleet-wide setup. Setup itself does not interview agents or create missing
cards. Note the [a3 Codex compatibility gap](first-consultation.md#release-compatibility)
before using a newer paginated Codex conversation as your first expert.

## Fresh Mac or Linux machine

Install:

```bash
curl -fsSL https://raw.githubusercontent.com/ayushjainr/pikamux/main/install.sh | bash
```

For a private release bundle supplied by the maintainer:

```bash
bash /path/to/pika-release/install.sh --bundle /path/to/pika-release
```

Read the script before running it if you prefer. Piping a downloaded script to
Bash executes code from that publisher; the HTTPS account/release is the trust
boundary. Package checksums detect corruption/substitution relative to the
manifest, not compromise of the account publishing both files. uv binary hashes
are pinned in the inspectable script. Python is downloaded using uv's managed
Python support; platform dependencies come from PyPI. This is not an offline or
fully vendored distribution.

Supported installer platforms: macOS/Linux arm64 and x86_64. Native Windows
hosting is not supported; its existing desktop bridge remains experimental.

The installer needs the OS's curl, tar, and SHA256 utility, but no preinstalled
Python, uv, or git. It obtains a verified uv binary and a managed Python 3.13,
installs Pika into a dedicated environment, verifies command startup, then makes
the `pika` launcher point at that release. It does not edit system Python or use
sudo. If tmux is missing, it supplies package-manager instructions. Provider
CLIs and authentication remain yours; installation does not log in to them.

Default paths:

- Launcher: `~/.local/bin/pika`.
- Managed versions/runtime helper: `~/.local/share/pikamux`.
- Python: uv's persistent user-managed Python directory.
- Existing Pika state/settings: unchanged.

If the command directory is absent from PATH, the installer offers a backed-up
change to zsh/Bash startup files. It never sources those files or overwrites a
symlinked dotfile. Open a new terminal after accepting, or use the printed full
command path/export in the current terminal: a child installer cannot change its
parent shell's environment. Shell edits and setup are opt-in; `--no-setup` skips
both. `--root` and `--bin-dir` support isolated/manual installations.

An existing unrelated `pika` launcher (including an editable development install)
is never silently replaced. Keep that installation method, or explicitly remove
it through its original installer before switching. Re-running the same release
is idempotent; reusing a version number for different package bytes is refused.

### Skill setup

Since version 0.5.0a2, Pika includes `agent-convo` in the same setup preview and
approval as provider hooks. Accepting setup installs it for each installed
provider: Codex's `skills/agent-convo` under its configured home, Claude's
`skills/agent-convo` under its configured home, and OpenCode's
`skills/agent-convo` under its configured configuration directory. Existing
instructions are diffed and backed up; supporting files are preserved. A
symlink-managed skill is left to its existing manager with an explicit notice.
Missing providers do not receive skill directories. No model call is made.

In the older **0.5.0a1**, skill installation was a separate command. Its immutable
release assets have not been changed. `--no-setup` skips skill installation
as well as other configuration changes. Updating the package alone still does
not overwrite installed instructions; rerun setup to review newer bundled skills.

## Updates

Version 0.5.0a3 adds background update notices to installer-managed
boards. Checks run after the first frame, read only public GitHub release
metadata, and cache results for six hours (one hour after a failed check).
Concurrent boards share the cache and lock. Offline checks never become agent
errors or delay navigation. No prompts, transcripts, or machine inventory are
sent; GitHub receives the normal release request and source IP. No model calls
are made. Set `PIKA_UPDATE_CHECK=0` to disable automatic checks.

An available version appears in the footer; the rotating usage tips remain.
Press `U` to review the exact release, then Enter or `y` to install it. Esc or
`n` cancels before installation. Once approved, installation continues in the
background even if you return to or leave the board. Success asks you to reopen
`pika`; the old board and live agents are not restarted. This affects only the
local managed installation, not remote machines or editable development copies.

Installer-managed copies can also check and update
explicitly with:

```bash
pika update --check
pika update
```

Alpha/beta/RC copies include newer prereleases and stable releases; stable copies
exclude prereleases. Selection uses numeric versions, not publication order, from
the newest 100 public GitHub release records with package assets. Once a stable
version is installed, future automatic checks stay on stable releases. Use
`pika update --release 0.5.0a3` to pin a specific release explicitly (downgrades
are refused). Each download uses the selected tag, not a moving `latest` URL.

For 0.5.0a2 and earlier, rerun the newer release's tagged installer
to upgrade. Alternatively download its assets into one directory and update
from that bundle. Private/offline-metadata installations use the same flow:

```bash
pika update --check --bundle /path/to/new-release
pika update --bundle /path/to/new-release
```

The new environment is prepared separately, with a lock preventing overlapping
installers. Package checksum, version, CLI help, and bundled skill startup are
checked before activation. Failures before activation leave the previous version
selected. Old environments are retained for processes still using them; there is
no automatic pruning or downgrade command.

Updates do not open or migrate Pika's database, restart agents, edit provider
settings, rerun onboarding, or overwrite installed skills. Reopen the board when
convenient. If release notes require hook/skill configuration changes, `pika setup`
previews them; `pika skill install` backs up the installed skill. Database changes
on subsequently opening a new release are a separate compatibility concern and
must be tested per release. Development checkouts are not self-updated.

## Source preview

For contributors testing an existing source checkout, use the
[development setup](../CONTRIBUTING.md#one-development-setup):

```bash
uv sync --locked --group dev
uv run pika --help
```

For the newer paginated Codex history fix, the checkout must include commit
`6b8af0f`. Verify with `git merge-base --is-ancestor 6b8af0f HEAD`;
exit status 0 means it is included.

For live testing, `uv run pika setup` previews configuration from this
checkout. The consulting agent must also resolve this checkout's Pika, not an
older installed launcher. In a dedicated test shell inside the checkout, activate
its environment with `source .venv/bin/activate`, verify `command -v pika`, then
start the caller's provider CLI from that shell. Recheck command resolution from
the caller before a consultation; desktop-launched agents may use a different PATH.
Use `deactivate` when finished. Do not overwrite an installer-managed launcher or
assume `pika --version` alone proves a source-only fix is present.

Source preview is not a release upgrade. It can share your existing Pika state;
setup is a real, previewed configuration change and consultations use real
provider quota. Run the automated development tests before live trials.

## Other machines, including private releases

Once Pika is installed from a release bundle, it retains the verified wheel.
Approved remote installs/upgrades can transfer that artifact over SSH instead
of asking each machine for GitHub credentials:

```bash
pika machine upgrade devbox
```

From a development checkout or when choosing a matching bundle:

```bash
pika machine upgrade devbox --bundle /path/to/pika-release
pika setup --machine devbox --install-bundle /path/to/pika-release
```

Only the wheel, manifest, and installer embedded in that verified wheel cross
SSH—not transcripts, expert cards, or credentials. Runtime/dependency downloads
may still be needed by the target. Remote commands can find the default
`~/.local/bin/pika` without editing shell profiles. Existing installations owned
by another installer are refused, not forcibly replaced. The remote installer
does not run setup; host identity is checked again before reporting the machine
added/upgraded. Upgrades also verify the stored node identity before installation,
so a repointed SSH alias does not silently authorize changing a different node.
Confirmation is required before remote installation, except an
explicit `machine upgrade --yes`. No machines are updated automatically.

## Build a release bundle

```bash
uv run python scripts/build_release.py --output dist/new-candidate
```

The output must not exist. It contains wheel, sdist, `install.sh`,
`pika-release.json`, and `SHA256SUMS`. This builds local files; it neither uploads
nor changes GitHub visibility. Follow the [release checklist](releasing.md)
before publishing.
