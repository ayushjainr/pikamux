#!/usr/bin/env bash
set -euo pipefail

root_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
wrapper="$root_dir/scripts/with-test-home.sh"
if [[ ! -x "$wrapper" ]]; then
  printf 'missing executable test wrapper: %s\n' "$wrapper" >&2
  exit 2
fi

toolchain="${PIKA_CARGO_TOOLCHAIN:-}"
if [[ "${1:-}" == "--toolchain" ]]; then
  [[ $# -ge 2 ]] || { printf '%s\n' '--toolchain requires a value' >&2; exit 2; }
  toolchain=$2
  shift 2
fi
[[ $# -eq 0 ]] || { printf 'unexpected argument: %s\n' "$1" >&2; exit 2; }

cargo_cmd=(cargo)
[[ -n "$toolchain" ]] && cargo_cmd+=("+$toolchain")
cd "$root_dir"
tmux -V >/dev/null || { echo 'Recovery checks require tmux' >&2; exit 2; }
command -v script >/dev/null || { echo 'Recovery checks require script (PTY driver)' >&2; exit 2; }

lib_tests=(
  core::tests::reconciliation_completes_only_an_independently_proven_pending_launch
  core::tests::recover_existing_session_with_vanished_pane_never_creates_pending_launch
  core::tests::recovery_rejects_a_session_removed_from_fresh_inventory
  core::tests::genuine_duplicate_is_open_twice_then_recovers_automatically
  core::tests::recovered_exact_attach_records_open_and_preserves_newer_event
  core::tests::another_process_commit_supersedes_stale_board_observation
  core::tests::foreground_reobserves_after_another_connection_commits
  core::tests::unverified_binding_accepts_one_tagged_live_provider_without_uuid_argv
  tmux::tests::guarded_attach_rejects_a_generation_change_before_handoff
  tmux::tests::failed_attach_before_handoff_does_not_run_callback
)
hooks_tests=(
  wrong_launch_identity_fails_closed_and_exact_home_can_certify
  stale_wrapper_cannot_demote_or_unbind_a_replacement_owner
  pid_reuse_does_not_let_an_old_token_clear_the_new_generation
)
store_tests=(
  ownership_reservations_and_bindings_preserve_pid_generations
  verified_exit_explicitly_clears_a_coalesced_runtime_pid
  launch_phase_and_provider_generation_advance_atomically_with_pending_state
)
board_tests=(
  failed_open_returns_to_filtered_board_without_replaying_the_action
  overdue_launch_can_be_hidden_without_deleting_recovery_or_untracking_a_conversation
  unverified_terminal_requires_choice_and_never_relaunches_or_acknowledges
)
real_tmux_tests=(
  real_isolated_tmux_reopens_unique_uuid_owner_beside_inert_stale_tag
  real_isolated_tmux_blocks_unproven_competing_live_uuid_pane
  real_isolated_tmux_nested_provider_helper_does_not_steal_uuid_owner
)
identity_tests=(
  tag_failure_occurs_before_provider_execution_and_keeps_recovery_record
  post_execution_readback_failure_retains_exact_pending_generation
)

build_and_run() {
  local label=$1
  shift
  local build_json executable listed test_name
  local target_args=(--test "$label")
  [[ "$label" != lib ]] || target_args=(--lib)

  printf '\n== build %s ==\n' "$label"
  build_json=$("$wrapper" "${cargo_cmd[@]}" test --locked "${target_args[@]}" --no-run --message-format=json)
  executable=$(printf '%s\n' "$build_json" |
    python3 -c 'import json,sys; rows=[json.loads(line) for line in sys.stdin]; matches=[r["executable"] for r in rows if r.get("reason")=="compiler-artifact" and r.get("profile",{}).get("test") and r.get("executable")]; assert len(matches)==1, matches; print(matches[0])')
  [[ -n "$executable" && -x "$executable" ]] || {
    printf 'could not locate test executable for %s\n' "$label" >&2
    exit 1
  }

  listed=$("$wrapper" "$executable" --list --format terse)
  for test_name in "$@"; do
    if ! printf '%s\n' "$listed" | grep -Fqx "$test_name: test"; then
      printf 'missing exact recovery test %s in %s\n' "$test_name" "$label" >&2
      exit 1
    fi
  done
  "$wrapper" "$executable" --exact --test-threads=1 "$@"
}

build_and_run lib "${lib_tests[@]}"
build_and_run hooks_contract "${hooks_tests[@]}"
build_and_run store_contract "${store_tests[@]}"
build_and_run board_journey_contract "${board_tests[@]}"
build_and_run real_tmux_contract "${real_tmux_tests[@]}"
build_and_run identity_safety_contract "${identity_tests[@]}"
build_and_run terminal_signal_contract real_terminal_bridge_forwards_signals_and_restores_caller_tty
