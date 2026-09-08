# Installation and updates

The installer and updater are implemented and locally tested. Public download
URLs are **not live until the owner publishes the corresponding GitHub release**.
Do not present the examples below as an available public release before then.

## Fresh Mac or Linux machine

For a private release bundle supplied by the maintainer:

```bash
bash /path/to/pika-release/install.sh --bundle /path/to/pika-release
```

After publication, the latest stable public release can be installed with:

```bash
curl -fsSL https://github.com/ayushjainr/pikamux/releases/latest/download/install.sh | bash
```

For a published prerelease, use its exact tag; GitHub's latest-stable URL does not
select alpha releases. For example, only after publishing `v0.5.0a1`:

```bash
curl -fsSL https://github.com/ayushjainr/pikamux/releases/download/v0.5.0a1/install.sh | bash -s -- --version v0.5.0a1
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

## Updates

Installer-managed copies use:

```bash
pika update --check
pika update
```

Private/prerelease installations can use another supplied bundle:

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

## Other machines, including private releases

Once Pika is installed from a release bundle, it retains the verified wheel.
Approved remote installs/upgrades can transfer that artifact over SSH instead
of asking each machine for GitHub credentials:

```bash
pika machine upgrade devbox
```

`machines` remains an alias-compatible existing spelling. From a development
checkout or when explicitly choosing a matching bundle:

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

## Maintainer: prepare a private bundle

```bash
uv run python scripts/build_release.py --output dist/new-candidate
```

The output must not exist. It contains wheel, sdist, `install.sh`,
`pika-release.json`, and `SHA256SUMS`. This builds local files; it neither uploads
nor changes GitHub visibility. Follow the [release checklist](releasing.md)
before publishing any artifact or advertising a public install command.
