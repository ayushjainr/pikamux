use pikamux::{
    config::Config,
    core::Pika,
    model::{Provider, Session, Status},
    paths::Paths,
    providers::Providers,
    store::Store,
    tmux::Tmux,
};
use rusqlite::{Connection, params};
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
    }
}

fn json_line(path: &Path, value: serde_json::Value) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        path,
        format!("{}\n", serde_json::to_string(&value).unwrap()),
    )
    .unwrap();
}

fn saved_codex(identity: &str, name: &str) -> Session {
    Session {
        provider: Provider::Codex,
        session_id: identity.into(),
        active_thread_id: None,
        name: Some(name.into()),
        cwd: Some("/project".into()),
        branch: None,
        transcript_path: None,
        tmux_session: None,
        tmux_pane: None,
        root_pid: None,
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
        live: false,
        attached: false,
        home_state: "missing".into(),
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
fn codex_excludes_archived_and_proven_workers_but_preserves_fork_identity() {
    let root = tempfile::tempdir().unwrap();
    let paths = paths(root.path());
    fs::create_dir_all(&paths.codex_home).unwrap();
    let db = Connection::open(paths.codex_home.join("state_1.sqlite")).unwrap();
    db.execute_batch(
        "CREATE TABLE threads(
            id TEXT PRIMARY KEY, name TEXT, cwd TEXT, git_branch TEXT,
            rollout_path TEXT, model TEXT, created_at INTEGER,
            updated_at INTEGER, archived INTEGER
         );",
    )
    .unwrap();

    let parent = "11111111-1111-4111-8111-111111111111";
    let named = "22222222-2222-4222-8222-222222222222";
    let fork = "33333333-3333-4333-8333-333333333333";
    let archived = "44444444-4444-4444-8444-444444444444";
    let worker = "55555555-5555-4555-8555-555555555555";
    let subagent = "66666666-6666-4666-8666-666666666666";
    for (id, name, payload, hidden, updated) in [
        (named, Some("master_quant"), serde_json::json!({}), 0, 10),
        (
            fork,
            Some("returns_tracker"),
            serde_json::json!({"forked_from_id":parent}),
            0,
            20,
        ),
        (archived, Some("old_archived"), serde_json::json!({}), 1, 30),
        (
            worker,
            Some("codex-generated"),
            serde_json::json!({"originator":"agentic_fund"}),
            0,
            40,
        ),
        (
            subagent,
            Some("side-worker"),
            serde_json::json!({"thread_source":"subagent"}),
            0,
            50,
        ),
    ] {
        let transcript = paths.codex_home.join(format!("{id}.jsonl"));
        json_line(
            &transcript,
            serde_json::json!({"type":"session_meta","payload":payload}),
        );
        db.execute(
            "INSERT INTO threads VALUES(?1,?2,'/project','main',?3,'gpt',1,?4,?5)",
            params![id, name, transcript.to_string_lossy(), updated, hidden],
        )
        .unwrap();
    }
    drop(db);

    let config = Config::default();
    let providers = Providers::new(&paths, &config);
    let records = providers.discover(Provider::Codex);
    assert_eq!(
        records
            .iter()
            .map(|item| item.session_id.as_str())
            .collect::<Vec<_>>(),
        vec![fork, named]
    );
    assert_eq!(records[0].parent_session_id.as_deref(), Some(parent));
    assert!(providers.find(Provider::Codex, archived).is_empty());
    assert!(providers.find(Provider::Codex, worker).is_empty());
}

#[test]
fn claude_first_screen_requires_explicit_name_and_exact_uuid_still_resolves() {
    let root = tempfile::tempdir().unwrap();
    let paths = paths(root.path());
    let custom = "11111111-1111-4111-8111-111111111111";
    let derived = "22222222-2222-4222-8222-222222222222";
    let history = "33333333-3333-4333-8333-333333333333";
    let worker = "44444444-4444-4444-8444-444444444444";
    let sessions = paths.claude_home.join("sessions");
    fs::create_dir_all(&sessions).unwrap();
    for (id, name, source) in [
        (custom, "qes_plugin", "custom"),
        (derived, "Generated title", "derived"),
        (worker, "Inherited worker name", "custom"),
    ] {
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
    json_line(
        &paths.claude_home.join(format!("projects/p/{custom}.jsonl")),
        serde_json::json!({"sessionId":custom,"isSidechain":false,"entrypoint":"cli"}),
    );
    json_line(
        &paths
            .claude_home
            .join(format!("projects/p/{derived}.jsonl")),
        serde_json::json!({"type":"ai-title","aiTitle":"Generated title"}),
    );
    json_line(
        &paths
            .claude_home
            .join(format!("projects/p/{history}.jsonl")),
        serde_json::json!({"type":"custom-title","customTitle":"durable_expert"}),
    );
    json_line(
        &paths.claude_home.join(format!("projects/p/{worker}.jsonl")),
        serde_json::json!({"sessionId":worker,"isSidechain":false,"entrypoint":"sdk-cli"}),
    );

    let config = Config::default();
    let providers = Providers::new(&paths, &config);
    let names = providers.discover(Provider::Claude);
    assert_eq!(
        names
            .iter()
            .map(|item| item.name.as_deref().unwrap())
            .collect::<std::collections::BTreeSet<_>>(),
        ["qes_plugin", "durable_expert"].into_iter().collect()
    );
    assert!(
        providers
            .find(Provider::Claude, "Generated title")
            .is_empty()
    );
    let exact = providers.find(Provider::Claude, derived);
    assert_eq!(exact.len(), 1);
    assert_eq!(exact[0].session_id, derived);
    assert!(exact[0].name.is_none());
    assert!(providers.find(Provider::Claude, worker).is_empty());
}

#[test]
fn opencode_never_claims_title_provenance_and_projects_child_lifecycle() {
    let root = tempfile::tempdir().unwrap();
    let paths = paths(root.path());
    fs::create_dir_all(&paths.opencode_data_home).unwrap();
    let db = Connection::open(paths.opencode_data_home.join("opencode.db")).unwrap();
    db.execute_batch(
        "CREATE TABLE session(
            id TEXT PRIMARY KEY, title TEXT, directory TEXT, parent_id TEXT,
            time_created INTEGER, time_updated INTEGER, time_archived INTEGER, model TEXT
         );
         CREATE TABLE message(id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, data TEXT);",
    ).unwrap();
    for row in [
        (
            "ses_root0001",
            "oc_qes_style",
            "/project",
            None,
            1,
            10,
            None,
        ),
        (
            "ses_child001",
            "child",
            "/project",
            Some("ses_root0001"),
            2,
            30,
            None,
        ),
        (
            "ses_archive1",
            "archived",
            "/project",
            None,
            1,
            40,
            Some(40),
        ),
        (
            "ses_auto000",
            "agentic-fund: worker",
            "/tmp/opencode-runtime/run",
            None,
            1,
            50,
            None,
        ),
        (
            "ses_new0000",
            "New session - 2026-01-01",
            "/project",
            None,
            1,
            20,
            None,
        ),
    ] {
        db.execute(
            "INSERT INTO session VALUES(?1,?2,?3,?4,?5,?6,?7,NULL)",
            params![row.0, row.1, row.2, row.3, row.4, row.5, row.6],
        )
        .unwrap();
    }
    db.execute(
        "INSERT INTO message VALUES('m1','ses_child001',31,?1)",
        [serde_json::json!({"role":"user"}).to_string()],
    )
    .unwrap();
    drop(db);

    let config = Config::default();
    let providers = Providers::new(&paths, &config);
    assert!(providers.discover(Provider::Opencode).is_empty());
    let browse = providers.browse(Provider::Opencode);
    assert_eq!(
        browse
            .iter()
            .map(|item| item.session_id.as_str())
            .collect::<Vec<_>>(),
        vec!["ses_root0001", "ses_new0000"]
    );
    assert_eq!(browse[0].updated_at, 30.0);
    assert_eq!(browse[0].lifecycle_status, Some(Status::Working));
    assert!(
        providers
            .find(Provider::Opencode, "ses_child001")
            .is_empty()
    );
    assert!(
        providers
            .find(Provider::Opencode, "ses_archive1")
            .is_empty()
    );
    assert!(providers.find(Provider::Opencode, "ses_auto000").is_empty());
}

#[test]
fn reconciliation_persists_native_rename_and_active_leaf_lifecycle() {
    let root = tempfile::tempdir().unwrap();
    let paths = paths(root.path());
    fs::create_dir_all(&paths.codex_home).unwrap();
    let db = Connection::open(paths.codex_home.join("state_1.sqlite")).unwrap();
    db.execute_batch(
        "CREATE TABLE threads(
            id TEXT PRIMARY KEY, name TEXT, cwd TEXT, rollout_path TEXT,
            created_at INTEGER, updated_at INTEGER, archived INTEGER
         );",
    )
    .unwrap();
    let root_id = "77777777-7777-4777-8777-777777777777";
    let leaf_id = "88888888-8888-4888-8888-888888888888";
    let transcript = paths.codex_home.join("leaf.jsonl");
    fs::write(
        &transcript,
        format!(
            "{}\n{}\n",
            serde_json::json!({"type":"session_meta","payload":{"forked_from_id":root_id}}),
            serde_json::json!({"type":"event_msg","payload":{"type":"task_complete"}})
        ),
    )
    .unwrap();
    db.execute(
        "INSERT INTO threads VALUES(?1,'strategy_dashboard','/project',?2,1,20,0)",
        params![leaf_id, transcript.to_string_lossy()],
    )
    .unwrap();
    drop(db);
    let store = Store::from_paths(&paths);
    store.initialize().unwrap();
    let mut saved = saved_codex(root_id, "old_name");
    saved.active_thread_id = Some(leaf_id.into());
    store.upsert_session(&saved, false).unwrap();
    let pika = Pika::with_components(
        paths,
        Config::default(),
        store.clone(),
        Tmux::with_executable("/usr/bin/false", Some("isolated".into())),
    );
    let reconciled = pika.reconcile_local().unwrap().sessions.remove(0);
    assert_eq!(reconciled.name.as_deref(), Some("strategy_dashboard"));
    assert_eq!(reconciled.status, Status::Ready);
    assert!(reconciled.unread);
    assert_eq!(
        store
            .get_session(Provider::Codex, root_id)
            .unwrap()
            .unwrap()
            .name
            .as_deref(),
        Some("strategy_dashboard")
    );
}

