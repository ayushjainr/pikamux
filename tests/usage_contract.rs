use pikamux::{
    model::{Provider, Session, Status},
    paths::Paths,
    store::Store,
    usage::{
        CostBasis, PRICING_AS_OF, format_cost, format_tokens, hydrate_sessions, usage_for_session,
    },
};
use rusqlite::Connection;
use serde_json::json;
use std::{
    fs,
    io::{Seek, Write},
    path::Path,
};

fn paths(root: &Path) -> Paths {
    Paths {
        config_dir: root.join("config"),
        state_dir: root.join("state"),
        config: root.join("config/config.json"),
        database: root.join("state/pika.db"),
        codex_home: root.join("codex"),
        claude_home: root.join("claude"),
        opencode_data_home: root.join("opencode"),
        opencode_config_home: root.join("opencode-config"),
    }
}

fn session(provider: Provider, id: &str, transcript: Option<&Path>) -> Session {
    Session {
        provider,
        session_id: id.to_owned(),
        name: Some("fixture".into()),
        cwd: None,
        branch: None,
        transcript_path: transcript.map(|path| path.to_string_lossy().into_owned()),
        tmux_session: None,
        tmux_pane: None,
        root_pid: None,
        status: Status::Parked,
        unread: false,
        model: None,
        source: "fixture".into(),
        managed: true,
        error: None,
        attention_reason: None,
        created_at: 0.0,
        updated_at: 0.0,
        last_event_at: 0.0,
        last_activity_at: 0.0,
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

fn line(value: serde_json::Value) -> String {
    format!("{}\n", serde_json::to_string(&value).unwrap())
}

#[test]
fn compact_formatting_matches_the_frozen_user_facing_contract() {
    assert_eq!(format_tokens(None), "—");
    assert_eq!(format_tokens(Some(999)), "999");
    assert_eq!(format_tokens(Some(1_250)), "1.2k");
    assert_eq!(format_tokens(Some(1_250_000)), "1.25m");
    assert_eq!(format_tokens(Some(1_250_000_000)), "1.25b");
    assert_eq!(format_cost(None), "—");
    assert_eq!(format_cost(Some(0.0094)), "~$0.009");
    assert_eq!(format_cost(Some(1.234)), "~$1.23");
}

#[test]
fn codex_uses_latest_cumulative_counter_and_dated_api_equivalent_cost() {
    let temp = tempfile::tempdir().unwrap();
    let paths = paths(temp.path());
    let store = Store::from_paths(&paths);
    let transcript = temp.path().join("codex.jsonl");
    fs::write(
        &transcript,
        [
            line(json!({"payload":{"info":{"total_token_usage":{
                "input_tokens":10,"cached_input_tokens":2,"output_tokens":3,"total_tokens":13
            }}}})),
            line(json!({"payload":{"model":"gpt-5.4"}})),
            line(json!({"payload":{"info":{"total_token_usage":{
                "input_tokens":20,"cached_input_tokens":4,"output_tokens":5,"total_tokens":25
            }}}})),
        ]
        .concat(),
    )
    .unwrap();

    let usage = usage_for_session(
        &paths,
        &store,
        &session(Provider::Codex, "codex-id", Some(&transcript)),
    )
    .unwrap()
    .unwrap();
    assert_eq!(usage.total_tokens, 25);
    assert_eq!(usage.model.as_deref(), Some("gpt-5.4"));
    assert_eq!(usage.cost_basis, Some(CostBasis::ApiEquivalent));
    assert_eq!(usage.pricing_as_of, Some(PRICING_AS_OF));
    let expected = ((16.0 * 2.5) + (4.0 * 0.25) + (5.0 * 15.0)) / 1_000_000.0;
    assert!((usage.estimated_cost_usd.unwrap() - expected).abs() < f64::EPSILON);
}

#[test]
fn unknown_model_never_receives_an_invented_price() {
    let temp = tempfile::tempdir().unwrap();
    let paths = paths(temp.path());
    let store = Store::from_paths(&paths);
    let transcript = temp.path().join("codex.jsonl");
    fs::write(
        &transcript,
        line(json!({"payload":{"info":{"total_token_usage":{
            "input_tokens":20,"output_tokens":5,"total_tokens":25
        }}}})),
    )
    .unwrap();
    let mut value = session(Provider::Codex, "codex-id", Some(&transcript));
    value.model = Some("private-frontier-model".into());
    let usage = usage_for_session(&paths, &store, &value).unwrap().unwrap();
    assert_eq!(usage.estimated_cost_usd, None);
    assert_eq!(usage.cost_basis, None);
    assert_eq!(usage.pricing_as_of, None);
}

#[test]
fn codex_tail_scan_is_bounded_and_ignores_malformed_records() {
    let temp = tempfile::tempdir().unwrap();
    let paths = paths(temp.path());
    let store = Store::from_paths(&paths);
    let transcript = temp.path().join("large.jsonl");
    let mut file = fs::File::create(&transcript).unwrap();
    writeln!(file, "{{not json").unwrap();
    file.set_len(9 * 1024 * 1024).unwrap();
    file.seek(std::io::SeekFrom::End(0)).unwrap();
    write!(
        file,
        "\n{}",
        line(json!({"payload":{"info":{"total_token_usage":{
            "input_tokens":1,"output_tokens":2,"total_tokens":3
        }}}}))
    )
    .unwrap();
    let usage = usage_for_session(
        &paths,
        &store,
        &session(Provider::Codex, "large", Some(&transcript)),
    )
    .unwrap()
    .unwrap();
    assert_eq!(usage.total_tokens, 3);
}

#[test]
fn claude_incrementally_adds_only_new_structured_usage() {
    let temp = tempfile::tempdir().unwrap();
    let paths = paths(temp.path());
    let store = Store::from_paths(&paths);
    let transcript = temp.path().join("claude.jsonl");
    let first = line(json!({
        "type":"assistant",
        "message":{"model":"claude-opus-4-8","usage":{
            "input_tokens":10,"output_tokens":4,
            "cache_read_input_tokens":2,"cache_creation_input_tokens":3
        }},
        "content":"private transcript text is ignored"
    }));
    fs::write(&transcript, first).unwrap();
    let target = session(Provider::Claude, "claude-id", Some(&transcript));
    let first = usage_for_session(&paths, &store, &target).unwrap().unwrap();
    assert_eq!(first.total_tokens, 19);

    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(&transcript)
        .unwrap();
    write!(
        file,
        "{}",
        line(json!({
            "type":"assistant",
            "message":{"model":"claude-opus-4-8","usage":{
                "input_tokens":5,"output_tokens":1
            }}
        }))
    )
    .unwrap();
    let second = usage_for_session(&paths, &store, &target).unwrap().unwrap();
    assert_eq!(second.input_tokens, 15);
    assert_eq!(second.output_tokens, 5);
    assert_eq!(second.total_tokens, 25);
    assert_eq!(second.cost_basis, Some(CostBasis::ApiEquivalent));

    // An unchanged third read hits the exact fingerprint cache, not a sum pass.
    let third = usage_for_session(&paths, &store, &target).unwrap().unwrap();
    assert_eq!(third, second);
}

#[test]
fn claude_partial_tail_is_counted_once_after_it_becomes_complete() {
    let temp = tempfile::tempdir().unwrap();
    let paths = paths(temp.path());
    let store = Store::from_paths(&paths);
    let transcript = temp.path().join("claude.jsonl");
    fs::write(
        &transcript,
        concat!(
            "{\"message\":{\"usage\":{\"input_tokens\":1}}}\n",
            "{\"message\":{\"usage\":{\"input_tokens\":"
        ),
    )
    .unwrap();
    let target = session(Provider::Claude, "claude-id", Some(&transcript));
    assert_eq!(
        usage_for_session(&paths, &store, &target)
            .unwrap()
            .unwrap()
            .input_tokens,
        1
    );
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(&transcript)
        .unwrap();
    file.write_all(b"2}}}\n").unwrap();
    let usage = usage_for_session(&paths, &store, &target).unwrap().unwrap();
    assert_eq!(usage.input_tokens, 3);
    assert_eq!(usage.total_tokens, 3);
}

#[test]
fn claude_same_size_rewrite_is_recounted_instead_of_incremented() {
    let temp = tempfile::tempdir().unwrap();
    let paths = paths(temp.path());
    let store = Store::from_paths(&paths);
    let transcript = temp.path().join("claude.jsonl");
    let original = line(json!({"message":{"usage":{"input_tokens":1}}}));
    let replacement = line(json!({"message":{"usage":{"input_tokens":9}}}));
    assert_eq!(original.len(), replacement.len());
    fs::write(&transcript, original).unwrap();
    let target = session(Provider::Claude, "claude-id", Some(&transcript));
    assert_eq!(
        usage_for_session(&paths, &store, &target)
            .unwrap()
            .unwrap()
            .input_tokens,
        1
    );
    std::thread::sleep(std::time::Duration::from_millis(20));
    fs::write(&transcript, replacement).unwrap();
    assert_eq!(
        usage_for_session(&paths, &store, &target)
            .unwrap()
            .unwrap()
            .input_tokens,
        9
    );
}

#[test]
fn claude_content_that_mentions_usage_is_not_accounted() {
    let temp = tempfile::tempdir().unwrap();
    let paths = paths(temp.path());
    let store = Store::from_paths(&paths);
    let transcript = temp.path().join("claude.jsonl");
    fs::write(
        &transcript,
        line(json!({
            "type":"user",
            "content":"{\"usage\":{\"input_tokens\":999999}}"
        })),
    )
    .unwrap();
    assert!(
        usage_for_session(
            &paths,
            &store,
            &session(Provider::Claude, "claude-id", Some(&transcript))
        )
        .unwrap()
        .is_none()
    );
}

#[test]
fn claude_refuses_an_unbounded_first_scan_instead_of_reporting_partial_totals() {
    let temp = tempfile::tempdir().unwrap();
    let paths = paths(temp.path());
    let store = Store::from_paths(&paths);
    let transcript = temp.path().join("huge.jsonl");
    fs::File::create(&transcript)
        .unwrap()
        .set_len(129 * 1024 * 1024)
        .unwrap();
    let error = usage_for_session(
        &paths,
        &store,
        &session(Provider::Claude, "claude-id", Some(&transcript)),
    )
    .unwrap_err();
    assert!(error.to_string().contains("bounded limit"));
}

#[test]
fn opencode_aggregates_the_active_tree_and_labels_provider_reported_cost() {
    let temp = tempfile::tempdir().unwrap();
    let paths = paths(temp.path());
    fs::create_dir_all(&paths.opencode_data_home).unwrap();
    let db = Connection::open(paths.opencode_data_home.join("opencode.db")).unwrap();
    db.execute_batch(
        "CREATE TABLE session (
            id TEXT PRIMARY KEY,parent_id TEXT,time_archived INTEGER,model TEXT,cost REAL,
            tokens_input INTEGER,tokens_output INTEGER,tokens_reasoning INTEGER,
            tokens_cache_read INTEGER,tokens_cache_write INTEGER
        );",
    )
    .unwrap();
    let model = json!({"providerID":"opencode","id":"x-preview","variant":"max"}).to_string();
    db.execute(
        "INSERT INTO session VALUES (?1,NULL,NULL,?2,0.01,10,2,1,20,0)",
        ("ses_root123", &model),
    )
    .unwrap();
    db.execute(
        "INSERT INTO session VALUES (?1,?2,NULL,?3,0.02,1,1,2,0,3)",
        ("ses_child456", "ses_root123", &model),
    )
    .unwrap();
    db.execute(
        "INSERT INTO session VALUES (?1,?2,1,?3,99.0,999,999,999,999,999)",
        ("ses_archived", "ses_root123", &model),
    )
    .unwrap();
    drop(db);

    let mut target = session(Provider::Opencode, "stable-pika-key", None);
    target.active_thread_id = Some("ses_root123".into());
    let usage = usage_for_session(&paths, &Store::from_paths(&paths), &target)
        .unwrap()
        .unwrap();
    assert_eq!(usage.model.as_deref(), Some("opencode/x-preview[max]"));
    assert_eq!(usage.input_tokens, 11);
    assert_eq!(usage.output_tokens, 3);
    assert_eq!(usage.cached_input_tokens, 20);
    assert_eq!(usage.cache_write_tokens, 3);
    assert_eq!(usage.total_tokens, 40);
    assert_eq!(usage.estimated_cost_usd, Some(0.03));
    assert_eq!(usage.cost_basis, Some(CostBasis::ProviderReported));
    assert_eq!(usage.pricing_as_of, None);
}

#[test]
fn opencode_without_provider_cost_keeps_money_unknown() {
    let temp = tempfile::tempdir().unwrap();
    let paths = paths(temp.path());
    fs::create_dir_all(&paths.opencode_data_home).unwrap();
    let db = Connection::open(paths.opencode_data_home.join("opencode.db")).unwrap();
    db.execute_batch(
        "CREATE TABLE session (id TEXT PRIMARY KEY,parent_id TEXT,tokens_input INTEGER);
         INSERT INTO session VALUES ('ses_root123',NULL,7);",
    )
    .unwrap();
    drop(db);
    let usage = usage_for_session(
        &paths,
        &Store::from_paths(&paths),
        &session(Provider::Opencode, "ses_root123", None),
    )
    .unwrap()
    .unwrap();
    assert_eq!(usage.total_tokens, 7);
    assert_eq!(usage.estimated_cost_usd, None);
    assert_eq!(usage.cost_basis, None);
}

#[test]
fn hydration_is_best_effort_across_sessions() {
    let temp = tempfile::tempdir().unwrap();
    let paths = paths(temp.path());
    let store = Store::from_paths(&paths);
    let transcript = temp.path().join("codex.jsonl");
    fs::write(
        &transcript,
        line(json!({"payload":{"info":{"total_token_usage":{
            "input_tokens":1,"output_tokens":2,"total_tokens":3
        }}}})),
    )
    .unwrap();
    let mut sessions = vec![
        session(Provider::Codex, "ok", Some(&transcript)),
        session(
            Provider::Claude,
            "missing",
            Some(&temp.path().join("missing.jsonl")),
        ),
    ];
    let report = hydrate_sessions(&paths, &store, &mut sessions);
    assert_eq!(report.hydrated, 1);
    assert_eq!(report.errors.len(), 1);
    assert_eq!(sessions[0].total_tokens, Some(3));
    assert_eq!(sessions[1].total_tokens, None);
}
