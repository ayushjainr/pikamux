# Release format

Pika ships one native executable for each supported host target. Fresh installs
and updates use prebuilt binaries.

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

`pika update --rollback` revalidates retained receipts, manifests, archives,
checksums, executable bytes, and the managed launcher before atomically selecting
the newest older release. `pika update --rollback VERSION` selects one exact
retained version. Tampered or foreign release directories are never activated.

The updater downloads only constructed URLs beneath the Pika GitHub release
path. API-provided URLs and manifest-provided paths are ignored. Manifests,
release listings and artifacts have hard size bounds. Target, filename, declared
size, sidecar checksum and manifest checksum must all agree before extraction.
Offline bundle files must be regular, non-symlink entries within the same bounds;
bounded streaming copies them into an owned scratch directory only after those
checks, and partial copies are removed on every failure.
Every native archive contains exactly the executable, `LICENSE`, and
`THIRD_PARTY.md`; all are regular, bounded files. Publication verification
requires both notices to match the audited source exactly. Installation and
rollback bind retained notices byte-for-byte to their own checksum-verified
archive, so an older valid release survives later dependency-notice changes.
The hidden activation boundary re-extracts that verified archive and compares
the supplied and running executable bytes before any candidate probe or managed
write. SIGINT and SIGTERM cancel and reap the exact archive/candidate subprocess
group before returning the conventional 130 or 143 status.

## Release assets

A native release contains:

```text
install.sh
pika-version
pika-native-release.json
pikamux-V-aarch64-apple-darwin.tar.gz     # pika + LICENSE + THIRD_PARTY.md
pikamux-V-x86_64-apple-darwin.tar.gz
pikamux-V-aarch64-unknown-linux-musl.tar.gz
pikamux-V-x86_64-unknown-linux-musl.tar.gz
<artifact>.sha256
SHA256SUMS
LICENSE
THIRD_PARTY.md
```

GitHub supplies source archives for the tag. The Windows ZIP contains the
experimental client/bridge executable and the same license notices; it is not a
native tmux host.

The native manifest uses schema 2:

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
stable version.

`scripts/package-release.sh` refuses an existing output directory or duplicate
target mappings, verifies each runnable binary's version/help/embedded skill,
creates archives with fixed ordering, ownership, modes, and timestamps, and
writes checksums over the final bytes. Repacking identical inputs therefore
reproduces identical native archive bytes. The verifier independently rejects
duplicate JSON keys.
Cross-target assembly may set the internal
`PIKA_CROSS_PACKAGE=1` flag only after every matrix job has run those checks on
the matching native runner. `scripts/generate-third-party.sh` deterministically
rebuilds `THIRD_PARTY.md` from `Cargo.lock` and the checksummed Cargo crate
archives. The shipped file covers normal and build dependencies for every native
host target and the Windows client target, records package checksums and declared
authors, reproduces the source license/notice texts, and identifies the
public-domain SQLite amalgamation compiled by the `bundled` feature. CI rejects
stale generated output. The same bundle reproduces the pinned Rust toolchain's
complete standard-library copyright report because that runtime is statically
linked into Pika. For the self-contained musl libc linked by Linux targets, the
generator extracts the complete musl-related notice blocks from the pinned Rust
1.88.0 full toolchain copyright report; the text is never maintained by hand.

## Managed installation

The managed installation layout:

```text
~/.local/bin/pika -> ~/.local/share/pikamux/current/bin/pika
~/.local/share/pikamux/
  .pika-install-root
  .install.lock
  current -> releases/V-TARGET-SHA
  releases/V-TARGET-SHA/bin/pika
  releases/V-TARGET-SHA/LICENSE
  releases/V-TARGET-SHA/THIRD_PARTY.md
  releases/V-TARGET-SHA/.pika-install.json
  releases/V-TARGET-SHA/bundle/        # exact verified release for explicit fleet use
```

The root marker remains `pikamux-installer-v1`. Native receipts use schema 2 and
record the managed paths, version, target, artifact and checksum. Before any root
write, Pika checks the candidate command surface, installation owner, launcher,
current symlink, version ordering and package identity. It takes a nonblocking
installation lock, stages a complete immutable release, and atomically switches
`current`. Copied payloads and their directories are synced before the stage is
renamed; the releases directory and activation directory are synced around the
atomic symlink swap. A process interruption or power loss therefore recovers to
either the prior release or the fully persisted new release. Failure leaves the
prior launcher usable. Downgrades and changed bytes under an existing version are
refused. Old releases remain available for running callbacks and rollback.
Every existing component Pika trusts beneath the managed root, plus its public
launcher symlink, must be owned by the current effective user; foreign-owned
components are rejected before validation, activation, or rollback.
Managed directories and launcher parents must also be non-group/other-writable;
new directory components are created atomically with mode 0700. Pika rejects a
permissive existing root or release directory before writing to it.

Managed native activation and `pika update` do not open Pika's state database,
scan provider histories, modify hooks or skills, run setup, restart agents, or
update another machine. Unless `--no-setup` is selected, the public `install.sh`
bootstrap then installs the embedded agent-convo skill with backup and symlink
safeguards before offering the separately previewed `pika setup`; provider hooks
and configuration still change only through that approved setup flow.

## CI and publication

Main-branch CI checks the native implementation. Tagged release builds run on
matching platform runners, verify artifact contents and checksums, and publish
only after CI succeeds for the exact commit. Release assets carry build
provenance attestations. See [releasing](releasing.md).

## Deliberately separate

- Setup migration and long-lived callback rewrites remain previewed operations.
- Remote upgrades remain explicit, version-pinned and per machine.
- Automatic pruning, delta updates and OS package managers are deferred.
- Native Windows hosting, PowerShell installation and Windows self-update are a
  future client features.
