use pikamux::hooks::{
    HookContext, HookDisposition, MAX_HOOK_PAYLOAD_BYTES, certify_hook_home, enrich_hook_payload,
    event_state, handle_hook, handle_process_exit, hook_stdout, parse_hook_payload,
};
use pikamux::model::{ObservationKind, Provider, Session, Status, StatusObservation};
use pikamux::store::{LiveOwner, PendingLaunch, Store};
use std::io::Cursor;
use tempfile::TempDir;

fn store() -> (TempDir, Store) {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::at(temp.path().join("state/pika.db"));
    (temp, store)
}

fn parse(provider: Provider, value: serde_json::Value) -> pikamux::hooks::HookPayload {
    parse_hook_payload(Cursor::new(serde_json::to_vec(&value).unwrap()), provider).unwrap()
}

fn event(provider: Provider, id: &str, name: &str) -> pikamux::hooks::HookPayload {
    parse(
        provider,
        serde_json::json!({
            "session_id": id,
            "hook_event_name": name,
            "cwd": "/project",
        }),
    )
}

fn session(provider: Provider, id: &str, status: Status) -> Session {
    Session {
        provider,
        session_id: id.into(),
        name: Some("project".into()),
        cwd: Some("/project".into()),
        branch: None,
        transcript_path: None,
        tmux_session: None,
        tmux_pane: None,
        root_pid: None,
        status,
        unread: false,
        model: None,
        source: "managed".into(),
        managed: true,
        error: None,
        attention_reason: None,
        created_at: 1.0,
        updated_at: 1.0,
        last_event_at: 1.0,
        last_activity_at: 1.0,
        live: false,
        attached: false,
        home_state: "unknown".into(),
        cpu_percent: None,
        rss_kb: None,
        input_tokens: None,
        output_tokens: None,
        cached_input_tokens: None,
        cache_write_tokens: None,
        total_tokens: None,
        estimated_cost_usd: None,
        active_thread_id: None,
    }
}

