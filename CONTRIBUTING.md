# Contributing to Pika

Start with [README](README.md). The full command reference is in
[the operating guide](docs/guide.md); [DESIGN](DESIGN.md) explains identity and
trust boundaries. Bugs with a small reproduction are as useful as pull requests.

## One development setup

Use Linux or macOS, Python 3.10 or newer, tmux, uv, and Node.js 22 for the OpenCode
plugin execution fixture. No provider login, SSH account,
API key, or paid model is required for the automated suite.

```bash
uv sync --locked --group dev
uv run pytest -q --timeout=90
uv run pika --help
```

Tests generate synthetic provider histories in temporary directories. Real tmux
tests use their own socket and fake provider processes; they skip when tmux is
absent. Do not run real-provider setup or consultations as an ordinary test step.
Use pytest, not unittest discovery: part of the suite uses pytest functions.

Build locally with `uv build --out-dir dist/candidate`. Keep older distributions
out of the candidate directory. Never infer installed behavior from an editable
checkout alone: verify the wheel and sdist as described in
[the release checklist](docs/releasing.md).

## Make a focused change

- Explain the user-visible problem, a reproduction, and what would disprove
  your fix. Include the provider and CLI versions, OS, and Pika version.
- Add tests for the changed behavior and its meaningful failure cases. Keep
  examples synthetic and model calls out of tests. Never include credentials,
  personal histories, raw doctor JSON, or internal project paths in an issue.
- Preserve exact native identity, unread state, and honest cleanup receipts.
  Unknown delivery must not trigger a blind retry. Do not disable a safety check
  merely to make an integration work.

Keep changes small. Pika integrates existing harnesses; it does not replace
them. Multi-machine routing and native provider subprocesses have real users
and remain supported boundaries, not speculative abstractions. Runtime
dependencies, new daemons, new commands, and provider fallbacks need a concrete
consumer and a simpler-alternative analysis.

Discuss large changes before implementation. Describe your decisions and
verification, and take responsibility for the submitted code. Contributions are
under the project's MIT license;
retain applicable third-party notices and only contribute material you may share.

## Report safely

Use a GitHub issue for ordinary bugs, with sensitive details removed. Follow
[SECURITY.md](SECURITY.md) for suspected vulnerabilities. Maintainer review and
response are best-effort; there is no support SLA.
