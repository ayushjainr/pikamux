# Integration test declaration audit

Scope: every `#[test]` declaration in the 27 other `tests/*.rs` integration files, excluding `setup_contract.rs`, `differential.rs`, and `third_party_notices_contract.rs` (the focused ledger owns those). The body review was read-only; the parent later validated the final full suite. R=retain, F=fix test harness/contract, C=consolidate, D=delete. Oracle is the concrete defect the declaration should detect. Most tests exercise distinct seams; no C/D recommendation is made absent evidence of equivalent behavior.

## Board, CLI, clients, consultation

### `board_journey_contract.rs`

Audit status: all function bodies reviewed.

Runtime prerequisite: the three live tmux journey tests now fail rather than pass when tmux is unavailable; script absence likewise fails the prerequisite. CI provisions and checks both tools.

| line | declaration | disposition | defect oracle |
|---:|---|:---:|---|
|311|remote_board_feed_verifies_node_bounds_frames_and_clears_on_disconnect|R|Changed node, oversized frame, or disconnect leaves trusted/stale feed.|
|386|unverified_terminal_requires_choice_and_never_relaunches_or_acknowledges|R|Unproven owner triggers relaunch or unread acknowledgement.|
|494|exact_open_detach_and_reopen_return_to_the_same_filtered_board|R|Real tmux + fake Codex: repeats keyboard/mouse open-return, checks live status/feed projection and unread-preserving peek, reopens same pane, uses F11 without clobbering user F12, and returns an existing client to origin without killing agent.|
|963|failed_open_returns_to_filtered_board_without_replaying_the_action|R|Isolated PTY filters `/audit`, Enter reaches OPEN NEEDS ATTENTION for exact UUID, Esc returns to same filter and board process remains alive.|
|981|add_named_native_conversation_without_setup_preserves_unread_and_cancel|R|Seeds Claude custom vs derived titles and Codex generated title; `+` lists only custom, cancellation keeps tombstone, confirmation restores watch/unread, and config/hooks/settings remain absent.|
|1139|board_excludes_unconfirmed_provider_titles_without_deleting_the_record|R|Board output omits generated title while DB still has unread external record and one unconfirmed session.|
|1156|saved_thread_peek_explains_unavailable_inside_board_and_preserves_unread|R|`p` displays No live Pika pane without clear-screen/exit; both session unread and authoritative lifecycle observation remain true.|
|1197|help_and_usage_are_real_board_actions_with_a_return_path|R|`?` and `u` render their panels incrementally (no full clear); Esc returns to selected row.|
|1217|unwatch_can_be_cancelled_and_then_confirmed_without_leaving_the_board|R|Esc from x confirmation leaves tombstone false; Enter confirms tombstone true and board stays alive.|
|1244|overdue_launch_can_be_hidden_without_deleting_recovery_or_untracking_a_conversation|R|A one-hour pending launch: Esc cancels hide; Enter removes visible row but exact pending record survives and conversation remains tracked.|
|1299|client_update_exit_keeps_one_reopenable_startup_home_and_an_actionable_board_row|R|With real tmux and fake Codex/Claude updaters exiting 0/1, waits for exact pending-exit receipts; retry reuses home (no “already starting”), board opens STARTUP EXITED, hide preserves pane/pending receipt, creates no conversation, and prior unread remains.|

### `claude_quota_contract.rs`

| line | declaration | disposition | defect oracle |
|---:|---|:---:|---|
|21|statusline_forwards_original_bytes_and_persists_quota_only_without_opening_pika_state|R|CLI receives JSON containing private transcript/session identifiers; stdout is byte-for-byte payload, cache contains only 28.0 quota and no `private-`, Pika state dir absent.|
|46|missing_telemetry_is_silent_and_does_not_replace_existing_display|R|Forwarded `printf` display remains exact, no-forward path prints nothing, and no quota cache is created for `{}`.|
|69|early_exiting_statusline_preserves_output_and_exit_status_without_reading_input|R|512KiB stdin forces closed-pipe case; child prints visible+warning then exits 17; wrapper preserves both streams/code and creates no state.|

### `cli_smoke.rs`

Audit status: all function bodies reviewed.

| line | declaration | disposition | defect oracle |
|---:|---|:---:|---|
|6|client_board_rejects_changed_machine_before_opening_inventory|R|The disposable CLI expects node A while its isolated state has node B; must exit failure with `NODE IDENTITY CHANGED`.|
|21|client_fleet_open_rejects_unknown_destination_without_ssh|R|A marker SSH executable exits 91 if run; unknown node must instead fail with “not in this board's trusted fleet”.|
|234|version_is_native_and_side_effect_free|R|`--version` must print the package version and leave the isolated config/state parent empty.|
|248|empty_list_does_not_create_state|R|`list --json` on empty disposable roots must emit `[]` and create no files.|
|263|wait_matches_only_unread_ready_and_uses_timeout_code_124|R|Same synthetic READY row times out when unread=false, but succeeds with READY text when unread=true.|
|299|ask_selects_fast_by_default_and_deep_only_when_requested|R|A fake Codex RPC returns model/turn frames; asserts fast Luna vs deep Sol, JSON/JSONL semantics, streaming-before-final, cleanup, unchanged transcript and preserved unread.|
|385|unavailable_consultation_is_jsonl_only_and_uses_legacy_failure_code|R|Unavailable provider and missing exact row both exit 1 with empty stderr and parseable JSONL error/closed events; first reports `not_sent`.|
|427|setup_separates_proven_names_from_bounded_recent_labels_and_routine_is_quiet|R|Synthetic provider histories include proven/custom, derived, and >bounded recent labels; checks first-run options vs routine output and explicit browse.|
|566|setup_commissioning_requires_matching_observation_and_healthy_pending_launches|R|First setup lacks matching Codex hook proof and reports uncommissioned; exact fingerprint+SessionStart observation commissions; adding overdue pending launch reports degraded and removes commissioned claim.|
|646|setup_never_commissions_an_uncertified_required_provider_version|R|Overrides fake OpenCode executable to 1.18.20; command succeeds but reports minimum 1.18.21 and never says commissioned.|

### `client_board_contract.rs`

| line | declaration | disposition | defect oracle |
|---:|---|:---:|---|
|136|combines_every_paired_machine_without_name_or_uuid_deduplication|R|Two fake nodes return same thread UUID; combined board has 2 items with distinct node IDs, no local DB rows, and survives pairing sync.|
|162|one_offline_machine_retains_its_cache_without_hiding_the_other|R|After successful cache then offline refresh of A, both rows remain, A is stale, B fresh, and health reports offline.|
|200|exact_open_uses_the_selected_server_directly_after_fresh_validation|R|Attach selected B; captured argv uses `rs2a`, not `rs6`, omits reverse forwarding, sets `ClearAllForwardings`/`RemoteCommand=none`, and pins node/provider/UUID.|
|229|changed_identity_and_cancelled_board_never_launch_a_window|R|Fake mismatched identity and pre-cancelled token each make attach fail while launcher call list remains empty; trusted ID stays A.|
|264|changing_or_removing_a_pairing_invalidates_its_cache_only|R|Changing A's SSH target removes only A snapshot; unpairing B removes only B node.|

### `client_bridge_contract.rs`

| line | declaration | disposition | defect oracle |
|---:|---|:---:|---|
|89|pairing_uses_authoritative_fleet_v2_and_strict_envelopes|R|Constructed v2 hello validates; changing version to 1 yields Incompatible, adding `claim` is rejected.|
|116|pairing_request_requires_exact_protocol_ids_secret_and_fields|R|Builder emits v2 protocol/token; additive field, malformed expected UUID, and short token each fail validation.|
|136|re_pairing_rotates_the_server_and_client_secret_only_after_exact_receipt|R|Server route gets exact request token; wrong-node receipt leaves client empty; exact receipt installs token and generated token is 64 hex chars.|
|191|terminal_command_is_argv_only_and_pins_node_provider_and_conversation|R|Built wt argv includes safe SSH settings and exact node/provider/session, excludes continue/last fallback, accepts valid OpenCode ID but rejects traversal-like ID.|
|239|chosen_board_routes_multiple_unpaired_servers_through_its_trusted_ssh_target|R|For two unpaired targets, fake launcher receives `_client-fleet-open` via trusted `developer@devbox` source, exact source/target/session args, no source token, and replay dedupes; revoked relay causes no third launch.|
|314|fleet_relay_still_authenticates_source_and_rejects_injected_targets|R|Wrong source secret and semicolon target each fail bridge handling; fake launcher remains uncalled.|
|354|authenticated_launch_is_deduplicated_and_request_id_is_identity_bound|R|Same request ID/body at +1s returns identical receipt with one launch; same ID changed provider is Rejected with still one launch.|
|407|wrong_client_source_target_or_secret_fails_closed_without_launch|R|Three requests vary client UUID, secret, and target UUID; each returns Rejected and launcher count stays zero.|
|456|secret_rotation_is_adopted_by_reload_without_changing_client_identity|R|Reloaded bridge rejects old token, accepts new one; reload with changed client ID is rejected.|
|507|absent_tunnel_falls_through_but_rejection_and_uncertainty_block|R|Transport-unavailable response returns ContinueWithExistingAttach; explicit error and outcome-unknown errors propagate distinct rejection/uncertainty.|
|583|launched_receipt_is_exact_and_never_claims_attach_confirmation|R|Fake receipt says launched and not attached; injecting `attached:true` makes second response invalid.|
|621|listener_and_endpoint_are_loopback_only_and_wire_is_bounded|R|0.0.0.0/public IPv4 endpoint and non-loopback bind rejected; duplicate-line and max+1 message rejected.|
|637|loopback_server_launches_once_without_real_windows_or_powershell|R|Real loopback TCP exchange to fake launcher yields one launch with `fake-wt.exe`, no PowerShell executable.|
|686|legacy_short_connect_budget_allows_a_delayed_launch_receipt|R|Launcher sleeps 500ms but client endpoint timeout is 50ms; exchange still returns launched and one launch.|
|734|launch_request_rejects_non_uuid_provider_identity_and_unknown_fields|R|`$(touch nope)` session ID and unsupported provider are rejected by request validator.|

