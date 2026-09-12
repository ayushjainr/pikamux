#![cfg(unix)]

use assert_cmd::Command;
use pikamux::experts::{CardStatus, SourceAvailability, local_source_availability};
use pikamux::fleet::{
    CAPABILITIES, ConsultationPolicy, FleetErrorKind, FleetManager, FleetSession, PROTOCOL_NAME,
    PROTOCOL_VERSION, RemoteConsultation, SshTransport, session_to_wire,
};
use pikamux::model::{ExpertProfile, FleetNode, Provider, Session, Status};
use pikamux::paths::Paths;
use pikamux::store::{Store, StoredExpertProfile};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tempfile::TempDir;
use uuid::Uuid;

const THREAD_ID: &str = "11111111-1111-4111-8111-111111111111";

fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
}

fn session(id: &str, name: &str, transcript: Option<&Path>) -> Session {
    Session {
        provider: Provider::Codex,
        session_id: id.to_owned(),
        name: Some(name.to_owned()),
        cwd: Some("/project".to_owned()),
        branch: Some("main".to_owned()),
        transcript_path: transcript.map(|path| path.to_string_lossy().into_owned()),
        tmux_session: None,
        tmux_pane: None,
        root_pid: None,
        status: Status::Working,
        unread: false,
        model: None,
        source: "fixture".to_owned(),
        managed: true,
        error: None,
        attention_reason: None,
        created_at: 1.0,
        updated_at: 2.0,
        last_event_at: 2.0,
        last_activity_at: 2.0,
        live: true,
        attached: false,
        home_state: "missing".to_owned(),
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

fn node(id: &str, alias: &str) -> FleetNode {
    FleetNode {
        node_id: id.to_owned(),
        alias: alias.to_owned(),
        ssh_target: alias.to_owned(),
        sources: vec!["explicit".to_owned()],
        status: "ready".to_owned(),
        protocol_version: Some(PROTOCOL_VERSION),
        package_version: Some("0.6.0-alpha.1".to_owned()),
        capabilities: CAPABILITIES
            .iter()
            .map(|value| (*value).to_owned())
            .collect(),
        last_seen: 0.0,
        last_attempt_at: 0.0,
        last_error: None,
        created_at: 1.0,
        updated_at: 1.0,
    }
}

fn profile(id: &str, scope: &str, updated_at: f64) -> ExpertProfile {
    ExpertProfile {
        provider: Provider::Codex,
        session_id: id.to_owned(),
        summary: scope.to_owned(),
        current_state: "Reviewing rollout".to_owned(),
        topics: vec!["pricing".to_owned()],
        artifacts: vec!["/reports/result.md".to_owned()],
        source: "interview".to_owned(),
        updated_at,
        scope_updated_at: updated_at,
        current_state_updated_at: updated_at,
    }
}

fn snapshot(node_id: &str, alias: &str, scope: &str, availability: &str) -> Value {
    let remote = session(THREAD_ID, "remote_pricing", None);
    let future = 4_000_000_000.0;
    json!({
        "type":"snapshot", "protocol":PROTOCOL_NAME, "version":PROTOCOL_VERSION,
        "node_id":node_id, "machine":alias, "captured_at":now(),
        "sessions":[],
        "expert_sessions":[session_to_wire(&remote, true)],
        "profiles":[{
            "provider":"codex", "session_id":THREAD_ID, "scope":scope,
            "current_state":"Reviewing rollout", "topics":["pricing"],
            "artifacts":["/reports/result.md"], "updated_at":future,
            "source":"interview", "scope_updated_at":future,
            "current_state_updated_at":future
        }],
        "cards":[{
            "provider":"codex", "session_id":THREAD_ID, "status":"CURRENT",
            "detail":"matches remote source", "watched":false,
            "availability":availability, "current_state_status":"CURRENT"
        }]
    })
}

fn isolated_paths(root: &Path) -> Paths {
    let config_dir = root.join("config");
    let state_dir = root.join("state");
    Paths {
        config: config_dir.join("config.json"),
        database: state_dir.join("pika.db"),
        config_dir,
        state_dir,
        codex_home: root.join("codex"),
        claude_home: root.join("claude"),
        opencode_data_home: root.join("opencode-data"),
        opencode_config_home: root.join("opencode-config"),
    }
}

fn seed_codex_source(paths: &Paths, current: &Session) {
    fs::create_dir_all(&paths.codex_home).unwrap();
    let db = Connection::open(paths.codex_home.join("state_fixture.sqlite")).unwrap();
    db.execute_batch(
        "CREATE TABLE threads(\
         id TEXT PRIMARY KEY,name TEXT,cwd TEXT,git_branch TEXT,rollout_path TEXT,model TEXT,\
         created_at INTEGER,updated_at INTEGER,archived INTEGER);",
    )
    .unwrap();
    db.execute(
        "INSERT INTO threads VALUES(?,?,?,?,?,?,?,?,0)",
        params![
            current.session_id,
            current.name,
            current.cwd,
            current.branch,
            current.transcript_path,
            Option::<String>::None,
            1_i64,
            2_i64,
        ],
    )
    .unwrap();
}

fn command(root: &Path, marker: &Path) -> Command {
    let fake_bin = root.join("bin");
    fs::create_dir_all(&fake_bin).unwrap();
    let ssh = fake_bin.join("ssh");
    fs::write(
        &ssh,
        format!(
            "#!/bin/sh\nprintf called > '{}'\nexit 91\n",
            marker.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();
    let mut result = Command::cargo_bin("pika").unwrap();
    result
        .env("HOME", root.join("home"))
        .env("PIKA_CONFIG_HOME", root.join("config"))
        .env("PIKA_STATE_HOME", root.join("state"))
        .env("CODEX_HOME", root.join("codex"))
        .env("CLAUDE_CONFIG_DIR", root.join("claude"))
        .env("OPENCODE_DATA_HOME", root.join("opencode-data"))
        .env("OPENCODE_CONFIG_DIR", root.join("opencode-config"))
        .env(
            "PATH",
            format!(
                "{}:{}",
                fake_bin.display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        );
    result
}

#[test]
fn equal_provider_uuid_on_two_nodes_stays_distinct_and_stale_truth_is_preserved() {
    let root = TempDir::new().unwrap();
    let store = Store::at(root.path().join("pika.db"));
    store.initialize().unwrap();
    let first_id = Uuid::new_v4().to_string();
    let second_id = Uuid::new_v4().to_string();
    store.upsert_fleet_node(&node(&first_id, "atlas")).unwrap();
    store
        .put_remote_snapshot(
            &first_id,
            &snapshot(
                &first_id,
                "atlas",
                "Owns pricing infrastructure",
                "source-available",
            ),
            now(),
        )
        .unwrap();
    store
        .upsert_fleet_node(&node(&second_id, "borealis"))
        .unwrap();
    let mut legacy = snapshot(
        &second_id,
        "borealis",
        "Owns pricing research",
        "source-available",
    );
    legacy["profiles"][0]["current_state"] = json!("");
    legacy["cards"][0]["current_state_status"] = json!("MISSING");
    store
        .put_remote_snapshot(&second_id, &legacy, now())
        .unwrap();
    store
        .mark_fleet_node_error(&second_id, "unreachable", "offline")
        .unwrap();

    let found = FleetManager::new(&store, SshTransport::default())
        .expert_matches("pricing")
        .unwrap();
    assert_eq!(found.len(), 2);
    assert_eq!(found[0].node_id.as_deref(), Some(first_id.as_str()));
    assert_eq!(found[0].qualified_name, format!("{THREAD_ID}@atlas"));
    assert_eq!(found[0].availability, "source-available");
    assert!(!found[0].watched);
    assert_eq!(found[1].node_id.as_deref(), Some(second_id.as_str()));
    assert_eq!(found[1].qualified_name, format!("{THREAD_ID}@borealis"));
    assert_eq!(found[1].availability, "machine-unreachable");
    assert_eq!(found[1].freshness.current_state_status, CardStatus::Unknown);
}

#[test]
fn experts_cli_merges_local_and_cached_remote_with_exact_json_and_no_ssh() {
    let root = TempDir::new().unwrap();
    let paths = isolated_paths(root.path());
    let marker = root.path().join("ssh-called");
    let transcript = root.path().join("local.jsonl");
    fs::write(&transcript, "fixture\n").unwrap();
    let local = session(THREAD_ID, "local_pricing", Some(&transcript));
    seed_codex_source(&paths, &local);
    let store = Store::at(&paths.database);
    store.upsert_session(&local, true).unwrap();
    let fingerprint = pikamux::experts::transcript_fingerprint(&local)
        .unwrap()
        .unwrap();
    let future = 4_000_000_000.0;
    store
        .put_expert_profile(&StoredExpertProfile {
            profile: profile(THREAD_ID, "Owns local pricing", future),
            transcript_mtime_ns: Some(fingerprint.checkpoint),
            transcript_size: Some(fingerprint.size),
            current_state_mtime_ns: Some(fingerprint.checkpoint),
            current_state_size: Some(fingerprint.size),
        })
        .unwrap();
    let remote_thread = "22222222-2222-4222-8222-222222222222";
    let remote_id = Uuid::new_v4().to_string();
    let mut remote_snapshot = snapshot(
        &remote_id,
        "atlas",
        "Owns remote pricing",
        "source-available",
    );
    for collection in ["expert_sessions", "profiles", "cards"] {
        remote_snapshot[collection][0]["session_id"] = json!(remote_thread);
    }
    store.upsert_fleet_node(&node(&remote_id, "atlas")).unwrap();
    store
        .put_remote_snapshot(&remote_id, &remote_snapshot, now())
        .unwrap();

    let output = command(root.path(), &marker)
        .args(["experts", "pricing", "--json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!marker.exists(), "metadata-only search must not start SSH");
    let payload: Value = serde_json::from_slice(&output.stdout).unwrap();
    let entries = payload.as_array().unwrap();
    assert_eq!(entries.len(), 2);
    let local_json = entries
        .iter()
        .find(|item| item["session_id"] == THREAD_ID)
        .unwrap();
    let remote_json = entries
        .iter()
        .find(|item| item["session_id"] == remote_thread)
        .unwrap();
    assert_eq!(local_json["qualified_name"], THREAD_ID);
    assert_eq!(local_json["availability"], "source-available");
    assert!(local_json.get("machine").is_none());
    assert_eq!(
        remote_json["qualified_name"],
        format!("{remote_thread}@atlas")
    );
    assert_eq!(remote_json["machine"], "atlas");
    assert_eq!(remote_json["node_id"], remote_id);
    assert_eq!(remote_json["snapshot_stale"], false);
    assert_eq!(remote_json["availability"], "source-available");
    assert_eq!(remote_json["watched"], false);
    let remote_age = remote_json["scope_age_seconds"].as_f64().unwrap();
    assert!((0.0..2.0).contains(&remote_age));
    assert_eq!(remote_json["current_state_status"], "CURRENT");
    let expected_common = BTreeSet::from([
        "artifacts",
        "availability",
        "branch",
        "card_status",
        "current_state",
        "current_state_age_seconds",
        "current_state_status",
        "current_state_updated_at",
        "discoverable",
        "live",
        "matched_on",
        "name",
        "profile_source",
        "profile_updated_at",
        "project",
        "provider",
        "qualified_name",
        "scope",
        "scope_age_seconds",
        "scope_status",
        "scope_updated_at",
        "score",
        "session_id",
        "status",
        "summary",
        "topics",
        "watched",
    ]);
    assert_eq!(
        local_json
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>(),
        expected_common
    );
    let mut expected_remote = expected_common;
    expected_remote.extend([
        "card_detail",
        "machine",
        "node_id",
        "snapshot_seen_at",
        "snapshot_stale",
    ]);
    assert_eq!(
        remote_json
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>(),
        expected_remote
    );
}

#[test]
fn expert_status_includes_unwatched_local_and_cached_remote_cards_without_ssh() {
    let root = TempDir::new().unwrap();
    let paths = isolated_paths(root.path());
    let marker = root.path().join("ssh-called");
    let transcript = root.path().join("local.jsonl");
    fs::write(&transcript, "fixture\n").unwrap();
    let local = session(THREAD_ID, "local_pricing", Some(&transcript));
    seed_codex_source(&paths, &local);
    let store = Store::at(&paths.database);
    store.upsert_session(&local, true).unwrap();
    let fingerprint = pikamux::experts::transcript_fingerprint(&local)
        .unwrap()
        .unwrap();
    store
        .put_expert_profile(&StoredExpertProfile {
            profile: profile(THREAD_ID, "Owns local pricing", now()),
            transcript_mtime_ns: Some(fingerprint.checkpoint),
            transcript_size: Some(fingerprint.size),
            current_state_mtime_ns: Some(fingerprint.checkpoint),
            current_state_size: Some(fingerprint.size),
        })
        .unwrap();
    store.untrack_session(Provider::Codex, THREAD_ID).unwrap();

    let remote_thread = "22222222-2222-4222-8222-222222222222";
    let remote_id = Uuid::new_v4().to_string();
    let mut remote_snapshot = snapshot(
        &remote_id,
        "atlas",
        "Owns remote pricing",
        "source-available",
    );
    for collection in ["expert_sessions", "profiles", "cards"] {
        remote_snapshot[collection][0]["session_id"] = json!(remote_thread);
    }
    store.upsert_fleet_node(&node(&remote_id, "atlas")).unwrap();
    store
        .put_remote_snapshot(&remote_id, &remote_snapshot, now())
        .unwrap();

    let output = command(root.path(), &marker)
        .args(["expert", "status", "--json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!marker.exists(), "cached expert status must not start SSH");
    let payload: Value = serde_json::from_slice(&output.stdout).unwrap();
    let values = payload.as_array().unwrap();
    assert_eq!(values.len(), 2);
    let local = values
        .iter()
        .find(|item| item["session_id"] == THREAD_ID)
        .unwrap();
    assert_eq!(local["watched"], false);
    assert_eq!(local["scope"], "Owns local pricing");
    let remote = values
        .iter()
        .find(|item| item["session_id"] == remote_thread)
        .unwrap();
    assert_eq!(remote["machine"], "atlas");
    assert_eq!(remote["node_id"], remote_id);
    assert_eq!(remote["scope"], "Owns remote pricing");
    assert_eq!(remote["detail"], "matches remote source");
}

#[test]
fn local_cli_and_remote_transport_refuse_unavailable_sources_before_spawn() {
    let root = TempDir::new().unwrap();
    let paths = isolated_paths(root.path());
    let transcript = root.path().join("saved.jsonl");
    fs::write(&transcript, "saved\n").unwrap();
    let saved = session(THREAD_ID, "saved_expert", Some(&transcript));
    let store = Store::at(&paths.database);
    store.upsert_session(&saved, true).unwrap();
    let provider_marker = root.path().join("provider-called");
    let provider = root.path().join("provider");
    fs::write(
        &provider,
        format!(
            "#!/bin/sh\nprintf called > '{}'\nexit 91\n",
            provider_marker.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&provider, fs::Permissions::from_mode(0o700)).unwrap();
    fs::create_dir_all(&paths.codex_home).unwrap();
    fs::write(paths.codex_home.join("state_corrupt.sqlite"), "not sqlite").unwrap();
    fs::create_dir_all(&paths.config_dir).unwrap();
    fs::write(
        &paths.config,
        serde_json::to_vec(&json!({
            "provider_executables":{"codex":provider.to_string_lossy()}
        }))
        .unwrap(),
    )
    .unwrap();
    let ssh_marker = root.path().join("ssh-called");
    let output = command(root.path(), &ssh_marker)
        .args(["ask", THREAD_ID, "question"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("source-unavailable"));
    assert!(String::from_utf8_lossy(&output.stderr).contains("No question was sent"));
    assert!(!provider_marker.exists());
    assert!(!ssh_marker.exists());

    assert_eq!(
        local_source_availability(&paths, &pikamux::config::Config::default(), &saved),
        SourceAvailability::SourceUnavailable
    );

    let remote_node_id = Uuid::new_v4().to_string();
    let remote_node = node(&remote_node_id, "atlas");
    let unavailable = FleetSession {
        node_id: remote_node_id,
        node_name: "atlas".to_owned(),
        session: session(THREAD_ID, "remote", None),
        stale: false,
        remote_error: None,
        seen_at: now(),
        card_status: Some("CURRENT".to_owned()),
        card_detail: Some("matches".to_owned()),
        watched: true,
        availability: Some("source-unavailable".to_owned()),
        scope_updated_at: None,
        current_state_updated_at: None,
        current_state_status: Some("UNKNOWN".to_owned()),
    };
    let error = match RemoteConsultation::open(
        &SshTransport::new(
            &ssh_marker,
            Duration::from_millis(50),
            Duration::from_millis(50),
        ),
        remote_node,
        unavailable,
        ConsultationPolicy {
            consultation_mode: "default".to_owned(),
            model: "gpt-test".to_owned(),
            effort: "low".to_owned(),
        },
        Duration::from_millis(50),
        Duration::from_millis(50),
        Duration::from_millis(50),
    ) {
        Ok(_) => panic!("unavailable remote source started a consultation"),
        Err(error) => error,
    };
    assert_eq!(error.kind, FleetErrorKind::InvalidRequest);
    assert!(error.message.contains("No question was sent"));
    assert!(!ssh_marker.exists());
}
