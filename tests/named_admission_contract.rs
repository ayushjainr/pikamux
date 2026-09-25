use pikamux::{
    config::Config,
    core::Pika,
    model::{ObservationKind, Provider, Session, Status, StatusObservation},
    paths::Paths,
    store::{LiveOwner, Store},
    tmux::Tmux,
};
use std::{fs, path::Path};

fn paths(root: &Path) -> Paths {
    let config_dir = root.join("config/pika");
    let state_dir = root.join("state/pika");
    Paths {
        config: config_dir.join("config.json"),
        config_dir,
        database: state_dir.join("pika.db"),
        state_dir,
        codex_home: root.join("codex"),
        claude_home: root.join("claude"),
        opencode_data_home: root.join("opencode-data"),
        opencode_config_home: root.join("opencode-config"),
        muse_data_home: root.join("muse-data"),
        muse_config_home: root.join("muse-config"),
    }
}

fn pika(paths: Paths, store: Store) -> Pika {
    Pika::with_components(
        paths,
        Config::default(),
        store,
        Tmux::with_executable("false", None),
    )
}

fn claude_session(paths: &Paths, id: &str, name: &str, source: &str) {
    let sessions = paths.claude_home.join("sessions");
    fs::create_dir_all(&sessions).unwrap();
    fs::write(
        sessions.join(format!("{id}.json")),
        serde_json::to_vec(&serde_json::json!({
            "kind":"interactive", "sessionId":id, "name":name,
            "nameSource":source, "cwd":"/project", "updatedAt":10
        }))
        .unwrap(),
    )
    .unwrap();
}

fn json_line(path: &Path, value: serde_json::Value) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        path,
        format!("{}\n", serde_json::to_string(&value).unwrap()),
    )
    .unwrap();
}

fn external_session(id: &str) -> Session {
    Session {
        provider: Provider::Claude,
        session_id: id.into(),
        active_thread_id: None,
        name: Some("earlier Pika label".into()),
        cwd: Some("/old-project".into()),
        branch: Some("preserved-branch".into()),
        transcript_path: Some("/old/transcript.jsonl".into()),
        tmux_session: Some("pika-existing".into()),
        tmux_pane: Some("%17".into()),
        root_pid: Some(4242),
        status: Status::Ready,
        unread: true,
        model: Some("preserved-model".into()),
        source: "external".into(),
        managed: false,
        error: None,
        attention_reason: Some("completed".into()),
        created_at: 1.0,
        updated_at: 10.0,
        last_event_at: 12.0,
        last_activity_at: 12.0,
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

#[test]
fn first_reconcile_auto_admits_only_provider_proven_claude_personal_names() {
    let root = tempfile::tempdir().unwrap();
    let paths = paths(root.path());
    let store = Store::at(paths.database.clone());
    assert!(
        pika(paths.clone(), store.clone())
            .reconcile_local()
            .unwrap()
            .sessions
            .is_empty()
    );

    let named = "11111111-1111-4111-8111-111111111111";
    let derived = "22222222-2222-4222-8222-222222222222";
    let helper = "33333333-3333-4333-8333-333333333333";
    claude_session(&paths, named, "first title", "custom");
    // The provider has now recorded a human-selected rename after Pika's first
    // observation. A new Pika handle must discover it without setup or `+`.
    json_line(
        &paths.claude_home.join(format!("projects/p/{named}.jsonl")),
        serde_json::json!({"type":"custom-title","customTitle":"personal_renamed"}),
    );
    claude_session(&paths, derived, "Generated title", "derived");
    json_line(
        &paths
            .claude_home
            .join(format!("projects/p/{derived}.jsonl")),
        serde_json::json!({"type":"ai-title","aiTitle":"Generated title"}),
    );
    claude_session(&paths, helper, "Inherited worker label", "custom");
    json_line(
        &paths.claude_home.join(format!("projects/p/{helper}.jsonl")),
        serde_json::json!({"sessionId":helper,"isSidechain":false,"entrypoint":"sdk-cli"}),
    );

    let inventory = pika(paths.clone(), store.clone())
        .reconcile_local()
        .unwrap();
    assert_eq!(
        inventory
            .sessions
            .iter()
            .map(|row| row.session_id.as_str())
            .collect::<Vec<_>>(),
        vec![named]
    );
    let row = &inventory.sessions[0];
    assert_eq!(row.session_id, named);
    assert_eq!(row.name.as_deref(), Some("personal_renamed"));
    assert_eq!(row.cwd.as_deref(), Some("/project"));
    assert!(row.transcript_path.is_some());
    assert!(store.is_watched(Provider::Claude, named).unwrap());
    assert!(!store.is_watched(Provider::Claude, derived).unwrap());
    assert!(!store.is_watched(Provider::Claude, helper).unwrap());

    // Auto-discovery is not an unwatch tombstone bypass.
    store.untrack_session(Provider::Claude, named).unwrap();
    let after_unwatch = pika(paths, store.clone()).reconcile_local().unwrap();
    assert!(after_unwatch.sessions.is_empty());
    assert!(store.is_untracked(Provider::Claude, named).unwrap());
}

#[test]
fn named_membership_grant_preserves_existing_unread_owner_and_metadata() {
    let root = tempfile::tempdir().unwrap();
    let paths = paths(root.path());
    let store = Store::at(paths.database.clone());
    let id = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    // An external observation may predate the verified name. Its existing
    // state is not replaced by the provider's metadata snapshot on admission.
    store.upsert_session(&external_session(id), false).unwrap();
    store
        .set_live_owner(&LiveOwner {
            provider: Provider::Claude,
            session_id: id.into(),
            pid: 4242,
            start_time: Some(99),
            owner_token: "existing-owner-token".into(),
            last_seen: 12.0,
        })
        .unwrap();
    store
        .record_status_observation(
            Provider::Claude,
            id,
            &StatusObservation {
                kind: ObservationKind::Lifecycle,
                status: Status::Ready,
                unread: true,
                attention_reason: Some("completed".into()),
                error: None,
                observed_at: 12.0,
                source: "test-existing-state".into(),
            },
        )
        .unwrap();
    let mut named = external_session(id);
    named.name = Some("native personal name".into());
    named.cwd = Some("/project".into());
    assert!(store.watch_named_session(&named).unwrap());
    let row = store.get_session(Provider::Claude, id).unwrap().unwrap();
    assert_eq!(row.status, Status::Ready);
    assert!(row.unread);
    assert_eq!(row.root_pid, Some(4242));
    assert_eq!(row.tmux_session.as_deref(), Some("pika-existing"));
    assert_eq!(row.tmux_pane.as_deref(), Some("%17"));
    assert_eq!(row.cwd.as_deref(), Some("/old-project"));
    assert_eq!(row.branch.as_deref(), Some("preserved-branch"));
    assert_eq!(row.model.as_deref(), Some("preserved-model"));
    let owners = store.live_owners(Provider::Claude, id).unwrap();
    assert_eq!(owners.len(), 1);
    assert_eq!(owners[0].owner_token, "existing-owner-token");
}
