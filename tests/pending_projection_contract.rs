#![cfg(unix)]

use assert_cmd::Command;
use pikamux::{
    model::Provider,
    store::{PendingLaunch, Store},
};
use serde_json::Value;
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    process::{Child, Command as StdCommand, Stdio},
    thread,
    time::{Duration, Instant},
};

struct HeldFakeProvider(Child);

impl HeldFakeProvider {
    fn start(
        executable: &std::path::Path,
        marker: &std::path::Path,
        fifo: &std::path::Path,
    ) -> Self {
        let script = format!(
            "#!/bin/sh\nprintf started\\n >> {}\nread _ < {}\n",
            shell_words::quote(&marker.display().to_string()),
            shell_words::quote(&fifo.display().to_string()),
        );
        fs::write(executable, script).unwrap();
        fs::set_permissions(executable, fs::Permissions::from_mode(0o700)).unwrap();
        let child = StdCommand::new(executable)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !marker.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(marker.exists(), "fake provider did not reach its FIFO wait");
        Self(child)
    }

    fn pid(&self) -> i64 {
        i64::from(self.0.id())
    }
}

impl Drop for HeldFakeProvider {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn pending(
    token: &str,
    name: &str,
    root: &std::path::Path,
    tmux_session: Option<&str>,
    tmux_pane: Option<&str>,
) -> PendingLaunch {
    PendingLaunch {
        launch_token: token.into(),
        provider: Provider::Codex,
        name: name.into(),
        cwd: root.display().to_string(),
        tmux_session: tmux_session.map(str::to_owned),
        tmux_pane: tmux_pane.map(str::to_owned),
        expected_session_id: None,
        root_pid: Some(999_999),
        root_pid_start: Some(1),
        preexisting_session_ids: Some(vec![]),
        candidate_session_id: None,
        candidate_observed_at: None,
        created_at: 1.0,
    }
}

fn pane(session: &str, pane_id: &str, pid: i64, dead: bool, token: &str) -> String {
    [
        session.to_owned(),
        pane_id.to_owned(),
        pid.to_string(),
        "/tmp".into(),
        "codex".into(),
        "0".into(),
        "0".into(),
        "0".into(),
        if dead { "1" } else { "0" }.into(),
        String::new(),
        "1".into(),
        "1".into(),
        "codex".into(),
        String::new(),
        String::new(),
        token.to_owned(),
    ]
    .join(&char::from(31).to_string())
}

#[test]
fn cli_lists_pending_liveness_only_from_exact_live_token_bound_runtime_and_never_relaunches() {
    let root = tempfile::tempdir().unwrap();
    let bin = root.path().join("bin");
    let state = root.path().join("state");
    let fifo = root.path().join("provider.fifo");
    let marker = root.path().join("provider-starts");
    fs::create_dir_all(&bin).unwrap();
    assert!(
        std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success()
    );

    let fake_provider = bin.join("codex");
    let provider = HeldFakeProvider::start(&fake_provider, &marker, &fifo);
    let pid = provider.pid();

    let rows = [
        pane("pika-startup", "%dead", pid, true, "dead-pane-token"),
        pane(
            "wrong-session",
            "%wrong-session",
            pid,
            false,
            "wrong-session-token",
        ),
        pane(
            "pika-startup",
            "%wrong-token",
            pid,
            false,
            "different-token",
        ),
        pane("pika-startup", "%live", pid, false, "live-token"),
    ]
    .join("\n");
    let tmux = bin.join("tmux");
    let script = format!(
        "#!/bin/sh\ncase \"$*\" in *list-panes*) printf '%s\\n' {} ;; *) exit 0 ;; esac\n",
        shell_words::quote(&rows)
    );
    fs::write(&tmux, script).unwrap();
    fs::set_permissions(&tmux, fs::Permissions::from_mode(0o700)).unwrap();

    let store = Store::at(state.join("pika.db"));
    let pending = vec![
        pending(
            "missing-token",
            "missing-pane",
            root.path(),
            Some("pika-startup"),
            Some("%missing"),
        ),
        pending(
            "dead-pane-token",
            "dead-pane",
            root.path(),
            Some("pika-startup"),
            Some("%dead"),
        ),
        pending(
            "wrong-session-token",
            "wrong-session",
            root.path(),
            Some("pika-startup"),
            Some("%wrong-session"),
        ),
        pending(
            "wrong-token",
            "wrong-token",
            root.path(),
            Some("pika-startup"),
            Some("%wrong-token"),
        ),
        pending(
            "live-token",
            "live-provider",
            root.path(),
            Some("pika-startup"),
            Some("%live"),
        ),
    ];
    for item in &pending {
        store.add_pending(item).unwrap();
    }

    let output = Command::cargo_bin("pika")
        .unwrap()
        .args(["list", "--json", "--no-usage"])
        .env("HOME", root.path().join("home"))
        .env("PIKA_CONFIG_HOME", root.path().join("config"))
        .env("PIKA_STATE_HOME", &state)
        .env("PIKA_DB_PATH", state.join("pika.db"))
        .env("CODEX_HOME", root.path().join("codex-home"))
        .env("CLAUDE_CONFIG_DIR", root.path().join("claude-home"))
        .env("OPENCODE_DATA_HOME", root.path().join("opencode-data"))
        .env("OPENCODE_CONFIG_DIR", root.path().join("opencode-config"))
        .env("PIKA_TMUX_SOCKET", "pending-projection-contract")
        .env(
            "PATH",
            format!(
                "{}:{}",
                bin.display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let rows: Vec<Value> = serde_json::from_slice(&output).unwrap();
    let live_by_name = rows
        .iter()
        .map(|session| {
            (
                session["name"].as_str().unwrap().to_owned(),
                session["live"].as_bool().unwrap(),
            )
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    assert!(!live_by_name["missing-pane"]);
    assert!(!live_by_name["dead-pane"]);
    assert!(!live_by_name["wrong-session"]);
    assert!(!live_by_name["wrong-token"]);
    assert!(live_by_name["live-provider"]);
    assert!(
        rows.iter()
            .all(|session| session["source"] == "pending-launch")
    );
    assert!(rows.iter().all(|session| {
        session["session_id"]
            .as_str()
            .unwrap()
            .starts_with("pending:")
    }));
    let live_row = rows
        .iter()
        .find(|session| session["name"] == "live-provider")
        .unwrap();
    assert!(
        live_row["error"]
            .as_str()
            .unwrap()
            .contains("provider process is present")
    );

    assert!(store.list_sessions().unwrap().is_empty());
    assert_eq!(store.list_pending().unwrap(), pending);
    assert_eq!(fs::read_to_string(marker).unwrap().lines().count(), 1);
}
