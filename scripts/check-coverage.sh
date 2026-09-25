#!/usr/bin/env bash
# A separate pinned nightly collects actual branches; releases stay on Rust 1.88.
set -euo pipefail
repo_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repo_dir"
toolchain=nightly-2025-06-27
[[ $(cargo llvm-cov --version) == 'cargo-llvm-cov 0.6.18' ]] || {
  echo 'Install cargo-llvm-cov --locked --version 0.6.18' >&2; exit 2;
}
tmux -V >/dev/null || { echo 'Coverage journeys require tmux' >&2; exit 2; }
command -v script >/dev/null || { echo 'Coverage journeys require script' >&2; exit 2; }
# Dedicated profile directory; never clean normal developer/release builds.
export CARGO_LLVM_COV_TARGET_DIR="$repo_dir/target/recovery-coverage-build"
report_dir="$repo_dir/target/recovery-coverage"
mkdir -p "$report_dir"
# Retire only generated entry points, so a failed new run cannot look successful
# when someone opens an earlier summary or index. Raw profiles are cleared by
# cargo-llvm-cov's default clean_partial (deliberately no --no-clean).
rm -f -- "$report_dir/summary.md" "$report_dir/coverage.json" "$report_dir/html/index.html"
wrapper="$repo_dir/scripts/with-test-home.sh"
"$wrapper" cargo "+$toolchain" llvm-cov --branch --locked --no-report \
  --lib --test hooks_contract --test store_contract --test board_journey_contract \
  --test real_tmux_contract --test identity_safety_contract --test terminal_signal_contract \
  -- --test-threads=1
# Reuse the same run's profiles for both reports. Do not mask failed tests.
"$wrapper" cargo "+$toolchain" llvm-cov report --json --output-path "$report_dir/coverage.json"
"$wrapper" cargo "+$toolchain" llvm-cov report --html --output-dir "$report_dir"
python3 scripts/coverage-summary.py "$report_dir/coverage.json" > "$report_dir/summary.md"
cat "$report_dir/summary.md"