### `client_cli_contract.rs`

| line | declaration | disposition | defect oracle |
|---:|---|:---:|---|
|37|pairing_overrides_alias_remote_command_and_finds_a_non_path_install|R|Fake ssh rejects missing `RemoteCommand=none`; fake remote accepts only `_client-pair --stdio` with minimal PATH and non-PATH install.|
|58|ssh_pairing_deadline_includes_a_peer_that_never_reads_input|R|Fake SSH sleeps without reading; request must time out in under one second for 50ms budget.|
|72|ssh_pairing_cleans_exited_launcher_descendants_holding_both_pipes|R|Fake launcher exits while background sleep holds pipes; call must complete under three seconds.|
|83|ssh_pairing_preserves_failed_exit_diagnostics|R|Fake SSH exits 7 with stderr; error must remain exactly `pairing rejected`.|
|91|oversized_pairing_request_is_rejected_before_ssh_starts|R|A marker-writing fake SSH must not start for over-limit payload.|
|261|client_update_requires_yes_and_never_pairs_or_starts_agents|R|Choices y/Y increment update only; n/empty/1 do not; every case asserts no SSH or saved config, and noninteractive default errors without update.|
|309|windows_help_exposes_only_client_commands_and_denies_host_commands|R|Rendered help includes status/setup/bridge and explicitly unsupported hosting, excludes list/new/ask; invoking list errors.|
|329|default_and_explicit_status_are_client_only_and_do_not_touch_ssh|R|Default status prints stopped bridge and unsupported hosting; explicit status prints READY when fake bridge exists; no SSH/config writes.|
|346|setup_uses_fleet_v2_two_receipts_and_saves_the_exact_secret_once|R|Runtime captures exactly two SSH calls (hello then v2 pair); one saved node uses expected ID/token/target/alias/port; no bridge start/reverse forward.|
|393|a_wrong_pairing_identity_is_not_saved_or_started|R|Fake hello returns wrong node; setup errors on identity validation and leaves saved/start logs empty.|
|413|bridge_start_and_serve_use_validated_loopback_options_only|R|IPv6/local hostname start/serve captured; 0.0.0.0 start errors without another start.|
|469|setup_pairs_without_starting_a_bridge_or_reverse_tunnel|R|Setup saves one node while fake runtime start log stays empty.|
|477|first_bare_run_pairs_only_selected_hosts_and_opens_locally|R|Two candidates, choice 1: only devbox contacted, local open is selected node; second invocation uses cache without more SSH/save.|
|504|existing_pairings_all_open_together_even_with_a_legacy_default|R|After adding second configured node with old default, bare run opens both from cache without SSH/bridge start or chooser.|
|523|multi_selection_is_validated_before_any_contact_and_deduplicates|R|`select_targets` dedupes indices, supports all/direct targets, and rejects out-of-range and option-like target.|
|540|first_run_cancellation_and_invalid_choices_never_contact_candidates|R|Choices 0, 999, injection-like, empty leave SSH/save/open/start logs empty.|
|555|setup_adds_without_replacing_existing_pairings_and_enter_keeps_board|R|Adding typed host opens it; subsequent empty choice uses cached pairing without new SSH and retains board.|
|571|explicit_repair_never_replaces_a_changed_machine_identity|R|Existing node's next hello changes ID; repair fails before pair write/save.|
|592|native_board_does_not_require_the_old_remote_board_capability|R|Fake old host still opens selected node without starting bridge.|
|605|board_connection_owns_its_forward_and_binds_exact_machine_without_shell_text|R|Generated argv pins forwarding controls, target, and node; token absent and shell-like node ID rejected.|
|635|remembered_machine_config_roundtrips_and_rejects_unpaired_default|R|Config value round-trips; unknown default node fails parsing; omitted default remains None.|

### `consult_contract.rs`

Audit status: all 25 function bodies reviewed.

| line | declaration | disposition | defect oracle |
|---:|---|:---:|---|
|169|codex_uses_one_confirmed_ephemeral_child_for_multiple_turns|R|Fake app-server verifies child=exact ephemeral fork, read-only/no-app-shell policy, one server process, two turn/start calls on child, answers and cleanup complete; parent transcript byte-identical.|
|215|question_default_pins_fast_profile_and_reuses_the_child|R|Fast-path policy/model Luna is configured; two asks share one process/fork and remain ephemeral/read-only/tool-less.|
|240|codex_delivers_policy_in_first_input_without_repeating_it_on_followups|R|Across normal/fast fixtures, one fork and two turns; first text exactly policy+question, follow-up exactly raw question, same child and read-only ephemeral flags.|
|295|preparation_timing_includes_provider_startup|R|Slow-open fake app-server delays startup; preparation duration remains >=180ms before and after first ask.|
|310|codex_streams_before_completion_with_exact_turn_usage|R|Rejects empty question first; callback receives answer- delta for ordinal 2 before worker completion; excludes foreign/old output and checks completion/first-text order plus token metrics.|
|347|partial_output_is_not_a_completed_answer_or_retry_permission|R|Fake stream-error returns partial data then error; receipt reports zero answers, not retry-safe, no completion metric.|
|363|final_message_and_idle_before_provider_failure_are_not_success|R|Fake sends final/idle then process failure; `ask` still errors with zero answer and no retry permission.|
|378|progress_observer_reports_real_prepare_delivery_response_and_cleanup_transitions|R|Captures full callback sequence and compares all 9 stage/delivery/cleanup/outcome tuples, including Unknown→Confirmed and cleanup completion.|
|462|codex_rejects_invalid_or_parent_fork_identity_before_sending_question|R|Four fixture fork results (empty, malformed, same ID in lower/upper case) fail distinct ephemeral identity proof, NotSent/CleanupComplete, no turn/start.|
|487|preparation_failure_emits_terminal_not_sent_progress_before_any_turn|R|Policy mismatch reports Prepare/NotSent/CleanupComplete, terminal failed+retry-safe event explaining nonconfirmation, no turn/start.|
|520|codex_reports_unknown_delivery_without_resending|R|Unknown-mode ask times out as Turn/Unknown/not retry-safe; fixture log contains exactly one turn/start.|
|540|codex_reports_confirmed_delivery_when_response_times_out|R|Timeout fixture confirms delivery but no response; receipt remains Confirmed and not retry-safe.|
|558|codex_notification_flood_is_bounded_before_turn_delivery_is_confirmed|R|Notification-flood fake reaches backlog error as Unknown/not retry-safe and one turn/start only.|
|583|cancellation_interrupts_blocked_codex_startup_and_reaps_owned_child|R|Fake startup spawns sleeper; cancellation returns under 1s with cleanup complete, leaves parent marker untouched and kills recorded child PID.|
|630|cancellation_interrupts_blocked_codex_turn_without_resending|R|Cancel blocked turn; error remains Confirmed, close succeeds under 1s and log has one turn/start.|
|660|oversized_question_is_rejected_before_provider_delivery|R|Question of MAX+1 returns NotSent/64KiB diagnostic and fake RPC log has no turn/start.|
|677|codex_rejects_unconfirmed_policy_and_cleans_its_owned_child|R|Mismatched fake policy makes open fail Prepare/NotSent/CleanupComplete and retry-safe.|
|695|claude_uses_one_nonpersistent_toolless_process_for_multiple_turns|R|Claude fake proves fork session/no persistence/tools disabled; two asks via one `-p` process, resumes exact leaf, logs two user messages.|
|723|unsupported_fast_profiles_fail_before_starting_a_provider|R|Claude/OpenCode fast policy constructors error; Codex fast returns configured fast model.|
|791|opencode_uses_one_exact_readonly_child_then_verifies_deletion|R|Fake OpenCode returns exact child; two turns use restricted agent/model; close verifies child rows deleted while parent remains and reports cleanup complete.|
|848|opencode_answer_survives_failed_exact_child_cleanup|R|Delete-disabled fake returns answer; close fails Cleanup/Failed but receipt retains one answer and exact child ID.|
|868|opencode_unknown_fork_identity_is_not_retry_safe_or_guessed_for_deletion|R|Fake fork returns invalid ID; fails NotSent/Unknown cleanup/not retry-safe and log proves no session-delete command.|
|885|fake_opencode_server|R|Explicitly ignored fixture entrypoint; the parent executes it with `--ignored` and observes fake server behavior rather than a green no-op in ordinary discovery.|
|924|fake_opencode_turn|R|Explicitly ignored fixture entrypoint; the parent executes it with `--ignored` and observes the streamed turn protocol.|
|965|fake_opencode_delete|R|Explicitly ignored fixture entrypoint; the parent executes it with `--ignored` and observes exact child cleanup.|

