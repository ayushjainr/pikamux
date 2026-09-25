//! Hermetic contract tests for the board-home assistant entry point.
//! No provider, fleet endpoint, tmux server, or installed user state is used.
#![cfg(unix)]

use assert_cmd::Command;
use pikamux::store::Store;
use serde_json::Value;
use std::{fs, path::PathBuf};
use tempfile::TempDir;

struct Fixture {
    root: TempDir,
    operational_db: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        for name in [
            "home", "config", "state", "data", "cache", "codex", "claude", "opencode",
        ] {
            fs::create_dir_all(root.path().join(name)).unwrap();
        }
        Self {
            operational_db: root.path().join("operational.sqlite"),
            root,
        }
    }

    fn command(&self, args: &[&str]) -> Command {
        let root = self.root.path();
        let mut command = Command::cargo_bin("pika").unwrap();
        command
            .env_clear()
            .current_dir(root.join("home"))
            .env("HOME", root.join("home"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_STATE_HOME", root.join("state"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env("PIKA_CONFIG_HOME", root.join("config/pika"))
            .env("PIKA_STATE_HOME", root.join("state"))
            .env("PIKA_DB_PATH", &self.operational_db)
            .env("CODEX_HOME", root.join("codex"))
            .env("CLAUDE_CONFIG_DIR", root.join("claude"))
            .env("OPENCODE_DATA_HOME", root.join("opencode"))
            .env("OPENCODE_CONFIG_DIR", root.join("config/opencode"))
            .env("PATH", "/usr/bin:/bin")
            .args(args);
        command
    }

    fn assistant_root(&self) -> PathBuf {
        self.root.path().join("state/assistant")
    }

    fn assert_host_exited(&self) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while self.assistant_root().join("view.sock").exists()
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            !self.assistant_root().join("view.sock").exists(),
            "foreground host did not exit after its last view"
        );
    }
}

fn json_output(fixture: &Fixture, args: &[&str]) -> Value {
    let output = fixture
        .command(args)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice(&output)
        .unwrap_or_else(|error| panic!("not JSON: {error}: {}", String::from_utf8_lossy(&output)))
}

#[test]
fn two_live_views_share_memory_forget_epoch_and_survive_one_view_closing() {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;
    use std::time::{Duration, Instant};
    struct OwnedHost(std::process::Child);
    impl Drop for OwnedHost {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    fn request(stream: &mut UnixStream, payload: Value) -> Value {
        let mut encoded = serde_json::to_vec(
            &serde_json::json!({"protocol":1,"generation":null,"payload":payload}),
        )
        .unwrap();
        encoded.push(b'\n');
        stream.write_all(&encoded).unwrap();
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).unwrap();
        let reply: Value = serde_json::from_str(&line).unwrap();
        assert!(reply["error"].is_null(), "{reply}");
        reply
    }
    let f = Fixture::new();
    let root = f.assistant_root();
    let command = f.command(&["_assistant-host", "--root", root.to_str().unwrap()]);
    let mut spawn = std::process::Command::new(command.get_program());
    spawn
        .env_clear()
        .args(command.get_args())
        .current_dir(command.get_current_dir().unwrap());
    for (key, value) in command.get_envs() {
        if let Some(value) = value {
            spawn.env(key, value);
        }
    }
    let mut host = OwnedHost(spawn.spawn().unwrap());
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut first = loop {
        if let Ok(socket) = UnixStream::connect(root.join("view.sock")) {
            break socket;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    };
    first
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut second = UnixStream::connect(root.join("view.sock")).unwrap();
    second
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let a = request(
        &mut first,
        serde_json::json!({"operation":"snapshot","scope":"personal"}),
    );
    let b = request(
        &mut second,
        serde_json::json!({"operation":"snapshot","scope":"personal"}),
    );
    assert_eq!(a["generation"], b["generation"]);
    assert_eq!(a["payload"]["profile_id"], b["payload"]["profile_id"]);
    request(
        &mut first,
        serde_json::json!({"operation":"save","scope":"personal","request_id":"two-view-memory","kind":"instruction","body":"Keep me involved in decisions","timestamp":1}),
    );
    let saved = request(
        &mut second,
        serde_json::json!({"operation":"snapshot","scope":"personal"}),
    );
    let records = saved["payload"]["records"].as_array().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["body"], "Keep me involved in decisions");
    request(
        &mut first,
        serde_json::json!({"operation":"forget","record_id":records[0]["id"]}),
    );
    let forgotten = request(
        &mut second,
        serde_json::json!({"operation":"snapshot","scope":"personal"}),
    );
    assert_eq!(forgotten["payload"]["memory_epoch"], 1);
    assert_eq!(forgotten["payload"]["records"], serde_json::json!([]));
    let recovery = request(
        &mut first,
        serde_json::json!({"operation":"fresh_context","request_id":"99999999-9999-4999-8999-999999999999"}),
    );
    assert_eq!(
        recovery["payload"]["recovery_id"],
        "99999999-9999-4999-8999-999999999999"
    );
    let recovery_deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let state = request(
            &mut second,
            serde_json::json!({"operation":"snapshot","scope":"personal"}),
        );
        if state["payload"]["recovery"]["state"] == "completed" {
            break;
        }
        assert!(Instant::now() < recovery_deadline, "{state}");
        std::thread::sleep(Duration::from_millis(10));
    }
    let decision = serde_json::json!({"chosen":"cache snapshots","rationale":"avoid repeated polling","rejected":["poll every view"],"owner":"user","open_questions":["stale reads"],"commitments":["measure freshness"]});
    let saved_decision = request(
        &mut first,
        serde_json::json!({"operation":"save","scope":"personal","request_id":"ipc-decision","kind":"decision","body":decision.to_string(),"timestamp":1}),
    );
    let decision_id = &saved_decision["payload"]["saved"];
    let brief = request(
        &mut second,
        serde_json::json!({"operation":"brief","scope":"personal"}),
    );
    assert_eq!(
        brief["payload"]["brief"]["decisions"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        brief["payload"]["brief"]["commitments"][0]["text"],
        "measure freshness"
    );
    assert!(
        brief["payload"]["local_output"]
            .as_str()
            .unwrap()
            .contains("Open questions / proposals")
    );
    let recall = request(
        &mut second,
        serde_json::json!({"operation":"recall","scope":"personal","record_id":decision_id}),
    );
    assert_eq!(
        recall["payload"]["recall"]["structured"]["rejected"][0],
        "poll every view"
    );
    let explain = request(
        &mut first,
        serde_json::json!({"operation":"explain","scope":"personal","record_id":decision_id,"reply":{"choice":"cache","why":"reduce polling"}}),
    );
    assert!(
        explain["payload"]["local_output"]
            .as_str()
            .unwrap()
            .contains("Which alternative")
    );
    let instruction = request(
        &mut first,
        serde_json::json!({"operation":"save","scope":"personal","request_id":"ipc-instruction","kind":"instruction","body":"Keep answers terse","timestamp":2}),
    );
    request(
        &mut second,
        serde_json::json!({"operation":"correct","scope":"personal","request_id":"ipc-correction","record_id":instruction["payload"]["saved"],"body":"Explain the trade-offs"}),
    );
    let corrected = request(
        &mut first,
        serde_json::json!({"operation":"brief","scope":"personal"}),
    );
    assert_eq!(
        corrected["payload"]["brief"]["instructions"][0]["text"],
        "Explain the trade-offs"
    );
    let excluded = request(
        &mut second,
        serde_json::json!({"operation":"brief","scope":"other"}),
    );
    assert_eq!(
        excluded["payload"]["brief"]["decisions"],
        serde_json::json!([])
    );
    drop(first);
    assert!(host.0.try_wait().unwrap().is_none());
    request(
        &mut second,
        serde_json::json!({"operation":"snapshot","scope":"personal"}),
    );
    drop(second);
    f.assert_host_exited();
    assert!(host.0.wait().unwrap().success());
}

