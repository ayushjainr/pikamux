//! A stale, identity-less launch must not strand a separately saved conversation.
#![cfg(unix)]

use pikamux::{
    hooks::handle_process_exit,
    model::{Provider, Session, Status},
    store::{LaunchPhase, PendingLaunch, Store},
};
use std::{
    fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Command as ProcessCommand, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

const SAVED_UUID: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
const OTHER_UUID: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct Journey {
    root: tempfile::TempDir,
    env: Vec<(String, String)>,
    script_executable: PathBuf,
    tmux_executable: PathBuf,
    tmux_socket: String,
    tmux_tmpdir: PathBuf,
}

impl Journey {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let base = fs::canonicalize(root.path()).unwrap();
        let inherited_path = std::env::var("PATH").unwrap_or_default();
        let tmux_executable = std::env::split_paths(&inherited_path)
            .map(|directory| directory.join("tmux"))
            .find(|candidate| candidate.is_file())
            .expect("tmux executable on PATH");
        let script_executable = std::env::split_paths(&inherited_path)
            .map(|directory| directory.join("script"))
            .find(|candidate| candidate.is_file())
            .expect("script PTY driver on PATH");
        let mut env = Vec::new();
        for (key, dirname) in [
            ("HOME", "home"),
            ("XDG_CONFIG_HOME", "config"),
            ("XDG_DATA_HOME", "data"),
            ("XDG_STATE_HOME", "state"),
            ("XDG_CACHE_HOME", "cache"),
            ("PIKA_CONFIG_HOME", "pika-config"),
            ("PIKA_STATE_HOME", "pika-state"),
            ("CODEX_HOME", "codex"),
            ("CLAUDE_CONFIG_DIR", "claude"),
            ("OPENCODE_DATA_HOME", "opencode"),
            ("OPENCODE_CONFIG_DIR", "opencode-config"),
            ("TMPDIR", "tmp"),
            ("TMUX_TMPDIR", "tmux"),
        ] {
            let path = base.join(dirname);
            fs::create_dir_all(&path).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
            env.push((key.to_owned(), path.display().to_string()));
        }
        let bin = base.join("bin");
        fs::create_dir_all(&bin).unwrap();
        let log = base.join("codex-launches");
        let codex = bin.join("codex");
        fs::write(
            &codex,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {}\nwhile :; do sleep 1; done\n",
                shell_words::quote(&log.display().to_string())
            ),
        )
        .unwrap();
        fs::set_permissions(&codex, fs::Permissions::from_mode(0o700)).unwrap();

        let tmux_socket = format!("pika-stale-pending-{}", uuid::Uuid::new_v4().simple());
        let tmux_tmpdir = base.join("tmux");
        env.extend([
            (
                "PATH".into(),
                format!("{}:{}", bin.display(), inherited_path),
            ),
            ("TERM".into(), "xterm-256color".into()),
            ("PIKA_UPDATE_CHECK".into(), "0".into()),
            (
                "PIKA_DB_PATH".into(),
                base.join("state/pika.db").display().to_string(),
            ),
            ("PIKA_TMUX_SOCKET".into(), tmux_socket.clone()),
        ]);
        Self {
            root,
            env,
            script_executable,
            tmux_executable,
            tmux_socket,
            tmux_tmpdir,
        }
    }

    fn base(&self) -> &Path {
        self.root.path()
    }

    fn tmux(&self, args: &[&str]) -> Output {
        ProcessCommand::new(&self.tmux_executable)
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("TMPDIR", self.base().join("tmp"))
            .env("TMUX_TMPDIR", &self.tmux_tmpdir)
            .args(["-L", &self.tmux_socket])
            .args(args)
            .output()
            .unwrap()
    }

    fn spawn_open(&self) -> std::process::Child {
        let mut command = ProcessCommand::new(&self.script_executable);
        command
            .env_clear()
            .envs(self.env.clone())
            .current_dir(self.base());
        #[cfg(target_os = "linux")]
        {
            let pika_command =
                shell_words::join([env!("CARGO_BIN_EXE_pika"), "open", "ts_monthly"]);
            command.args(["-q", "-e", "-c", &pika_command, "/dev/null"]);
        }
        #[cfg(not(target_os = "linux"))]
        command.args([
            "-q",
            "/dev/null",
            env!("CARGO_BIN_EXE_pika"),
            "open",
            "ts_monthly",
        ]);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    }

    fn launch_log(&self) -> String {
        fs::read_to_string(self.base().join("codex-launches")).unwrap_or_default()
    }
}

impl Drop for Journey {
    fn drop(&mut self) {
        let _ = self.tmux(&["kill-server"]);
    }
}

