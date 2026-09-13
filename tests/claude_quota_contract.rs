#![cfg(unix)]
use assert_cmd::Command;
use serde_json::json;
use std::{
    fs,
    time::{SystemTime, UNIX_EPOCH},
};

#[allow(deprecated)]
fn command(root: &std::path::Path) -> Command {
    let mut cmd = Command::cargo_bin("pika").unwrap();
    cmd.env("HOME", root)
        .env("CLAUDE_CONFIG_DIR", root.join("claude"))
        .env("PIKA_CONFIG_HOME", root.join("config"))
        .env("PIKA_STATE_HOME", root.join("state"))
        .env("PIKA_DB_PATH", root.join("state/pika.db"));
    cmd
}

#[test]
fn statusline_forwards_original_bytes_and_persists_quota_only_without_opening_pika_state() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("claude")).unwrap();
    let reset = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        + 86400;
    let payload = format!(
        "{}\n",
        json!({"rate_limits":{"seven_day":{"used_percentage":28,"resets_at":reset}},"transcript_path":"private-transcript","session_id":"private-session"})
    );
    command(root.path())
        .args(["_claude-statusline", "--forward", "/bin/cat"])
        .write_stdin(payload.clone())
        .assert()
        .success()
        .stdout(payload);
    let saved = fs::read_to_string(root.path().join("claude/.pika-quota.json")).unwrap();
    assert!(!saved.contains("private-"));
    assert!(saved.contains("28.0"));
    assert!(!root.path().join("state").exists());
}

#[test]
fn missing_telemetry_is_silent_and_does_not_replace_existing_display() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("claude")).unwrap();
    command(root.path())
        .args([
            "_claude-statusline",
            "--forward",
            "printf 'existing display\\n'",
        ])
        .write_stdin("{}")
        .assert()
        .success()
        .stdout("existing display\n");
    command(root.path())
        .arg("_claude-statusline")
        .write_stdin("{}")
        .assert()
        .success()
        .stdout("");
    assert!(!root.path().join("claude/.pika-quota.json").exists());
}

#[test]
fn early_exiting_statusline_preserves_output_and_exit_status_without_reading_input() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join("claude")).unwrap();
    // Exceed the pipe capacity so a non-reading child reliably closes it while
    // the writer still has bytes, rather than relying on scheduler timing.
    let payload = format!("{{}}{}", " ".repeat(512 * 1024));
    command(root.path())
        .args([
            "_claude-statusline",
            "--forward",
            "exec sh -c 'printf visible; printf warning >&2; exit 17'",
        ])
        .write_stdin(payload)
        .assert()
        .code(17)
        .stdout("visible")
        .stderr("warning");
    assert!(!root.path().join("state").exists());
}
