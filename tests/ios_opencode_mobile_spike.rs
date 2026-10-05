//! Actual future native launch → _mobile, disposable provider state and fake inference only.
#![cfg(unix)]
use pikamux::process;
use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::TcpListener,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

// Reuse the production bounded HTTP/peer proof for fixture-only native seeding.
// The certificate is created by the actual launcher, never synthesized here.
#[path = "../src/mobile_opencode_transport.rs"]
mod native_transport;
#[derive(serde::Deserialize)]
struct Binding {
    version: u32,
    token: String,
    thread: String,
    cwd: String,
    server_pid: i64,
    server_start: u64,
    native_pid: i64,
    native_start: u64,
    supervisor_pid: i64,
    supervisor_start: u64,
    port: u16,
    password: String,
}
fn alive(pid: i64, start: u64) -> bool {
    process::process_generation(pid).is_some_and(|actual| actual.start_time == start)
}

struct Endpoint {
    child: Child,
    input: std::process::ChildStdin,
    frames: mpsc::Receiver<Value>,
}
impl Endpoint {
    fn spawn(fixture: &Fixture) -> Self {
        let mut child = fixture
            .command(env!("CARGO_BIN_EXE_pika"))
            .arg("_mobile")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let output = child.stdout.take().unwrap();
        let (sender, frames) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(output).lines() {
                let Ok(line) = line else { break };
                let Ok(frame) = serde_json::from_str(&line) else {
                    break;
                };
                if sender.send(frame).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            input,
            frames,
        }
    }
    fn write(&mut self, method: &str, params: Value) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        writeln!(
            self.input,
            "{}",
            json!({"v":1,"id":id,"method":method,"params":params})
        )
        .unwrap();
        self.input.flush().unwrap();
        id
    }
    fn rpc(&mut self, method: &str, params: Value) -> Value {
        let id = self.write(method, params);
        let deadline = Instant::now() + Duration::from_secs(25);
        loop {
            let frame = self
                .frames
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap();
            if frame["id"] == id {
                return frame;
            }
        }
    }
}
impl Drop for Endpoint {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
struct Fixture {
    _temporary: Option<tempfile::TempDir>,
    root: PathBuf,
    env: Vec<(String, String)>,
    tmux: PathBuf,
    socket: String,
    stop: Arc<AtomicBool>,
    requests: Arc<Mutex<Vec<Value>>>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl Fixture {
    fn command(&self, executable: impl AsRef<std::ffi::OsStr>) -> Command {
        let mut command = Command::new(executable);
        command
            .env_clear()
            .envs(self.env.clone())
            .current_dir(&self.root);
        command
    }
    fn screen(&self, pane: &str) -> String {
        let output = self
            .command(&self.tmux)
            .args([
                "-L",
                &self.socket,
                "capture-pane",
                "-p",
                "-t",
                pane,
                "-S",
                "-200",
            ])
            .output()
            .unwrap();
        if output.status.success() {
            String::from_utf8(output.stdout).unwrap()
        } else {
            format!(
                "Unavailable fixture pane: {}",
                String::from_utf8_lossy(&output.stderr)
            )
        }
    }
    fn wait_model(&self, text: &str) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !self
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|v| v.to_string().contains(text))
        {
            assert!(
                Instant::now() < deadline,
                "No synthetic inference for {text}"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self
            .command(&self.tmux)
            .args(["-L", &self.socket, "kill-server"])
            .output();
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        if std::thread::panicking()
            && let Some(temp) = self._temporary.take()
        {
            eprintln!(
                "Failed isolated native fixture retained at {}",
                temp.keep().display()
            );
        }
    }
}
fn fixture() -> Fixture {
    let executable =
        std::env::var("PIKA_IOS_OPENCODE").expect("Explicit installed native OpenCode binary");
    let tmux = PathBuf::from(std::env::var_os("PIKA_IOS_TMUX").expect("Explicit tmux binary"));
    let temporary = tempfile::Builder::new()
        .prefix("pika-opencode-mobile-")
        .tempdir_in("/tmp")
        .unwrap();
    let root = fs::canonicalize(temporary.path()).unwrap();
    let private_bin = root.join("bin");
    fs::create_dir(&private_bin).unwrap();
    std::os::unix::fs::symlink(fs::canonicalize(&tmux).unwrap(), private_bin.join("tmux")).unwrap();
    let runtime_path = format!("{}:/usr/bin:/bin", private_bin.display());
    let socket = format!("pika-opencode-{}", uuid::Uuid::new_v4().simple());
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let model_url = format!("http://{}/v1", listener.local_addr().unwrap());
    let mut native = json!({"model":"fixture/fixture","small_model":"fixture/fixture","enabled_providers":["fixture"],"provider":{"fixture":{"name":"Synthetic loopback","npm":"@ai-sdk/openai-compatible","options":{"baseURL":model_url,"apiKey":"fixture-only"},"models":{"fixture":{"name":"Fixture","limit":{"context":131072,"output":4096}}}}},"permission":"deny","share":"disabled","plugin":[]});
    let mut env = Vec::new();
    for (key, directory) in [
        ("HOME", "home"),
        ("PIKA_CONFIG_HOME", "pika-config"),
        ("PIKA_STATE_HOME", "pika-state"),
        ("XDG_CONFIG_HOME", "config"),
        ("XDG_DATA_HOME", "data"),
        ("XDG_STATE_HOME", "state"),
        ("XDG_CACHE_HOME", "cache"),
        ("TMPDIR", "tmp"),
        ("TMUX_TMPDIR", "tmp"),
        ("OPENCODE_CONFIG_DIR", "config/opencode"),
        ("OPENCODE_DATA_HOME", "data/opencode"),
        ("CODEX_HOME", "codex"),
        ("CLAUDE_CONFIG_DIR", "claude"),
    ] {
        let path = root.join(directory);
        fs::create_dir_all(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        env.push((key.into(), path.display().to_string()));
    }
    for (key, value) in [
        ("TERM", "xterm-256color"),
        ("PIKA_UPDATE_CHECK", "0"),
        ("OPENCODE_DISABLE_MODELS_FETCH", "1"),
        ("OPENCODE_DISABLE_AUTOUPDATE", "1"),
        ("OPENCODE_DISABLE_DEFAULT_PLUGINS", "1"),
        ("OPENCODE_DISABLE_PROJECT_CONFIG", "1"),
        ("OPENCODE_DISABLE_EXTERNAL_SKILLS", "1"),
        ("OPENCODE_DISABLE_LSP_DOWNLOAD", "1"),
    ] {
        env.push((key.into(), value.into()));
    }
    env.push(("PATH".into(), runtime_path.clone()));
    env.push(("PIKA_TMUX_SOCKET".into(), socket.clone()));
    env.push((
        "PIKA_DB_PATH".into(),
        root.join("pika.db").display().to_string(),
    ));
    let plugin = root.join("config/opencode/plugins/pika.js");
    fs::create_dir_all(plugin.parent().unwrap()).unwrap();
    fs::write(
        &plugin,
        pikamux::setup::opencode_plugin_source(std::path::Path::new(env!("CARGO_BIN_EXE_pika")))
            .unwrap(),
    )
    .unwrap();
    native["plugin"] = json!([format!("file://{}", plugin.display())]);
    env.push(("OPENCODE_CONFIG_CONTENT".into(), native.to_string()));
    fs::write(root.join("pika-config/config.json"),json!({"provider_executables":{"opencode":executable},"provider_runtime_path":runtime_path}).to_string()).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let worker = {
        let stop = stop.clone();
        let requests = requests.clone();
        std::thread::spawn(move || fake_model(listener, stop, requests))
    };
    Fixture {
        _temporary: Some(temporary),
        root,
        env,
        tmux,
        socket,
        stop,
        requests,
        worker: Some(worker),
    }
}
fn fake_model(listener: TcpListener, stop: Arc<AtomicBool>, requests: Arc<Mutex<Vec<Value>>>) {
    listener.set_nonblocking(true).unwrap();
    while !stop.load(Ordering::Relaxed) {
        let Ok((mut stream, _)) = listener.accept() else {
            std::thread::sleep(Duration::from_millis(10));
            continue;
        };
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut header = Vec::new();
        while !header.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            if stream.read_exact(&mut byte).is_err() {
                break;
            }
            header.push(byte[0]);
            assert!(header.len() < 65536);
        }
        let header = String::from_utf8_lossy(&header);
        let size = header
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .map(|value| value.trim().parse::<usize>().unwrap())
            })
            .unwrap_or(0);
        assert!(size < 4 * 1024 * 1024);
        let mut body = vec![0; size];
        if stream.read_exact(&mut body).is_err() {
            continue;
        }
        let body: Value = serde_json::from_slice(&body).unwrap();
        let text = if body.to_string().contains("Concurrent native A") {
            "Concurrent native A reply."
        } else if body.to_string().contains("Concurrent native B") {
            "Concurrent native B reply."
        } else if body.to_string().contains("Native simulator phone reply") {
            "Native simulator reply retained on original OpenCode."
        } else if body.to_string().contains("Phone cold resume") {
            "Cold resume reply on original native conversation."
        } else if body.to_string().contains("Phone lost acknowledgement") {
            "Lost acknowledgement reply retained."
        } else if body.to_string().contains("Phone follow-up") {
            "Phone follow-up on original native owner."
        } else {
            "First phone reply on original native owner."
        };
        requests.lock().unwrap().push(body);
        let chunks = [
            json!({"id":"fixture","object":"chat.completion.chunk","created":1,"model":"fixture","choices":[{"index":0,"delta":{"role":"assistant","content":text},"finish_reason":null}]}),
            json!({"id":"fixture","object":"chat.completion.chunk","created":1,"model":"fixture","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}),
        ];
        let events = format!(
            "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            chunks[0], chunks[1]
        );
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{events}",
            events.len()
        );
        let _ = stream.write_all(response.as_bytes());
    }
}

#[test]
#[ignore = "Two actual future native launches; disposable state and synthetic loopback inference only"]
fn concurrent_managed_native_launches_keep_exact_ports_owners_and_messages() {
    let fixture = fixture();
    let store = pikamux::store::Store::at(fixture.root.join("pika.db"));
    store.initialize().unwrap();
    for label in ["A", "B"] {
        let starter = format!("concurrent-{}", label.to_lowercase());
        assert!(
            fixture
                .command(&fixture.tmux)
                .args([
                    "-L",
                    &fixture.socket,
                    "new-session",
                    "-d",
                    "-s",
                    &starter,
                    "-x",
                    "140",
                    "-y",
                    "50",
                    "-c"
                ])
                .arg(&fixture.root)
                .arg("sleep 120")
                .status()
                .unwrap()
                .success()
        );
        assert!(
            fixture
                .command(&fixture.tmux)
                .args([
                    "-L",
                    &fixture.socket,
                    "set-option",
                    "-g",
                    "remain-on-exit",
                    "on"
                ])
                .status()
                .unwrap()
                .success()
        );
        let name = format!("Concurrent native {label}");
        let argv = shell_words::join([
            env!("CARGO_BIN_EXE_pika"),
            "new",
            &name,
            "--agent",
            "opencode",
        ]);
        let stderr = fixture.root.join(format!("concurrent-{label}-stderr.txt"));
        let launch = format!("{argv} 2>{}", shell_words::quote(stderr.to_str().unwrap()));
        assert!(
            fixture
                .command(&fixture.tmux)
                .args([
                    "-L",
                    &fixture.socket,
                    "respawn-pane",
                    "-k",
                    "-t",
                    &format!("{starter}:0.0"),
                    &launch
                ])
                .status()
                .unwrap()
                .success()
        );
    }
    let deadline = Instant::now() + Duration::from_secs(40);
    let sessions = loop {
        let sessions = ["A", "B"].map(|label| {
            store.list_sessions().unwrap().into_iter().find(|session| {
                session.name.as_deref() == Some(&format!("Concurrent native {label}"))
            })
        });
        if sessions.iter().all(|session| {
            session.as_ref().is_some_and(|session| {
                store
                    .get_recovery_owner(session.provider, session.provider_thread_id())
                    .unwrap()
                    .is_some()
            })
        }) {
            break sessions.map(Option::unwrap);
        }
        assert!(
            Instant::now() < deadline,
            "Concurrent launches did not certify two original native owners: sessions={:?}, pending={:?}, stderrA={}, stderrB={}, starterA={}, starterB={}, nativepanes={:?}",
            store.list_sessions().unwrap(),
            store.list_pending().unwrap(),
            fs::read_to_string(fixture.root.join("concurrent-A-stderr.txt")).unwrap_or_default(),
            fs::read_to_string(fixture.root.join("concurrent-B-stderr.txt")).unwrap_or_default(),
            fixture.screen("concurrent-a:0.0"),
            fixture.screen("concurrent-b:0.0"),
            store
                .list_pending()
                .unwrap()
                .iter()
                .filter_map(|pending| pending.tmux_pane.as_deref())
                .map(|pane| fixture.screen(pane))
                .collect::<Vec<_>>()
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    let bindings = sessions.each_ref().map(|session| {
        serde_json::from_slice::<Binding>(
            &fs::read(
                fixture
                    .root
                    .join("pika-state/opencode-shared")
                    .join(format!("{}.json", session.provider_thread_id())),
            )
            .unwrap(),
        )
        .unwrap()
    });
    assert_ne!(bindings[0].thread, bindings[1].thread);
    assert_ne!(
        bindings[0].port, bindings[1].port,
        "Native launches silently reused a server port"
    );
    assert_ne!(bindings[0].server_pid, bindings[1].server_pid);
    assert_ne!(bindings[0].native_pid, bindings[1].native_pid);
    assert_ne!(bindings[0].token, bindings[1].token);
    let reconcile = || {
        let result = fixture
            .command(env!("CARGO_BIN_EXE_pika"))
            .args(["list", "--json", "--no-usage"])
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        for index in 0..2 {
            let current = store
                .get_session(
                    sessions[index].provider,
                    sessions[index].provider_thread_id(),
                )
                .unwrap()
                .unwrap();
            assert_eq!(
                current.root_pid,
                Some(bindings[index].native_pid),
                "Concurrent reconciliation lost exact native owner"
            );
            assert_eq!(current.tmux_pane, sessions[index].tmux_pane);
            assert_ne!(current.status, pikamux::model::Status::OpenTwice);
        }
    };
    reconcile();
    let identities = sessions.each_ref().map(|session| json!({"nodeId":store.ensure_local_node_id().unwrap(),"provider":"opencode","threadId":session.provider_thread_id()}));
    let mut phones = [Endpoint::spawn(&fixture), Endpoint::spawn(&fixture)];
    for index in 0..2 {
        let opened = phones[index].rpc("conversation/open", json!({"identity":identities[index]}));
        assert_eq!(opened["result"]["capabilities"]["send"], true, "{opened}");
    }
    assert!(
        fixture.requests.lock().unwrap().is_empty(),
        "Future launch invoked inference"
    );
    let operation_ids = [
        uuid::Uuid::new_v4().to_string(),
        uuid::Uuid::new_v4().to_string(),
    ];
    for index in 0..2 {
        let label = ["A", "B"][index];
        let sent = phones[index].rpc("conversation/send", json!({"identity":identities[index],"clientMessageId":operation_ids[index],"text":format!("Concurrent native {label}")}));
        assert_eq!(sent["result"]["state"], "accepted", "{sent}");
    }
    for index in 0..2 {
        let label = ["A", "B"][index];
        let other = ["B", "A"][index];
        let prompt = format!("Concurrent native {label}");
        let reply = format!("Concurrent native {label} reply.");
        let observed = wait_reply(&mut phones[index], &identities[index], &reply);
        assert_reading_order(&observed, &prompt, &reply);
        assert!(
            !observed
                .to_string()
                .contains(&format!("Concurrent native {other}")),
            "Native conversations cross-delivered messages"
        );
        let receipt = phones[index].rpc(
            "conversation/receipt",
            json!({"identity":identities[index],"clientMessageId":operation_ids[index]}),
        );
        assert_eq!(receipt["result"]["state"], "delivered", "{receipt}");
        let retried = phones[index].rpc("conversation/send", json!({"identity":identities[index],"clientMessageId":operation_ids[index],"text":prompt}));
        assert_eq!(retried["result"]["state"], "delivered", "{retried}");
        let deadline = Instant::now() + Duration::from_secs(20);
        while !fixture
            .screen(sessions[index].tmux_pane.as_deref().unwrap())
            .contains(&reply)
        {
            assert!(
                Instant::now() < deadline,
                "Reply missing in original concurrent native TUI {label}"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        let owner = store
            .get_recovery_owner(
                sessions[index].provider,
                sessions[index].provider_thread_id(),
            )
            .unwrap()
            .unwrap();
        assert_eq!(owner.pid, bindings[index].native_pid);
        assert_eq!(
            u64::try_from(owner.start_time).unwrap(),
            bindings[index].native_start
        );
    }
    std::thread::sleep(Duration::from_millis(500));
    let requests = fixture.requests.lock().unwrap();
    assert_eq!(requests.len(), 2, "Concurrent sends were replayed");
    for label in ["A", "B"] {
        let own = requests
            .iter()
            .find(|request| {
                request
                    .to_string()
                    .contains(&format!("Concurrent native {label}"))
            })
            .unwrap();
        let other = if label == "A" { "B" } else { "A" };
        assert!(
            !own.to_string()
                .contains(&format!("Concurrent native {other}")),
            "Inference context crossed native sessions"
        );
    }
    drop(requests);
    reconcile();
    for index in 0..2 {
        assert!(
            fixture
                .command(&fixture.tmux)
                .args([
                    "-L",
                    &fixture.socket,
                    "kill-pane",
                    "-t",
                    sessions[index].tmux_pane.as_deref().unwrap()
                ])
                .status()
                .unwrap()
                .success()
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while alive(bindings[index].native_pid, bindings[index].native_start)
            || alive(bindings[index].server_pid, bindings[index].server_start)
            || alive(
                bindings[index].supervisor_pid,
                bindings[index].supervisor_start,
            )
        {
            assert!(
                Instant::now() < deadline,
                "Concurrent terminal left owned processes alive"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        if index == 0 {
            assert!(alive(bindings[1].native_pid, bindings[1].native_start));
            assert!(alive(bindings[1].server_pid, bindings[1].server_start));
        }
    }
    eprintln!(
        "REAL CONCURRENT OPENCODE: two public managed launches, distinct native threads/server ports/exact owner generations, independent native TUI and inference contexts, delivered receipts and same-ID retries without replay, terminal A cleanup preserves B, both owned groups reaped; fake inference only"
    );
}
fn wait_reply(phone: &mut Endpoint, identity: &Value, text: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let result = phone.rpc("conversation/open", json!({"identity":identity}));
        assert!(result.get("error").is_none(), "{result}");
        if result.to_string().contains(text) {
            return result;
        }
        assert!(Instant::now() < deadline, "Reply missing: {result}");
        std::thread::sleep(Duration::from_millis(50));
    }
}
fn assert_reading_order(snapshot: &Value, prompt: &str, reply: &str) {
    let texts = snapshot["result"]["turns"]["data"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|turn| turn["items"].as_array().unwrap())
        .filter_map(|item| item["text"].as_str())
        .collect::<Vec<_>>();
    let user = texts.iter().position(|text| *text == prompt).unwrap();
    let assistant = texts.iter().position(|text| *text == reply).unwrap();
    assert!(
        user < assistant,
        "Reply precedes its original prompt: {texts:?}"
    );
    assert_eq!(
        texts.last().copied(),
        Some(reply),
        "Reopen did not retain latest reply last: {texts:?}"
    );
}

#[test]
#[ignore = "Explicit installed OpenCode/tmux, future managed native launch, fake loopback inference only"]
fn future_native_launch_mobile_reopen_and_lost_ack_never_replay() {
    let fixture = fixture();
    let store = pikamux::store::Store::at(fixture.root.join("pika.db"));
    store.initialize().unwrap();
    // The public launch creates the native home; opening on the phone must not.
    let argv = shell_words::join([
        env!("CARGO_BIN_EXE_pika"),
        "new",
        "Native mobile proof",
        "--agent",
        "opencode",
    ]);
    let launch = format!(
        "{argv} 2>{}",
        shell_words::quote(fixture.root.join("launch-stderr.txt").to_str().unwrap())
    );
    let started = fixture
        .command(&fixture.tmux)
        .args([
            "-L",
            &fixture.socket,
            "new-session",
            "-d",
            "-s",
            "mobile-launcher",
            "-x",
            "140",
            "-y",
            "50",
            "-c",
        ])
        .arg(&fixture.root)
        .arg("sleep 120")
        .output()
        .unwrap();
    assert!(
        started.status.success(),
        "{}",
        String::from_utf8_lossy(&started.stderr)
    );
    assert!(
        fixture
            .command(&fixture.tmux)
            .args([
                "-L",
                &fixture.socket,
                "set-option",
                "-g",
                "remain-on-exit",
                "on"
            ])
            .status()
            .unwrap()
            .success()
    );
    assert!(
        fixture
            .command(&fixture.tmux)
            .args([
                "-L",
                &fixture.socket,
                "respawn-pane",
                "-k",
                "-t",
                "mobile-launcher:0.0",
                &launch
            ])
            .status()
            .unwrap()
            .success()
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    let session = loop {
        if let Some(session) = store
            .list_sessions()
            .unwrap()
            .into_iter()
            .find(|s| s.provider == pikamux::model::Provider::Opencode && s.managed)
            && store
                .get_recovery_owner(session.provider, session.provider_thread_id())
                .unwrap()
                .is_some()
        {
            break session;
        }
        if Instant::now() >= deadline {
            panic!(
                "Future native launch did not certify original owner; pending={:?}, sessions={:?}, stderr={}, screen={}",
                store.list_pending().unwrap(),
                store.list_sessions().unwrap(),
                fs::read_to_string(fixture.root.join("launch-stderr.txt")).unwrap_or_default(),
                store
                    .list_pending()
                    .unwrap()
                    .first()
                    .and_then(|pending| pending.tmux_pane.as_deref())
                    .map(|pane| fixture.screen(pane))
                    .unwrap_or_else(|| fixture.screen("mobile-launcher:0.0"))
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let identity = json!({"nodeId":store.ensure_local_node_id().unwrap(),"provider":"opencode","threadId":session.provider_thread_id()});
    let owner = store
        .get_recovery_owner(session.provider, session.provider_thread_id())
        .unwrap()
        .unwrap();
    let pane = session.tmux_pane.as_deref().unwrap();
    let mut phone = Endpoint::spawn(&fixture);
    let opened = phone.rpc("conversation/open", json!({"identity":identity}));
    assert_eq!(opened["result"]["capabilities"]["send"], true, "{opened}");
    assert_eq!(opened["result"]["capabilities"]["answer"], false);
    let controls = phone.rpc("conversation/controls", json!({"identity":identity}));
    assert!(controls["result"]["modelsError"].is_string());
    assert!(controls["result"]["skillsError"].is_string());
    for (text, reply) in [
        (
            "First exact phone prompt",
            "First phone reply on original native owner.",
        ),
        (
            "Phone follow-up",
            "Phone follow-up on original native owner.",
        ),
    ] {
        let id = uuid::Uuid::new_v4().to_string();
        let sent = phone.rpc(
            "conversation/send",
            json!({"identity":identity,"clientMessageId":id,"text":text}),
        );
        assert_eq!(sent["result"]["state"], "accepted", "{sent}");
        let snapshot = wait_reply(&mut phone, &identity, reply);
        assert_eq!(snapshot["result"]["turns"]["order"], "chronological");
        assert_reading_order(&snapshot, text, reply);
        let receipt = phone.rpc(
            "conversation/receipt",
            json!({"identity":identity,"clientMessageId":id}),
        );
        assert_eq!(receipt["result"]["state"], "delivered", "{receipt}");
    }
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let screen = fixture.screen(pane);
        if screen.contains("Phone follow-up on original native owner.") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "Native terminal did not see phone reply: {screen}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    let original = store
        .get_recovery_owner(session.provider, session.provider_thread_id())
        .unwrap()
        .unwrap();
    assert_eq!(
        (
            original.pid,
            original.start_time,
            original.launch_token.clone()
        ),
        (owner.pid, owner.start_time, owner.launch_token.clone())
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if store
            .get_hook_observation(session.provider)
            .unwrap()
            .is_some_and(|hook| hook.source.as_deref() == Some("opencode-plugin"))
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "Generated Pika plugin did not report actual native lifecycle"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let reconciled = fixture
        .command(env!("CARGO_BIN_EXE_pika"))
        .args(["list", "--json", "--no-usage"])
        .output()
        .unwrap();
    assert!(
        reconciled.status.success(),
        "{}",
        String::from_utf8_lossy(&reconciled.stderr)
    );
    let current = store
        .get_session(session.provider, session.provider_thread_id())
        .unwrap()
        .unwrap();
    assert_eq!(
        current.root_pid,
        Some(owner.pid),
        "Generated hook/reconciliation displaced original TUI owner"
    );
    assert_eq!(current.tmux_pane.as_deref(), Some(pane));
    assert_ne!(
        current.status,
        pikamux::model::Status::OpenTwice,
        "Paired native server became duplicate owner"
    );
    let id = uuid::Uuid::new_v4().to_string();
    let payload =
        json!({"identity":identity,"clientMessageId":id,"text":"Phone lost acknowledgement"});
    phone.write("conversation/send", payload.clone());
    fixture.wait_model("Phone lost acknowledgement");
    // Lose only our mobile response transport, not the native conversation.
    drop(phone);
    let mut phone = Endpoint::spawn(&fixture);
    wait_reply(
        &mut phone,
        &identity,
        "Lost acknowledgement reply retained.",
    );
    let before = fixture.requests.lock().unwrap().len();
    let receipt = phone.rpc(
        "conversation/receipt",
        json!({"identity":identity,"clientMessageId":id}),
    );
    assert_eq!(receipt["result"]["state"], "delivered", "{receipt}");
    let retry = phone.rpc("conversation/send", payload.clone());
    assert_eq!(retry["result"]["state"], "delivered", "{retry}");
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(
        fixture.requests.lock().unwrap().len(),
        before,
        "Original operation replayed inference"
    );
    let mut stale = owner.clone();
    stale.start_time += 1;
    store.set_recovery_owner(&stale).unwrap();
    let replay = phone.rpc("conversation/send", payload);
    assert_eq!(replay["result"]["state"], "delivered", "{replay}");
    let blocked=phone.rpc("conversation/send",json!({"identity":identity,"clientMessageId":uuid::Uuid::new_v4().to_string(),"text":"Must not reach changed owner"}));
    assert_eq!(
        blocked["error"]["code"], "rejected_before_dispatch",
        "{blocked}"
    );
    assert_eq!(fixture.requests.lock().unwrap().len(), before);
    store.set_recovery_owner(&owner).unwrap();
    let binding: Binding = serde_json::from_slice(
        &fs::read(
            fixture
                .root
                .join("pika-state/opencode-shared")
                .join(format!("{}.json", session.provider_thread_id())),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(binding.version, 1);
    assert_eq!(binding.token, owner.launch_token);
    assert_eq!(binding.thread, session.provider_thread_id());
    assert_eq!(fs::canonicalize(&binding.cwd).unwrap(), fixture.root);
    assert_eq!(
        (binding.native_pid, binding.native_start),
        (owner.pid, u64::try_from(owner.start_time).unwrap())
    );
    assert!(alive(binding.supervisor_pid, binding.supervisor_start));
    let before_seed = fixture.requests.lock().unwrap().len();
    for index in 0..105 {
        native_transport::request(&binding,"POST",&format!("/session/{}/prompt_async",session.provider_thread_id()),Some(json!({"noReply":true,"parts":[{"type":"text","text":format!("History seed {index:03}")}]}))).unwrap();
    }
    let latest = wait_reply(&mut phone, &identity, "History seed 104");
    let newest = latest["result"]["turns"]["data"].as_array().unwrap();
    assert_eq!(newest.len(), 100, "Native bounded latest page: {latest}");
    let cursor = latest["result"]["turns"]["nextCursor"]
        .as_str()
        .expect("Actual native cursor required");
    let older = phone.rpc(
        "conversation/history",
        json!({"identity":identity,"cursor":cursor}),
    );
    assert!(older.get("error").is_none(), "{older}");
    let old = older["result"]["turns"]["data"].as_array().unwrap();
    assert!(
        old.len() >= 11 && old.len() < 100,
        "Native older page did not cross100 boundary: {older}"
    );
    assert!(
        newest
            .iter()
            .all(|new| old.iter().all(|old| new["id"] != old["id"])),
        "Native cursor duplicated messages"
    );
    assert!(older.to_string().contains("History seed 000"));
    assert!(!older.to_string().contains("History seed 104"));
    assert_eq!(older["result"]["turns"]["order"], "chronological");
    assert_eq!(
        fixture.requests.lock().unwrap().len(),
        before_seed,
        "Native noReply history seeding unexpectedly invoked inference"
    );
    let revert = newest.last().unwrap()["id"].as_str().unwrap();
    native_transport::request(
        &binding,
        "POST",
        &format!("/session/{}/revert", session.provider_thread_id()),
        Some(json!({"messageID":revert})),
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let frame = phone
            .frames
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap();
        if frame["event"] == "conversation/disconnected" {
            assert_eq!(frame["params"]["identity"], identity);
            break;
        }
    }
    let reopened = phone.rpc("conversation/open", json!({"identity":identity}));
    assert!(
        reopened.get("error").is_some(),
        "Reverted unsupported history falsely reopened: {reopened}"
    );
    let blocked=phone.rpc("conversation/send",json!({"identity":identity,"clientMessageId":uuid::Uuid::new_v4().to_string(),"text":"Must not reach reverted branch"}));
    assert_eq!(
        blocked["error"]["code"], "rejected_before_dispatch",
        "{blocked}"
    );
    assert_eq!(fixture.requests.lock().unwrap().len(), before_seed);
    native_transport::request(
        &binding,
        "POST",
        &format!("/session/{}/unrevert", session.provider_thread_id()),
        None,
    )
    .unwrap();
    let restored = phone.rpc("conversation/open", json!({"identity":identity}));
    assert_eq!(
        restored["result"]["capabilities"]["send"], true,
        "{restored}"
    );
    assert!(restored.to_string().contains("History seed 104"));
    assert!(
        fixture
            .command(&fixture.tmux)
            .args(["-L", &fixture.socket, "kill-pane", "-t", pane])
            .status()
            .unwrap()
            .success()
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while alive(binding.native_pid, binding.native_start)
        || alive(binding.server_pid, binding.server_start)
        || alive(binding.supervisor_pid, binding.supervisor_start)
    {
        assert!(
            Instant::now() < deadline,
            "Owned native terminal exit left native/server/supervisor alive"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    drop(phone);
    let mut archived_phone = Endpoint::spawn(&fixture);
    let archived = archived_phone.rpc("conversation/open", json!({"identity":identity}));
    assert_eq!(
        archived["result"]["capabilities"]["send"], false,
        "Exited native owner retained send authority: {archived}"
    );
    assert_eq!(
        archived["result"]["readOnly"], true,
        "Exited native owner did not expose saved history: {archived}"
    );
    assert!(archived.to_string().contains("History seed 104"));
    let denied = archived_phone.rpc("conversation/send", json!({"identity":identity,"clientMessageId":uuid::Uuid::new_v4().to_string(),"text":"Must not dispatch after native exit"}));
    assert!(
        denied.get("error").is_some(),
        "Saved history permitted a new send: {denied}"
    );
    drop(archived_phone);
    let cold_argv = shell_words::join([
        env!("CARGO_BIN_EXE_pika"),
        "open",
        session.provider_thread_id(),
    ]);
    let cold_launch = format!(
        "{cold_argv} 2>{}",
        shell_words::quote(fixture.root.join("cold-stderr.txt").to_str().unwrap())
    );
    assert!(
        fixture
            .command(&fixture.tmux)
            .args([
                "-L",
                &fixture.socket,
                "respawn-pane",
                "-k",
                "-t",
                "mobile-launcher:0.0",
                &cold_launch,
            ])
            .status()
            .unwrap()
            .success()
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    let cold_owner = loop {
        if let Some(candidate) = store
            .get_recovery_owner(session.provider, session.provider_thread_id())
            .unwrap()
            && (candidate.pid, candidate.start_time) != (owner.pid, owner.start_time)
            && alive(candidate.pid, u64::try_from(candidate.start_time).unwrap())
        {
            break candidate;
        }
        assert!(
            Instant::now() < deadline,
            "Cold native resume failed: stderr={}, screen={}",
            fs::read_to_string(fixture.root.join("cold-stderr.txt")).unwrap_or_default(),
            fixture.screen("mobile-launcher:0.0")
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_ne!(cold_owner.launch_token, owner.launch_token);
    assert_eq!(
        store
            .list_sessions()
            .unwrap()
            .iter()
            .filter(|s| s.provider == session.provider)
            .count(),
        1,
        "Cold resume forked the watched conversation"
    );
    let cold_session = store
        .get_session(session.provider, session.provider_thread_id())
        .unwrap()
        .unwrap();
    let cold_pane = cold_session.tmux_pane.as_deref().unwrap();
    let mut phone = Endpoint::spawn(&fixture);
    let cold_open = phone.rpc("conversation/open", json!({"identity":identity}));
    assert_eq!(
        cold_open["result"]["capabilities"]["send"], true,
        "{cold_open}"
    );
    assert!(cold_open.to_string().contains("History seed 104"));
    let cold_cursor = cold_open["result"]["turns"]["nextCursor"].as_str().unwrap();
    let cold_history = phone.rpc(
        "conversation/history",
        json!({"identity":identity,"cursor":cold_cursor}),
    );
    assert!(
        cold_history
            .to_string()
            .contains("Lost acknowledgement reply retained."),
        "{cold_history}"
    );
    assert_eq!(
        fixture.requests.lock().unwrap().len(),
        before_seed,
        "Cold launch replayed inference"
    );
    let cold_id = uuid::Uuid::new_v4().to_string();
    let cold_send = phone.rpc(
        "conversation/send",
        json!({"identity":identity,"clientMessageId":cold_id,"text":"Phone cold resume"}),
    );
    assert_eq!(cold_send["result"]["state"], "accepted", "{cold_send}");
    let cold_reply = wait_reply(
        &mut phone,
        &identity,
        "Cold resume reply on original native conversation.",
    );
    assert_reading_order(
        &cold_reply,
        "Phone cold resume",
        "Cold resume reply on original native conversation.",
    );
    let cold_receipt = phone.rpc(
        "conversation/receipt",
        json!({"identity":identity,"clientMessageId":cold_id}),
    );
    assert_eq!(
        cold_receipt["result"]["state"], "delivered",
        "{cold_receipt}"
    );
    let deadline = Instant::now() + Duration::from_secs(20);
    while !fixture
        .screen(cold_pane)
        .contains("Cold resume reply on original native conversation.")
    {
        assert!(Instant::now() < deadline, "Cold native TUI reply missing");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(fixture.requests.lock().unwrap().len(), before_seed + 1);
    let cold_binding: Binding = serde_json::from_slice(
        &fs::read(
            fixture
                .root
                .join("pika-state/opencode-shared")
                .join(format!("{}.json", session.provider_thread_id())),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(cold_binding.thread, session.provider_thread_id());
    assert_ne!(
        (cold_binding.server_pid, cold_binding.server_start),
        (binding.server_pid, binding.server_start)
    );
    if let Some(ready) = std::env::var_os("PIKA_IOS_OPENCODE_READY") {
        let ready = PathBuf::from(ready);
        let stop = ready.with_extension("stop");
        fs::write(&ready, json!({"root":fixture.root,"env":fixture.env,"threadId":session.provider_thread_id(),"nodeId":identity["nodeId"],"binary":env!("CARGO_BIN_EXE_pika"),"stopMarker":stop,"expectedContext":"Cold resume reply on original native conversation.","reply":"Native simulator phone reply","finalResponse":"Native simulator reply retained on original OpenCode."}).to_string()).unwrap();
        eprintln!("PIKA_OPENCODE_UI_READY {}", ready.display());
        let deadline = Instant::now() + Duration::from_secs(600);
        while !stop.exists() {
            assert!(
                Instant::now() < deadline,
                "Native simulator fixture stop timed out"
            );
            assert!(
                alive(cold_binding.native_pid, cold_binding.native_start),
                "Original native UI owner exited during simulator journey"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
        fixture.wait_model("Native simulator phone reply");
        let observed = wait_reply(
            &mut phone,
            &identity,
            "Native simulator reply retained on original OpenCode.",
        );
        assert_reading_order(
            &observed,
            "Native simulator phone reply",
            "Native simulator reply retained on original OpenCode.",
        );
        let deadline = Instant::now() + Duration::from_secs(20);
        while !fixture
            .screen(cold_pane)
            .contains("Native simulator reply retained on original OpenCode.")
        {
            assert!(
                Instant::now() < deadline,
                "Simulator reply did not appear in original native TUI"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(
            fixture.requests.lock().unwrap().len(),
            before_seed + 2,
            "Simulator journey replayed inference"
        );
        assert_eq!(
            store
                .get_recovery_owner(session.provider, session.provider_thread_id())
                .unwrap()
                .unwrap()
                .pid,
            cold_owner.pid
        );
        eprintln!(
            "PIKA_OPENCODE_UI_VERIFIED original native TUI/session/generation retained, exact simulator prompt+reply chronological and inference once"
        );
    }
    assert!(
        fixture
            .command(&fixture.tmux)
            .args(["-L", &fixture.socket, "kill-pane", "-t", cold_pane])
            .status()
            .unwrap()
            .success()
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while alive(cold_binding.native_pid, cold_binding.native_start)
        || alive(cold_binding.server_pid, cold_binding.server_start)
        || alive(cold_binding.supervisor_pid, cold_binding.supervisor_start)
    {
        assert!(
            Instant::now() < deadline,
            "Cold terminal left owned processes alive"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    eprintln!(
        "REAL OPENCODE / REAL MOBILE: future launch, same original native TUI/session/generation with generated Pika lifecycle plugin and reconciliation, consecutive replies, reopen, exact delivered receipt, lost phone acknowledgement and same-ID retry no inference replay, stale-owner new-send rejection,105 noReply history messages cross actual opaque100-message page boundary without inference/duplicates, native revert disconnects/blocks then supported unrevert restores, terminal exit reaps owned TUI/server/supervisor, public cold open retains exact native conversation/history with new generations and fresh mobile reply visible in original native TUI; fake inference only."
    );
}
