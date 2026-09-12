#![cfg(unix)]

use pikamux::fleet::{
    FleetError, FleetManager, FleetService, FleetTransport, NodeCandidate, PROTOCOL_NAME,
    PROTOCOL_VERSION, handle_fleet_stdio, session_to_wire,
};
use pikamux::hooks::{HookContext, HookPayload, handle_hook};
use pikamux::model::{Candidate, Provider, Session, Status};
use pikamux::store::Store;
use serde_json::{Value, json};
use std::fs;
use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

const SESSION_ID: &str = "11111111-1111-4111-8111-111111111111";
const REQUEST_ID: &str = "33333333-3333-4333-8333-333333333333";

fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
}

fn copy_tree(source: &Path, destination: &Path) {
    fs::create_dir_all(destination).unwrap();
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let kind = entry.file_type().unwrap();
        let target = destination.join(entry.file_name());
        if kind.is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            assert!(kind.is_file(), "reference contains a non-file entry");
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

struct PythonLab {
    _temp: tempfile::TempDir,
    reference: PathBuf,
    driver: PathBuf,
    home: PathBuf,
}

impl PythonLab {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/python-v0.5.0a4");
        let reference = temp.path().join("reference");
        copy_tree(&fixture, &reference);
        let driver =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mixed_runtime_driver.py");
        let home = temp.path().join("home");
        fs::create_dir(&home).unwrap();
        Self {
            _temp: temp,
            reference,
            driver,
            home,
        }
    }

    fn database(&self, name: &str) -> PathBuf {
        self.home.join(name)
    }

    fn command(&self, arguments: &[&str], input: Option<&[u8]>) -> std::process::Output {
        let mut command = Command::new("python3");
        command
            .arg(&self.driver)
            .args(arguments)
            .env_clear()
            .env("HOME", &self.home)
            .env("PATH", "/usr/bin:/bin")
            .env("PYTHONPATH", &self.reference)
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .env("PIKA_CONFIG_HOME", self.home.join("config"))
            .env("PIKA_STATE_HOME", self.home.join("state"))
            .env("TMPDIR", self.home.join("tmp"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        fs::create_dir_all(self.home.join("tmp")).unwrap();
        let mut child = command.spawn().expect("python3 must be available");
        if let Some(input) = input {
            child.stdin.as_mut().unwrap().write_all(input).unwrap();
        }
        drop(child.stdin.take());
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "frozen Python reference failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    fn json(&self, arguments: &[&str]) -> Value {
        let output = self.command(arguments, None);
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn fleet_request(&self, database: &Path, request: &Value) -> Value {
        let mut input = serde_json::to_vec(request).unwrap();
        input.push(b'\n');
        let output = self.command(&["fleet-serve", database.to_str().unwrap()], Some(&input));
        let lines: Vec<_> = String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect();
        assert_eq!(lines.len(), 1, "fleet response must be one JSONL frame");
        serde_json::from_str(&lines[0]).unwrap()
    }
}

fn hook_payload(event: &str) -> HookPayload {
    HookPayload {
        session_id: SESSION_ID.to_owned(),
        hook_event_name: event.to_owned(),
        cwd: Some("/synthetic/project".to_owned()),
        transcript_path: None,
        tool_name: None,
        notification_type: None,
        error: None,
        source: Some("compat-rust-hook".to_owned()),
        originator: None,
        entrypoint: None,
        thread_source: None,
        parent_session_id: None,
        session_title: Some("compat-thread".to_owned()),
        desired_name: None,
        native_name_error: None,
        model: None,
        deleted: false,
        background_tasks: 0,
    }
}

fn rust_hook(store: &Store, event: &str) {
    let result = handle_hook(
        store,
        Provider::Claude,
        &hook_payload(event),
        &HookContext::at(now()),
    )
    .unwrap();
    assert_eq!(result.session_id.as_deref(), Some(SESSION_ID));
}

#[test]
fn python_and_rust_share_schema_and_alternate_real_hook_writes() {
    let lab = PythonLab::new();

    let python_first = lab.database("python-first.db");
    let first = lab.json(&[
        "hook",
        python_first.to_str().unwrap(),
        "claude",
        "SessionStart",
    ]);
    assert_eq!(first["version"], "0.5.0a4");
    assert_eq!(first["session"]["status"], "READY");

    let store = Store::at(&python_first);
    store.initialize().unwrap();
    assert_eq!(
        store
            .get_session(Provider::Claude, SESSION_ID)
            .unwrap()
            .unwrap()
            .name
            .as_deref(),
        Some("compat-thread")
    );
    rust_hook(&store, "UserPromptSubmit");
    let after_rust = lab.json(&["inspect", python_first.to_str().unwrap(), "claude"]);
    assert_eq!(after_rust["session"]["status"], "WORKING");
    assert_eq!(after_rust["hook"]["event_name"], "UserPromptSubmit");
    assert_eq!(after_rust["hook"]["source"], "compat-rust-hook");

    let after_python = lab.json(&["hook", python_first.to_str().unwrap(), "claude", "Stop"]);
    assert_eq!(after_python["session"]["status"], "READY");
    assert_eq!(after_python["session"]["unread"], true);
    let session = store
        .get_session(Provider::Claude, SESSION_ID)
        .unwrap()
        .unwrap();
    assert_eq!(session.status, Status::Ready);
    assert!(session.unread);
    assert_eq!(
        store
            .get_hook_observation(Provider::Claude)
            .unwrap()
            .unwrap()
            .event_name,
        "Stop"
    );

    let rust_first = lab.database("rust-first.db");
    let store = Store::at(&rust_first);
    store.initialize().unwrap();
    rust_hook(&store, "SessionStart");
    let seen_by_python = lab.json(&["inspect", rust_first.to_str().unwrap(), "claude"]);
    assert_eq!(seen_by_python["session"]["status"], "READY");
    let python_write = lab.json(&[
        "hook",
        rust_first.to_str().unwrap(),
        "claude",
        "UserPromptSubmit",
    ]);
    assert_eq!(python_write["session"]["status"], "WORKING");
    assert_eq!(
        store
            .get_session(Provider::Claude, SESSION_ID)
            .unwrap()
            .unwrap()
            .status,
        Status::Working
    );
}

struct PythonTransport<'a> {
    lab: &'a PythonLab,
    database: PathBuf,
    requests: Mutex<Vec<(Value, bool)>>,
}

impl FleetTransport for &PythonTransport<'_> {
    fn request(&self, _target: &str, payload: &Value, mutating: bool) -> Result<Value, FleetError> {
        self.requests
            .lock()
            .unwrap()
            .push((payload.clone(), mutating));
        Ok(self.lab.fleet_request(&self.database, payload))
    }

    fn run_exact(
        &self,
        _node: &pikamux::model::FleetNode,
        _arguments: &[String],
        _tty: bool,
    ) -> Result<i32, FleetError> {
        unreachable!("attach is outside the mixed-runtime contract")
    }
}

#[test]
fn rust_client_drives_python_v050a4_fleet_envelopes_end_to_end() {
    let lab = PythonLab::new();
    let python_database = lab.database("python-fleet.db");
    let local_store = Store::at(lab.database("rust-client.db"));
    local_store.initialize().unwrap();
    let transport = PythonTransport {
        lab: &lab,
        database: python_database,
        requests: Mutex::new(Vec::new()),
    };
    let manager = FleetManager::new(&local_store, &transport);
    let node = manager
        .add(
            &NodeCandidate {
                alias: "python-node".to_owned(),
                ssh_target: "python.invalid".to_owned(),
                sources: vec!["compat".to_owned()],
                hostname: None,
                online: None,
                os_name: None,
            },
            Some("python-node"),
        )
        .unwrap();
    assert_eq!(node.package_version.as_deref(), Some("0.5.0a4"));
    let session = manager.cached_sessions(Some(&node.node_id), false).unwrap()[0].clone();
    assert_eq!(manager.capture(&session, 17).unwrap(), "python-tail:17");
    assert!(manager.acknowledge(&session).unwrap());
    assert_eq!(manager.untrack(&session, Some(REQUEST_ID)).unwrap(), 1);
    assert!(
        manager
            .cached_sessions(Some(&node.node_id), false)
            .unwrap()
            .is_empty()
    );

    let requests = transport.requests.into_inner().unwrap();
    let operations: Vec<_> = requests
        .iter()
        .map(|(request, _)| request["op"].as_str().unwrap())
        .collect();
    assert_eq!(
        operations,
        [
            "hello",
            "snapshot",
            "peek",
            "acknowledge",
            "untrack",
            "snapshot"
        ]
    );
    assert_eq!(
        requests
            .iter()
            .map(|(_, mutating)| *mutating)
            .collect::<Vec<_>>(),
        [false, false, false, true, true, false]
    );
}

struct RustService {
    tracked: bool,
}

impl RustService {
    fn session() -> Session {
        Session {
            provider: Provider::Codex,
            session_id: SESSION_ID.to_owned(),
            name: Some("rust-expert".to_owned()),
            cwd: Some("/synthetic/project".to_owned()),
            branch: Some("main".to_owned()),
            transcript_path: None,
            tmux_session: Some("pika-compat".to_owned()),
            tmux_pane: Some("%compat".to_owned()),
            root_pid: None,
            status: Status::Ready,
            unread: true,
            model: Some("compat".to_owned()),
            source: "compat-rust".to_owned(),
            managed: true,
            error: None,
            attention_reason: Some("completed".to_owned()),
            created_at: 100.0,
            updated_at: 4242.0,
            last_event_at: 4242.0,
            last_activity_at: 4242.0,
            live: true,
            attached: false,
            home_state: "exact".to_owned(),
            cpu_percent: Some(0.0),
            rss_kb: Some(1024),
            input_tokens: None,
            output_tokens: None,
            cached_input_tokens: None,
            cache_write_tokens: None,
            total_tokens: None,
            estimated_cost_usd: None,
            active_thread_id: None,
        }
    }
}

impl FleetService for RustService {
    fn snapshot(&mut self, _expert_directory: bool) -> Result<Value, FleetError> {
        Ok(json!({
            "type":"snapshot", "protocol":PROTOCOL_NAME, "version":PROTOCOL_VERSION,
            "node_id":"22222222-2222-4222-8222-222222222222",
            "machine":"rust-node", "captured_at":now(),
            "sessions": if self.tracked { vec![session_to_wire(&Self::session(), false)] } else { vec![] },
            "profiles":[], "cards":[]
        }))
    }

    fn candidates(&mut self, _include_unconfirmed: bool) -> Result<Vec<Candidate>, FleetError> {
        Ok(Vec::new())
    }

    fn adopt(&mut self, _provider: Provider, _session_id: &str) -> Result<Session, FleetError> {
        unreachable!("adopt is outside this compatibility contract")
    }

    fn peek(
        &mut self,
        provider: Provider,
        session_id: &str,
        lines: usize,
    ) -> Result<String, FleetError> {
        assert_eq!((provider, session_id), (Provider::Codex, SESSION_ID));
        Ok(format!("rust-tail:{lines}"))
    }

    fn acknowledge(&mut self, provider: Provider, session_id: &str) -> Result<bool, FleetError> {
        assert_eq!((provider, session_id), (Provider::Codex, SESSION_ID));
        Ok(true)
    }

    fn untrack(&mut self, provider: Provider, session_id: &str) -> Result<(i64, bool), FleetError> {
        assert_eq!((provider, session_id), (Provider::Codex, SESSION_ID));
        self.tracked = false;
        Ok((2, true))
    }
}

#[test]
fn python_v050a4_client_accepts_rust_fleet_envelopes_end_to_end() {
    let lab = PythonLab::new();
    let server_store = Store::at(lab.database("rust-server.db"));
    server_store.initialize().unwrap();
    server_store
        .set_meta("fleet:node_id", "22222222-2222-4222-8222-222222222222")
        .unwrap();
    let node_id = server_store.ensure_local_node_id().unwrap();
    let requests = [
        json!({"op":"hello", "protocol":PROTOCOL_NAME, "version":PROTOCOL_VERSION}),
        json!({"op":"snapshot", "protocol":PROTOCOL_NAME, "version":PROTOCOL_VERSION, "expected_node_id":node_id, "expert_directory":true}),
        json!({"op":"peek", "protocol":PROTOCOL_NAME, "version":PROTOCOL_VERSION, "expected_node_id":node_id, "provider":"codex", "session_id":SESSION_ID, "lines":17}),
        json!({"op":"acknowledge", "protocol":PROTOCOL_NAME, "version":PROTOCOL_VERSION, "expected_node_id":node_id, "provider":"codex", "session_id":SESSION_ID}),
        json!({"op":"untrack", "protocol":PROTOCOL_NAME, "version":PROTOCOL_VERSION, "expected_node_id":node_id, "provider":"codex", "session_id":SESSION_ID, "request_id":REQUEST_ID}),
        json!({"op":"snapshot", "protocol":PROTOCOL_NAME, "version":PROTOCOL_VERSION, "expected_node_id":node_id, "expert_directory":true}),
    ];
    let input = requests
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    let mut output = Vec::new();
    handle_fleet_stdio(
        &server_store,
        "rust-node",
        env!("CARGO_PKG_VERSION"),
        &mut RustService { tracked: true },
        Cursor::new(input),
        &mut output,
    )
    .unwrap();
    let transcript = lab.database("rust-transcript.jsonl");
    fs::write(&transcript, output).unwrap();
    let validated = lab.json(&[
        "validate-rust",
        lab.database("python-client.db").to_str().unwrap(),
        transcript.to_str().unwrap(),
    ]);
    assert_eq!(validated["version"], "0.5.0a4");
    assert_eq!(validated["node_id"], node_id);
    assert_eq!(
        validated["operations"],
        json!(["hello", "snapshot", "peek", "ack", "untrack"])
    );
}
