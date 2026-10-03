//! Process-level native hook endpoint contracts, NOT proof of provider invocation.
//! No Codex process, real credentials, model request, fleet or installed state.
use serde_json::{Value, json};
use std::os::unix::{fs::PermissionsExt, net::UnixStream};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

struct Fixture {
    directory: tempfile::TempDir,
    root: PathBuf,
    profile: String,
    session: String,
    generation: String,
    host: Option<Child>,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        for name in [
            "home", "config", "state", "data", "cache", "tmp", "tmux", "codex", "claude",
            "opencode", "muse",
        ] {
            fs::create_dir_all(directory.path().join(name)).unwrap();
        }
        let root = directory.path().join("state/assistant");
        let profile = crate::assistant_memory::Store::open(root.join("memory.sqlite"))
            .unwrap()
            .profile_id()
            .to_owned();
        let session = uuid::Uuid::new_v4().to_string();
        let generation = uuid::Uuid::new_v4().to_string();
        let registry_path = root.join("native-registry/pika.db");
        crate::assistant_storage::database(&registry_path).unwrap();
        let registry = crate::store::Store::at(registry_path);
        registry.initialize().unwrap();
        let candidate = crate::model::Candidate {
            provider: crate::model::Provider::Codex,
            session_id: session.clone(),
            name: Some("Native hook fixture".into()),
            cwd: Some(root.display().to_string()),
            branch: None,
            transcript_path: None,
            model: None,
            updated_at: 1.0,
            live: false,
            pid: None,
            source: "synthetic-fixture".into(),
            parent_session_id: None,
            created_at: 1.0,
            lifecycle_status: None,
        };
        registry
            .upsert_session(&crate::core::session_from_candidate(&candidate), false)
            .unwrap();
        registry
            .bind_launch(&generation, crate::model::Provider::Codex, &session)
            .unwrap();
        private_json(
            &root.join("native-binding.json"),
            &json!({"profile_id":profile,"scope":"personal","launch_token":generation,"thread_id":session,"memory_epoch":0,"provider_executable":"/usr/bin/false"}),
        );
        crate::assistant_native_recovery::record_context(&root, &profile, "personal", &session)
            .unwrap();
        let mut policy =
            crate::assistant_policy::AssistantPolicy::open(root.join("policy.sqlite")).unwrap();
        let mut config = policy.config().unwrap();
        config.max_total_calls = crate::assistant_policy::NO_CALL_LIMIT;
        policy.configure(&config).unwrap();
        Self {
            directory,
            root,
            profile,
            session,
            generation,
            host: None,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(assert_cmd::cargo::cargo_bin("pika"));
        let base = self.directory.path();
        command.env_clear().current_dir(base.join("home"));
        for (name, suffix) in [
            ("HOME", "home"),
            ("XDG_CONFIG_HOME", "config"),
            ("XDG_STATE_HOME", "state"),
            ("XDG_DATA_HOME", "data"),
            ("XDG_CACHE_HOME", "cache"),
            ("TMPDIR", "tmp"),
            ("TMUX_TMPDIR", "tmux"),
            ("PIKA_CONFIG_HOME", "config/pika"),
            ("PIKA_STATE_HOME", "state"),
            ("CODEX_HOME", "codex"),
            ("CLAUDE_CONFIG_DIR", "claude"),
            ("OPENCODE_DATA_HOME", "opencode"),
            ("OPENCODE_CONFIG_DIR", "config/opencode"),
            ("MUSE_DATA_HOME", "muse"),
            ("MUSE_CONFIG_DIR", "config/muse"),
        ] {
            command.env(name, base.join(suffix));
        }
        command
            .env("PATH", "/usr/bin:/bin")
            .env("PIKA_UPDATE_CHECK", "0")
            .env("PIKA_TMUX_SOCKET", "native-hook-disposable")
            .env("PIKA_DB_PATH", self.root.join("native-registry/pika.db"))
            .env(
                "PIKA_ASSISTANT_BOARD_DB_PATH",
                base.join("state/ordinary.sqlite"),
            )
            .env("PIKA_ASSISTANT_PROFILE_ROOT", &self.root)
            .env("PIKA_ASSISTANT_PROFILE_ID", &self.profile)
            .env("PIKA_ASSISTANT_SCOPE", "personal")
            .env("PIKA_ASSISTANT_NATIVE_LAUNCH_TOKEN", &self.generation);
        command
    }

