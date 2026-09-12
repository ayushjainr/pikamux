# Contributor instructions

Read `DESIGN.md` and `docs/RELEASE_FORMAT.md` before changing behavior. Preserve Pika's exact identity,
state-integrity, and fail-closed safety contracts; port user outcomes, not an
earlier implementation's module structure.

## Safe development

- Run executable tests with disposable HOME, XDG, Pika, provider, database,
  temporary, and tmux-socket paths.
- Use fake providers and SSH endpoints by default. Tests must not spend model
  quota, inspect real transcripts, contact fleet machines, or alter a user's
  installed Pika, hooks, configuration, database, PATH, or tmux sessions.
- Keep the runtime native. Provider-owned JavaScript is allowed only where
  that provider requires it.

## Validation

- Run formatting, warnings-as-errors Clippy, native tests, third-party-notice
  verification, and the security audit for a release. Reuse passing evidence
  until a relevant change invalidates it; do not repeat endurance measurements.
- Record measured results rather than assuming performance from the language.
- Treat publication, live migration, installed-command cutover, and retirement
  of rollback runtimes as separate release-owner decisions.
