# Release checklist

Use this checklist to build, verify, and publish a release. CI runs verification;
release publication is a separate maintainer step.

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
fixture, sample size, environment, limits, and supporting evidence.

## Before publication

- Keep `src/pikamux/__init__.py`,
  `pyproject.toml`, `uv.lock`, and the changelog consistent. Never reuse an old tag or overwrite
  an existing distribution to represent different code.
- Verify that CI passes for the exact release commit and that documentation
  and issue-reporting links resolve.
- Verify private vulnerability reporting and the link in `SECURITY.md`.
- Create the matching version tag only after candidate checks pass. Upload the
  exact bundle assets (including installer, manifest, checksums, wheel and sdist)
  without rebuilding different bytes under the same version. Verify anonymous
  HTTPS installation from that tag and through the README install command.
  Use a clean, isolated home without provider credentials. The opt-in public
  bootstrap test takes the expected version in `PIKA_BOOTSTRAP_PUBLIC_VERSION`;
  it passes no version argument to the installer.
- Validate provider versions on an authorized disposable workspace and record
  results. Unit tests, Linux tmux tests, and a Windows client smoke test do not
  establish universal native TUI fidelity or live-provider compatibility.
