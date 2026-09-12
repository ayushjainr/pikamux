# Python reference provenance

- Product: Pika / pikamux.
- Version: `0.5.0a4`.
- Source commit: `36de1b1eed1a182ed4e608d47f95d362f15d571a`.
- Source repository: `https://github.com/ayushjainr/pikamux`.
- Exported from a clean local checkout of the source commit above.
- Export directory: `reference/python/` (ignored).
- Export date: 2026-09-11.
- Method: `git archive` of the exact commit, extracted into this workspace.

This includes committed source and fixtures only, not the source repository's
working changes, Git metadata, local state, credentials or running processes.
The export includes the upstream license. No historical commits were rewritten.

Treat the export as read-only. Refresh only through a recorded baseline-change
decision, including the old/new commit, changed contracts and affected tests.
Tests must run on disposable copies with isolated environment and fake external
commands unless an authorized checkpoint explicitly permits a live integration.

The maintained Python repository can advance independently. Before phase acceptance
and release, compare its approved fixes against this baseline and port the relevant
regression contracts. Passing an old snapshot alone is not current product parity.