fn session(id: &str, name: &str, cwd: &Path) -> Session {
    Session {
        provider: Provider::Codex,
        session_id: id.into(),
        name: Some(name.into()),
        cwd: Some(cwd.display().to_string()),
        branch: None,
        transcript_path: None,
        tmux_session: None,
        tmux_pane: None,
        root_pid: None,
        status: Status::Ready,
        unread: true,
        model: None,
        source: "codex".into(),
        managed: true,
        error: None,
        attention_reason: Some("result ready".into()),
        created_at: 1.0,
        updated_at: 2.0,
        last_event_at: 2.0,
        last_activity_at: 2.0,
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

fn pending(token: &str, name: &str, cwd: &Path, pid: i64) -> PendingLaunch {
    PendingLaunch {
        launch_token: token.into(),
        provider: Provider::Codex,
        name: name.into(),
        cwd: cwd.display().to_string(),
        tmux_session: Some(format!("pika-stale-{token}")),
        tmux_pane: Some("%99999".into()),
        expected_session_id: None,
        root_pid: Some(pid),
        root_pid_start: Some(1),
        preexisting_session_ids: Some(vec![]),
        candidate_session_id: None,
        candidate_observed_at: None,
        created_at: 1.0,
    }
}

fn add_authenticated_dead_pending(store: &Store, cwd: &Path, token: &str, pid: i64) {
    store
        .add_pending(&pending(token, "ts_monthly", cwd, pid))
        .unwrap();
    store
        .set_launch_phase(token, LaunchPhase::PanePrepared)
        .unwrap();
    let owner_token = format!("fixture-owner-{token}");
    assert!(
        store
            .register_pending_wrapper_owner(token, &owner_token)
            .unwrap()
    );
    store
        .set_launch_phase(token, LaunchPhase::ProviderStarting)
        .unwrap();
    assert!(
        handle_process_exit(
            store,
            Provider::Codex,
            0,
            None,
            Some(token),
            Some(&owner_token),
            2.0,
        )
        .unwrap()
    );
}

fn run_open(journey: &Journey) -> Output {
    let mut child = journey.spawn_open();
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let clients = journey.tmux(&["list-clients", "-F", "#{client_name}"]);
        if clients.status.success() && !clients.stdout.is_empty() {
            // The tmux socket is private to this fixture; send its normal detach
            // key sequence so the open command returns without closing the provider.
            child.stdin.as_mut().unwrap().write_all(b"\x02d").unwrap();
            break;
        }
        if let Some(status) = child.try_wait().unwrap() {
            let output = child.wait_with_output().unwrap();
            return Output {
                status,
                stdout: output.stdout,
                stderr: output.stderr,
            };
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("pika did not reach an isolated tmux attach before the deadline");
        }
        thread::sleep(Duration::from_millis(25));
    }
    let completion_deadline = Instant::now() + Duration::from_secs(10);
    while child.try_wait().unwrap().is_none() && Instant::now() < completion_deadline {
        thread::sleep(Duration::from_millis(20));
    }
    if child.try_wait().unwrap().is_none() {
        let _ = child.kill();
        let _ = child.wait();
        panic!("pika open did not return after its isolated tmux client detached");
    }
    child.wait_with_output().unwrap()
}

#[test]
fn stale_identityless_pending_resumes_saved_uuid_once_and_preserves_neighbors() {
    let _guard = TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let journey = Journey::new();
    let cwd = journey.base();
    let store = Store::at(journey.base().join("state/pika.db"));
    store.initialize().unwrap();
    store
        .upsert_session(&session(SAVED_UUID, "ts_monthly", cwd), true)
        .unwrap();
    store
        .upsert_session(&session(OTHER_UUID, "other_conversation", cwd), true)
        .unwrap();
    // This old launch has no expected UUID, a dead root generation, and no surviving pane.
    add_authenticated_dead_pending(&store, cwd, "old-dead-launch", i64::MAX - 1);
    store
        .add_pending(&pending(
            "unrelated-launch",
            "other_pending",
            cwd,
            i64::MAX - 2,
        ))
        .unwrap();
    let first = run_open(&journey);
    assert!(
        first.status.success(),
        "open should recover the saved conversation, not stop at stale pending state; stdout={} stderr={}",
        String::from_utf8_lossy(&first.stdout),
        String::from_utf8_lossy(&first.stderr)
    );
    let starts = journey.launch_log();
    assert_eq!(
        starts.lines().count(),
        1,
        "one exact provider launch expected: {starts:?}"
    );
    assert!(
        starts
            .lines()
            .next()
            .unwrap()
            .split_whitespace()
            .any(|arg| arg == SAVED_UUID),
        "Codex must resume the saved UUID, not create a replacement: {starts:?}"
    );

    let after_first = Store::at(journey.base().join("state/pika.db"));
    let pending = after_first.list_pending().unwrap();
    assert_eq!(
        pending.len(),
        1,
        "only unrelated pending handle should remain: {pending:?}"
    );
    assert_eq!(pending[0].launch_token, "unrelated-launch");
    assert!(
        after_first
            .get_session(Provider::Codex, SAVED_UUID)
            .unwrap()
            .is_some()
    );
    let unrelated = after_first
        .get_session(Provider::Codex, OTHER_UUID)
        .unwrap()
        .unwrap();
    assert_eq!(unrelated.name.as_deref(), Some("other_conversation"));
    assert_eq!(unrelated.session_id, OTHER_UUID);

    // A second normal open must attach to the exact resumed home, never launch again.
    let second = run_open(&journey);
    assert!(
        second.status.success(),
        "repeat open should attach to exact resumed home; stdout={} stderr={}",
        String::from_utf8_lossy(&second.stdout),
        String::from_utf8_lossy(&second.stderr)
    );
    assert_eq!(
        journey.launch_log().lines().count(),
        1,
        "reopening must attach, never launch a second Codex client"
    );
    assert_eq!(
        Store::at(journey.base().join("state/pika.db"))
            .list_pending()
            .unwrap()
            .iter()
            .map(|item| item.launch_token.as_str())
            .collect::<Vec<_>>(),
        ["unrelated-launch"]
    );
}

#[test]
fn concurrent_opens_of_identityless_dead_pending_create_one_exact_home() {
    let _guard = TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let journey = Journey::new();
    let cwd = journey.base();
    let store = Store::at(journey.base().join("state/pika.db"));
    store.initialize().unwrap();
    store
        .upsert_session(&session(SAVED_UUID, "ts_monthly", cwd), true)
        .unwrap();
    add_authenticated_dead_pending(&store, cwd, "concurrent-old-launch", i64::MAX - 4);

    let mut children = vec![journey.spawn_open(), journey.spawn_open()];
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut detach_sent = false;
    loop {
        let clients = journey.tmux(&["list-clients", "-F", "#{client_name}"]);
        if !detach_sent && clients.status.success() && !clients.stdout.is_empty() {
            // Both callers receive the detach sequence. A losing caller may already
            // have exited; a winner still in attach detaches without ending Codex.
            for child in &mut children {
                if let Some(stdin) = child.stdin.as_mut() {
                    let _ = stdin.write_all(b"\x02d");
                }
            }
            detach_sent = true;
        }
        if children
            .iter_mut()
            .all(|child| child.try_wait().unwrap().is_some())
        {
            break;
        }
        if Instant::now() >= deadline {
            for child in &mut children {
                let _ = child.kill();
                let _ = child.wait();
            }
            panic!("concurrent opens did not settle before the bounded deadline");
        }
        thread::sleep(Duration::from_millis(25));
    }
    let outputs = children
        .into_iter()
        .map(|child| child.wait_with_output().unwrap())
        .collect::<Vec<_>>();
    assert!(
        outputs.iter().any(|output| output.status.success()),
        "at least one explicit open should establish the exact home: {outputs:?}"
    );
    let starts = journey.launch_log();
    assert_eq!(
        starts.lines().count(),
        1,
        "concurrent opens may launch only one Codex client: {starts:?}"
    );
    assert!(
        starts
            .lines()
            .next()
            .unwrap()
            .split_whitespace()
            .any(|arg| arg == SAVED_UUID),
        "the one launch must resume the saved UUID: {starts:?}"
    );

    let recovered = Store::at(journey.base().join("state/pika.db"))
        .get_session(Provider::Codex, SAVED_UUID)
        .unwrap()
        .unwrap();
    assert!(
        recovered.tmux_pane.is_some(),
        "the exact pane should be recorded"
    );
    assert!(
        recovered.root_pid.is_some(),
        "the exact provider root should be recorded"
    );
    let panes = journey.tmux(&[
        "list-panes",
        "-a",
        "-F",
        "#{pane_id}|#{@pika_provider}|#{@pika_session_id}|#{pane_dead}",
    ]);
    assert!(
        panes.status.success(),
        "{}",
        String::from_utf8_lossy(&panes.stderr)
    );
    let owned = String::from_utf8_lossy(&panes.stdout)
        .lines()
        .filter(|row| {
            let fields = row.split('|').collect::<Vec<_>>();
            fields.len() == 4 && fields[1] == "codex" && fields[2] == SAVED_UUID && fields[3] == "0"
        })
        .count();
    assert_eq!(
        owned,
        1,
        "there must be one live Pika pane bound to the saved UUID: {}",
        String::from_utf8_lossy(&panes.stdout)
    );
}

#[test]
fn dead_root_without_authenticated_exit_remains_fail_closed() {
    let _guard = TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let journey = Journey::new();
    let cwd = journey.base();
    let store = Store::at(journey.base().join("state/pika.db"));
    store.initialize().unwrap();
    store
        .upsert_session(&session(SAVED_UUID, "ts_monthly", cwd), true)
        .unwrap();
    store
        .add_pending(&pending(
            "uncertain-launch",
            "ts_monthly",
            cwd,
            i64::MAX - 3,
        ))
        .unwrap();

    let output = run_open(&journey);
    assert!(
        !output.status.success(),
        "root death alone must not authorize a replacement launch"
    );
    let stderr = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        stderr.contains("no authenticated launch-exit receipt"),
        "the failure should explain why recovery is uncertain: {stderr}"
    );
    assert!(
        journey.launch_log().is_empty(),
        "no provider may start without exit proof"
    );
    let pending = Store::at(journey.base().join("state/pika.db"))
        .list_pending()
        .unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].launch_token, "uncertain-launch");
}
