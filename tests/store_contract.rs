use pikamux::model::{
    ExpertProfile, FleetNode, ObservationKind, Provider, Session, Status, StatusObservation,
};
use pikamux::store::{
    HookObservation, LaunchPhase, LiveOwner, MAX_REMOTE_SNAPSHOT_BYTES, PendingLaunch, Store,
    StoredExpertProfile, UsageCacheRecord,
};
use rusqlite::Connection;
use serde_json::json;
use std::time::{Duration, Instant};
use tempfile::tempdir;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

fn session(id: &str, status: Status, unread: bool, event_at: f64) -> Session {
    Session {
        provider: Provider::Codex,
        session_id: id.into(),
        active_thread_id: Some(format!("leaf-{id}")),
        name: Some(format!("name-{id}")),
        cwd: Some("/work/project".into()),
        branch: Some("main".into()),
        transcript_path: Some(format!("/transcripts/{id}.jsonl")),
        tmux_session: Some("pika-test".into()),
        tmux_pane: Some("%1".into()),
        root_pid: Some(42),
        status,
        unread,
        model: Some("test-model".into()),
        source: "managed".into(),
        managed: true,
        error: None,
        attention_reason: None,
        created_at: 1.0,
        updated_at: event_at,
        last_event_at: event_at,
        last_activity_at: event_at,
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
    }
}

fn store_fixture() -> (tempfile::TempDir, Store) {
    let temp = tempdir().unwrap();
    let store = Store::at(temp.path().join("state/pika.db"));
    (temp, store)
}

