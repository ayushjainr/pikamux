# Windows publisher signing

Status: integration prepared; SignPath enrollment and a real signed release
are still required. Existing public releases are not retroactively signed.
GitHub provenance attestations do not replace Windows Authenticode trust.

## Publisher enrollment

Apply to [SignPath Foundation](https://signpath.org/apply) for the public
`ayushjainr/pikamux` repository. Approval and service terms are separate owner
decisions. No account was created or application submitted by this change.

After approval, configure a production signing policy, a timestamped Authenticode
certificate, and the GitHub integration for this repository. The uploaded input
is a ZIP containing one root `pika.exe`; the artifact configuration must sign
that file and preserve the same output shape. Restrict signing to reviewed
release tags and GitHub-hosted builds. Review the Foundation's required signing
policy, approvers, attribution and repository controls before enabling it.

Repository configuration:

| Kind | Name | Value |
| --- | --- | --- |
| Secret | `SIGNPATH_API_TOKEN` | Restricted signing submitter token |
| Variable | `SIGNPATH_ORGANIZATION_ID` | Assigned organization ID |
| Variable | `SIGNPATH_PROJECT_SLUG` | Approved project |
| Variable | `SIGNPATH_SIGNING_POLICY_SLUG` | Production policy, not test signing |
| Variable | `SIGNPATH_CERTIFICATE_THUMBPRINT` | Approved certificate's 40-hex thumbprint |
| Variable | `PIKA_WINDOWS_SIGNING` | `signpath`, set last |

An unset mode preserves legacy unsigned release builds with a CI warning.
Unknown modes, missing configuration, service failures, unsigned output, an
unexpected signer, missing timestamp or a version mismatch fail a signing-enabled
build. Certificate rotation requires reviewing and updating the thumbprint.
Never unset signing as an automatic outage fallback.

## Byte flow and activation

1. Build and check the Windows command boundary on its native CI runner.
2. Upload the unsigned input for SignPath origin verification.
3. Sign, download, verify trust/timestamp/expected signer, and check version again.
4. Copy those bytes to the final artifact; package and hash only afterwards.
5. On installation, verify the pinned archive and inspect the executable and
   signature without executing a signed candidate in staging.

Legacy unsigned archives retain their bounded startup check. Invalid signatures
are rejected, not treated as unsigned. Neither branch changes endpoint security,
requires elevation, pairs a host, or alters a running agent.

## Required completion evidence

- Native Windows PowerShell 5.1 and PowerShell 7 installer contracts pass.
- A real production-signed artifact has a valid chain, approved signer and
  timestamp; extracted bytes match the pre-packaging signed executable.
- Install/update preserve the old receipt and PATH on verification failure.
- On the affected endpoint, separately test final `pika --version` and opening
  the board. Record correlated Defender events if policy still blocks launch.

Mocked signature tests validate branching only, not Windows trust or Defender.
Managed endpoint policies can still reject third-party software; successful
installation is not a claim of universal enterprise-policy compatibility.

References: [SignPath GitHub integration](https://docs.signpath.io/trusted-build-systems/github),
[Microsoft ASR rule](https://learn.microsoft.com/en-us/defender-endpoint/attack-surface-reduction-rules-reference#use-advanced-protection-against-ransomware).
