//! Public opt-in launch through real private MCP/lifecycle helpers; no inference.
#![cfg(unix)]
use serde_json::Value;
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

struct Fixture {
    root: tempfile::TempDir,
    env: Vec<(String, String)>,
    tmux: PathBuf,
    socket: String,
}
impl Fixture {
    fn command(&self, executable: impl AsRef<std::ffi::OsStr>) -> Command {
        let mut command = Command::new(executable);
        command
            .env_clear()
            .envs(self.env.clone())
            .current_dir(self.root.path());
        command
    }
    fn tmux(&self, args: &[&str]) -> std::process::Output {
        self.command(&self.tmux)
            .args(["-L", &self.socket])
            .args(args)
            .output()
            .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.tmux(&["kill-server"]);
    }
}

#[test]
#[ignore = "Requires explicitly approved tmux executable; isolated synthetic Claude, no quota"]
fn public_opt_in_launch_certifies_exact_native_and_preserves_configuration() {
    let tmux = PathBuf::from(
        std::env::var_os("PIKA_TEST_TMUX").expect("Explicit tmux fixture executable"),
    );
    let root = tempfile::Builder::new()
        .prefix("pika-claude-launch-")
        .tempdir_in("/tmp")
        .unwrap();
    let base = fs::canonicalize(root.path()).unwrap();
    let mut env = Vec::new();
    for (key, dir) in [
        ("HOME", "home"),
        ("XDG_CONFIG_HOME", "config"),
        ("XDG_DATA_HOME", "data"),
        ("XDG_STATE_HOME", "state"),
        ("XDG_CACHE_HOME", "cache"),
        ("PIKA_CONFIG_HOME", "pika-config"),
        ("PIKA_STATE_HOME", "pika-state"),
        ("CLAUDE_CONFIG_DIR", "claude"),
        ("CODEX_HOME", "codex"),
        ("TMPDIR", "tmp"),
        ("TMUX_TMPDIR", "tmp"),
    ] {
        let path = base.join(dir);
        fs::create_dir_all(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        env.push((key.into(), path.display().to_string()));
    }
    let bin = base.join("bin");
    fs::create_dir(&bin).unwrap();
    std::os::unix::fs::symlink(fs::canonicalize(&tmux).unwrap(), bin.join("tmux")).unwrap();
    let fake = bin.join("claude");
    fs::write(&fake, r#"#!/bin/sh
identity=
previous=
for argument in "$@"; do
  case "$previous" in --session-id|--resume) identity="$argument";; esac
  previous="$argument"
done
printf '%s\n' "$@" > "$FIXTURE_ROOT/native-argv"
mkdir -p "$CLAUDE_CONFIG_DIR/sessions"
printf '{"kind":"interactive","sessionId":"%s","cwd":"%s","startedAt":1}\n' "$identity" "$FIXTURE_ROOT" > "$CLAUDE_CONFIG_DIR/sessions/$$.json"
mkfifo "$FIXTURE_ROOT/mcp-input"
exec 3<> "$FIXTURE_ROOT/mcp-input"
"$FIXTURE_BINARY" _claude-channel --launch-token "$PIKA_LAUNCH_TOKEN" < "$FIXTURE_ROOT/mcp-input" > "$FIXTURE_ROOT/mcp-output" &
printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"fixture","version":"1"}}}' '{"jsonrpc":"2.0","method":"notifications/initialized"}' >&3
printf '{"hook_event_name":"SessionStart","session_id":"%s","source":"startup"}\n' "$identity" > "$FIXTURE_ROOT/session-event"
"$FIXTURE_BINARY" _claude-session --launch-token "$PIKA_LAUNCH_TOKEN" < "$FIXTURE_ROOT/session-event"
sleep 120
"#).unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).unwrap();
    let socket = format!("pika-claude-{}", uuid::Uuid::new_v4().simple());
    env.extend([
        ("PATH".into(), format!("{}:/usr/bin:/bin", bin.display())),
        ("TERM".into(), "xterm-256color".into()),
        ("PIKA_UPDATE_CHECK".into(), "0".into()),
        ("PIKA_TMUX_SOCKET".into(), socket.clone()),
        (
            "PIKA_DB_PATH".into(),
            base.join("pika.db").display().to_string(),
        ),
        ("FIXTURE_ROOT".into(), base.display().to_string()),
        ("FIXTURE_BINARY".into(), env!("CARGO_BIN_EXE_pika").into()),
    ]);
    let fixture = Fixture {
        root,
        env,
        tmux,
        socket,
    };
    let store = pikamux::store::Store::at(base.join("pika.db"));
    store.initialize().unwrap();
    assert!(
        fixture
            .tmux(&[
                "new-session",
                "-d",
                "-s",
                "starter",
                "-x",
                "140",
                "-y",
                "50",
                "sleep 120"
            ])
            .status
            .success()
    );
    assert!(
        fixture
            .tmux(&["set-option", "-g", "remain-on-exit", "on"])
            .status
            .success()
    );
    let launch = shell_words::join([
        env!("CARGO_BIN_EXE_pika"),
        "--claude-mobile",
        "new",
        "Synthetic Claude shared",
        "--agent",
        "claude",
    ]);
    assert!(
        fixture
            .tmux(&["respawn-pane", "-k", "-t", "starter:0.0", &launch])
            .status
            .success()
    );
    let deadline = Instant::now() + Duration::from_secs(25);
    let (session, binding) = loop {
        // Public inventory reconciles provider-authored registry metadata; the fixture
        // never manufactures a Pika session or recovery certificate directly.
        let refreshed = fixture
            .command(env!("CARGO_BIN_EXE_pika"))
            .args(["list", "--json"])
            .output()
            .unwrap();
        assert!(
            refreshed.status.success(),
            "{}",
            String::from_utf8_lossy(&refreshed.stderr)
        );
        if let Some(session) = store
            .list_sessions()
            .unwrap()
            .into_iter()
            .find(|s| s.name.as_deref() == Some("Synthetic Claude shared"))
        {
            let path = base
                .join("pika-state/claude-shared")
                .join(format!("{}.json", session.provider_thread_id()));
            if let Ok(bytes) = fs::read(path) {
                let binding: Value = serde_json::from_slice(&bytes).unwrap();
                if binding["ready"] == true {
                    break (session, binding);
                }
            }
        }
        assert!(
            Instant::now() < deadline,
            "Launch did not become ready: pending={:?}, screen={}, native={}",
            store.list_pending().unwrap(),
            String::from_utf8_lossy(
                &fixture
                    .tmux(&["capture-pane", "-p", "-t", "starter:0.0", "-S", "-100"])
                    .stdout
            ),
            String::from_utf8_lossy(
                &fixture
                    .tmux(&["capture-pane", "-p", "-t", "%1", "-S", "-100"])
                    .stdout
            )
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    let owner = store
        .get_recovery_owner(session.provider, session.provider_thread_id())
        .unwrap()
        .unwrap();
    assert_eq!(binding["thread"], session.provider_thread_id());
    assert_eq!(binding["native_pid"], owner.pid);
    assert_eq!(binding["native_start"], owner.start_time);
    assert_eq!(binding["token"], owner.launch_token);
    assert!(binding["mcp_pid"].as_i64().unwrap() > 0);
    let generation = pikamux::process::process_generation(owner.pid).unwrap();
    assert_eq!(generation.start_time, owner.start_time as u64);
    let argv = fs::read_to_string(base.join("native-argv")).unwrap();
    assert!(argv.contains("--dangerously-load-development-channels"));
    assert!(argv.contains("--session-id"));
    for forbidden in [
        "--allowedTools",
        "--strict-mcp-config",
        "--debug-file",
        "--dangerously-skip-permissions",
    ] {
        assert!(!argv.contains(forbidden));
    }
    assert!(
        fs::read_to_string(base.join("mcp-output"))
            .unwrap()
            .contains("claude/channel")
    );
    assert!(!base.join("claude/settings.json").exists());
    assert!(store.list_pending().unwrap().is_empty());
    let identity = serde_json::json!({"nodeId":store.ensure_local_node_id().unwrap(),"provider":"claude","threadId":session.provider_thread_id()});
    let mut phone = fixture
        .command(env!("CARGO_BIN_EXE_pika"))
        .arg("_mobile")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let output = phone.stdout.take().unwrap();
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(output).lines().map_while(Result::ok) {
            if let Ok(frame) = serde_json::from_str::<Value>(&line) {
                let _ = sender.send(frame);
            }
        }
    });
    let request_id = uuid::Uuid::new_v4().to_string();
    writeln!(phone.stdin.as_mut().unwrap(),"{}",serde_json::json!({"v":1,"id":request_id,"method":"conversation/open","params":{"identity":identity}})).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let opened = loop {
        let frame = receiver
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap();
        if frame["id"] == request_id {
            break frame;
        }
    };
    let _ = phone.kill();
    let _ = phone.wait();
    assert_eq!(opened["result"]["capabilities"]["send"], true, "{opened}");
    assert_eq!(
        opened["result"]["turns"]["data"],
        serde_json::json!([]),
        "{opened}"
    );
}