## Distribution, diagnosis, expertise, files, fleet

### `distribution_safety_contract.rs`

| line | declaration | disposition | defect oracle |
|---:|---|:---:|---|
|172|reused_release_binary_is_compared_before_it_can_run_or_activate|R|After installing two releases, test points current back to old then replaces retained new binary with marker-writing payload; install must report candidate mismatch, leave marker absent, preserve old current.|
|221|supplied_candidate_must_match_verified_archive_before_any_probe|R|Passes same-version marker-writing candidate not extracted from archive; rejects exact mismatch before marker or root write; then archived candidate installs.|
|260|same_version_tampered_current_is_rejected_before_it_can_run|R|Replaces active same-version executable with marker writer; staged reinstall rejects without marker/current change; restoring executable but tampering LICENSE is also rejected.|
|314|rollback_rejects_a_tampered_binary_before_executing_it|R|Replaces retained old binary with marker writer; rollback reports absent/invalid, leaves marker absent and newer release active.|
|361|rollback_and_remote_bundle_accept_notices_bound_to_an_older_archive|R|Packages first release with custom old notices, upgrades, stages remote bundle and rolls back; exact old notice bytes remain installed.|
|412|native_archives_are_reproducible_and_carry_exact_notices|R|Packages same native executable twice; bytes identical, tar contains exactly LICENSE/THIRD_PARTY/pika, extracted notices byte-match source, remote bundle stages.|
|450|windows_archives_are_reproducible_and_carry_exact_notices|R|Cross-packages same executable twice for Windows; ZIP bytes identical and exact three-member listing/notice bytes verified; remote bundle stages.|
|501|install_rejects_permissive_managed_directories_before_writes_and_creates_private_ones|R|0777 root/bin rejected before writes; successful 0700 install creates all owned dirs without group/other bits; subsequently chmodded releases dir is rejected.|
|593|offline_bootstrap_rejects_oversized_bundle_files_before_copying_or_writing_root|R|64KiB+1 manifest through shell bootstrap fails with size-limit diagnostic and no managed root.|
|627|install_native_reaps_archive_subprocess_groups_on_int_and_term|R|Fake tar spawns long sleep; sends SIGINT/SIGTERM to actual install process, requires 130/143 and descendant PID gone.|

### `doctor_contract.rs`

| line | declaration | disposition | defect oracle |
|---:|---|:---:|---|
|164|exact_recovery_produces_a_copy_safe_certificate|R|Commissioned fixture with one exact home yields safe/recoverable counts 1; JSON/human certificate redacts UUID, transcript text, and temp path.|
|183|genuine_second_identity_is_reported_as_duplicate_and_outside|R|Adds a second UUID-bearing Codex process; doctor reports unsafe, duplicates=1/outside=1 and identity.duplicates Error without disclosing UUID.|
|202|unproven_tagged_provider_is_mismatched_not_a_second_proven_owner|R|Adds tmux-tagged shell plus app-server descendant lacking UUID; result is mismatch=1, duplicates=0 and identity.mismatch Error.|
|233|missing_hook_observation_and_unsafe_permissions_block_certificate|R|Replaces expected hook fingerprint and chmods config 0644; report unsafe with hooks warning and Unix permission warning.|
|266|malformed_config_and_schema_are_reported_without_panicking|R|Config `[]` and non-SQLite DB produce config.format/database.schema Errors instead of panic.|
|303|repair_removes_only_records_disproved_by_complete_snapshots|R|Complete fake tmux/process snapshots remove stale pending/resume/owner (3 receipts) but preserve live counterparts; receipts redact token/UUID and evidence unchanged.|
|401|incomplete_runtime_evidence_can_never_repair_state|R|Default incomplete evidence passed to inspect-and-repair leaves stale pending record intact and blocks repair.|

### `expert_contract.rs`

| line | declaration | disposition | defect oracle |
|---:|---|:---:|---|
|66|card_input_is_cleaned_without_inventing_content|R|Whitespace/unsafe material is normalized to the expected summary/state/topics/artifact; empty topics returns explicit validation error.|
|93|search_requires_all_lexical_terms_and_explains_ranking|R|Two seeded profiles: query “factor attribution” returns only profile matching both terms and marks topic+scope; stopword-only and factor+unrelated return none.|
|151|exact_publisher_cannot_cross_workstreams_or_active_leaves|R|Wrong active leaf proof fails; valid proof for row one cannot publish against row two sharing transcript; only exact row receives card.|
|185|current_work_is_separate_deduplicated_and_requires_a_card|R|Current work before durable card errors; after publish it changes state only, preserves summary/time, repeated normalized update is identical.|
|210|transcript_growth_stales_work_but_not_durable_publication_age|R|Append transcript makes current state stale, keeps published scope age fixed; milestone refresh restores current state without changing summary/topics/artifacts.|
|249|opencode_fingerprint_is_scoped_to_parent_tree|R|Changing unrelated root session/messages leaves fingerprint unchanged; adding child under exact root changes it.|
|299|unwatched_card_remains_discoverable_without_retracking|R|After publishing then untracking, search still returns card with watched=false/discoverable=true while tracked session list stays empty.|
|323|batched_source_index_maps_active_leaves_back_to_stable_workstreams|R|Fixture begins with active and archived leaves in provider source DB; active leaf maps to stable workstream while archived leaf does not (body continuation).|

### `expert_federation_contract.rs`

| line | declaration | disposition | defect oracle |
|---:|---|:---:|---|
|204|equal_provider_uuid_on_two_nodes_stays_distinct_and_stale_truth_is_preserved|R|Two cached nodes expose same thread UUID; results remain qualified by atlas/borealis, preserve source-unavailable state and legacy missing freshness as Unknown.|
|256|twenty_node_expert_search_ranks_experts_before_ordinary_board_slices|R|20 nodes each fill ordinary row allocation but add one needle profile; expert directory still returns all 20 matching profiles with no notices.|
|312|experts_cli_merges_local_and_cached_remote_with_exact_json_and_no_ssh|R|CLI combines one local and one cached remote card, checks exact key sets/node qualification/age/notices/text, and an SSH marker stays absent.|
|469|expert_status_includes_unwatched_local_and_cached_remote_cards_without_ssh|R|Status JSON shows untracked local card, missing-source local row and cached remote card with exact fields; SSH marker stays absent.|
|603|local_cli_and_remote_transport_refuse_unavailable_sources_before_spawn|R|Corrupt provider DB and unavailable remote card make local/remote consultation return “No question was sent”; provider and SSH markers remain absent.|

### `files_journey_contract.rs`

| line | declaration | disposition | defect oracle |
|---:|---|:---:|---|
|185|files_keyboard_preview_is_inert_and_restores_terminal|R|Real PTY selects source containing OSC52 marker; preview never emits control sequence, idle 350ms emits no repaint, navigation works, then finish verifies tty/mouse reset.|
|211|small_files_viewer_always_has_an_exit|R|At 32x8, viewer renders Files, accepts Esc and cleanly finishes.|
|219|tree_can_be_hidden_and_restored_from_the_keyboard|R|After preview, `t` removes tree file but keeps preview; second `t` restores tree entry.|
|241|markdown_and_wrap_toggles_work_in_a_real_narrow_terminal|R|60x24 rendered Markdown hides markup; `m` exposes source, `w` removes/restores wrap tail, second `m` returns rendered, source bytes unchanged.|
|275|files_refuses_redirected_input_without_opening_state|R|Non-PTY `_files-view` exits with interactive-terminal error and leaves temp HOME empty.|
|291|code_highlighting_renders_and_respects_no_color|R|For colored/uncolored PTYs, checks `fn` magenta escape presence matches flag, no color escape under NO_COLOR, OSC52 inert and source unchanged.|
|322|mouse_file_wheel_and_divider_drag_restore_mouse_reporting|R|SGR file-wheel down/up changes ROW_00→ROW_03→ROW_00; drag moves divider cursor; exit emits both mouse-disable sequences.|
|371|mouse_tree_wheel_scrolls_viewport_without_opening_file|R|With 36 tree files and a preview open, SGR wheel-down over tree changes visible tree entry to tree_27, keeps `ROW_00` visible, and neither opens a `TREE_FILE_` nor scrolls the preview before clean exit.|

