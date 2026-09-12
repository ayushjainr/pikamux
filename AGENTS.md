# Native migration workspace instructions

Read `PRD.md` and `REFERENCE.md` before migration work. This directory is a separate
local repository, not a worktree of the shipping Python project.

## Boundaries

- Work only on approved migration checkpoints. Workspace creation is not approval
  to implement Phase 0 or later phases, install software, publish or cut over.
- Do not edit the sibling Python repository, its Git configuration, or its dirty files.
- Keep `reference/python/` unchanged; make disposable copies for test execution.
- Do not change the installed `pika`, global PATH, provider configuration, hooks,
  expert schedules, real databases, live tmux sessions or trusted fleet nodes.
- No GitHub remote, push, release or remote upgrade without explicit authorization.
- Preserve the repository's configured commit attribution.

## Runtime isolation is mandatory

A separate directory does not isolate processes or user configuration. Before any
executable test, provide disposable HOME/XDG/Pika/provider state, an explicit test
database, a dedicated tmux socket if needed, and a PATH/command adapter using test
doubles for providers and SSH. Configure these in the child-process environment;
do not repurpose shell `$HOME` as a scratch-directory variable.

Prevent accidental model calls, network access, user-process cleanup and reads of
real provider transcripts. Fake-provider tests are the default. A live integration
needs explicit authorization for its targets and quota, with bounded cleanup of
only resources created by that test. Never run baseline tests assuming that a
copied source tree makes their effects harmless.

## Execution

- Port user behaviour, not Python files. Preserve exact identity and safety rules.
- Follow plan-hash, fixture, independent-review and approval gates in the PRD.
- Record measured results; do not present proposed performance targets as achieved.
- Review baseline drift at each phase exit; do not overwrite fixtures to hide bugs.
- Keep one native implementation. Do not introduce Python subprocess fallbacks or
  FFI merely to make a partial port appear complete.
- Production integration is a separate approved, reversible change after evidence
  of compatibility. Keep the normal user command `pika` unchanged until then.
