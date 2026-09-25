# Development quality checks

Three complementary checks protect recovery work. None adds a runtime dependency
or a background process to Pika.

## Recovery outcomes

Run `scripts/check-recovery.sh --toolchain 1.88.0` for the focused regression
inventory in [recovery scenarios](recovery-scenarios.md). It requires tmux and
`script`, uses fake providers, and checks that every selected test actually
exists before running it. The ordinary native CI suite includes these tests;
it need not rerun the focused runner afterward.

`scripts/with-test-home.sh COMMAND ...` supplies disposable HOME, XDG, Pika,
provider, database, temporary and tmux-socket paths. It drops inherited secrets
and terminal context, and blocks unconfigured provider/SSH commands. Fixtures
can substitute their own fake executables. It is an isolation harness, not an
OS sandbox: tests must still never use absolute live-machine paths or endpoints.

## Complexity ratchet

```sh
python3 -m venv .venv/quality
.venv/quality/bin/pip install --require-hashes -r scripts/quality-requirements.txt
.venv/quality/bin/python -m unittest discover -s tests/quality -p 'test_*.py'
.venv/quality/bin/python scripts/check-complexity.py
```

The pinned [Lizard](https://github.com/terryyin/lizard) analyzer measures Rust
function cyclomatic complexity (CCN) across `src`, including in-module tests.
New functions may have CCN up to 15. Existing hotspots have explicit ceilings in
`tests/quality/complexity-baseline.json`; any increase fails CI. Reductions or
removed hotspots require lowering the baseline so old allowances cannot be
reused. Same-named functions within a file are compared as descending multisets
because the analyzer does not retain Rust impl scopes; line movement is harmless.

The checker applies one bounded correction to Lizard 1.24.0's Rust tokenizer:
its lifetime-token alternative otherwise consumes the start of character
literals such as `'c'`, which can hide later functions from the measurement.
The correction distinguishes a lifetime from a character literal without
rewriting Rust source and is covered by fixtures for ordinary and escaped
characters followed by lifetime-bearing functions. The analyzer identity in the
baseline includes this correction; adopting it therefore requires a reviewed
baseline identity/hotspot refresh, never a silent baseline rewrite.

This is the initial reviewed baseline, not an increase to an established CI
allowance. Comparing the same source with and without the correction exposed
311 previously missed functions (1,996 to 2,307) and 30 additional hotspot
instances (110 to 140). Those differences reflect analyzer visibility, not an
improvement or regression in the source. Existing high-complexity functions are
explicitly grandfathered; subsequent changes must respect or reduce their
ceilings, and new functions have the 15-CCN limit.

After a deliberate reduction, run `scripts/check-complexity.py --write-baseline`
using the same environment and review the diff. Do **not** regenerate it to
silence a regression. An exceptional ceiling increase needs an explicit rationale
in code review. CI never updates the baseline. Analyzer upgrades require a
separately reviewed baseline change.

CCN is a structural warning, not a correctness score or the number of tests to
write. Lizard uses a source-token heuristic, not the compiler's control-flow
graph; individual Rust match arms, macro expansion, type semantics and runtime
interleavings are not exhaustively represented. Split functions only when doing so clarifies ownership
or decisions—not to move complexity out of view.

## Recovery branch reports

```sh
rustup toolchain install nightly-2025-06-27 --profile minimal --component llvm-tools-preview
cargo +1.88.0 install cargo-llvm-cov --locked --version 0.6.18
scripts/check-coverage.sh
```

The pinned coverage-only nightly enables real branch instrumentation through
[cargo-llvm-cov](https://github.com/taiki-e/cargo-llvm-cov/tree/v0.6.18).
Release builds and normal checks stay on Rust 1.88. The instrumented run covers
library tests plus the recovery contract targets. One run produces JSON, an
HTML report and a focused summary for core, store, hooks, process, tmux and CLI
under `target/recovery-coverage/`. CI publishes them as an artifact and job summary.
Open `target/recovery-coverage/html/index.html` to inspect uncovered decisions.

The build/profile directory is separate from normal Cargo output. Report export
reuses the run's profiles; it does not run the tests again. Test failures remain
failures. Each measurement clears old profiles and invalidates prior report entry
points before tests start. Missing source metrics or zero branch instrumentation fail reporting
rather than appearing as 100% coverage. There is no blanket coverage target:
review gaps in changed recovery decisions, and add an outcome test where needed.
Compiler branches do not enumerate every ordering of asynchronous events.

Coverage is currently collected on Linux CI. It does not prove Windows or macOS
behavior, every provider version, or real fleet reliability; platform contracts
and explicit live validation remain separate.