#[test]
fn pika_pika_json_creates_only_private_assistant_state() {
    let fixture = Fixture::new();
    let value = json_output(&fixture, &["pika", "--json"]);
    assert_eq!(value["state"], "not_enabled");
    assert_eq!(value["provider"], "none");
    assert!(fixture.assistant_root().join("memory.sqlite").is_file());
    assert!(
        !fixture.operational_db.exists(),
        "assistant preview touched operational DB"
    );
    assert!(!fixture.root.path().join("config/pika/config.json").exists());
    assert!(fixture.assistant_root().join("owner.lock").is_file());
    fixture.assert_host_exited();
}

#[test]
fn remember_decision_restart_identity_and_scope_are_durable() {
    let fixture = Fixture::new();
    let first = json_output(
        &fixture,
        &[
            "pika",
            "--json",
            "--scope",
            "project-a",
            "--remember",
            "prefer bounded work",
        ],
    );
    let profile = first["profile_id"].as_str().unwrap().to_owned();
    let second = json_output(
        &fixture,
        &[
            "pika",
            "--json",
            "--scope",
            "project-a",
            "--decision",
            "ship only after review",
        ],
    );
    assert_eq!(second["profile_id"], profile);
    let reopened = json_output(&fixture, &["pika", "--json", "--scope", "project-a"]);
    assert_eq!(reopened["profile_id"], profile);
    assert_eq!(reopened["records"].as_array().unwrap().len(), 2);
    let excluded = json_output(&fixture, &["pika", "--json", "--scope", "project-b"]);
    assert!(excluded["records"].as_array().unwrap().is_empty());
}

#[test]
fn control_text_is_rejected_as_scope_and_does_not_leak_to_output() {
    let fixture = Fixture::new();
    fixture
        .command(&["pika", "--json", "--scope", "bad\u{1b}[31m"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("printable"));
}

#[test]
fn explicit_open_keeps_project_thread_named_pika_on_the_existing_parser() {
    let fixture = Fixture::new();
    let store = Store::at(&fixture.operational_db);
    store.initialize().unwrap();
    let db = rusqlite::Connection::open(store.path()).unwrap();
    db.execute("INSERT INTO sessions(provider,session_id,name,status,unread,managed,source,created_at,updated_at,last_event_at,last_activity_at) VALUES ('codex','aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa','pika','PARKED',0,1,'fixture',1,1,1,1)", []).unwrap();
    drop(db);
    let output = fixture
        .command(&["open", "pika"])
        .assert()
        .failure()
        .get_output()
        .clone();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("unexpected argument"),
        "open pika was parsed as a new assistant command: {stderr}"
    );
    assert!(
        !stderr.contains("No exact conversation named"),
        "named project conversation was not resolved: {stderr}"
    );
}

#[test]
fn assistant_preview_never_starts_a_provider_or_fleet_process() {
    let fixture = Fixture::new();
    let value = json_output(&fixture, &["pika", "--json"]);
    assert_eq!(value["background_calls"], 0);
    assert!(!fixture.root.path().join("state/pika.db").exists());
    fixture.assert_host_exited();
}