### `fleet_contract.rs`

Audit status: all 42 bodies were reviewed by the delegated fleet-contract auditor. The two initially weak oracles were strengthened and all 42 fleet tests passed; the private-value wire sentinel also passed focused validation.

| line | declaration | disposition | defect oracle |
|---:|---|:---:|---|
|273|ssh_config_discovery_is_file_only_and_configured_hosts_rank_first|R|Temp SSH config/includes plus fake Tailscale JSON yield exact sorted aliases, unsupported-OS count, source ranking, and laptop candidate without executing SSH.|
|309|wire_never_exports_transcript_tmux_or_process_identity|R|Serialization fixture seeds secret transcript/pane/root values and requires both those values and their serialized field names absent while preserving exact-home data.|
|319|strict_snapshot_rejects_identity_changes_duplicates_and_extensions|R|Wrong UUID, repeated session, or injected secret profile key separately yields quarantine/repeated-row/error instead of accepted snapshot.|
|345|strict_snapshot_rejects_more_than_two_thousand_sessions|R|Generated MAX+1-session snapshot is Incompatible with safety-limit message.|
|367|sender_reserves_untracked_expert_inside_the_two_thousand_row_wire_bound|R|MAX watched rows plus one expert serialize to exactly MAX rows, retain expert, and report omitted_rows=1.|
|415|sender_bounds_large_expert_profiles_without_rejecting_the_whole_snapshot|R|Six oversized expert profiles/cards keep serialized snapshot <= message bound and valid while reporting positive omissions and incomplete expert directory.|
|464|cached_fleet_accepts_twenty_small_nodes_and_fairly_slices_aggregate_rows|R|Twenty one-row nodes aggregate fully; five oversized nodes aggregate at MAX with equal per-node shares and omission notices, while exact-node query is unsliced.|
|547|cached_fleet_twenty_heavy_nodes_parses_only_the_fair_prepaint_projection|R|Twenty MAX-row payloads exceed aggregate source bytes but SQL projection rows/bytes stay within fleet bound, with per-node quota/notices; optimized-only timing guard is <2s.|
|628|sixty_four_dense_nodes_each_contribute_with_bounded_board_and_expert_input|R|64 dense node snapshots produce nonempty bounded board/expert projection for every node and manager board/expert result includes all nodes (release-only <5s bound).|
|713|expert_search_finds_the_only_match_after_a_sixty_four_node_fair_slice|R|Only matching expert is last row of target's 64-node slice; indexed revision query finds exact UUID; legacy revision omission yields no match plus input-limited notice.|
|832|remote_name_resolution_exposes_exact_candidates_and_error_routes|R|Same display label for Codex/Claude yields two exact candidates; ambiguous resolve errors with UUID@machine candidates rather than guessing.|
|870|failed_initial_snapshot_trusts_nothing|R|Fake hello followed by incompatible/truncated initial snapshot makes add fail and persists no trusted node.|
|900|stale_add_response_cannot_trust_a_node_after_a_newer_add_claim|R|Transport supersedes in-flight add claim; older completion returns superseded error and no node becomes trusted.|
|924|intentional_refresh_cancellation_preserves_ready_node_and_cached_snapshot|R|Pre-cancel refresh returns Unreachable while status remains Ready, error empty, and original cached snapshot unchanged.|
|951|cached_board_carries_real_expertise_with_independent_clocks_and_node_identity|R|Two-node cached profile projections preserve node scoping and sanitization, compute distinct received-relative timestamps, retain stale metadata, omit transcript, and issue no transport calls.|
|999|cached_board_missing_legacy_or_other_identity_profiles_do_not_invent_expertise|R|Aggregate+exact reads across absent/wrong UUID/provider/blank profile fixtures retain only matching nonblank scope/topics; do not invent work/timestamps and do no transport.|
|1055|remote_peek_cancellation_never_dispatches_and_success_is_read_only_and_exact|R|Pre-cancel capture makes zero transport calls; fake exact-node peek sanitizes ANSI/OSC/DCS/bidi, caps lines at 2000, and leaves cached unread row unchanged.|
|1110|cached_remote_identity_includes_node_and_stale_cache_cannot_need_attention|R|Identical remote thread IDs under two nodes map to distinct keys; marking one error makes it stale but not needs-attention.|
|1137|snapshot_notice_field_is_sent_only_to_capable_nodes|R|Removing/adding capability toggles snapshot request field absent/true across refresh.|
|1166|remote_acknowledgement_request_carries_the_selected_event|R|Capability-absent fake node receives read then mutation calls; mutation's expected_last_event_at equals selected event 9.|
|1206|remote_clock_skew_does_not_control_local_cache_age_or_polling|R|±1h remote clock captures still age by local receipt time; legacy past/future timestamps are stale and future receipt node is selected before normally due node.|
|1272|refresh_scheduler_is_bounded_oldest_first_and_manual_selection_only|R|Three-node due fixture chooses oldest automatically and honors explicit manual node selection.|
|1295|attach_refreshes_and_routes_only_exact_node_provider_uuid|R|Cached selection is refreshed before fake runner exits 0; runner receives exact UUID (not label) and selected node.|
|1315|attach_ignores_reassigned_display_alias_and_uses_immutable_node_id|R|Renamed original plus replacement reusing its display alias still routes attach to original immutable node ID.|
|1350|attach_fails_closed_when_immutable_node_was_deleted|R|Deleting selected node returns NotFound and exact-run call count remains zero.|
|1404|attach_fails_closed_when_route_is_replaced_during_refresh|R|Fake refresh swaps SSH route; operation returns Quarantined route-changed and exact-run is never called.|
|1431|mutation_timeout_reuses_durable_idempotency_key|R|Fake transport times out after the first mutating request; retrying via a second adoption must send the same durable UUID idempotency key, so a regenerated key fails the explicit equality assertion (strengthened and passed in fleet focused run).|
|1513|server_validates_exact_route_and_replays_mutation_receipt_once|R|Two identical untrack JSONL requests produce identical replies but service untrack counter equals one.|
|1545|quota_endpoint_requires_exact_machine_and_does_not_touch_conversations|R|Missing/wrong/correct expected node requests error/error/return exact quota; quota call count one, untrack zero, local sessions empty.|
|1584|server_rejects_changed_node_without_service_action|R|Changed-node request fails before service action; strengthened assertions require empty acknowledgement events, zero untracks, and zero quota reads, catching side effects hidden by the error result (strengthened and passed in fleet focused run).|
|1607|remote_acknowledgement_is_bound_to_the_observed_event|R|Request for event 10 against fake current event 20 returns false while service receives exactly the selected 10.|
|1636|remote_acknowledgement_rejects_a_missing_event_without_service_action|R|Ack JSON lacking selected event is Incompatible and fake service records no ack call.|
|1664|ssh_payload_stays_on_stdin_and_remote_argv_is_fixed|R|Malicious payload passed through fake SSH stdin yields valid response, fixed `_fleet --stdio` argv, no injected `touch`, and no sentinel file.|
|1693|ssh_timeout_is_bounded_and_mutation_becomes_outcome_unknown|R|Fake sleeping SSH under 30ms timeout returns mutating OutcomeUnknown in <1s.|
|1708|snapshot_request_cancellation_joins_and_kills_owned_descendants|R|Fake SSH spawns 30s child; cancellation returns Unreachable <1s and exact child PID disappears within 2s (zombie state caveat).|
|1755|remote_consultation_reuses_one_connection_and_requires_v2_cleanup|R|Persistent fake SSH JSONL session produces v2 ephemeral exact-child receipt, two asks/answers and complete cleanup.|
|1814|remote_consultation_streams_and_rejects_foreign_turn_preview|R|Valid turn exposes delta before final plus metrics/cleanup; wrong turn and no-capability variants return Incompatible without delta.|
|1903|remote_opening_requires_v2_ephemeral_exact_child_and_provider_isolation|R|Mutated v1, persistent, same-child, and weak-sandbox receipts each quarantine with explicit no-question-sent evidence.|
|1963|remote_opening_validates_claude_flags_and_opencode_exact_child|R|Fake valid provider receipts require no Claude child but exact OpenCode child; close succeeds.|
|2029|remote_consultation_cancellation_kills_owned_transport_promptly|R|Fake SSH stalls after question and owns sleeper child; cancellation yields OutcomeUnknown/cleanup unknown <1s and child is killed.|
|2113|remote_consultation_refuses_wrong_leaf_before_sending_question|R|Fake open receipt leaf differs from selected active leaf; result quarantines and question-sent remains false.|
|2168|remote_consultation_rejects_partial_frames_and_unverified_cleanup|R|Truncated open frame is rejected; valid open+answer followed by unverifiable close returns answer but OutcomeUnknown cleanup receipt.|

