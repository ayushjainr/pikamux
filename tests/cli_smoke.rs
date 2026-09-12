use assert_cmd::Command;
use predicates::prelude::*;

#[cfg(unix)]
use pikamux::{
    model::{Provider, Session, Status},
    paths::Paths,
    setup,
    store::{HookObservation, PendingLaunch, Store},
};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::path::{Path, PathBuf};

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

#[cfg(unix)]
struct SetupFixture {
    root: tempfile::TempDir,
    fake_bin: PathBuf,
    codex: PathBuf,
    claude: PathBuf,
    opencode: PathBuf,
}

#[cfg(unix)]
impl SetupFixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let fake_bin = root.path().join("bin");
        std::fs::create_dir_all(&fake_bin).unwrap();
        for (name, version) in [
            ("codex", "codex-cli 1.0.0"),
            ("claude", "claude 1.0.0"),
            ("opencode", "opencode 1.18.21"),
        ] {
            write_test_executable(
                &fake_bin.join(name),
                &format!("#!/bin/sh\nprintf '%s\\n' '{version}'\n"),
            );
        }
        write_test_executable(
            &fake_bin.join("tmux"),
            "#!/bin/sh\ncase \"$*\" in *list-panes*) printf '%s\\n' 'no server running' >&2; exit 1;; esac\nexit 0\n",
        );
        write_test_executable(&fake_bin.join("launchctl"), "#!/bin/sh\nexit 0\n");
        write_test_executable(&fake_bin.join("systemctl"), "#!/bin/sh\nexit 0\n");
        Self {
            codex: fake_bin.join("codex"),
            claude: fake_bin.join("claude"),
            opencode: fake_bin.join("opencode"),
            root,
            fake_bin,
        }
    }

    fn paths(&self) -> Paths {
        let config_dir = self.root.path().join("config/pika");
        let state_dir = self.root.path().join("state/pika");
        Paths {
            config: config_dir.join("config.json"),
            config_dir,
            database: state_dir.join("pika.db"),
            state_dir,
            codex_home: self.root.path().join("codex-home"),
            claude_home: self.root.path().join("claude-home"),
            opencode_data_home: self.root.path().join("opencode-data"),
            opencode_config_home: self.root.path().join("opencode-config"),
        }
    }

    fn command(&self) -> Command {
        let paths = self.paths();
        let mut command = Command::cargo_bin("pika").unwrap();
        command
            .env("HOME", self.root.path().join("home"))
            .env("PIKA_CONFIG_HOME", &paths.config_dir)
            .env("PIKA_STATE_HOME", &paths.state_dir)
            .env("PIKA_DB_PATH", &paths.database)
            .env("CODEX_HOME", &paths.codex_home)
            .env("CLAUDE_CONFIG_DIR", &paths.claude_home)
            .env("OPENCODE_DATA_HOME", &paths.opencode_data_home)
            .env("OPENCODE_CONFIG_DIR", &paths.opencode_config_home)
            .env("XDG_CONFIG_HOME", self.root.path().join("xdg-config"))
            .env("XDG_DATA_HOME", self.root.path().join("xdg-data"))
            .env("XDG_STATE_HOME", self.root.path().join("xdg-state"))
            .env("PIKA_TMUX_SOCKET", "pika-setup-contract")
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    self.fake_bin.display(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            );
        command
    }

    fn initial_setup_args(&self) -> Vec<String> {
        vec![
            "setup".into(),
            "--yes".into(),
            "--no-machines".into(),
            "--skip-walkthrough".into(),
            "--codex-executable".into(),
            self.codex.to_string_lossy().into_owned(),
            "--claude-executable".into(),
            self.claude.to_string_lossy().into_owned(),
            "--opencode-executable".into(),
            self.opencode.to_string_lossy().into_owned(),
        ]
    }
}

