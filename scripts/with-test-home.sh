#!/usr/bin/env bash
# Run tests against disposable state, never the developer's Pika/provider homes.
set -euo pipefail
[[ $# -gt 0 ]] || { echo 'usage: with-test-home.sh COMMAND [ARGS...]' >&2; exit 2; }
repo_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
test_dir=$(mktemp -d /tmp/pq.XXXXXX)
cleanup() {
  # Only the exact directory created above; never a caller-supplied path.
  case "$test_dir" in /tmp/pq.??????) rm -rf -- "$test_dir" ;; esac
}
trap cleanup EXIT
mkdir -p "$test_dir"/{home,config,cache,state,data,tmp,tmux,bin,codex,claude,oc}
for command in codex claude opencode muse muse-code ssh tailscale curl; do
  ln -s "$repo_dir/scripts/test-command-denied.sh" "$test_dir/bin/$command"
done
test_env=(
  "PATH=$test_dir/bin:$PATH"
  "CARGO_HOME=${CARGO_HOME:-$HOME/.cargo}"
  "RUSTUP_HOME=${RUSTUP_HOME:-$HOME/.rustup}"
  "HOME=$test_dir/home" "XDG_CONFIG_HOME=$test_dir/config"
  "XDG_CACHE_HOME=$test_dir/cache" "XDG_STATE_HOME=$test_dir/state"
  "XDG_DATA_HOME=$test_dir/data" "TMPDIR=$test_dir/tmp"
  "TMUX_TMPDIR=$test_dir/tmux" "PIKA_TMUX_SOCKET=quality-tests"
  "PIKA_CONFIG_HOME=$test_dir/config/pika" "PIKA_STATE_HOME=$test_dir/state/pika"
  "PIKA_DB_PATH=$test_dir/state/pika.db" "PIKA_UPDATE_CHECK=0"
  "CODEX_HOME=$test_dir/codex" "CLAUDE_CONFIG_DIR=$test_dir/claude"
  "OPENCODE_DATA_HOME=$test_dir/oc" "OPENCODE_CONFIG_DIR=$test_dir/config/oc"
  "TERM=xterm-256color" "SHELL=/bin/sh" "LANG=en_US.UTF-8"
)
# Compiler/profile settings are explicit; auth tokens and live-agent context are not.
for name in RUSTUP_TOOLCHAIN CARGO_TARGET_DIR CARGO_TERM_COLOR RUSTFLAGS \
  CARGO_ENCODED_RUSTFLAGS RUSTDOCFLAGS CARGO_BUILD_JOBS CARGO_INCREMENTAL \
  CARGO_PROFILE_DEV_DEBUG CARGO_PROFILE_TEST_DEBUG CARGO_LLVM_COV_TARGET_DIR \
  LLVM_PROFILE_FILE LLVM_COV LLVM_PROFDATA; do
  if [[ -n "${!name+x}" ]]; then test_env+=("$name=${!name}"); fi
done
env -i "${test_env[@]}" "$@"