## Hooks, identity, onboarding, provider discovery, persistence, release

### `hooks_contract.rs`

| line | declaration | disposition | defect oracle |
|---:|---|:---:|---|
|32|generated_titles_and_inherited_panes_cannot_subscribe_or_alert|R|For every provider, Stop with title + pane context causes neither alert nor tag/watch; after explicit restore, same hook can request tag/watch.|
|60|stale_pika_name_environment_does_not_revert_provider_rename|R|Existing Codex row has new title; SessionStart with old desired name yields no native-name request and keeps new title.|
|123|payload_validation_is_bounded_typed_and_provider_specific|R|Oversize, array, numeric ID and Codex-invalid heartbeat reject; same heartbeat is accepted for OpenCode.|
|158|provider_events_map_to_structured_attention_without_transcript_text|R|Codex request_user_input and Claude permission notification map NeedsYou; Stop is Working only with background task, else Ready.|
|180|ephemeral_hook_has_no_durable_side_effect|R|Ephemeral Stop returns Ignored and does not initialize store.|
|196|invalid_identity_context_fails_before_state_is_initialized|R|NaN time + NUL launch token errors before store creation.|
|213|tombstone_survives_worker_and_lifecycle_hooks|R|After untrack, worker Stop and SessionStart both Ignored; tombstone remains and tracked rows stay empty.|
|238|proven_workers_and_inherited_cross_provider_children_never_enter_inventory|R|Codex subagent and expected-Claude context each return Ignored; no session or hook observation persisted.|
|265|bounded_immutable_metadata_corrects_codex_identity_and_proves_workers|R|Rollout session_meta changes reported ID to canonical and marks subagent; hook ignores it and creates no inventory row.|
|286|newest_lifecycle_wins_and_safety_remains_stronger|R|Older UserPromptSubmit cannot replace Ready at t=20 though unread rises; later Safety OpenTwice remains after newer hook.|
|346|equal_time_hook_updates_preserve_acknowledgement_and_question_rules|R|Acknowledged completion stays read under older prompt; equal-time permission question becomes unread and cannot be acked by same watermark.|
|397|equal_time_attached_completion_clears_unread_without_advancing_watermark|R|Second equal-time attached Stop clears unread but leaves event/observation time 20.|
|418|session_end_preserves_an_unread_completion_and_its_event_time|R|Stop at t10 then SessionEnd at t20 projects Ready/unread=true with last_event_at still 10.|
|446|older_session_end_preserves_a_newer_exact_owner_and_live_projection|R|Prompt owner at t20 then end t10 returns Working/read=false, retains owner last_seen=20 and prompt observation.|
|488|older_ordinary_hook_preserves_newer_runtime_evidence|R|Runtime Error observation at t30 survives ordinary PostToolUse at t20.|
|535|older_opencode_deletion_preserves_a_newer_live_session|R|OpenCode SessionEnd marked deleted at t10 after start t20 is Ignored as stale; row and owner remain.|
|570|unnamed_external_hook_records_only_an_exact_owner_lease|R|Unnamed Codex PostToolUse returns OwnerOnly; no session row, exact PID 42 lease exists.|
|606|shared_codex_daemon_updates_lifecycle_without_claiming_the_terminal_root|R|Two root cases: Stop sets Ready/unread, omits tag; daemon PID lease retained while inherited PID does not overwrite root.|
|645|shared_codex_daemon_strips_inherited_terminal_and_launch_claims|R|Child SessionStart with all forged inherited pane/launch/provider/name claims updates lifecycle only; no tags/name/certification/binding, parent home unaffected.|
|722|wrong_launch_identity_fails_closed_and_exact_home_can_certify|R|Wrong ID keeps pending/no binding and records expected-ID error; exact ID binds but only `certify_hook_home` with PID+start removes pending and writes recovery owner.|
|798|attached_completion_is_read_but_permission_remains_unread|R|Attached Stop is read, later attached PermissionRequest becomes unread.|
|835|claude_title_output_and_codex_noop_stdout_are_protocol_safe|R|Fresh Claude SessionStart returns intended title JSON; after provider-native rename resume emits no title; Codex emits `{}` while requesting initial name only on fresh launch, not resume.|
|967|opencode_delete_removes_only_the_exact_non_tombstoned_root|R|Deleted OpenCode root returns Deleted and removes exact session plus owner.|
|1001|process_exit_uses_binding_and_nonzero_status_becomes_actionable|R|Binding maps token even when supplied session ID is stale; exit 9 makes exact session Error/unread/exited and consumes binding.|
|1041|identityless_wrapper_exit_records_only_an_authenticated_pending_receipt|R|Across code/launch-phase cases, registered owner token records exit while preserving pending and unrelated unread question; exact replay is idempotent and stale/different-code replay rejected.|
|1161|identityless_wrapper_exit_rejects_forged_stale_and_unmatched_receipts|R|Wrong provider/ID/owner/time/token and wrong launch phase all refuse receipt; deleted pending also refuses.|
|1275|identityless_exit_for_reserved_uuid_needs_matching_binding_and_no_session|R|Matching reservation+binding+no session accepts; wrong binding, existing session, and already-certified launch each refuse pending exit.|
|1411|stale_wrapper_cannot_demote_or_unbind_a_replacement_owner|R|New launch binding and live owner generation 20 survive exit callback carrying old owner token.|
|1460|one_duplicate_exit_preserves_the_other_generation_and_safety_state|R|Of OpenTwice owners PID51/PID52, leaving token removes only first; status/root remain OpenTwice/PID51 and PID52 generation survives.|
|1506|pid_reuse_does_not_let_an_old_token_clear_the_new_generation|R|Same PID at start 10/20 with old/new tokens: old-token exit leaves Working root and exactly new token/start20 owner.|
|1553|process_exit_cannot_demote_a_stronger_identity_failure|R|Nonzero exit on OpenTwice identity error leaves OpenTwice and identity reason.|

### `identity_safety_contract.rs`

Runtime prerequisite: all five live tmux safety tests now fail rather than pass when tmux is unavailable; script absence likewise fails the prerequisite. CI provisions and checks both tools.

| line | declaration | disposition | defect oracle |
|---:|---|:---:|---|
|68|guarded_tag_removal_rejects_a_replaced_pane_generation|R|Real isolated tmux respawns same pane ID; clear-if-unchanged errors, PID generation differs, and replacement retains original identity tags.|
|111|terminal_guard_never_overwrites_an_existing_user_key_binding|R|Isolated server reserves User500/User501; guard installs at key 502, preserves user binding and reserved user-key value.|
|209|tag_failure_occurs_before_provider_execution_and_keeps_recovery_record|R|Wrapper fails tmux `if-shell`; new_session errors but leaves PaneAllocated pending root and no provider process in observed tree.|
|255|pane_replacement_between_readback_and_respawn_never_starts_provider|R|Wrapper replaces pane during if-shell/respawn; operation errors, root PID differs, and replacement process tree contains no provider.|
|302|post_execution_readback_failure_retains_exact_pending_generation|R|Wrapper lets respawn execute then fails list-panes readback; launch errors but pending retains pane/root, ProviderStarting phase and exact provider/launch tags.|

### `install_signal_contract.rs`

| line | declaration | disposition | defect oracle |
|---:|---|:---:|---|
|122|shell_bootstrap_sigint_kills_and_reaps_active_probe_group|R|Disposable installer reaches fake `--help` probe with sleeper child; SIGINT must return 130 and both recorded PIDs disappear.|
|127|shell_bootstrap_sigterm_kills_and_reaps_active_probe_group|R|Same subprocess fixture under SIGTERM must return 143 and reap installer probe plus sleeper.|

### `muse_native_contract.rs`

| line | declaration | disposition | defect oracle |
|---:|---|:---:|---|
|20|native_echo_accepts_generated_user_hooks_without_model_quota (ignored)|R|Installed native Muse no longer emits compatible hooks offline; opt-in only, not routine CI evidence.|

### `named_admission_contract.rs`

Audit status: both function bodies reviewed.

| line | declaration | disposition | defect oracle |
|---:|---|:---:|---|
|98|first_reconcile_auto_admits_only_provider_proven_claude_personal_names|R|Empty first reconcile, then provider custom-title event vs derived AI title and SDK helper: only exact renamed root is watched; explicit unwatch remains empty after new reconcile.|
|161|named_membership_grant_preserves_existing_unread_owner_and_metadata|R|Preexisting external row, unread lifecycle observation and owner token survive watch_named_session; asserts old cwd/branch/model/pane/PID and sole owner are preserved over provider candidate fields.|

