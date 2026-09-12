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

`pika update --rollback` revalidates retained receipts, manifests, archives,
checksums, executable bytes, and the managed launcher before atomically selecting
the newest older release. `pika update --rollback VERSION` selects one exact
retained version. Tampered or foreign release directories are never activated.

The updater downloads only constructed URLs beneath the Pika GitHub release
path. API-provided URLs and manifest-provided paths are ignored. Manifests,
release listings and artifacts have hard size bounds. Target, filename, declared
size, sidecar checksum and manifest checksum must all agree before extraction.
The POSIX archive must contain exactly one regular file named `pika`.

## One-time Python bridge

The frozen Python v0.5.0a4 updater understands only schema 1 and a PEP 440 wheel
version. It cannot discover the current SemVer-style alpha or parse a schema-2
native artifact. Existing managed users therefore need one final Python bridge
release:

1. Unmodified v0.5.0a4 discovers and installs a schema-1 bridge wheel.
2. Its required `--version`, `--help`, and `skill show` validation probes are
   side-effect-free.
3. On the first ordinary Pika command, the bridge selects one exact embedded
   Mac/Linux target, activates it in the same managed root, and executes the
   original command natively. No second approval or manual migration is needed.

The compatibility test executes the unmodified v0.5.0a4 updater itself against
the built wheel in a disposable managed environment, then proves first-use
native activation. The wheel embeds all four supported Mac/Linux archives and
fails closed on an unsupported host; it never labels one platform binary as
universal. It remains a transition envelope, not a second implementation.

## Release assets

A native release contains:

```text
install.sh
pika-version
pika-native-release.json
pika-release.json                         # schema 1, stable/bridge releases only
pikamux-V-py3-none-any.whl                # stable/bridge releases only
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

Native schema 2 uses the distinct `pika-native-release.json` name so a stable
tag may also carry the schema-1 manifest that the frozen updater requires.
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

`scripts/package-release.sh` refuses an existing output directory or duplicate
target mappings, verifies each runnable binary's version/help/embedded skill,
creates fresh archives, and writes checksums over the final bytes. The verifier
independently rejects duplicate JSON keys.
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
`current`. Copied payloads and their directories are synced before the stage is
renamed; the releases directory and activation directory are synced around the
atomic symlink swap. A process interruption or power loss therefore recovers to
either the prior release or the fully persisted new release. Failure leaves the
prior launcher usable. Downgrades and changed bytes under an existing version are
refused. Old releases remain available for running callbacks and rollback.

Managed native activation and `pika update` do not open Pika's state database,
scan provider histories, modify hooks or skills, run setup, restart agents, or
update another machine. Unless `--no-setup` is selected, the public `install.sh`
bootstrap then installs the embedded agent-convo skill with backup and symlink
safeguards before offering the separately previewed `pika setup`; provider hooks
and configuration still change only through that approved setup flow.

## CI and publication

`.github/workflows/ci.yml` runs formatting, warnings-as-errors Clippy, the full
test suite, a locked release build and diagnostic startup measurements on macOS
arm64 and Linux x86_64.

`.github/workflows/release.yml` builds macOS and musl Linux binaries on matching
arm64/x86_64 runners, verifies each natively, assembles one manifest and checksum
set, then pauses at the protected `release` environment. Publication uploads the
exact assembled artifact only after that environment's owner approval. Every
third-party workflow action is pinned to an immutable commit. GitHub also issues
build-provenance attestations for the release payload before publication.
Configure the GitHub `release` environment with required reviewers before
creating a tag.

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