#[test]
fn payload_validation_is_bounded_typed_and_provider_specific() {
    assert!(
        parse_hook_payload(
            Cursor::new(vec![b' '; MAX_HOOK_PAYLOAD_BYTES + 1]),
            Provider::Codex
        )
        .unwrap_err()
        .to_string()
        .contains("exceeds")
    );
    assert!(parse_hook_payload(Cursor::new(br#"[]"#), Provider::Codex).is_err());
    assert!(
        parse_hook_payload(
            Cursor::new(br#"{"session_id":7,"hook_event_name":"Stop"}"#),
            Provider::Codex
        )
        .is_err()
    );
    assert!(
        parse_hook_payload(
            Cursor::new(br#"{"session_id":"id","hook_event_name":"SessionHeartbeat"}"#),
            Provider::Codex
        )
        .is_err()
    );
    assert!(
        parse_hook_payload(
            Cursor::new(br#"{"session_id":"id","hook_event_name":"SessionHeartbeat"}"#),
            Provider::Opencode
        )
        .is_ok()
    );
}

#[test]
fn provider_events_map_to_structured_attention_without_transcript_text() {
    let mut question = event(Provider::Codex, "one", "PreToolUse");
    question.tool_name = Some("request_user_input".into());
    let mapped = event_state(Provider::Codex, &question);
    assert_eq!((mapped.status, mapped.unread), (Status::NeedsYou, true));
    assert_eq!(mapped.attention_reason.as_deref(), Some("question"));

    let mut notification = event(Provider::Claude, "two", "Notification");
    notification.notification_type = Some("permission_prompt".into());
    assert_eq!(
        event_state(Provider::Claude, &notification).status,
        Status::NeedsYou
    );

    let mut stop = event(Provider::Claude, "three", "Stop");
    stop.background_tasks = 1;
    assert_eq!(event_state(Provider::Claude, &stop).status, Status::Working);
    stop.background_tasks = 0;
    assert_eq!(event_state(Provider::Claude, &stop).status, Status::Ready);
}

#[test]
fn ephemeral_hook_has_no_durable_side_effect() {
    let (_temp, store) = store();
    let mut context = HookContext::at(10.0);
    context.ephemeral = true;
    let result = handle_hook(
        &store,
        Provider::Codex,
        &event(Provider::Codex, "ephemeral", "Stop"),
        &context,
    )
    .unwrap();
    assert_eq!(result.disposition, HookDisposition::Ignored);
    assert!(!store.exists());
}

#[test]
fn invalid_identity_context_fails_before_state_is_initialized() {
    let (_temp, store) = store();
    let mut context = HookContext::at(f64::NAN);
    context.launch_token = Some("bad\0token".into());
    assert!(
        handle_hook(
            &store,
            Provider::Codex,
            &event(Provider::Codex, "identity", "Stop"),
            &context,
        )
        .is_err()
    );
    assert!(!store.exists());
}

#[test]
fn tombstone_survives_worker_and_lifecycle_hooks() {
    let (_temp, store) = store();
    store
        .upsert_session(&session(Provider::Codex, "ignored", Status::Working), true)
        .unwrap();
    store.untrack_session(Provider::Codex, "ignored").unwrap();
    let mut worker = event(Provider::Codex, "ignored", "Stop");
    worker.originator = Some("automation_worker".into());
    let result = handle_hook(&store, Provider::Codex, &worker, &HookContext::at(11.0)).unwrap();
    assert_eq!(result.disposition, HookDisposition::Ignored);
    assert!(store.is_untracked(Provider::Codex, "ignored").unwrap());
    assert!(store.list_sessions().unwrap().is_empty());

    let result = handle_hook(
        &store,
        Provider::Codex,
        &event(Provider::Codex, "ignored", "SessionStart"),
        &HookContext::at(12.0),
    )
    .unwrap();
    assert_eq!(result.disposition, HookDisposition::Ignored);
    assert!(store.is_untracked(Provider::Codex, "ignored").unwrap());
}

#[test]
fn proven_workers_and_inherited_cross_provider_children_never_enter_inventory() {
    let (_temp, store) = store();
    let mut worker = event(Provider::Codex, "worker", "Stop");
    worker.thread_source = Some("subagent".into());
    let result = handle_hook(&store, Provider::Codex, &worker, &HookContext::at(10.0)).unwrap();
    assert_eq!(result.disposition, HookDisposition::Ignored);
    assert!(store.list_sessions().unwrap().is_empty());

    let mut inherited = HookContext::at(11.0);
    inherited.expected_provider = Some(Provider::Claude);
    let result = handle_hook(
        &store,
        Provider::Codex,
        &event(Provider::Codex, "child", "SessionStart"),
        &inherited,
    )
    .unwrap();
    assert_eq!(result.disposition, HookDisposition::Ignored);
    assert!(
        store
            .get_hook_observation(Provider::Codex)
            .unwrap()
            .is_none()
    );
}

#[test]
fn bounded_immutable_metadata_corrects_codex_identity_and_proves_workers() {
    let temp = tempfile::tempdir().unwrap();
    let transcript = temp.path().join("rollout.jsonl");
    std::fs::write(
        &transcript,
        "{\"type\":\"session_meta\",\"payload\":{\"id\":\"canonical\",\"source\":{\"subagent\":{}},\"originator\":\"Codex Desktop\"}}\nprivate conversation text",
    )
    .unwrap();
    let mut payload = event(Provider::Codex, "reported", "Stop");
    payload.transcript_path = Some(transcript.to_string_lossy().into_owned());
    let enriched = enrich_hook_payload(Provider::Codex, &payload);
    assert_eq!(enriched.session_id, "canonical");
    assert_eq!(enriched.thread_source.as_deref(), Some("subagent"));

    let store = Store::at(temp.path().join("pika.db"));
    let result = handle_hook(&store, Provider::Codex, &payload, &HookContext::at(10.0)).unwrap();
    assert_eq!(result.disposition, HookDisposition::Ignored);
    assert!(store.list_sessions().unwrap().is_empty());
}

#[test]
fn newest_lifecycle_wins_and_safety_remains_stronger() {
    let (_temp, store) = store();
    let mut context = HookContext::at(20.0);
    context.desired_name = Some("research".into());
    let completed = handle_hook(
        &store,
        Provider::Codex,
        &event(Provider::Codex, "exact", "Stop"),
        &context,
    )
    .unwrap();
    assert_eq!(completed.alert.as_ref().unwrap().status, Status::Ready);
    context.now = 10.0;
    handle_hook(
        &store,
        Provider::Codex,
        &event(Provider::Codex, "exact", "UserPromptSubmit"),
        &context,
    )
    .unwrap();
    let current = store
        .get_session(Provider::Codex, "exact")
        .unwrap()
        .unwrap();
    assert_eq!((current.status, current.unread), (Status::Ready, true));
    assert_eq!(current.last_event_at, 20.0);

    store
        .record_status_observation(
            Provider::Codex,
            "exact",
            &StatusObservation {
                kind: ObservationKind::Safety,
                status: Status::OpenTwice,
                unread: true,
                attention_reason: Some("identity".into()),
                error: Some("two exact owners".into()),
                observed_at: 30.0,
                source: "test".into(),
            },
        )
        .unwrap();
    context.now = 40.0;
    handle_hook(
        &store,
        Provider::Codex,
        &event(Provider::Codex, "exact", "UserPromptSubmit"),
        &context,
    )
    .unwrap();
    let current = store
        .get_session(Provider::Codex, "exact")
        .unwrap()
        .unwrap();
    assert_eq!(current.status, Status::OpenTwice);
    assert_eq!(current.attention_reason.as_deref(), Some("identity"));
}

#[test]
fn session_end_preserves_an_unread_completion_and_its_event_time() {
    let (_temp, store) = store();
    let mut context = HookContext::at(10.0);
    context.desired_name = Some("work".into());
    handle_hook(
        &store,
        Provider::Codex,
        &event(Provider::Codex, "result", "Stop"),
        &context,
    )
    .unwrap();
    context.now = 20.0;
    handle_hook(
        &store,
        Provider::Codex,
        &event(Provider::Codex, "result", "SessionEnd"),
        &context,
    )
    .unwrap();
    let result = store
        .get_session(Provider::Codex, "result")
        .unwrap()
        .unwrap();
    assert_eq!((result.status, result.unread), (Status::Ready, true));
    assert_eq!(result.last_event_at, 10.0);
}

#[test]
fn unnamed_external_hook_records_only_an_exact_owner_lease() {
    let (_temp, store) = store();
    let mut context = HookContext::at(10.0);
    context.owner_pid = Some(42);
    context.owner_start_time = Some(7);
    context.owner_token = "client".into();
    let result = handle_hook(
        &store,
        Provider::Codex,
        &event(Provider::Codex, "hidden", "PostToolUse"),
        &context,
    )
    .unwrap();
    assert_eq!(result.disposition, HookDisposition::OwnerOnly);
    assert!(
        store
            .get_session(Provider::Codex, "hidden")
            .unwrap()
            .is_none()
    );
    assert_eq!(
        store.live_owners(Provider::Codex, "hidden").unwrap()[0].pid,
        42
    );
}

#[test]
fn wrong_launch_identity_fails_closed_and_exact_home_can_certify() {
    let (_temp, store) = store();
    store.initialize().unwrap();
    store
        .add_pending(&PendingLaunch {
            launch_token: "launch".into(),
            provider: Provider::Codex,
            name: "project".into(),
            cwd: "/project".into(),
            tmux_session: Some("pika-c-test".into()),
            tmux_pane: Some("%1".into()),
            expected_session_id: Some("wanted".into()),
            root_pid: Some(99),
            root_pid_start: Some(123),
            preexisting_session_ids: Some(Vec::new()),
            candidate_session_id: None,
            candidate_observed_at: None,
            created_at: 1.0,
        })
        .unwrap();
    let mut context = HookContext::at(10.0);
    context.launch_token = Some("launch".into());
    context.pane_id = Some("%1".into());
    context.pane_session = Some("pika-c-test".into());
    context.owner_pid = Some(99);
    context.owner_start_time = Some(123);
    let rejected = handle_hook(
        &store,
        Provider::Codex,
        &event(Provider::Codex, "wrong", "SessionStart"),
        &context,
    )
    .unwrap();
    assert_eq!(rejected.disposition, HookDisposition::Ignored);
    assert!(store.get_launch_binding("launch").unwrap().is_none());
    assert!(store.get_pending("launch").unwrap().is_some());
    assert!(
        store
            .get_meta("launch_binding_error:launch")
            .unwrap()
            .unwrap()
            .contains("expected session")
    );

    let accepted = handle_hook(
        &store,
        Provider::Codex,
        &event(Provider::Codex, "wanted", "SessionStart"),
        &context,
    )
    .unwrap();
    assert!(!accepted.launch_certified);
    assert_eq!(
        store.get_launch_binding("launch").unwrap(),
        Some((Provider::Codex, "wanted".into()))
    );
    assert!(store.get_pending("launch").unwrap().is_some());
    assert!(
        certify_hook_home(
            &store,
            "launch",
            accepted.tag_request.as_ref().unwrap(),
            99,
            123,
        )
        .unwrap()
    );
    assert!(store.get_pending("launch").unwrap().is_none());
    let owner = store
        .get_recovery_owner(Provider::Codex, "wanted")
        .unwrap()
        .unwrap();
    assert_eq!((owner.pid, owner.start_time), (99, 123));
}

#[test]
fn attached_completion_is_read_but_permission_remains_unread() {
    let (_temp, store) = store();
    let mut context = HookContext::at(10.0);
    context.pane_attached = true;
    context.desired_name = Some("visible".into());
    handle_hook(
        &store,
        Provider::Codex,
        &event(Provider::Codex, "attached", "Stop"),
        &context,
    )
    .unwrap();
    assert!(
        !store
            .get_session(Provider::Codex, "attached")
            .unwrap()
            .unwrap()
            .unread
    );
    context.now = 20.0;
    handle_hook(
        &store,
        Provider::Codex,
        &event(Provider::Codex, "attached", "PermissionRequest"),
        &context,
    )
    .unwrap();
    assert!(
        store
            .get_session(Provider::Codex, "attached")
            .unwrap()
            .unwrap()
            .unread
    );
}

#[test]
fn claude_title_output_and_codex_noop_stdout_are_protocol_safe() {
    let (_temp, store) = store();
    let mut context = HookContext::at(10.0);
    context.expected_session_id = Some("claude-id".into());
    context.desired_name = Some("named".into());
    let result = handle_hook(
        &store,
        Provider::Claude,
        &event(Provider::Claude, "claude-id", "SessionStart"),
        &context,
    )
    .unwrap();
    assert_eq!(
        result.provider_output.as_ref().unwrap()["hookSpecificOutput"]["sessionTitle"],
        "named"
    );
    assert!(hook_stdout(Provider::Claude, &result).contains("sessionTitle"));
    let mut codex_context = HookContext::at(11.0);
    codex_context.desired_name = Some("codex".into());
    let codex = handle_hook(
        &store,
        Provider::Codex,
        &event(Provider::Codex, "codex-id", "SessionStart"),
        &codex_context,
    )
    .unwrap();
    assert_eq!(hook_stdout(Provider::Codex, &codex), "{}");
    assert_eq!(codex.native_name_request.as_ref().unwrap().name, "codex");
}

#[test]
fn opencode_delete_removes_only_the_exact_non_tombstoned_root() {
    let (_temp, store) = store();
    store
        .upsert_session(&session(Provider::Opencode, "root", Status::Working), true)
        .unwrap();
    store
        .set_live_owner(&LiveOwner {
            provider: Provider::Opencode,
            session_id: "root".into(),
            pid: 42,
            start_time: Some(7),
            owner_token: "client".into(),
            last_seen: 1.0,
        })
        .unwrap();
    let mut deleted = event(Provider::Opencode, "root", "SessionEnd");
    deleted.deleted = true;
    let result = handle_hook(&store, Provider::Opencode, &deleted, &HookContext::at(10.0)).unwrap();
    assert_eq!(result.disposition, HookDisposition::Deleted);
    assert!(
        store
            .get_session(Provider::Opencode, "root")
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .live_owners(Provider::Opencode, "root")
            .unwrap()
            .is_empty()
    );
}

#[test]
fn process_exit_uses_binding_and_nonzero_status_becomes_actionable() {
    let (_temp, store) = store();
    store
        .upsert_session(&session(Provider::Codex, "exact", Status::Working), true)
        .unwrap();
    store
        .bind_launch("token", Provider::Codex, "exact")
        .unwrap();
    store
        .set_live_owner(&LiveOwner {
            provider: Provider::Codex,
            session_id: "exact".into(),
            pid: 41,
            start_time: Some(10),
            owner_token: "client".into(),
            last_seen: 10.0,
        })
        .unwrap();
    assert!(
        handle_process_exit(
            &store,
            Provider::Codex,
            9,
            Some("stale"),
            Some("token"),
            Some("client"),
            20.0,
        )
        .unwrap()
    );
    let result = store
        .get_session(Provider::Codex, "exact")
        .unwrap()
        .unwrap();
    assert_eq!((result.status, result.unread), (Status::Error, true));
    assert_eq!(result.attention_reason.as_deref(), Some("exited"));
    assert!(store.get_launch_binding("token").unwrap().is_none());
}

#[test]
fn stale_wrapper_cannot_demote_or_unbind_a_replacement_owner() {
    let (_temp, store) = store();
    let mut current = session(Provider::Codex, "replacement", Status::Working);
    current.root_pid = Some(42);
    current.live = true;
    store.upsert_session(&current, true).unwrap();
    store
        .bind_launch("new-launch", Provider::Codex, "replacement")
        .unwrap();
    store
        .set_live_owner(&LiveOwner {
            provider: Provider::Codex,
            session_id: "replacement".into(),
            pid: 42,
            start_time: Some(20),
            owner_token: "new-owner".into(),
            last_seen: 20.0,
        })
        .unwrap();

    assert!(
        !handle_process_exit(
            &store,
            Provider::Codex,
            0,
            Some("replacement"),
            Some("new-launch"),
            Some("old-owner"),
            30.0,
        )
        .unwrap()
    );
    let result = store
        .get_session(Provider::Codex, "replacement")
        .unwrap()
        .unwrap();
    assert_eq!(result.status, Status::Working);
    assert_eq!(result.root_pid, Some(42));
    assert!(store.get_launch_binding("new-launch").unwrap().is_some());
    assert_eq!(
        store
            .live_owners(Provider::Codex, "replacement")
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn one_duplicate_exit_preserves_the_other_generation_and_safety_state() {
    let (_temp, store) = store();
    let mut current = session(Provider::Codex, "duplicate-live", Status::OpenTwice);
    current.root_pid = Some(51);
    current.live = true;
    current.unread = true;
    current.attention_reason = Some("identity".into());
    store.upsert_session(&current, true).unwrap();
    for (pid, start_time, token) in [(51, 10, "leaving"), (52, 20, "remaining")] {
        store
            .set_live_owner(&LiveOwner {
                provider: Provider::Codex,
                session_id: "duplicate-live".into(),
                pid,
                start_time: Some(start_time),
                owner_token: token.into(),
                last_seen: 20.0,
            })
            .unwrap();
    }
    assert!(
        handle_process_exit(
            &store,
            Provider::Codex,
            0,
            Some("duplicate-live"),
            None,
            Some("leaving"),
            30.0,
        )
        .unwrap()
    );
    let result = store
        .get_session(Provider::Codex, "duplicate-live")
        .unwrap()
        .unwrap();
    assert_eq!(result.status, Status::OpenTwice);
    assert_eq!(result.root_pid, Some(51));
    let owners = store
        .live_owners(Provider::Codex, "duplicate-live")
        .unwrap();
    assert_eq!(owners.len(), 1);
    assert_eq!((owners[0].pid, owners[0].start_time), (52, Some(20)));
}

#[test]
fn pid_reuse_does_not_let_an_old_token_clear_the_new_generation() {
    let (_temp, store) = store();
    let mut current = session(Provider::Codex, "pid-reuse", Status::Working);
    current.root_pid = Some(61);
    current.live = true;
    store.upsert_session(&current, true).unwrap();
    for (start_time, token) in [(10, "old"), (20, "new")] {
        store
            .set_live_owner(&LiveOwner {
                provider: Provider::Codex,
                session_id: "pid-reuse".into(),
                pid: 61,
                start_time: Some(start_time),
                owner_token: token.into(),
                last_seen: 20.0,
            })
            .unwrap();
    }
    assert!(
        handle_process_exit(
            &store,
            Provider::Codex,
            0,
            Some("pid-reuse"),
            None,
            Some("old"),
            30.0,
        )
        .unwrap()
    );
    let result = store
        .get_session(Provider::Codex, "pid-reuse")
        .unwrap()
        .unwrap();
    assert_eq!(
        (result.status, result.root_pid),
        (Status::Working, Some(61))
    );
    let owners = store.live_owners(Provider::Codex, "pid-reuse").unwrap();
    assert_eq!(owners.len(), 1);
    assert_eq!(
        (owners[0].owner_token.as_str(), owners[0].start_time),
        ("new", Some(20))
    );
}

#[test]
fn process_exit_cannot_demote_a_stronger_identity_failure() {
    let (_temp, store) = store();
    let mut current = session(Provider::Codex, "duplicate", Status::OpenTwice);
    current.unread = true;
    current.attention_reason = Some("identity".into());
    current.error = Some("two exact owners".into());
    store.upsert_session(&current, true).unwrap();
    handle_process_exit(
        &store,
        Provider::Codex,
        9,
        Some("duplicate"),
        None,
        None,
        20.0,
    )
    .unwrap();
    let result = store
        .get_session(Provider::Codex, "duplicate")
        .unwrap()
        .unwrap();
    assert_eq!(result.status, Status::OpenTwice);
    assert_eq!(result.attention_reason.as_deref(), Some("identity"));
}