#[test]
fn renamed_independent_codex_fork_becomes_a_distinct_watched_row() {
    let root = tempfile::tempdir().unwrap();
    let paths = paths(root.path());
    fs::create_dir_all(&paths.codex_home).unwrap();
    let db = Connection::open(paths.codex_home.join("state_1.sqlite")).unwrap();
    db.execute_batch(
        "CREATE TABLE threads(
            id TEXT PRIMARY KEY, name TEXT, cwd TEXT, rollout_path TEXT,
            created_at INTEGER, updated_at INTEGER, archived INTEGER
         );",
    )
    .unwrap();
    let root_id = "99999999-9999-4999-8999-999999999999";
    let fork_id = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    let root_transcript = paths.codex_home.join("root.jsonl");
    let fork_transcript = paths.codex_home.join("fork.jsonl");
    json_line(
        &root_transcript,
        serde_json::json!({"type":"session_meta","payload":{}}),
    );
    json_line(
        &fork_transcript,
        serde_json::json!({"type":"session_meta","payload":{"forked_from_id":root_id}}),
    );
    for (id, name, transcript, updated) in [
        (root_id, "returns_tracker", &root_transcript, 10),
        (fork_id, "strategy_dashboard", &fork_transcript, 20),
    ] {
        db.execute(
            "INSERT INTO threads VALUES(?1,?2,'/project',?3,1,?4,0)",
            params![id, name, transcript.to_string_lossy(), updated],
        )
        .unwrap();
    }
    drop(db);
    let store = Store::from_paths(&paths);
    store.initialize().unwrap();
    store
        .upsert_session(&saved_codex(root_id, "returns_tracker"), false)
        .unwrap();
    let pika = Pika::with_components(
        paths,
        Config::default(),
        store,
        Tmux::with_executable("/usr/bin/false", Some("isolated".into())),
    );
    let rows = pika.reconcile_local().unwrap().sessions;
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().any(|row| {
        row.session_id == fork_id && row.name.as_deref() == Some("strategy_dashboard")
    }));
}