### `onboarding_journey_contract.rs`

| line | declaration | disposition | defect oracle |
|---:|---|:---:|---|
|225|first_pika_is_a_quiet_consent_screen_and_escape_preserves_settings|R|First run writes/changes settings before consent.|
|241|detailed_diff_is_available_before_approval_and_cancel_writes_nothing|R|Preview/cancel performs writes before approval.|
|255|approved_setup_preserves_settings_opens_board_and_never_interviews|R|Approval clobbers config, fails board handoff, or interviews provider.|
|282|tiny_terminal_cannot_approve_an_invisible_choice|R|Hidden option can be activated below minimum size.|
|294|choosing_work_keeps_identity_explicit_and_does_not_import_everything|R|Selection silently imports all provider history.|
|322|windows_screen_fixture_entry|R|Explicitly ignored fixture entrypoint; the parent executes it with `--ignored` to observe the native screen outcome rather than a green no-op.|
|422|windows_screen_pairs_only_selected_hosts_and_keeps_success_after_failure|R|Unselected hosts contacted or successful pairing discarded on sibling failure.|
|445|windows_screen_cancel_never_contacts_a_host|R|Cancel initiates host contact.|

### `provider_discovery_contract.rs`

| line | declaration | disposition | defect oracle |
|---:|---|:---:|---|
|100|codex_first_screen_requires_proven_authorship_but_browse_preserves_safe_labels|R|SQLite fixtures include explicit name, fork, archive, automation worker, subagent and index-only label: import candidates empty; discovery allows explicit/fork/index label but excludes archive/worker; browse retains safe label.|
|222|codex_later_name_change_appears_without_importing_generated_or_helper_titles|R|Index transitions include true rename, generated-only, same title, stale DB conflict, archive/helper/fork; exactly independent renamed root is imported/watched.|
|308|codex_database_name_wins_over_older_index_label|R|New DB rename timestamp 200 beats old index label timestamp 100 in discovered candidate.|
|351|claude_title_refresh_uses_provider_file_change_not_hook_activity_clock|R|Claude transcript custom-title file mtime=100 while hook activity=200; reconcile candidate still receives new provider title.|
|388|claude_title_scan_round_robins_past_the_first_batch|R|40 watched 2MiB Claude transcripts with rename at index39; repeated reconcile eventually imports it and 12 more passes do not revert title.|
|470|claude_first_screen_requires_explicit_name_and_exact_uuid_still_resolves|R|Custom titles/history explicit rename become import candidates; derived title excluded; exact UUID for derived session resolves unlabeled; SDK worker excluded.|
|538|claude_history_only_exact_lookup_ignores_title_and_browser_budget|R|No session metadata plus 1001 recent generated-title files yields no candidates, yet exact UUID finds transcript and tracked lookup resolves it.|
|581|claude_exact_uuid_survives_the_ten_thousand_transcript_browse_cap|R|With 10,001 filler files, exact UUID lookup and tracked lookup still return target's custom title.|
|607|codex_archive_lookup_is_scoped_to_bounded_index_candidates|R|Archive DB includes integer-ID unrelated row; candidate lookup for target/index title returns empty, not parser failure or import.|
|655|claude_large_history_uses_bounded_recent_title_and_worker_records|R|3MiB transcript with custom title at end is found by exact lookup in under 2s.|
|680|opencode_never_claims_title_provenance_and_projects_child_lifecycle|R|Fixture has root+completed child, archived row, worker-prefix row, and new-session title; discover empty, browse includes only roots and projects child's Ready/time, exact child/archive/worker lookup empty.|
|782|reconciliation_persists_native_rename_and_active_leaf_lifecycle|R|Watched stable root points to leaf transcript/source DB; reconcile updates durable name from leaf and sets Ready/unread from task_complete.|
|839|renamed_independent_codex_fork_requires_its_own_tracking_choice|R|Renamed fork is excluded from named auto-import, shown in recent candidates, then only appears after explicit adoption.|
|908|codex_reconciliation_handles_two_thousand_watched_rows_in_one_bounded_pass|R|Two thousand SQLite threads reconciled in one call; all returned, exactly bounded 16 carry Ready/task completion; rest retain zero activity time.|
|971|opencode_reconciliation_batches_two_thousand_roots_and_their_lifecycle|R|Two thousand SQLite roots/messages return in one call, each lifecycle Ready from assistant completion.|
|1024|claude_reconciliation_defers_changed_titles_beyond_its_cycle_budget|R|Two thousand title transcripts all return, exactly first budget 8 have title+updated timestamp and 1992 remain deferred with zero timestamp.|

### `real_tmux_contract.rs`

Audit status: all six behavior journeys and the receipt helper body have been reviewed. The six live tmux tests now fail rather than pass when tmux is unavailable; script absence likewise fails the prerequisite. CI provisions and checks both tools.

| line | declaration | disposition | defect oracle |
|---:|---|:---:|---|
|176|real_isolated_tmux_reopens_unique_uuid_owner_beside_inert_stale_tag|R|A real isolated tmux owner and inert pane sharing UUID tags are reconciled; open must reattach the owner, preserve both tags and the stale pane PID, and leave exactly one provider process.|
|306|real_isolated_tmux_blocks_unproven_competing_live_uuid_pane|R|A second live provider with the same identity makes capture/open reject ambiguity; both UUID processes remain alive and competing tags remain untouched.|
|409|real_isolated_tmux_nested_provider_helper_does_not_steal_uuid_owner|R|A fake provider starts an untagged child helper; process-tree proof binds the pane PID to the provider root and pane capture contains the owner's output.|
|498|real_isolated_tmux_attach_observes_receipt_after_proven_commit|R|A real tmux pane receives the commit marker; a script-PTY attach succeeds only after receipt callback writes `after-receipt`, and transcript shows exact `CONTINUITY PROVEN ... ATTACHED LIVE`.|
|587|real_isolated_tmux_receipt_helper|R|Explicitly ignored fixture entrypoint; the parent invokes it with `--ignored` in an isolated real-tmux PTY and asserts the receipt order.|
|610|real_isolated_tmux_list_clients_proves_the_exact_selected_pane|R|With real tmux attached through a script PTY, polling actual `list-clients -F` output requires a positive client PID paired with the exact selected pane ID.|
|701|real_isolated_tmux_resumes_all_providers_and_reuses_each_exact_home|R|Four fake provider executables for Codex/Claude/OpenCode/Muse are resumed by exact provider+ID; each waits for exact tagged home and provider output, second open is `ATTACHED LIVE`, pane count stays one-per-provider, and cleanup kills only those fixture panes.|

### `scheduler_contract.rs`

| line | declaration | disposition | defect oracle |
|---:|---|:---:|---|
|15|platform_directories_are_explicit_and_host_independent|R|Platform path depends on host OS or wrong XDG path.|
|32|linux_schedule_is_finite_quota_aware_and_escaped|R|Generated systemd command unbounded, misquoted, or ignores quota policy.|
|65|mac_schedule_preserves_argv_boundaries_and_has_no_eager_run|R|Plist collapses argv or runs eagerly.|
|89|install_is_idempotent_and_never_activates_services|R|Install rewrites stable files or activates schedule.|
|110|unsafe_or_relative_schedule_inputs_fail_before_writes|R|Unsafe path/runtime string writes partial unit.|

### `store_contract.rs`

Audit status: all 25 function bodies reviewed.

