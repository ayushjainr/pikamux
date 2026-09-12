# Frozen Python compatibility reference

This directory is an unmodified copy of `src/pikamux` from the public Python
Pika tag `v0.5.0a4`.

- Commit: `36de1b1eed1a182ed4e608d47f95d362f15d571a`
- Source tree: `7c6144590f23624cf83edf92706b6911bbf714ee`
- Copy command: `git archive v0.5.0a4 src/pikamux`

Compatibility tests copy this reference into a disposable directory before
importing it. They never import an installed Pika or access the user's state.
