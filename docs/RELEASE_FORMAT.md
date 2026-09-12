# Native release and transition contract

Pika ships one native executable for each supported host target. Fresh installs
and native updates need neither Python nor a Rust toolchain. This document is the
maintainer contract; it does not claim that an unpublished build is available.

## User path

The public installer is versionless. It reads the one-line `pika-version` asset
from GitHub's latest stable release, then pins every remaining download to that
immutable `vVERSION` tag. An exact `--version` remains available for diagnostics
and rollback workflows, but is not part of the normal install command.

`pika update` first proves that its running executable is the active member of a
Pika-managed installation. With no flags it selects the newest complete release
compatible with the current channel. `--check` downloads metadata only;
`--release VERSION` pins a release; `--bundle DIRECTORY` stays offline. A stable
installation never selects a prerelease. A preview installation may select a
newer preview or stable release.

The updater downloads only constructed URLs beneath the Pika GitHub release
path. API-provided URLs and manifest-provided paths are ignored. Manifests,
release listings and artifacts have hard size bounds. Target, filename, declared
size, sidecar checksum and manifest checksum must all agree before extraction.
The POSIX archive must contain exactly one regular file named `pika`.

## One-time Python bridge

The frozen Python v0.5.0a4 updater understands only schema 1 and a universal
wheel. It cannot safely activate a schema-2 native artifact. Existing managed
users therefore need one final Python bridge release:

1. Unmodified v0.5.0a4 discovers and installs a schema-1 bridge wheel.
2. That bridge understands schema 2 and stages the exact native target.
3. A subsequent explicit update activates native Pika in the same managed root.

The compatibility test executes the unmodified v0.5.0a4 manifest reader against
the bridge fixture. A separate test starts from a valid schema-1 installation
receipt and activates a schema-2 native release without moving Pika state. The
bridge artifact itself must be built and tested before publication; it is not a
second long-lived implementation.

## Release assets

A native release contains:

```text
install.sh
pika-version
pika-release.json
pikamux-V-aarch64-apple-darwin.tar.gz
pikamux-V-x86_64-apple-darwin.tar.gz
pikamux-V-aarch64-unknown-linux-musl.tar.gz
pikamux-V-x86_64-unknown-linux-musl.tar.gz
<artifact>.sha256
SHA256SUMS
LICENSE
THIRD_PARTY.md
```

GitHub supplies source archives for the tag. The Windows ZIP contains only the
experimental client/bridge executable; it is not a native tmux host.

Schema 2 is strict:

```json
{
  "schema": 2,
  "package": "pikamux",
  "version": "0.6.0",
  "channel": "stable",
  "artifacts": {
    "aarch64-apple-darwin": {
      "file": "pikamux-0.6.0-aarch64-apple-darwin.tar.gz",
      "sha256": "<64 lowercase hexadecimal characters>",
      "bytes": 1234567
    }
  }
}
```

Targets and filenames are allowlisted. A manifest cannot supply a URL or local
path. `preview` accepts prerelease or stable versions; `stable` contains only a
stable version. Historical `0.5.0a5` and Cargo-style `0.6.0-alpha.1` spellings
are accepted only for the transition.

`scripts/package-release.sh` refuses an existing output directory, verifies
each runnable binary's version/help/embedded skill, creates fresh archives, and
writes checksums over the final bytes. Cross-target assembly may set the internal
`PIKA_CROSS_PACKAGE=1` flag only after every matrix job has run those checks on
the matching native runner.

## Managed installation

The native layout stays compatible with the Python installer:

```text
~/.local/bin/pika -> ~/.local/share/pikamux/current/bin/pika
~/.local/share/pikamux/
  .pika-install-root
  .install.lock
  current -> releases/V-TARGET-SHA
  releases/V-TARGET-SHA/bin/pika
  releases/V-TARGET-SHA/.pika-install.json
  releases/V-TARGET-SHA/bundle/        # exact verified release for explicit fleet use
```

The root marker remains `pikamux-installer-v1`. Native receipts use schema 2 and
record the managed paths, version, target, artifact and checksum. Before any root
write, Pika checks the candidate command surface, installation owner, launcher,
current symlink, version ordering and package identity. It takes a nonblocking
installation lock, stages a complete immutable release, and atomically switches
`current`. Failure leaves the prior launcher usable. Downgrades and changed bytes
under an existing version are refused. Old releases remain available for running
callbacks and rollback.

Installation and update do not open Pika's state database, scan provider
histories, modify hooks or skills, run setup, restart agents, or update another
machine. `pika setup` separately previews integration changes.

## CI and publication

`.github/workflows/ci.yml` runs formatting, warnings-as-errors Clippy, the full
test suite, a locked release build and diagnostic startup measurements on macOS
arm64 and Linux x86_64.

`.github/workflows/release.yml` builds macOS and musl Linux binaries on matching
arm64/x86_64 runners, verifies each natively, assembles one manifest and checksum
set, then pauses at the protected `release` environment. Publication uploads the
exact assembled artifact only after that environment's owner approval. Configure
the GitHub `release` environment with required reviewers before creating a tag.

Before approval, inspect `SHA256SUMS`, CI results, artifact sizes, benchmark
evidence and platform smoke results. Checksums detect transfer mismatch; they are
not an independent trust anchor if the release account is compromised. macOS
signing/notarization and clean-host runtime tests remain publication gates, not
claims made by cross-compilation.

## Deliberately separate

- Setup migration and long-lived callback rewrites remain previewed operations.
- Remote upgrades remain explicit, version-pinned and per machine.
- Automatic pruning, delta updates and OS package managers are deferred.
- Native Windows hosting, PowerShell installation and Windows self-update are a
  later client checkpoint.