    fn start_host(&mut self) {
        let child = self
            .command()
            .args(["_assistant-host", "--root"])
            .arg(&self.root)
            .args(["--expected-profile-id", &self.profile])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        self.host = Some(child);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if UnixStream::connect(self.root.join("view.sock")).is_ok() {
                break;
            }
            assert!(
                self.host.as_mut().unwrap().try_wait().unwrap().is_none(),
                "fixture host exited"
            );
            assert!(Instant::now() < deadline, "fixture host unavailable");
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn payload(&self, event: &str, prompt: &str, turn: &str) -> Value {
        json!({"hook_event_name":event,"session_id":self.session,"prompt":prompt,"turn_id":turn})
    }

    fn hook(&self, event: &str, bytes: &[u8], override_env: Option<(&str, &str)>) -> Output {
        let mut command = self.command();
        command
            .args(["_assistant-native-hook", "--profile-root"])
            .arg(&self.root)
            .args([
                "--expected-profile-id",
                &self.profile,
                "--scope",
                "personal",
                "--event",
                event,
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some((name, value)) = override_env {
            if value.is_empty() {
                command.env_remove(name);
            } else {
                command.env(name, value);
            }
        }
        let mut child = command.spawn().unwrap();
        child.stdin.take().unwrap().write_all(bytes).unwrap();
        let deadline = Instant::now() + Duration::from_secs(8);
        while child.try_wait().unwrap().is_none() {
            if Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("hook endpoint exceeded bounded fixture deadline");
            }
            thread::sleep(Duration::from_millis(10));
        }
        child.wait_with_output().unwrap()
    }

    fn prompt(&self, prompt: &str, turn: &str) -> Output {
        self.hook(
            "UserPromptSubmit",
            &serde_json::to_vec(&self.payload("UserPromptSubmit", prompt, turn)).unwrap(),
            None,
        )
    }

    fn human_words(&self) -> Vec<(String, String)> {
        let db = rusqlite::Connection::open(self.root.join("memory.sqlite")).unwrap();
        let mut query = db
            .prepare(
                "SELECT body,json_extract(origin,'$') FROM memory_records ORDER BY timestamp,id",
            )
            .unwrap();
        query
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    fn assert_no_provider_dispatch(&self) {
        for name in ["provider-home", "author-runtime.sqlite"] {
            assert!(!self.root.join(name).exists());
        }
        let native = crate::assistant_native_turns::snapshot(&self.root, "personal").unwrap();
        assert!(
            native["latest_turn"]["model_calls"].is_null(),
            "native receipts cannot invent model accounting"
        );
        let runtime = rusqlite::Connection::open(self.root.join("runtime.sqlite")).unwrap();
        let has_legacy: bool = runtime
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='assistant_runtime_turns')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        if has_legacy {
            let turns: i64 = runtime
                .query_row("SELECT count(*) FROM assistant_runtime_turns", [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(turns, 0, "no custom main provider turn was started");
        }
        let db = rusqlite::Connection::open(self.root.join("policy.sqlite")).unwrap();
        let reservations: i64 = db
            .query_row("SELECT count(*) FROM assistant_reservations", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(
            reservations, 0,
            "the endpoint must not start a provider or worker"
        );
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(child) = self.host.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn private_json(path: &Path, value: &Value) {
    fs::write(path, serde_json::to_vec(value).unwrap()).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}

fn blocked(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let reply: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(reply["decision"], "block");
    assert!(
        reply["reason"]
            .as_str()
            .is_some_and(|reason| !reason.is_empty())
    );
    reply
}

#[test]
fn hook_endpoint_captures_exact_human_words_before_reply_and_finishes_exact_turn() {
    let mut fixture = Fixture::new();
    fixture.start_host();
    let words = "Remember these exact words: quotes \"and\"\nsecond line.";
    let accepted = fixture.prompt(words, "turn-one");
    assert!(
        accepted.status.success(),
        "{}",
        String::from_utf8_lossy(&accepted.stderr)
    );
    assert!(
        accepted.stdout.is_empty(),
        "hooks must not inject model context"
    );
    assert_eq!(
        fixture.human_words(),
        vec![(words.to_owned(), "Human".to_owned())]
    );
    let before = crate::assistant_native_turns::snapshot(&fixture.root, "personal").unwrap();
    assert_eq!(before["latest_turn"]["state"], "dispatched");
    blocked(&fixture.prompt(words, "turn-one"));
    assert_eq!(
        fixture.human_words().len(),
        1,
        "duplicate hook cannot duplicate input"
    );
    let stop = fixture.hook(
        "Stop",
        &serde_json::to_vec(&fixture.payload("Stop", "", "turn-one")).unwrap(),
        None,
    );
    assert!(
        stop.status.success(),
        "{}",
        String::from_utf8_lossy(&stop.stderr)
    );
    assert!(stop.stdout.is_empty());
    assert_eq!(
        crate::assistant_native_turns::snapshot(&fixture.root, "personal").unwrap()["latest_turn"]
            ["state"],
        "completed"
    );
    for event in ["PreCompact", "PostCompact", "PreCompact"] {
        let output = fixture.hook(
            event,
            &serde_json::to_vec(&fixture.payload(event, "", "turn-one")).unwrap(),
            None,
        );
        assert!(output.status.success());
        assert!(output.stdout.is_empty());
    }
    fixture.assert_no_provider_dispatch();
}

#[test]
fn hook_endpoint_pause_blocks_normal_prompt_but_controls_and_exact_feedback_stay_local() {
    let mut fixture = Fixture::new();
    fixture.start_host();
    blocked(&fixture.prompt("$pika-control pause", "pause"));
    blocked(&fixture.prompt("Do not spend while paused", "denied"));
    blocked(&fixture.prompt("$pika-control status", "status"));
    let note = " exact feedback \"quoted\"  \nnext line ";
    blocked(&fixture.prompt(&format!("$pika-user-feedback{note}"), "feedback"));
    assert!(
        fs::read_to_string(fixture.root.join("user_feedback.md"))
            .unwrap()
            .contains("> exact feedback \"quoted\"  \n> next line \n")
    );
    // Capturing genuine submitted words is distinct from dispatching reasoning.
    assert!(
        crate::assistant_native_turns::snapshot(&fixture.root, "personal").unwrap()["latest_turn"]
            .is_null()
    );
    fixture.assert_no_provider_dispatch();
}

#[test]
fn hook_endpoint_malformed_binding_generation_and_backend_failure_block_without_capture() {
    let mut fixture = Fixture::new();
    fixture.start_host();
    blocked(&fixture.hook("UserPromptSubmit", b"not json", None));
    blocked(&fixture.hook("UserPromptSubmit", &vec![b'x'; 64 * 1024 + 1], None));
    let words = serde_json::to_vec(&fixture.payload(
        "UserPromptSubmit",
        "Never capture rejected words",
        "rejected",
    ))
    .unwrap();
    blocked(&fixture.hook(
        "UserPromptSubmit",
        &words,
        Some(("PIKA_ASSISTANT_SCOPE", "other")),
    ));
    blocked(&fixture.hook(
        "UserPromptSubmit",
        &words,
        Some(("PIKA_ASSISTANT_NATIVE_LAUNCH_TOKEN", "retired-generation")),
    ));
    blocked(&fixture.hook(
        "UserPromptSubmit",
        &words,
        Some(("PIKA_ASSISTANT_NATIVE_LAUNCH_TOKEN", "")),
    ));
    let wrong = serde_json::to_vec(&fixture.payload("Stop", "", "wrong-event")).unwrap();
    blocked(&fixture.hook("UserPromptSubmit", &wrong, None));
    let rejected_stop = fixture.hook(
        "Stop",
        &wrong,
        Some(("PIKA_ASSISTANT_NATIVE_LAUNCH_TOKEN", "retired-generation")),
    );
    assert!(!rejected_stop.status.success());
    assert!(
        rejected_stop.stdout.is_empty(),
        "Stop errors must not request a paid continuation"
    );
    let locked = rusqlite::Connection::open(fixture.root.join("memory.sqlite")).unwrap();
    locked.execute_batch("BEGIN EXCLUSIVE").unwrap();
    blocked(&fixture.hook("UserPromptSubmit", &words, None));
    locked.execute_batch("ROLLBACK").unwrap();
    assert!(fixture.human_words().is_empty());
    assert!(
        crate::assistant_native_turns::snapshot(&fixture.root, "personal").unwrap()["latest_turn"]
            .is_null()
    );
    fixture.assert_no_provider_dispatch();
}
