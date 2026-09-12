use assert_cmd::Command;
use predicates::prelude::*;

#[cfg(unix)]
use pikamux::{
    model::{Provider, Session, Status},
    store::Store,
};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

#[cfg(unix)]
fn wait_session(unread: bool) -> Session {
    Session {
        provider: Provider::Codex,
        session_id: "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa".into(),
        name: Some("research_thread".into()),
        cwd: None,
        branch: None,
        transcript_path: None,
        tmux_session: None,
        tmux_pane: None,
        root_pid: None,
        status: Status::Ready,
        unread,
        model: None,
        source: "test".into(),
        managed: true,
        error: None,
        attention_reason: Some("result ready".into()),
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
        active_thread_id: None,
    }
}

#[cfg(unix)]
fn wait_command(unread: bool) -> (tempfile::TempDir, Command) {
    let temp = tempfile::tempdir().unwrap();
    let state = temp.path().join("state");
    let fake_bin = temp.path().join("bin");
    std::fs::create_dir_all(&fake_bin).unwrap();
    let tmux = fake_bin.join("tmux");
    std::fs::write(&tmux, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&tmux, std::fs::Permissions::from_mode(0o700)).unwrap();
    Store::at(state.join("pika.db"))
        .upsert_session(&wait_session(unread), true)
        .unwrap();
    let mut command = Command::cargo_bin("pika").unwrap();
    let path = format!(
        "{}:{}",
        fake_bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    command
        .env("HOME", temp.path().join("home"))
        .env("PIKA_CONFIG_HOME", temp.path().join("config"))
        .env("PIKA_STATE_HOME", &state)
        .env("CODEX_HOME", temp.path().join("codex"))
        .env("CLAUDE_CONFIG_DIR", temp.path().join("claude"))
        .env("OPENCODE_DATA_HOME", temp.path().join("opencode"))
        .env("PATH", path);
    (temp, command)
}

#[test]
fn version_is_native_and_side_effect_free() {
    let temp = tempfile::tempdir().unwrap();
    let mut command = Command::cargo_bin("pika").unwrap();
    command
        .env("PIKA_CONFIG_HOME", temp.path().join("config"))
        .env("PIKA_STATE_HOME", temp.path().join("state"))
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains("pika 0.6.0-alpha.1"));
    assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 0);
}

#[test]
fn empty_list_does_not_create_state() {
    let temp = tempfile::tempdir().unwrap();
    Command::cargo_bin("pika")
        .unwrap()
        .env("PIKA_CONFIG_HOME", temp.path().join("config"))
        .env("PIKA_STATE_HOME", temp.path().join("state"))
        .args(["list", "--json"])
        .assert()
        .success()
        .stdout("[]\n");
    assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 0);
}

#[cfg(unix)]
#[test]
fn wait_matches_only_unread_ready_and_uses_python_timeout_code_124() {
    let (_temp, mut command) = wait_command(false);
    command
        .args([
            "wait",
            "research_thread",
            "--for",
            "ready",
            "--timeout",
            "0",
        ])
        .assert()
        .code(124)
        .stderr(predicate::str::contains(
            "Timed out waiting for research_thread",
        ));

    let (_temp, mut command) = wait_command(true);
    command
        .args([
            "wait",
            "research_thread",
            "--for",
            "ready",
            "--timeout",
            "0",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "research_thread: READY — result ready",
        ));
}
