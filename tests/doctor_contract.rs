use pikamux::{
    config::Config,
    doctor::{
        CheckLevel, ProviderRuntime, RuntimeEvidence, inspect_and_repair_stale,
        inspect_with_evidence, repair_stale,
    },
    model::{Pane, Provider, Session, Status},
    paths::Paths,
    process::ProcessRecord,
    setup::{apply_changes, codex_hooks_change, hook_spec_fingerprint},
    store::{HookObservation, LiveOwner, PendingLaunch, Store},
};
use std::{collections::BTreeMap, fs, path::Path};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

const ID: &str = "11111111-1111-4111-8111-111111111111";

fn paths(root: &Path) -> Paths {
    Paths {
        config_dir: root.join("config"),
        state_dir: root.join("state"),
        config: root.join("config/config.json"),
        database: root.join("state/pika.db"),
        codex_home: root.join("codex"),
        claude_home: root.join("claude"),
        opencode_data_home: root.join("opencode-data"),
        opencode_config_home: root.join("opencode-config"),
    }
}

fn session(root: &Path) -> Session {
    Session {
        provider: Provider::Codex,
        session_id: ID.into(),
        name: Some("strategy_dashboard".into()),
        cwd: Some(root.to_string_lossy().into_owned()),
        branch: Some("main".into()),
        transcript_path: Some(
            root.join("private transcript.jsonl")
                .to_string_lossy()
                .into(),
        ),
        tmux_session: Some("pika-c-one".into()),
        tmux_pane: Some("%1".into()),
        root_pid: Some(11),
        status: Status::Working,
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
        live: true,
        attached: false,
        home_state: "exact".into(),
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

fn pane(id: &str, pid: i64, token: Option<&str>) -> Pane {
    Pane {
        session_name: "pika-c-one".into(),
        pane_id: id.into(),
        pane_pid: pid,
        cwd: "/fixture".into(),
        current_command: "codex".into(),
        attached: false,
        dead: false,
        dead_status: None,
        activity: 0.0,
        created: 0.0,
        pika_provider: Some(Provider::Codex),
        pika_session_id: Some(ID.into()),
        pika_name: Some("strategy_dashboard".into()),
        pika_launch_token: token.map(str::to_owned),
    }
}

fn process(pid: i64, parent: Option<i64>, start: u64, argv: &[&str]) -> ProcessRecord {
    ProcessRecord {
        pid,
        parent_pid: parent,
        start_time: start,
        argv: argv.iter().map(|value| (*value).to_owned()).collect(),
    }
}

fn evidence() -> RuntimeEvidence {
    RuntimeEvidence {
        tmux_available: true,
        tmux_snapshot_complete: true,
        tmux_error: None,
        panes: vec![pane("%1", 10, None)],
        process_snapshot_complete: true,
        processes: BTreeMap::from([
            (10, process(10, None, 10, &["zsh"])),
            (11, process(11, Some(10), 11, &["codex", "resume", ID])),
        ]),
        providers: BTreeMap::from([
            (
                Provider::Codex,
                ProviderRuntime {
                    available: true,
                    version: Some("codex 1.0".into()),
                },
            ),
            (Provider::Claude, ProviderRuntime::default()),
            (Provider::Opencode, ProviderRuntime::default()),
        ]),
        platform: "fixture-os".into(),
    }
}

fn commissioned_fixture() -> (
    tempfile::TempDir,
    Paths,
    Store,
    std::path::PathBuf,
    RuntimeEvidence,
) {
    let temp = tempfile::tempdir().unwrap();
    let paths = paths(temp.path());
    let binary = temp.path().join("bin/pika");
    fs::create_dir_all(binary.parent().unwrap()).unwrap();
    fs::write(&binary, "fixture").unwrap();
    Config::default().write(&paths).unwrap();
    let store = Store::from_paths(&paths);
    store.initialize().unwrap();
    store.upsert_session(&session(temp.path()), false).unwrap();
    let hook = codex_hooks_change(&paths.codex_home, &binary).unwrap();
    apply_changes(&[hook], "doctor-fixture").unwrap();
    store
        .record_hook_observation(&HookObservation {
            provider: Provider::Codex,
            fingerprint: hook_spec_fingerprint(Provider::Codex, &binary).unwrap(),
            event_name: "SessionStart".into(),
            session_id: ID.into(),
            observed_at: 90.0,
            source: Some("hook".into()),
            managed: true,
        })
        .unwrap();
    let runtime = evidence();
    (temp, paths, store, binary, runtime)
}

#[test]
fn exact_recovery_produces_a_copy_safe_certificate() {
    let (temp, paths, store, binary, runtime) = commissioned_fixture();
    let report = inspect_with_evidence(&paths, &store, &binary, &runtime, 100.0);
    assert!(report.safe_to_disconnect, "{}", report.render_human(true));
    assert_eq!(report.recovery.tracked, 1);
    assert_eq!(report.recovery.exact_homes, 1);
    assert_eq!(report.recovery.recoverable, 1);
    let encoded = serde_json::to_string_pretty(&report).unwrap();
    assert_eq!(report.schema, "pikamux-doctor/v1");
    assert!(!encoded.contains(ID));
    assert!(!encoded.contains("private transcript"));
    assert!(!encoded.contains(&temp.path().to_string_lossy().to_string()));
    let human = report.render_human(false);
    assert!(human.contains("Safe to disconnect this terminal"));
    assert!(human.contains("Copy-safe recovery passport"));
    assert!(human.contains("PIKA VERIFIED"));
}

#[test]
fn genuine_second_identity_is_reported_as_duplicate_and_outside() {
    let (_temp, paths, store, binary, mut runtime) = commissioned_fixture();
    runtime
        .processes
        .insert(12, process(12, None, 12, &["codex", "resume", ID]));
    let report = inspect_with_evidence(&paths, &store, &binary, &runtime, 100.0);
    assert!(!report.safe_to_disconnect);
    assert_eq!(report.recovery.duplicates, 1);
    assert_eq!(report.recovery.outside, 1);
    assert!(
        report.checks.iter().any(|check| {
            check.code == "identity.duplicates" && check.level == CheckLevel::Error
        })
    );
    let encoded = serde_json::to_string(&report).unwrap();
    assert!(!encoded.contains(ID));
}

#[test]
fn missing_hook_observation_and_unsafe_permissions_block_certificate() {
    let (_temp, paths, store, binary, runtime) = commissioned_fixture();
    store
        .record_hook_observation(&HookObservation {
            provider: Provider::Codex,
            fingerprint: "old-definition".into(),
            event_name: "Stop".into(),
            session_id: ID.into(),
            observed_at: 99.0,
            source: None,
            managed: true,
        })
        .unwrap();
    #[cfg(unix)]
    fs::set_permissions(&paths.config, fs::Permissions::from_mode(0o644)).unwrap();
    let report = inspect_with_evidence(&paths, &store, &binary, &runtime, 100.0);
    assert!(!report.safe_to_disconnect);
    assert!(
        report
            .checks
            .iter()
            .any(|check| { check.code == "hooks.codex" && check.level == CheckLevel::Warn })
    );
    #[cfg(unix)]
    assert!(
        report
            .checks
            .iter()
            .any(|check| { check.code == "config.permissions" && check.level == CheckLevel::Warn })
    );
}

#[test]
fn malformed_config_and_schema_are_reported_without_panicking() {
    let temp = tempfile::tempdir().unwrap();
    let paths = paths(temp.path());
    fs::create_dir_all(&paths.config_dir).unwrap();
    fs::create_dir_all(&paths.state_dir).unwrap();
    fs::write(&paths.config, "[]").unwrap();
    fs::write(&paths.database, "not sqlite").unwrap();
    #[cfg(unix)]
    {
        fs::set_permissions(&paths.config, fs::Permissions::from_mode(0o600)).unwrap();
        fs::set_permissions(&paths.database, fs::Permissions::from_mode(0o600)).unwrap();
    }
    let binary = temp.path().join("pika");
    fs::write(&binary, "fixture").unwrap();
    let report = inspect_with_evidence(
        &paths,
        &Store::from_paths(&paths),
        &binary,
        &RuntimeEvidence::default(),
        10.0,
    );
    assert!(!report.safe_to_disconnect);
    assert!(
        report
            .checks
            .iter()
            .any(|check| { check.code == "config.format" && check.level == CheckLevel::Error })
    );
    assert!(
        report
            .checks
            .iter()
            .any(|check| { check.code == "database.schema" && check.level == CheckLevel::Error })
    );
}

#[test]
fn repair_removes_only_records_disproved_by_complete_snapshots() {
    let temp = tempfile::tempdir().unwrap();
    let paths = paths(temp.path());
    let store = Store::from_paths(&paths);
    store.initialize().unwrap();
    let stale = PendingLaunch {
        launch_token: "stale-token".into(),
        provider: Provider::Codex,
        name: "stale".into(),
        cwd: temp.path().to_string_lossy().into(),
        tmux_session: None,
        tmux_pane: None,
        expected_session_id: None,
        root_pid: None,
        root_pid_start: None,
        preexisting_session_ids: None,
        candidate_session_id: None,
        candidate_observed_at: None,
        created_at: 1.0,
    };
    let mut live = stale.clone();
    live.launch_token = "live-token".into();
    live.name = "live".into();
    live.tmux_pane = Some("%2".into());
    live.root_pid = Some(21);
    live.root_pid_start = Some(21);
    store.add_pending(&stale).unwrap();
    store.add_pending(&live).unwrap();
    store
        .reserve_resume(Provider::Codex, ID, "dead-resume", 99, 1)
        .unwrap();
    let other = "22222222-2222-4222-8222-222222222222";
    store
        .reserve_resume(Provider::Codex, other, "live-resume", 21, 21)
        .unwrap();
    store
        .set_live_owner(&LiveOwner {
            provider: Provider::Claude,
            session_id: "33333333-3333-4333-8333-333333333333".into(),
            pid: 98,
            start_time: Some(1),
            owner_token: "dead-owner".into(),
            last_seen: 1.0,
        })
        .unwrap();
    store
        .set_live_owner(&LiveOwner {
            provider: Provider::Codex,
            session_id: other.into(),
            pid: 21,
            start_time: Some(21),
            owner_token: "live-owner".into(),
            last_seen: 999.0,
        })
        .unwrap();
    let mut runtime = RuntimeEvidence {
        tmux_available: true,
        tmux_snapshot_complete: true,
        process_snapshot_complete: true,
        panes: vec![pane("%2", 20, Some("live-token"))],
        platform: "fixture".into(),
        ..RuntimeEvidence::default()
    };
    runtime.processes = BTreeMap::from([
        (20, process(20, None, 20, &["zsh"])),
        (21, process(21, Some(20), 21, &["codex"])),
    ]);

    let future = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
        + 600.0;
    let repairs = repair_stale(&store, &runtime, future, 300.0);
    assert_eq!(repairs.len(), 3);
    assert!(store.get_pending("stale-token").unwrap().is_none());
    assert!(store.get_pending("live-token").unwrap().is_some());
    assert!(
        store
            .get_resume_reservation(Provider::Codex, ID)
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .get_resume_reservation(Provider::Codex, other)
            .unwrap()
            .is_some()
    );
    assert_eq!(store.list_live_owners().unwrap().len(), 1);
    assert!(repairs.iter().all(|receipt| {
        !receipt.reference.contains("token") && !receipt.reference.contains(ID)
    }));
    assert_eq!(runtime.processes.len(), 2);
    assert_eq!(runtime.panes.len(), 1);
}

#[test]
fn incomplete_runtime_evidence_can_never_repair_state() {
    let temp = tempfile::tempdir().unwrap();
    let paths = paths(temp.path());
    let store = Store::from_paths(&paths);
    store.initialize().unwrap();
    store
        .add_pending(&PendingLaunch {
            launch_token: "old".into(),
            provider: Provider::Codex,
            name: "old".into(),
            cwd: "/tmp".into(),
            tmux_session: None,
            tmux_pane: None,
            expected_session_id: None,
            root_pid: None,
            root_pid_start: None,
            preexisting_session_ids: None,
            candidate_session_id: None,
            candidate_observed_at: None,
            created_at: 1.0,
        })
        .unwrap();
    let report = inspect_and_repair_stale(
        &paths,
        &store,
        &temp.path().join("pika"),
        &RuntimeEvidence::default(),
        10_000.0,
        300.0,
    );
    assert!(report.repairs.is_empty());
    assert!(store.get_pending("old").unwrap().is_some());
}