#[cfg(unix)]
fn write_test_executable(path: &Path, contents: &str) {
    std::fs::write(path, contents).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
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

#[cfg(unix)]
#[test]
fn unavailable_consultation_is_jsonl_only_and_uses_legacy_failure_code() {
    let (_temp, mut command) = wait_command(true);
    let output = command
        .args(["ask", "research_thread", "--jsonl"])
        .write_stdin("{\"close\":true}\n")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let events = String::from_utf8(output.stdout).unwrap();
    assert!(
        events
            .lines()
            .all(|line| serde_json::from_str::<serde_json::Value>(line).is_ok())
    );
    assert!(events.contains("\"type\":\"error\""));
    assert!(events.contains("\"delivery\":\"not_sent\""));
    assert!(events.contains("\"type\":\"closed\""));

    let (_temp, mut missing) = wait_command(true);
    let output = missing
        .args(["ask", "claude:archived-or-missing", "--jsonl"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stderr.is_empty());
    let events = String::from_utf8(output.stdout).unwrap();
    assert!(
        events
            .lines()
            .all(|line| serde_json::from_str::<serde_json::Value>(line).is_ok())
    );
    assert!(events.contains("No exact conversation"));
    assert!(events.contains("\"type\":\"closed\""));
}

#[cfg(unix)]
#[test]
fn setup_separates_proven_names_from_bounded_recent_labels_and_routine_is_quiet() {
    let fixture = SetupFixture::new();
    let paths = fixture.paths();
    std::fs::create_dir_all(&paths.codex_home).unwrap();
    let database = rusqlite::Connection::open(paths.codex_home.join("state_1.sqlite")).unwrap();
    database
        .execute_batch(
            "CREATE TABLE threads(
                id TEXT PRIMARY KEY, name TEXT, cwd TEXT, rollout_path TEXT,
                created_at INTEGER, updated_at INTEGER, archived INTEGER
             );",
        )
        .unwrap();
    for index in 0..25 {
        let identity = format!("10000000-0000-4000-8000-{index:012x}");
        let transcript = paths.codex_home.join(format!("{identity}.jsonl"));
        std::fs::write(
            &transcript,
            format!(
                "{}\n",
                serde_json::json!({"type":"session_meta","payload":{}})
            ),
        )
        .unwrap();
        database
            .execute(
                "INSERT INTO threads VALUES(?1,?2,'/project',?3,1,?4,0)",
                rusqlite::params![
                    identity,
                    format!("provider-label-{index:02}"),
                    transcript.to_string_lossy(),
                    index + 1
                ],
            )
            .unwrap();
    }
    drop(database);

    let claude_sessions = paths.claude_home.join("sessions");
    std::fs::create_dir_all(&claude_sessions).unwrap();
    for (identity, name, source) in [
        (
            "20000000-0000-4000-8000-000000000001",
            "personally_named",
            "custom",
        ),
        (
            "20000000-0000-4000-8000-000000000002",
            "generated-summary",
            "derived",
        ),
    ] {
        std::fs::write(
            claude_sessions.join(format!("{identity}.json")),
            serde_json::to_vec(&serde_json::json!({
                "kind":"interactive", "sessionId":identity, "name":name,
                "nameSource":source, "cwd":"/project", "updatedAt":100
            }))
            .unwrap(),
        )
        .unwrap();
    }

    let mut first = fixture.command();
    let output = first.args(fixture.initial_setup_args()).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let first_output = String::from_utf8_lossy(&output.stdout);
    assert!(first_output.contains("Named conversations available to watch:"));
    assert!(first_output.contains("personally_named"));
    assert!(!first_output.contains("provider-label-24"));
    assert!(!first_output.contains("generated-summary"));

    let mut routine = fixture.command();
    let output = routine
        .args(["setup", "--yes", "--skip-walkthrough"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let routine_output = String::from_utf8_lossy(&output.stdout);
    assert!(routine_output.contains("Reconciled 0 tracked conversation name(s)."));
    assert!(!routine_output.contains("Named conversations available"));
    assert!(!routine_output.contains("Recent unnamed conversations"));
    assert!(!routine_output.contains("personally_named"));
    assert!(!routine_output.contains("machine candidates"));

    let mut browse = fixture.command();
    let output = browse
        .args([
            "setup",
            "--yes",
            "--no-machines",
            "--skip-walkthrough",
            "--browse-all",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let browse_output = String::from_utf8_lossy(&output.stdout);
    assert!(browse_output.contains("personally_named"));
    assert!(browse_output.contains("Recent unnamed conversations"));
    assert!(browse_output.contains("provider-label-24"));
    assert!(!browse_output.contains("provider-label-00"));
    assert!(!browse_output.contains("provider-label-04"));
}

#[cfg(unix)]
#[test]
fn setup_commissioning_requires_matching_observation_and_healthy_pending_launches() {
    let fixture = SetupFixture::new();
    let mut first = fixture.command();
    let output = first.args(fixture.initial_setup_args()).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains(
        "Pika not yet commissioned · missing proof: Codex matching hook event has not been observed."
    ));

    let paths = fixture.paths();
    let store = Store::at(paths.database.clone());
    let pika_binary = assert_cmd::cargo::cargo_bin!("pika")
        .canonicalize()
        .unwrap();
    let fingerprint = setup::hook_spec_fingerprint(Provider::Codex, &pika_binary).unwrap();
    store
        .record_hook_observation(&HookObservation {
            provider: Provider::Codex,
            fingerprint,
            event_name: "SessionStart".into(),
            session_id: "30000000-0000-4000-8000-000000000001".into(),
            observed_at: 1.0,
            source: Some("test-fixture".into()),
            managed: true,
        })
        .unwrap();

    let mut proven = fixture.command();
    let output = proven
        .args(["setup", "--yes", "--skip-walkthrough"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains(
        "Pika commissioned · required integrations are compatible, active, observed, and healthy."
    ));

    store
        .add_pending(&PendingLaunch {
            launch_token: "overdue-launch".into(),
            provider: Provider::Codex,
            name: "unfinished_identity".into(),
            cwd: "/project".into(),
            tmux_session: None,
            tmux_pane: None,
            expected_session_id: None,
            root_pid: None,
            root_pid_start: None,
            preexisting_session_ids: Some(Vec::new()),
            candidate_session_id: None,
            candidate_observed_at: None,
            created_at: 1.0,
        })
        .unwrap();
    let mut degraded = fixture.command();
    let output = degraded
        .args(["setup", "--yes", "--skip-walkthrough"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let degraded_output = String::from_utf8_lossy(&output.stdout);
    assert!(degraded_output.contains("launches DEGRADED · required"));
    assert!(degraded_output.contains("unfinished_identity has awaited exact identity"));
    assert!(!degraded_output.contains("Pika commissioned ·"));
}

#[cfg(unix)]
#[test]
fn setup_never_commissions_an_uncertified_required_provider_version() {
    let fixture = SetupFixture::new();
    write_test_executable(
        &fixture.opencode,
        "#!/bin/sh\nprintf '%s\\n' 'opencode 1.18.20'\n",
    );
    let mut arguments = fixture.initial_setup_args();
    arguments.extend(["--default-provider".into(), "opencode".into()]);
    let mut command = fixture.command();
    let output = command.args(arguments).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let output = String::from_utf8_lossy(&output.stdout);
    assert!(output.contains("OpenCode binary ✗"));
    assert!(output.contains("OpenCode requires version 1.18.21 or newer"));
    assert!(!output.contains("Pika commissioned ·"));
}
