# Releasing Pika

Keep `Cargo.toml`, `Cargo.lock`, the changelog, and
`docs/release-notes.md` consistent. Publish a new version; never replace
the bytes of an existing release.

1. Review the diff and run the affected native checks. Main-branch CI verifies
   formatting, Clippy, native contracts, dependency notices, and the security audit.
2. Push the reviewed commit to main and create its matching `vVERSION` tag.
3. The release workflow builds macOS and Linux on ARM64 and x86-64, plus the
   Windows client. Each runner checks its executable before packaging.
4. Publication requires passing CI for the exact commit. The workflow verifies
   archive contents and checksums, attests provenance, and publishes the assets.
5. Verify the public install command resolves to the new release and opens the
   expected native executable.

The installer and updater use the published native manifest. A release contains
no runtime environment or package-manager bootstrap.
See [release format](RELEASE_FORMAT.md) for validation and activation details.

Apple Developer ID signing and notarization are not currently configured.
The terminal installer verifies release checksums; provenance attestations are
published alongside the assets.