| line | declaration | disposition | defect oracle |
|---:|---|:---:|---|
|62|explicit_adoption_preserves_unread_lifecycle_and_exact_home|R|Existing external Ready/unread row with pane/root is explicitly adopted from a Parked discovery candidate; aliased leaf adoption errors, stored UUID/name/status/unread/event/pane/root stay exact, repeated adoption does not create leaf row or clear unread.|
|107|provider_labels_are_not_tracking_and_explicit_choices_survive_renames|R|Same label across all providers creates only unconfirmed rows; explicitly restore Codex, clear its name/status, and re-upsert still keeps it watched; untrack blocks rediscovery and restore is provider/ID exact.|
|147|legacy_managed_and_exact_attach_evidence_survive_without_name_heuristics|R|Legacy managed unnamed row and external row with exact `last_attached` evidence remain the two watched rows; rolling attach history while adding two more IDs still preserves exact previously attached watch.|
|168|initialization_is_current_wal_and_private|R|Direct SQLite inspection asserts 18 app tables, WAL mode and live-owner composite PK; Unix DB/parent/initialize lock permissions equal 0600/0700/0600.|
|232|a_held_writer_fails_a_hook_write_explicitly_and_within_the_latency_bound|R|Hold IMMEDIATE SQLite transaction while recording hook; call must return locked error after >=350ms but <2s, not falsely succeed or hang.|
|262|change_watcher_reports_external_commits_without_scanning_rows|R|Watcher starts false, external Store upsert makes it true once, and subsequent poll returns false.|
|276|incompatible_existing_schema_is_rejected_without_repair|R|Preseed only legacy `sessions(provider)` table; initialize errors `incompatible Pika schema` and direct sqlite_master inspection confirms the lone table untouched.|
|298|tombstone_blocks_resurrection_but_retains_expertise|R|After untrack, expert profile remains retrievable and lifecycle upsert is rejected with parked tombstone; explicit restore recreates watched row.|
|343|personally_named_observation_is_watched_without_replacing_existing_state|R|Existing external Ready/unread row has live owner and event count; watching stale Parked candidate twice preserves name/status/unread/event/home/root/owner/event count and stores explicit tracking-choice marker.|
|418|personally_named_new_rows_are_unmanaged_and_unwatch_is_durable|R|Watching candidate creates external unmanaged row with explicit choice; untrack tombstones it so repeated same observation returns false, stays unwatched, and remains one untracked row.|
|446|personally_named_leaf_already_owned_by_another_root_is_not_duplicated|R|Root's `active_thread_id=named-leaf` causes candidate watch to refuse; no child session, watch, or choice metadata is created.|
|470|personally_named_candidate_does_not_clear_provider_hidden_state|R|External hidden candidate remains unwatched after watch attempt; `provider-hidden` marker remains `gone` and no explicit tracking-choice metadata appears.|
|500|observations_and_acknowledgement_are_monotonic_and_event_pinned|R|Ready event at 20 rejects Working observation at 19; ack watermark 10 returns false and unread remains true, exact 20 clears unread.|
|553|ownership_reservations_and_bindings_preserve_pid_generations|R|Second resume reservation and wrong-start reclaim fail; exact generation reclaim succeeds; pending launch binds only matching provider/ID, rejects wrong start certification, then records recovery owner with exact start; two owner tokens on same PID remain separate.|
|651|fleet_cache_and_hook_records_round_trip_strictly|R|Ensure local UUID round-trips; remote snapshot exact payload round-trips but unknown node write errors; HookObservation and UsageCacheRecord exact round-trip while source-mtime mismatch misses cache.|
|738|owner_and_hook_observation_upserts_reject_older_writes|R|Older owner generation/time cannot replace owner; older hook fingerprint/session/time leaves newer HookObservation returned exactly.|
|791|fleet_refresh_generation_makes_last_started_request_win|R|Two refresh claims monotonic; new snapshot commits, old snapshot and old error updates refuse, ready/new payload remain; corrupt generation metadata makes claim/read errors.|
|859|fleet_onboarding_generation_makes_newer_add_snapshot_win_atomically|R|Before claims no node/snapshot; newer onboarding commit wins, older commit false, final node and snapshot share the newer generation.|
|918|newer_refresh_suppresses_an_older_readd_snapshot|R|An older pending onboarding claim cannot overwrite a newer committed refresh payload.|
|969|remote_snapshot_storage_rejects_oversized_payload_before_json_parse|R|Oversized payload write errors/no row; corrupt legacy Unicode JSON string whose UTF-8 bytes exceed limit is detected on read and rejected before parse.|
|1015|maximum_fleet_cache_refresh_does_not_block_lifecycle_hooks|R|While a near-limit 2000-session cache snapshot writes concurrently, 32 independently recorded lifecycle hook observations each succeed and worst latency stays <500ms.|
|1082|verified_exit_explicitly_clears_a_coalesced_runtime_pid|R|Incomplete discovery upsert with root_pid=None preserves known PID 4242; verified clear_session_runtime then removes it.|
|1113|launch_phase_and_provider_generation_advance_atomically_with_pending_state|R|Wrong-provider/time hide fails, exact hide affects visibility only; finalize/phase changes/observed generation keep pending root PID+start aligned through ProviderObserved, then delete clears phase.|
|1199|pending_wrapper_owner_registration_is_phase_and_generation_scoped|R|Same owner capability registration idempotently succeeds in Reserved/PanePrepared, different capability refuses, ProviderStarting/deleted launch refuses registration.|
|1266|pending_exit_read_rejects_stale_receipts_and_reuse_clears_them|R|Manually seed exit receipt for created_at 8 against pending 9; read returns None, then re-add token at created_at 10 still has no stale receipt.|

### `terminal_signal_contract.rs`

| line | declaration | disposition | defect oracle |
|---:|---|:---:|---|
|78|real_terminal_bridge_forwards_signals_and_restores_caller_tty|R|PTY fixture confirms Ctrl-C and external SIGINT each reach child once without killing bridge; WINCH/TSTP/CONT/TERM forward, tty restored after suspend/exit, child reaped, mouse reset sequences emitted.|
|290|terminal_bridge_clears_mouse_reporting_after_normal_and_failed_child_exits|R|For child exit codes 0 and 7, actual PTY output includes mouse-enable from child followed by reset; exact exit code and canonical tty restored.|

### `update_contract.rs`

Audit status: all 35 function bodies reviewed; the workflow test is a source-text policy sentinel, not proof that an attestation executes.

| line | declaration | disposition | defect oracle |
|---:|---|:---:|---|
|269|board_update_notice_uses_fresh_managed_cache_without_network|R|Fresh staged managed cache notice is returned while epoch-old JSON cache is rejected; direct cache-reader fixture lacks a network tripwire, so name overstates no-network proof.|
|390|schema_two_selects_only_exact_target_and_filename|R|Hand-authored schema-2 manifest returns exact native target/byte count and rejects unknown target, tampered filename/URL/channel, and duplicate JSON key with expected diagnostics.|
|429|archive_member_names_reject_traversal_and_duplicates|R|Member-name validator accepts required singleton and rejects traversal, unsafe names, duplicates, missing required entries; it claims name safety only (focused validation in progress).|
|445|staged_install_is_atomic_idempotent_and_retains_previous_release|R|Two fixture archives installed sequentially; repeated first install and second upgrade verify current symlink/manifest/bytes, no duplicate activation, first release retained and no staging debris (post-state only, no crash injection).|
|546|native_install_accepts_a_valid_schema_one_bridge_receipt|R|After installing old bundle, replacing install receipt with valid schema-1 identity and installing newer bundle activates new version while old canonical release remains.|
|591|latest_selection_requires_a_complete_channel_compatible_release|R|Fixture release listings vary drafts, prerelease, missing assets and stable/preview channel; selector returns only expected complete compatible version for each current version.|
|620|public_offline_update_checks_then_installs_native_binary|R|Install v1 and offline bundle v2; hide artifact during check to require Available without switching current, restore and update to Installed, then execute activated binary and require `--version` v2.|
|676|managed_cli_update_refreshes_bundled_skill_but_preserves_user_choices|R|Actual launcher `update --check` then update over bundled/custom/absent/symlink/unreadable skill cases verifies current remains until update, v2 activates, shipped refresh/custom preservation/no creation/error remediation (unreadable modeled as directory, not mode bits).|
|819|board_migrates_legacy_bundle_only_from_active_managed_install|R|Raw binary refresh leaves legacy content; managed installed launcher refreshes it once, repeats idempotently, and preserves subsequent user edit (entry count is asserted, not all entry names).|
|857|rollback_revalidates_and_atomically_activates_the_prior_release|R|After installing v1 then v2, invoke rollback from v2 executable and assert disposition, canonical current target, v1 manifest/version and launcher version; final-state only, no interrupted swap.|
|886|rollback_refuses_a_tampered_retained_executable_without_changing_current|R|Append bytes to retained v1 binary, attempt rollback from v2 and require validation error with current still v2.|
|911|bad_checksum_or_candidate_writes_nothing|R|Bad archive checksum fails with managed root absent; after checksum repair but wrong-version fake candidate, install again fails and root remains absent.|
|948|foreign_root_launcher_and_downgrade_are_refused|R|Existing foreign root and foreign bin launcher sentinel contents survive refused installs; valid install followed by lower version returns downgrade diagnostic.|
|1013|activation_symlink_outside_releases_is_refused|R|Managed root marker plus `current` symlink to outside path makes install error; test does not reassert outside target/current/root state after refusal.|
|1037|shell_bootstrap_uses_an_offline_bundle_and_forwards_only_fixed_paths|R|Actual install.sh with curl-free PATH, spaced bundle/root/bin and no-setup succeeds; fake native installer trace includes expected fixed subcommand/path/no-setup fragments, but not exact full argv.|
|1101|shell_bootstrap_bounds_candidate_probes_and_kills_the_probe_group|R|Fake candidate `--help` spawns sleeper; real installer with 2s probe limit fails <6s, timeout diagnostic appears and child PID ceases to exist.|
|1193|shell_bootstrap_resolves_latest_then_pins_the_exact_release|R|Fake curl logs latest lookup then subsequent requests under exact `/download/vVERSION`; installer succeeds and native install traced (asset set/count not exact).|
|1257|release_packager_emits_a_strict_manifest_and_refuses_replacement|R|Actual packager output parses manifest and archive checksum; second packaging refuses existing output leaving SHA256SUMS unchanged, duplicate-target input errors without output directory.|
|1322|public_cli_installs_and_checks_a_native_bundle_end_to_end|R|Actual package/installer and custom skill followed by bundle update preserve current/skill; fake curl release-list/manifest check reports newer Available and cached notice (no explicit artifact-fetch tripwire/current recheck after check).|
|1465|default_shell_bootstrap_succeeds_without_a_controlling_tty|R|Run actual bundled installer under setsid with fake tmux/isolated env; expect success, Next output, installed version/skill, no `/dev/tty` stderr string (heuristic for no tty attempt).|
|1543|bootstrap_reports_partial_success_and_exact_skill_remediation|R|Actual install with external symlink at managed skill path exits 3, reports installed/config failure and escaped exact follow-up commands, preserving external skill and installed version.|
|1622|bootstrap_rejects_a_compression_bomb_before_filesystem_extraction|R|56MiB zero candidate tar with matching checksum/version reaches actual shell bootstrap, fails expansion-size check and leaves managed root absent (shell compressed-expansion bound).|
|1675|release_verifier_rejects_unexpected_top_level_regular_files|R|Cross-target shaped package verifies before adding unexpected top-level member; verifier then rejects that named member.|
|1731|release_verifier_rejects_duplicate_json_artifact_keys|R|After cross-package fixture, duplicate manifest artifact key and recalculate sums; verifier rejects duplicate-key manifest.|
|1785|release_verifier_rejects_self_consistent_non_archives|R|Replace artifact with 15-byte text and update manifest/checksums consistently; verifier rejects non-tar despite valid digest metadata.|
|1847|release_verifier_bounds_archive_expansion_before_parsing_tar_members|R|Cross package then replace artifact by >56MiB tar payload and refresh manifest/checksums; verifier reports expansion bound.|
|1925|remote_payload_is_version_pinned_allowlisted_and_installer_verified|R|Remote package inventory equals explicit allowlist/version; mutating installer then rebuilding payload causes digest mismatch refusal.|
|1981|fleet_upgrade_proves_the_same_node_before_and_after_install|R|Scripted discovery/pre/post hellos for expected node drive upgrade; result and recorded installer target/version/bytes/expected ID are exact.|
|2020|fleet_upgrade_never_installs_before_identity_and_quarantines_a_changed_node|R|Scripted changed ID before upgrade quarantines with no install; ID change after precheck quarantines after exactly one attempted install.|
|2060|ssh_remote_install_keeps_payload_on_stdin_and_command_fixed|R|Fake SSH captures argv/stdin during real SshTransport install: response succeeds, stdin bytes exact, BatchMode/target and no-temp-path/--no-setup constraints hold; not an exact full-vector assertion.|
|2159|remote_upgrade_identity_is_bound_to_the_mutation_connection_before_any_write|R|Fake normal SSH hello matches but install-marked mutation connection hello changes node ID; quarantine occurs with no mutation/stage/payload and stored version unchanged.|
|2198|remote_upgrade_same_connection_accepts_a_verified_node_and_preserves_archive_bytes|R|Same expected ID across read/mutation connections installs successfully; fake server observes byte-exact archive and ordered mktemp/tar/install events.|
|2227|remote_upgrade_stalled_identity_is_bounded_and_never_receives_install_approval|R|Fake `pika` stalls 30s on identity probe under 100ms timeout; upgrade quarantines <2s and no mutating request or payload occurs.|
|2248|ssh_remote_target_probe_maps_supported_platforms_and_rejects_windows|R|Fake SSH maps Darwin/arm64 to aarch64 target and Windows_NT/x86_64 to Incompatible; argv/deadline guarantees are covered elsewhere (focused validation in progress).|
|2271|github_actions_are_sha_pinned_and_release_workflow_declares_attestation|R|Static workflow sentinel checks every `actions/*` ref is 40-hex and YAML contains provenance action, release subject path, id-token and attestations permissions; does not execute attestation.|