#[test]
fn initialization_is_current_wal_and_private() {
    let (_temp, store) = store_fixture();
    store.initialize().unwrap();

    let db = Connection::open(store.path()).unwrap();
    let table_count: i64 = db
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let journal: String = db
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .unwrap();
    assert_eq!(table_count, 18);
    assert_eq!(journal, "wal");

    let owner_pk: Vec<String> = db
        .prepare("PRAGMA table_info(live_owners)")
        .unwrap()
        .query_map([], |row| {
            Ok((row.get::<_, String>(1)?, row.get::<_, i64>(5)?))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
        .into_iter()
        .filter(|(_, position)| *position > 0)
        .map(|(name, _)| name)
        .collect();
    assert_eq!(owner_pk, ["provider", "session_id", "pid", "owner_token"]);

    #[cfg(unix)]
    {
        assert_eq!(
            std::fs::metadata(store.path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(store.path().parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        let lock = store
            .path()
            .parent()
            .unwrap()
            .join(".pika.db.initialize.lock");
        assert_eq!(
            std::fs::metadata(lock).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[test]
fn a_held_writer_fails_a_hook_write_explicitly_and_within_the_latency_bound() {
    let (_temp, store) = store_fixture();
    store.initialize().unwrap();
    let mut blocker = Connection::open(store.path()).unwrap();
    let transaction = blocker
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .unwrap();
    let started = Instant::now();
    let error = store
        .record_hook_observation(&HookObservation {
            provider: Provider::Codex,
            fingerprint: "bounded-lock".into(),
            event_name: "stop".into(),
            session_id: "11111111-1111-4111-8111-111111111111".into(),
            observed_at: 1.0,
            source: Some("fixture".into()),
            managed: true,
        })
        .unwrap_err();
    let elapsed = started.elapsed();
    assert!(elapsed >= Duration::from_millis(350), "{elapsed:?}");
    assert!(elapsed < Duration::from_secs(2), "{elapsed:?}");
    assert!(
        error.to_string().to_lowercase().contains("locked"),
        "{error:#}"
    );
    drop(transaction);
}

#[test]
fn change_watcher_reports_external_commits_without_scanning_rows() {
    let (_temp, store) = store_fixture();
    store.initialize().unwrap();
    let mut watcher = store.change_watcher().unwrap();
    assert!(!watcher.changed().unwrap());

    store
        .upsert_session(&session("changed", Status::Working, false, 2.0), false)
        .unwrap();
    assert!(watcher.changed().unwrap());
    assert!(!watcher.changed().unwrap());
}

#[test]
fn incompatible_existing_schema_is_rejected_without_repair() {
    let (_temp, store) = store_fixture();
    std::fs::create_dir_all(store.path().parent().unwrap()).unwrap();
    let db = Connection::open(store.path()).unwrap();
    db.execute("CREATE TABLE sessions(provider TEXT)", [])
        .unwrap();
    drop(db);

    let error = store.initialize().unwrap_err().to_string();
    assert!(error.contains("incompatible Pika schema"), "{error}");
    let db = Connection::open(store.path()).unwrap();
    let tables: Vec<String> = db
        .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(tables, ["sessions"]);
}

#[test]
fn tombstone_blocks_resurrection_but_retains_expertise() {
    let (_temp, store) = store_fixture();
    let original = session("one", Status::Working, false, 10.0);
    assert!(store.upsert_session(&original, false).unwrap());
    let card = StoredExpertProfile {
        profile: ExpertProfile {
            provider: Provider::Codex,
            session_id: "one".into(),
            summary: "Knows the signal pipeline".into(),
            current_state: "Backfilling".into(),
            topics: vec!["signals".into()],
            artifacts: vec!["README.md".into()],
            source: "interview".into(),
            updated_at: 11.0,
            scope_updated_at: 0.0,
            current_state_updated_at: 0.0,
        },
        transcript_mtime_ns: Some(100),
        transcript_size: Some(200),
        current_state_mtime_ns: None,
        current_state_size: None,
    };
    store.put_expert_profile(&card).unwrap();

    store.untrack_session(Provider::Codex, "one").unwrap();
    assert!(store.list_sessions().unwrap().is_empty());
    assert_eq!(store.list_untracked_sessions().unwrap().len(), 1);
    assert!(
        store
            .get_stored_expert_profile(Provider::Codex, "one")
            .unwrap()
            .is_some()
    );
    let renamed = session("one", Status::Ready, true, 20.0);
    assert!(!store.upsert_session(&renamed, false).unwrap());
    assert_eq!(
        store.list_untracked_sessions().unwrap()[0].status,
        Status::Parked
    );

    assert!(store.restore_tracking(Provider::Codex, "one").unwrap());
    assert_eq!(store.list_sessions().unwrap().len(), 1);
}

#[test]
fn observations_and_acknowledgement_are_monotonic_and_event_pinned() {
    let (_temp, store) = store_fixture();
    let ready = session("ready", Status::Ready, true, 10.0);
    store.upsert_session(&ready, false).unwrap();
    let newer = StatusObservation {
        kind: ObservationKind::Lifecycle,
        status: Status::Ready,
        unread: true,
        attention_reason: Some("result".into()),
        error: None,
        observed_at: 20.0,
        source: "hook".into(),
    };
    assert!(
        store
            .record_status_observation(Provider::Codex, "ready", &newer)
            .unwrap()
    );
    let stale = StatusObservation {
        observed_at: 19.0,
        status: Status::Working,
        unread: false,
        ..newer.clone()
    };
    assert!(
        !store
            .record_status_observation(Provider::Codex, "ready", &stale)
            .unwrap()
    );

    let later = session("ready", Status::Ready, true, 20.0);
    store.upsert_session(&later, false).unwrap();
    assert!(
        !store
            .acknowledge_attention(Provider::Codex, "ready", 10.0, false)
            .unwrap()
    );
    assert!(
        store
            .get_session(Provider::Codex, "ready")
            .unwrap()
            .unwrap()
            .unread
    );
    assert!(
        store
            .acknowledge_attention(Provider::Codex, "ready", 20.0, false)
            .unwrap()
    );
    assert!(!store.status_observations(Provider::Codex, "ready").unwrap()[0].unread);
}

#[test]
fn ownership_reservations_and_bindings_preserve_pid_generations() {
    let (_temp, store) = store_fixture();
    assert!(
        store
            .reserve_resume(Provider::Claude, "thread", "a", 100, 1000)
            .unwrap()
    );
    assert!(
        !store
            .reserve_resume(Provider::Claude, "thread", "b", 100, 1001)
            .unwrap()
    );
    assert!(
        !store
            .reclaim_resume(Provider::Claude, "thread", 100, 999, "b", 200, 2000)
            .unwrap()
    );
    assert_eq!(
        store
            .get_resume_reservation(Provider::Claude, "thread")
            .unwrap()
            .unwrap()
            .owner_start_time,
        Some(1000)
    );
    assert!(
        store
            .reclaim_resume(Provider::Claude, "thread", 100, 1000, "b", 200, 2000)
            .unwrap()
    );

    let pending = PendingLaunch {
        launch_token: "launch".into(),
        provider: Provider::Claude,
        name: "thread".into(),
        cwd: "/work".into(),
        tmux_session: Some("pika".into()),
        tmux_pane: Some("%2".into()),
        expected_session_id: None,
        root_pid: Some(200),
        root_pid_start: Some(2000),
        preexisting_session_ids: Some(vec!["old".into()]),
        candidate_session_id: None,
        candidate_observed_at: None,
        created_at: 1.0,
    };
    assert!(store.add_pending(&pending).unwrap());
    assert!(
        store
            .bind_launch("launch", Provider::Claude, "thread")
            .unwrap()
    );
    assert!(
        !store
            .bind_launch("launch", Provider::Claude, "other")
            .unwrap()
    );
    assert!(
        !store
            .certify_launch("launch", Provider::Claude, "thread", 200, 2001)
            .unwrap()
    );
    assert!(
        store
            .certify_launch("launch", Provider::Claude, "thread", 200, 2000)
            .unwrap()
    );
    assert!(store.get_pending("launch").unwrap().is_none());
    assert_eq!(
        store
            .get_recovery_owner(Provider::Claude, "thread")
            .unwrap()
            .unwrap()
            .start_time,
        2000
    );

    for token in ["client-a", "client-b"] {
        assert!(
            store
                .set_live_owner(&LiveOwner {
                    provider: Provider::Claude,
                    session_id: "thread".into(),
                    pid: 500,
                    start_time: Some(5000),
                    owner_token: token.into(),
                    last_seen: 50.0,
                })
                .unwrap()
        );
    }
    assert_eq!(
        store.live_owners(Provider::Claude, "thread").unwrap().len(),
        2
    );
}

#[test]
fn fleet_cache_and_hook_records_round_trip_strictly() {
    let (_temp, store) = store_fixture();
    let local = store.ensure_local_node_id().unwrap();
    assert_eq!(
        store.local_node_id().unwrap().as_deref(),
        Some(local.as_str())
    );

    let node = FleetNode {
        node_id: "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa".into(),
        alias: "research-node".into(),
        ssh_target: "research-node.example".into(),
        sources: vec!["ssh-config".into()],
        status: "unknown".into(),
        protocol_version: Some(1),
        package_version: Some("0.6".into()),
        capabilities: vec!["snapshot".into()],
        last_seen: 0.0,
        last_attempt_at: 0.0,
        last_error: None,
        created_at: 1.0,
        updated_at: 1.0,
    };
    store.upsert_fleet_node(&node).unwrap();
    store
        .put_remote_snapshot(&node.node_id, &json!({"sessions": []}), 12.0)
        .unwrap();
    assert_eq!(
        store
            .get_remote_snapshot(&node.node_id)
            .unwrap()
            .unwrap()
            .payload,
        json!({"sessions": []})
    );
    assert!(
        store
            .put_remote_snapshot("bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb", &json!({}), 1.0)
            .is_err()
    );

    let hook = HookObservation {
        provider: Provider::Opencode,
        fingerprint: "fingerprint".into(),
        event_name: "idle".into(),
        session_id: "oc".into(),
        observed_at: 22.0,
        source: Some("hook".into()),
        managed: true,
    };
    store.record_hook_observation(&hook).unwrap();
    assert_eq!(
        store.get_hook_observation(Provider::Opencode).unwrap(),
        Some(hook)
    );

    let usage = UsageCacheRecord {
        provider: Provider::Codex,
        session_id: "one".into(),
        source_path: "/transcript".into(),
        source_mtime_ns: 123,
        source_size: 456,
        model: Some("model".into()),
        input_tokens: 1,
        output_tokens: 2,
        cached_input_tokens: 3,
        cache_write_tokens: 4,
        total_tokens: 10,
        estimated_cost_usd: Some(0.01),
        updated_at: 30.0,
    };
    store.put_cached_usage(&usage).unwrap();
    assert_eq!(
        store
            .get_cached_usage(Provider::Codex, "one", "/transcript", 123, 456)
            .unwrap(),
        Some(usage)
    );
    assert!(
        store
            .get_cached_usage(Provider::Codex, "one", "/transcript", 124, 456)
            .unwrap()
            .is_none()
    );
}

#[test]
fn fleet_refresh_generation_makes_last_started_request_win() {
    let (_temp, store) = store_fixture();
    let node = FleetNode {
        node_id: "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa".into(),
        alias: "atlas".into(),
        ssh_target: "atlas".into(),
        sources: vec!["fixture".into()],
        status: "unknown".into(),
        protocol_version: Some(2),
        package_version: Some("0.6".into()),
        capabilities: vec!["snapshot".into()],
        last_seen: 0.0,
        last_attempt_at: 0.0,
        last_error: None,
        created_at: 1.0,
        updated_at: 1.0,
    };
    store.upsert_fleet_node(&node).unwrap();
    let older = store.claim_fleet_refresh(&node.node_id).unwrap();
    let newer = store.claim_fleet_refresh(&node.node_id).unwrap();
    assert!(newer > older);
    assert!(
        store
            .put_remote_snapshot_if_current(
                &node.node_id,
                &json!({"generation":"newer"}),
                20.0,
                newer,
            )
            .unwrap()
    );
    assert!(
        !store
            .put_remote_snapshot_if_current(
                &node.node_id,
                &json!({"generation":"older"}),
                30.0,
                older,
            )
            .unwrap()
    );
    assert!(
        !store
            .mark_fleet_node_error_if_current(&node.node_id, older, "unreachable", "late failure",)
            .unwrap()
    );
    let stored = store.get_remote_snapshot(&node.node_id).unwrap().unwrap();
    assert_eq!(stored.payload["generation"], "newer");
    assert_eq!(
        store.get_fleet_node(&node.node_id).unwrap().unwrap().status,
        "ready"
    );

    store
        .set_meta(
            &format!("fleet:refresh-generation:{}", node.node_id),
            "not-a-generation",
        )
        .unwrap();
    assert!(store.claim_fleet_refresh(&node.node_id).is_err());
    assert!(
        store
            .current_fleet_refresh_generation(&node.node_id)
            .is_err()
    );
}

#[test]
fn remote_snapshot_storage_rejects_oversized_payload_before_json_parse() {
    let (_temp, store) = store_fixture();
    let node = FleetNode {
        node_id: "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa".into(),
        alias: "atlas".into(),
        ssh_target: "atlas".into(),
        sources: vec![],
        status: "ready".into(),
        protocol_version: Some(2),
        package_version: None,
        capabilities: vec![],
        last_seen: 0.0,
        last_attempt_at: 0.0,
        last_error: None,
        created_at: 1.0,
        updated_at: 1.0,
    };
    store.upsert_fleet_node(&node).unwrap();
    let payload = json!({"padding":"x".repeat(MAX_REMOTE_SNAPSHOT_BYTES)});
    assert!(
        store
            .put_remote_snapshot(&node.node_id, &payload, 1.0)
            .is_err()
    );
    assert!(store.get_remote_snapshot(&node.node_id).unwrap().is_none());

    // SQLite length(TEXT) counts Unicode code points, not encoded bytes. Seed a
    // legacy/corrupt row directly to prove the read-side guard measures the
    // UTF-8 blob before rusqlite allocates and parses the JSON string.
    let oversized_unicode = format!(
        "{{\"padding\":\"{}\"}}",
        "💡".repeat(MAX_REMOTE_SNAPSHOT_BYTES / 4 + 1)
    );
    assert!(oversized_unicode.len() > MAX_REMOTE_SNAPSHOT_BYTES);
    let db = Connection::open(store.path()).unwrap();
    db.execute(
        "INSERT INTO remote_snapshots(node_id,payload_json,captured_at) VALUES (?,?,?)",
        (&node.node_id, &oversized_unicode, 1.0),
    )
    .unwrap();
    assert!(store.get_remote_snapshot(&node.node_id).is_err());
}

#[test]
fn verified_exit_explicitly_clears_a_coalesced_runtime_pid() {
    let (_temp, store) = store_fixture();
    let mut value = session("exact-runtime", Status::Working, false, 40.0);
    value.root_pid = Some(4242);
    store.upsert_session(&value, false).unwrap();

    value.root_pid = None;
    store.upsert_session(&value, true).unwrap();
    assert_eq!(
        store
            .get_session(Provider::Codex, "exact-runtime")
            .unwrap()
            .unwrap()
            .root_pid,
        Some(4242),
        "an incomplete discovery read cannot erase process ownership"
    );

    assert!(
        store
            .clear_session_runtime(Provider::Codex, "exact-runtime", 55.0)
            .unwrap()
    );
    let cleared = store
        .get_session(Provider::Codex, "exact-runtime")
        .unwrap()
        .unwrap();
    assert_eq!(cleared.root_pid, None);
}

#[test]
fn launch_phase_and_provider_generation_advance_atomically_with_pending_state() {
    let (_temp, store) = store_fixture();
    let pending = PendingLaunch {
        launch_token: "phased-launch".into(),
        provider: Provider::Claude,
        name: "phased".into(),
        cwd: "/tmp".into(),
        tmux_session: None,
        tmux_pane: None,
        expected_session_id: Some("thread".into()),
        root_pid: None,
        root_pid_start: None,
        preexisting_session_ids: None,
        candidate_session_id: None,
        candidate_observed_at: None,
        created_at: 1.0,
    };
    assert!(store.add_pending(&pending).unwrap());
    assert_eq!(
        store.get_launch_phase(&pending.launch_token).unwrap(),
        Some(LaunchPhase::Reserved)
    );
    store
        .finalize_pending_pane(
            &pending.launch_token,
            "pika-a-thread",
            "%7",
            Some(70),
            Some(700),
        )
        .unwrap();
    assert_eq!(
        store.get_launch_phase(&pending.launch_token).unwrap(),
        Some(LaunchPhase::PaneAllocated)
    );
    assert!(
        store
            .set_launch_phase(&pending.launch_token, LaunchPhase::PanePrepared)
            .unwrap()
    );
    assert!(
        store
            .set_launch_phase(&pending.launch_token, LaunchPhase::ProviderStarting)
            .unwrap()
    );
    assert!(
        store
            .observe_launched_generation(&pending.launch_token, 71, 701)
            .unwrap()
    );
    let observed = store.get_pending(&pending.launch_token).unwrap().unwrap();
    assert_eq!(
        (observed.root_pid, observed.root_pid_start),
        (Some(71), Some(701))
    );
    assert_eq!(
        store.get_launch_phase(&pending.launch_token).unwrap(),
        Some(LaunchPhase::ProviderObserved)
    );
    store.delete_pending(&pending.launch_token).unwrap();
    assert_eq!(store.get_launch_phase(&pending.launch_token).unwrap(), None);
}
