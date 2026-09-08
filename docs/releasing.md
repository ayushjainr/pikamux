# Release checklist

This is a local preparation checklist, not authorization to push, create a tag,
upload a package, or change GitHub visibility. Those are separate owner actions.

## Verify one candidate

1. Review the exact diff, tracked and untracked files, and reachable Git history.
   Confirm attribution, upstream URLs, dependency notices, and publication
   permissions. A gitignore does not remove already tracked or historical files.
   Keep machine-local audit notes, real consultation results, and backups out of
   Git and both distribution formats. Pattern scans do not prove absence of secrets.
2. Run `uv sync --locked --group dev`, then
   `uv run pytest -q --timeout=90`. Use clean Linux and macOS environments with tmux as
   well as the maintainer's host. CI should run without provider credentials.
3. Run `uv run python scripts/build_release.py --output dist/candidate` into a nonexistent candidate directory.
   Keep older distributions separate. Inspect wheel and sdist contents: the skill and MIT license
   must be present; databases, transcripts, credentials, backup files, and local
   audit notes must be absent. Build a wheel from the unpacked sdist too.
4. Install the wheel into a fresh environment and check `pika --help`,
   `pika --version`, `pika skill show`, and skill installation into a temporary
   directory. Run the synthetic monitor demo. Do not run `pika setup` against a
   real home during package verification.
5. Run `PIKA_INSTALL_BUNDLE=dist/candidate PIKA_BOOTSTRAP_BUNDLE=dist/candidate uv run pytest -q tests/test_installation.py tests/test_bootstrap.py --timeout=300`.
   These opt-in tests use temporary homes and real package/runtime downloads,
   exercise the fresh-host bootstrap and a synthetic next-version update, and
   must not access provider credentials or transcripts. Synthetic versions are
   test fixtures, never release candidates.

Performance fixtures and `tests/live_consult_probe.py` are opt-in. The latter
uses real provider quota and reads parent histories to verify unchanged bytes;
run it only with explicit authorization and keep its outputs private. Never
make it a CI requirement. Published performance/quality claims must name the
fixture, sample size, environment, limits, and evidence—not an agent review score.

## Before publication

- The prepared candidate is `0.5.0a1`. Keep `src/pikamux/__init__.py`,
  `pyproject.toml`, `uv.lock`, and the changelog consistent. Never reuse an old tag or overwrite
  an existing distribution to represent different code.
- Verify issue links and the first hosted CI run after the owner authorizes a
  private push. Do not show a passing CI badge before that run exists.
- Public visibility is a separate owner decision. At the approved public
  cutover, enable GitHub private vulnerability reporting and verify the reporting
  link and notifications; GitHub does not offer it for private repositories.
- Create the matching version tag only after candidate checks pass. Upload the
  exact bundle assets (including installer, manifest, checksums, wheel and sdist)
  without rebuilding different bytes under the same version. Verify anonymous
  HTTPS installation from that tag after public cutover. Until then, use local
  bundles or approved SSH bundle transfer. Do not claim anonymous installation
  works before verifying it; GitHub's latest-stable endpoint excludes alpha tags.
- Validate provider versions on an authorized disposable workspace and record
  results. Unit tests, Linux tmux tests, and a Windows client smoke test do not
  establish universal native TUI fidelity or live-provider compatibility.

## Simplification boundaries

There is one development command (`uv run pytest`) and one packaging command
(`uv build --out-dir dist/candidate`). CI verifies; it does not deploy or publish. The public README is a
short entry point; detailed operating contracts live in the guide. Local audit
history is retained privately rather than repackaged as user documentation.

Provider-native subprocesses, platform dispatch, immutable identity leases,
and fleet caching remain because they enforce current user contracts. This pass
does not add a release daemon, generic plugin framework, new command family,
telemetry service, or a second state store.