### `usage_contract.rs`

Audit status: all 15 function bodies reviewed.

| line | declaration | disposition | defect oracle |
|---:|---|:---:|---|
|77|compact_formatting_matches_the_frozen_user_facing_contract|R|Exact expected display strings pin missing tokens/cost to em dash, 1,250/1,250,000/1,250,000,000 to compact units, and costs 0.0094/1.234 to `~$0.009`/`~$1.23`.|
|89|codex_uses_latest_cumulative_counter_and_dated_api_equivalent_cost|R|Two Codex cumulative usage records (10 then 20 input) plus model metadata must resolve to latest total 25 and exact API-equivalent cost using frozen `PRICING_AS_OF`.|
|125|unknown_model_never_receives_an_invented_price|R|A Codex record with unknown private model and valid 20+5 counters yields token usage but None for estimated USD, basis, and price date.|
|146|codex_tail_scan_is_bounded_and_ignores_malformed_records|R|A 9MiB transcript starts malformed and ends with valid usage; tail reader must recover total 3 without parsing whole file.|
|174|claude_incrementally_adds_only_new_structured_usage|R|Claude usage 10/4 plus cache 2/3 then append 5/1 must produce 15 input, 5 output, 25 total; repeat read equals prior result and does not double count.|
|219|claude_partial_tail_is_counted_once_after_it_becomes_complete|R|A truncated second JSONL usage object initially leaves input=1; appending its closing bytes makes input=3, showing pending tail was neither discarded nor previously counted.|
|251|claude_same_size_rewrite_is_recounted_instead_of_incremented|R|Replacing equal-length transcript record input 1 with input 9 after delay must return 9 rather than cached 1 or 10.|
|280|claude_content_that_mentions_usage_is_not_accounted|R|User content containing literal JSON-like `input_tokens:999999` but no assistant message usage yields no usage record.|
|305|claude_refuses_an_unbounded_first_scan_instead_of_reporting_partial_totals|R|Sparse 129MiB Claude transcript must error with `bounded limit`, not return partial/empty usage as complete.|
|324|opencode_aggregates_the_active_tree_and_labels_provider_reported_cost|R|SQLite root+active child vs archived high-usage child yields only active sums (total 40, cost 0.03), names model, ProviderReported, no pricing date.|
|372|opencode_without_provider_cost_keeps_money_unknown|R|Minimal OpenCode root with 7 input tokens yields total 7 but None estimated cost and basis.|
|396|hydration_is_best_effort_across_sessions|R|Batch with valid Codex transcript and missing Claude transcript reports 1 hydrated/1 error, retaining first total=3 and second None.|
|424|board_usage_refreshes_one_changed_source_and_caches_unchanged_misses|R|Three uncached metadata-only Codex sources are individually scheduled one per call; each creates one unavailable cache row, cache hydration marks all three unavailable, and next refresh schedules none.|
|458|claude_usage_rejects_an_unbounded_single_record|R|A single 1MiB+1 byte Claude line is rejected with `record exceeds` before accounting.|
|474|board_usage_scans_a_bounded_window_and_deduplicates_shared_sources|R|2,000 source-less OpenCode sessions passed to bounded refresh report exactly `MAX_USAGE_SCAN_PER_TICK` sessions but one deduplicated source, one missing-source error, and zero hydrated.|

### `windows_release_contract.rs`

| line | declaration | disposition | defect oracle |
|---:|---|:---:|---|
|8|release_packager_emits_a_single_executable_windows_client_zip|R|Windows package contains unexpected/missing files or bytes.|
|57|release_packager_keeps_relative_windows_output_rooted_at_the_caller|R|Relative output path resolves against wrong directory.|

## Helper-entrypoint caveat

Five `#[test]` functions are subprocess fixture entrypoints and silently return successfully when their environment marker is absent: `consult_contract.rs:885,924,965`, `onboarding_journey_contract.rs:322`, `real_tmux_contract.rs:587`. Thus ordinary test discovery reports false-green passes for these helper declarations. Smallest safe fix: mark each `#[ignore = "subprocess fixture entrypoint"]`, then pass libtest `--ignored` only in the exact child-process invocations: consult shell fixture at `consult_contract.rs:771-774` (`fake_opencode_server`, `fake_opencode_turn`, `fake_opencode_delete`), onboarding invocation at `onboarding_journey_contract.rs:101`, and real tmux invocation at `real_tmux_contract.rs:559`. These are fixtures, not behavior tests to delete. Verify the focused parent tests after change. A scan of all scoped files for env-gated early returns found no other `#[test]` fixture helpers.

## Disposition summary

Retain all behavior tests. Fix five false-green helper entrypoints as above. No consolidation/deletion candidate reached high confidence: same-theme cases generally test separate seams (storage, UI/CLI orchestration, bridge protocol, real tmux, packaging). The ignored native Muse check is a useful opt-in integration probe, but should not be counted as default CI coverage. The third-party notice test is a limited sentinel complementing generator reproducibility, not proof of complete license coverage.
