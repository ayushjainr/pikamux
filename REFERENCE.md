# Python reference provenance

- Product: Pika / pikamux.
- Version: `0.5.0a4`.
- Source commit: `36de1b1eed1a182ed4e608d47f95d362f15d571a`.
- Source repository: `https://github.com/ayushjainr/pikamux`.
- Exported from a clean local checkout of the source commit above.
- Runtime contract fixture: `tests/fixtures/python-v0.5.0a4/pikamux/` (tracked).
- Audit export: `reference/python/` (optional and ignored).
- Export date: 2026-09-11.
- Method: `git archive` of the exact commit, extracted into this workspace.

The tracked runtime fixture was copied byte-for-byte from the clean export and
contains the package files needed by clean-checkout compatibility tests. The
audit export includes the wider committed source and upstream license; neither
copy contains Git metadata, local state, credentials or running processes. No
historical commits were rewritten.

Treat the export as read-only. Refresh only through a recorded baseline-change
decision, including the old/new commit, changed contracts and affected tests.
Tests must run on disposable copies with isolated environment and fake external
commands unless an authorized checkpoint explicitly permits a live integration.

The maintained Python repository can advance independently. Before phase acceptance
and release, compare its approved fixes against this baseline and port the relevant
regression contracts. Passing an old snapshot alone is not current product parity.
