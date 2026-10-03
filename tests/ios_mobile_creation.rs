//! Installed-provider creation journey. No paid model, installed state, or user data.
#![cfg(unix)]
use serde_json::{Value, json};
use std::os::fd::AsRawFd;
use std::{
    io::{BufRead, Read, Write},
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

struct Fixture {
    root: tempfile::TempDir,
    provider: Option<Child>,
    mobile: Option<Child>,
    socket: String,
}
impl Fixture {
    fn command(&self, executable: impl AsRef<std::ffi::OsStr>) -> Command {
        let mut command = Command::new(executable);
        command
            .env_clear()
            .env("PATH", "/opt/homebrew/bin:/usr/bin:/bin")
            .env("HOME", self.root.path().join("home"))
            .env("CODEX_HOME", self.root.path().join("home"))
            .env("PIKA_CONFIG_HOME", self.root.path().join("config"))
            .env("PIKA_STATE_HOME", self.root.path().join("state"))
            .env("PIKA_DB_PATH", self.root.path().join("pika.db"))
            .env("XDG_CONFIG_HOME", self.root.path().join("xdg-config"))
            .env("XDG_DATA_HOME", self.root.path().join("xdg-data"))
            .env("XDG_STATE_HOME", self.root.path().join("xdg-state"))
            .env("TMPDIR", self.root.path().join("tmp"))
            .env("TMUX_TMPDIR", self.root.path().join("tmux"))
            .env("PIKA_TMUX_SOCKET", &self.socket)
            .env("PIKA_UPDATE_CHECK", "0")
            .current_dir(self.root.path());
        command
    }
    fn endpoint(
        &mut self,
    ) -> (
        std::process::ChildStdin,
        std::io::BufReader<std::process::ChildStdout>,
    ) {
        let mut child = self
            .command(env!("CARGO_BIN_EXE_pika"))
            .arg("_mobile")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let output = child.stdout.take().unwrap();
        let descriptor = output.as_raw_fd();
        let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFL) };
        assert!(flags >= 0);
        assert_eq!(
            unsafe { libc::fcntl(descriptor, libc::F_SETFL, flags | libc::O_NONBLOCK) },
            0
        );
        self.mobile = Some(child);
        (input, std::io::BufReader::new(output))
    }
    fn stop_endpoint(&mut self) {
        if let Some(mut child) = self.mobile.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop_endpoint();
        let _ = self
            .command("tmux")
            .args(["-L", &self.socket, "kill-server"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if let Some(mut child) = self.provider.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
fn request(
    input: &mut impl Write,
    output: &mut impl BufRead,
    method: &str,
    params: Value,
) -> Value {
    let id = uuid::Uuid::new_v4().to_string();
    request_fixed(input, output, &id, method, params)
}
fn request_fixed(
    input: &mut impl Write,
    output: &mut impl BufRead,
    id: &str,
    method: &str,
    params: Value,
) -> Value {
    writeln!(
        input,
        "{}",
        json!({"v":1,"id":id,"method":method,"params":params})
    )
    .unwrap();
    input.flush().unwrap();
    loop {
        let frame = read_frame(output);
        if frame["id"] == id {
            return frame;
        }
    }
}
fn read_frame(output: &mut impl BufRead) -> Value {
    let deadline = Instant::now() + Duration::from_secs(15);
    read_frame_before(output, deadline)
}
fn read_frame_before(output: &mut impl BufRead, deadline: Instant) -> Value {
    let mut line = String::new();
    loop {
        match output.read_line(&mut line) {
            Ok(0) => panic!("Endpoint closed before bounded response"),
            Ok(_) => {
                if line.ends_with('\n') {
                    return serde_json::from_str(&line).unwrap();
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => panic!("{error}"),
        }
        assert!(
            Instant::now() < deadline,
            "No complete endpoint frame: {line}"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn loaded(socket: &Path) -> Vec<Value> {
    native_rpc(socket, "thread/loaded/list", json!({"limit":64}))["data"]
        .as_array()
        .unwrap()
        .clone()
}
fn native_rpc(socket: &Path, method: &str, params: Value) -> Value {
    let stream = std::os::unix::net::UnixStream::connect(socket).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let (mut wire, _) = tungstenite::client("ws://localhost", stream).unwrap();
    wire.send(tungstenite::Message::Text(json!({"id":1,"method":"initialize","params":{"clientInfo":{"name":"isolated-creation-proof","version":"1"},"capabilities":{"experimentalApi":true}}}).to_string().into())).unwrap();
    loop {
        let frame: Value = serde_json::from_str(wire.read().unwrap().to_text().unwrap()).unwrap();
        if frame["id"] == 1 && frame.get("result").is_some() {
            break;
        }
    }
    wire.send(tungstenite::Message::Text(
        json!({"method":"initialized","params":{}})
            .to_string()
            .into(),
    ))
    .unwrap();
    wire.send(tungstenite::Message::Text(
        json!({"id":2,"method":method,"params":params})
            .to_string()
            .into(),
    ))
    .unwrap();
    loop {
        let frame: Value = serde_json::from_str(wire.read().unwrap().to_text().unwrap()).unwrap();
        if frame["id"] == 2 && (frame.get("result").is_some() || frame.get("error").is_some()) {
            assert!(frame.get("error").is_none(), "{method}: {frame}");
            return frame["result"].clone();
        }
    }
}

#[test]
#[ignore = "Explicit installed Codex; actual disposable native tmux, synthetic loopback model"]
fn actual_mobile_creation_reuses_same_uuid_after_lost_receipt_and_delivers_first_message() {
    let codex = std::fs::canonicalize(
        std::env::var_os("PIKA_IOS_CODEX").expect("Explicit installed provider required"),
    )
    .unwrap();
    let mut fixture = Fixture {
        root: tempfile::Builder::new()
            .prefix("pm-")
            .tempdir_in("/tmp")
            .unwrap(),
        provider: None,
        mobile: None,
        socket: format!("mobile-{}", uuid::Uuid::new_v4().simple()),
    };
    for directory in [
        "home",
        "home/app-server-control",
        "config",
        "state",
        "tmp",
        "tmux",
        "xdg-config",
        "xdg-data",
        "xdg-state",
    ] {
        std::fs::create_dir_all(fixture.root.path().join(directory)).unwrap();
    }
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    std::fs::write(fixture.root.path().join("home/config.toml"),format!("model=\"isolated-fake\"\nmodel_provider=\"isolated\"\n[model_providers.isolated]\nname=\"Synthetic\"\nbase_url=\"{url}\"\nwire_api=\"responses\"\nrequires_openai_auth=false\nsupports_websockets=false\n")).unwrap();
    std::fs::write(fixture.root.path().join("config/config.json"),json!({"provider_executables":{"codex":codex},"provider_runtime_path":"/opt/homebrew/bin:/usr/bin:/bin"}).to_string()).unwrap();
    let (delivered, received) = std::sync::mpsc::channel();
    let model = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(45);
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        Instant::now() < deadline,
                        "No first message reached synthetic inference"
                    );
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(error) => panic!("{error}"),
            }
        };
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut header = Vec::new();
        while !header.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            stream.read_exact(&mut byte).unwrap();
            header.push(byte[0]);
            assert!(header.len() < 65536);
        }
        let header = String::from_utf8(header).unwrap();
        let length = header
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .map(|v| v.trim().parse::<usize>().unwrap())
            })
            .unwrap();
        assert!(length < 8 * 1024 * 1024);
        let mut body = vec![0; length];
        stream.read_exact(&mut body).unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert!(
            body["input"]
                .to_string()
                .contains("First exact mobile project message"),
            "{body}"
        );
        delivered.send(()).unwrap();
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            )
            .unwrap();
        let message = json!({"type":"message","id":"creation-output","role":"assistant","status":"completed","content":[{"type":"output_text","text":"First mobile message received.","annotations":[]}]});
        for event in [
            json!({"type":"response.created","response":{"id":"creation-response","status":"in_progress","output":[]}}),
            json!({"type":"response.output_item.added","output_index":0,"item":message}),
            json!({"type":"response.output_item.done","output_index":0,"item":message}),
            json!({"type":"response.completed","response":{"id":"creation-response","status":"completed","output":[message],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}),
        ] {
            writeln!(
                stream,
                "event: {}\ndata: {}\n",
                event["type"].as_str().unwrap(),
                event
            )
            .unwrap();
        }
    });
    let socket = fixture
        .root
        .path()
        .join("home/app-server-control/app-server-control.sock");
    fixture.provider = Some(
        fixture
            .command(&codex)
            .args([
                "app-server",
                "--listen",
                &format!("unix://{}", socket.display()),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    let owner = fixture.provider.as_ref().unwrap().id();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !socket.exists() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(
        loaded(&socket).is_empty(),
        "Fresh server must have no threads"
    );
    let store = pikamux::store::Store::at(fixture.root.path().join("pika.db"));
    store.initialize().unwrap();
    let seed = pikamux::model::Candidate {
        provider: pikamux::model::Provider::Codex,
        session_id: uuid::Uuid::new_v4().to_string(),
        name: Some("Seed Project".into()),
        cwd: Some(fixture.root.path().to_string_lossy().into_owned()),
        branch: None,
        transcript_path: None,
        model: None,
        updated_at: 1.0,
        live: false,
        pid: None,
        source: "isolated-seed".into(),
        parent_session_id: None,
        created_at: 1.0,
        lifecycle_status: None,
    };
    store
        .adopt_session(&pikamux::core::session_from_candidate(&seed))
        .unwrap();
    let node = store.ensure_local_node_id().unwrap();
    let (mut input, output) = fixture.endpoint();
    let operation = uuid::Uuid::new_v4().to_string();
    let payload = json!({"nodeId":node,"provider":"codex","clientOperationId":operation,"name":"Fresh Mobile Project","projectId":fixture.root.path()});
    // Dispatch, but deliberately never read the creation acknowledgement.
    writeln!(input,"{}",json!({"v":1,"id":uuid::Uuid::new_v4().to_string(),"method":"conversation/create","params":payload})).unwrap();
    input.flush().unwrap();
    let deadline = Instant::now() + Duration::from_secs(35);
    let creation = loop {
        let db = rusqlite::Connection::open(store.path()).unwrap();
        let outcome = db
            .query_row(
                "SELECT outcome FROM mobile_deliveries WHERE id=?",
                [&operation],
                |row| row.get::<_, String>(0),
            )
            .ok()
            .map(|s| serde_json::from_str::<Value>(&s).unwrap());
        if let Some(outcome) = &outcome {
            assert_ne!(outcome["state"], "rejected", "{outcome}");
            if outcome["state"] == "created" {
                break outcome.clone();
            }
            if outcome["message"]
                .as_str()
                .is_some_and(|m| m.contains("partial"))
            {
                panic!("Native launch failed closed: {outcome}");
            }
        }
        assert!(
            Instant::now() < deadline,
            "Creation never reached certified native home: {outcome:?}; pending={:?}",
            store.get_pending(&operation).unwrap()
        );
        std::thread::sleep(Duration::from_millis(25));
    };
    let identity = creation["identity"].clone();
    let thread = identity["threadId"].as_str().unwrap();
    let saved = store
        .list_sessions()
        .unwrap()
        .into_iter()
        .find(|row| row.provider_thread_id() == thread)
        .unwrap();
    assert!(saved.managed && saved.root_pid.is_some());
    assert_eq!(loaded(&socket), vec![json!(thread)]);
    assert!(
        fixture
            .provider
            .as_mut()
            .unwrap()
            .try_wait()
            .unwrap()
            .is_none()
    );
    assert_eq!(fixture.provider.as_ref().unwrap().id(), owner);
    fixture.stop_endpoint();
    drop(input);
    drop(output);
    let (mut input, mut output) = fixture.endpoint();
    let receipt = request(
        &mut input,
        &mut output,
        "conversation/receipt",
        json!({"nodeId":node,"clientOperationId":operation}),
    );
    assert_eq!(receipt["result"]["identity"], identity);
    assert_eq!(receipt["result"]["state"], "created");
    let retry = request(&mut input, &mut output, "conversation/create", payload);
    assert_eq!(retry["result"], creation);
    assert_eq!(loaded(&socket), vec![json!(thread)]);
    let opened = request(
        &mut input,
        &mut output,
        "conversation/open",
        json!({"identity":identity}),
    );
    assert!(opened.get("error").is_none(), "{opened}");
    assert!(
        opened["result"]["turns"]["data"]
            .as_array()
            .unwrap()
            .is_empty(),
        "No initialization model turn is permitted"
    );
    let message = uuid::Uuid::new_v4().to_string();
    let sent = request(
        &mut input,
        &mut output,
        "conversation/send",
        json!({"identity":identity,"clientMessageId":message,"text":"First exact mobile project message"}),
    );
    assert_eq!(sent["result"]["state"], "accepted", "{sent}");
    received.recv_timeout(Duration::from_secs(10)).unwrap();
    let receipt = request(
        &mut input,
        &mut output,
        "conversation/receipt",
        json!({"identity":identity,"clientMessageId":message}),
    );
    assert_eq!(receipt["result"]["state"], "delivered", "{receipt}");
    model.join().unwrap();
    let native_pid = saved.root_pid.unwrap();
    let native_generation = pikamux::process::process_record(native_pid)
        .expect("Certified native TUI must still exist")
        .generation();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let capture = fixture
            .command("tmux")
            .args([
                "-L",
                &fixture.socket,
                "capture-pane",
                "-p",
                "-t",
                saved.tmux_pane.as_deref().unwrap(),
                "-S",
                "-200",
            ])
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&capture.stdout);
        if text.contains("First mobile message received.") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "Original native desktop TUI did not render the response: {text}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(
        pikamux::process::process_record(native_pid)
            .unwrap()
            .generation(),
        native_generation
    );
    assert_eq!(loaded(&socket), vec![json!(thread)]);
    eprintln!(
        "Actual mobile creation: one original app-server, one new UUID/native TUI, no initialization turn, lost creation receipt/retry preserved UUID, first message reached synthetic inference/native history and rendered in the same live desktop TUI."
    );
}

fn fake_http(listener: &std::net::TcpListener) -> (std::net::TcpStream, Value) {
    fake_http_until(listener, None, 30).expect("Synthetic inference did not arrive")
}
fn fake_http_until(
    listener: &std::net::TcpListener,
    stop: Option<&Path>,
    seconds: u64,
) -> Option<(std::net::TcpStream, Value)> {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let mut stream = loop {
        match listener.accept() {
            Ok((stream, _)) => break stream,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline || stop.is_some_and(Path::exists) {
                    return None;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("{error}"),
        }
    };
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut header = Vec::new();
    while !header.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        stream.read_exact(&mut byte).unwrap();
        header.push(byte[0]);
        assert!(header.len() < 65536);
    }
    let header = String::from_utf8(header).unwrap();
    let length = header
        .lines()
        .find_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .map(|v| v.trim().parse::<usize>().unwrap())
        })
        .unwrap();
    assert!(length < 8 * 1024 * 1024);
    let mut body = vec![0; length];
    stream.read_exact(&mut body).unwrap();
    Some((stream, serde_json::from_slice(&body).unwrap()))
}
fn fake_output(stream: &mut std::net::TcpStream, item: Value, id: &str) {
    stream
        .write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
        )
        .unwrap();
    for event in [
        json!({"type":"response.created","response":{"id":id,"status":"in_progress","output":[]}}),
        json!({"type":"response.output_item.added","output_index":0,"item":item}),
        json!({"type":"response.output_item.done","output_index":0,"item":item}),
        json!({"type":"response.completed","response":{"id":id,"status":"completed","output":[item],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}),
    ] {
        writeln!(
            stream,
            "event: {}\ndata: {}\n",
            event["type"].as_str().unwrap(),
            event
        )
        .unwrap();
    }
}
fn next_approval(output: &mut impl BufRead, method: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let frame = read_frame(output);
        if frame["event"] == "conversation/event" && frame["params"]["method"] == method {
            return frame["params"].clone();
        }
        assert!(
            Instant::now() < deadline,
            "No typed native approval: {frame}"
        );
    }
}

#[test]
#[ignore = "Actual installed provider once-command approval, isolated native TUI and fake inference"]
fn actual_command_once_approval_latejoins_original_owner() {
    approval_probe();
}

fn approval_probe() {
    let held_seconds = std::env::var("PIKA_IOS_APPROVAL_FIXTURE_SECONDS")
        .ok()
        .map(|value| value.parse::<u64>().unwrap());
    if let Some(seconds) = held_seconds {
        assert!((30..=600).contains(&seconds));
    }
    let codex = std::fs::canonicalize(
        std::env::var_os("PIKA_IOS_CODEX").expect("Explicit installed provider required"),
    )
    .unwrap();
    let mut fixture = Fixture {
        root: tempfile::Builder::new()
            .prefix("pv-")
            .tempdir_in("/tmp")
            .unwrap(),
        provider: None,
        mobile: None,
        socket: format!("approval-{}", uuid::Uuid::new_v4().simple()),
    };
    for directory in [
        "home",
        "home/app-server-control",
        "config",
        "state",
        "tmp",
        "tmux",
        "xdg-config",
        "xdg-data",
        "xdg-state",
    ] {
        std::fs::create_dir_all(fixture.root.path().join(directory)).unwrap();
    }
    std::fs::set_permissions(
        fixture.root.path().join("state"),
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    std::fs::write(fixture.root.path().join("home/config.toml"),format!("model='isolated-fake'\nmodel_provider='isolated'\napproval_policy='on-request'\nsandbox_mode='read-only'\n[model_providers.isolated]\nname='Synthetic approval'\nbase_url='{url}'\nwire_api='responses'\nrequires_openai_auth=false\nsupports_websockets=false\n[analytics]\nenabled=false\n")).unwrap();
    std::fs::write(fixture.root.path().join("config/config.json"),json!({"provider_executables":{"codex":codex},"provider_runtime_path":"/opt/homebrew/bin:/usr/bin:/bin"}).to_string()).unwrap();
    let cwd = fixture.root.path().to_owned();
    let command_cwd = cwd.clone();
    let (continued, continuation) = std::sync::mpsc::channel();
    let model = std::thread::spawn(move || {
        let (mut first, body) = fake_http(&listener);
        let tool = "exec_command";
        let offered = body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["name"] == tool)
            .expect("Installed provider must offer the exact native tool");
        eprintln!(
            "Installed native {tool} tool parameter keys: {:?}",
            offered["parameters"]["properties"]
                .as_object()
                .map(|properties| properties.keys().collect::<Vec<_>>())
        );
        let call = json!({"type":"function_call","id":"approval-command-call","call_id":"approval-call","name":"exec_command","arguments":json!({"cmd":"printf synthetic-approved","workdir":command_cwd,"sandbox_permissions":"require_escalated","justification":"Confirm this isolated synthetic printf"}).to_string()});
        fake_output(&mut first, call, "approval-inference");
        drop(first);
        let (mut second, body) = fake_http_until(&listener, None, held_seconds.unwrap_or(30))
            .expect("Original native approval must be answered");
        let output = body["input"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| {
                item["call_id"] == "approval-call"
                    && matches!(
                        item["type"].as_str(),
                        Some("function_call_output" | "custom_tool_call_output")
                    )
            })
            .expect("Native tool output must enter the original inference");
        let output = output["output"].to_string();
        assert!(
            output.contains("synthetic-approved"),
            "Actual command once approval was not applied: {output}"
        );
        continued.send(()).unwrap();
        fake_output(
            &mut second,
            json!({"type":"message","id":"approval-final-message","role":"assistant","status":"completed","content":[{"type":"output_text","text":"Approval journey finished.","annotations":[]}]}),
            "approval-final",
        );
    });
    let socket = fixture
        .root
        .path()
        .join("home/app-server-control/app-server-control.sock");
    fixture.provider = Some(
        fixture
            .command(&codex)
            .args([
                "app-server",
                "--listen",
                &format!("unix://{}", socket.display()),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    let owner = fixture.provider.as_ref().unwrap().id();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !socket.exists() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    let store = pikamux::store::Store::at(fixture.root.path().join("pika.db"));
    store.initialize().unwrap();
    let seed = pikamux::model::Candidate {
        provider: pikamux::model::Provider::Codex,
        session_id: uuid::Uuid::new_v4().to_string(),
        name: Some("Approval seed".into()),
        cwd: Some(cwd.to_string_lossy().into_owned()),
        branch: None,
        transcript_path: None,
        model: None,
        updated_at: 1.0,
        live: false,
        pid: None,
        source: "isolated-approval".into(),
        parent_session_id: None,
        created_at: 1.0,
        lifecycle_status: None,
    };
    store
        .adopt_session(&pikamux::core::session_from_candidate(&seed))
        .unwrap();
    let node = store.ensure_local_node_id().unwrap();
    let (mut input, mut output) = fixture.endpoint();
    let created = request(
        &mut input,
        &mut output,
        "conversation/create",
        json!({"nodeId":node,"provider":"codex","clientOperationId":uuid::Uuid::new_v4().to_string(),"name":"Native Approval","projectId":cwd}),
    );
    assert_eq!(created["result"]["state"], "created", "{created}");
    let identity = created["result"]["identity"].clone();
    let thread = identity["threadId"].as_str().unwrap();
    let native_pid = store
        .list_sessions()
        .unwrap()
        .into_iter()
        .find(|row| row.provider_thread_id() == thread)
        .unwrap()
        .root_pid
        .unwrap();
    let native_generation = pikamux::process::process_record(native_pid)
        .unwrap()
        .generation();
    let sent = request(
        &mut input,
        &mut output,
        "conversation/send",
        json!({"identity":identity,"clientMessageId":uuid::Uuid::new_v4().to_string(),"text":"Synthetic explicit approval journey"}),
    );
    assert_eq!(sent["result"]["state"], "accepted", "{sent}");
    let method = "item/commandExecution/requestApproval";
    let original = next_approval(&mut output, method);
    fixture.stop_endpoint();
    drop(input);
    drop(output);
    let (mut input, mut output) = fixture.endpoint();
    let opened = request(
        &mut input,
        &mut output,
        "conversation/open",
        json!({"identity":identity}),
    );
    assert!(opened.get("error").is_none(), "{opened}");
    let replayed = next_approval(&mut output, method);
    assert_eq!(
        replayed, original,
        "Late client must receive the exact original request/details"
    );
    assert_eq!(replayed["params"]["threadId"], thread);
    assert!(
        replayed["params"]["command"]
            .to_string()
            .contains("printf synthetic-approved"),
        "{replayed}"
    );
    if let Some(seconds) = held_seconds {
        fixture.stop_endpoint();
        drop(input);
        drop(output);
        let stop = fixture.root.path().join("stop");
        eprintln!(
            "PIKA_IOS_APPROVAL_FIXTURE {}",
            json!({"root":fixture.root.path(),"identity":identity,"providerPid":owner,"nativePid":native_pid,"tmuxSocket":fixture.socket,"name":"Native Approval","context":"Synthetic explicit approval journey","command":"printf synthetic-approved","request":replayed,"expectedResponse":"Approval journey finished.","stopMarker":stop,"binary":env!("CARGO_BIN_EXE_pika")})
        );
        continuation
            .recv_timeout(Duration::from_secs(seconds))
            .expect("Phone must answer exact original approval");
        model.join().unwrap();
        let session = store
            .list_sessions()
            .unwrap()
            .into_iter()
            .find(|row| row.provider_thread_id() == thread)
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let capture = fixture
                .command("/opt/homebrew/bin/tmux")
                .args([
                    "-L",
                    &fixture.socket,
                    "capture-pane",
                    "-p",
                    "-t",
                    session.tmux_pane.as_ref().unwrap(),
                ])
                .output()
                .unwrap();
            if String::from_utf8_lossy(&capture.stdout).contains("Approval journey finished.") {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "Original native TUI must render approval result"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
        assert_eq!(loaded(&socket), vec![json!(thread)]);
        assert_eq!(fixture.provider.as_ref().unwrap().id(), owner);
        assert_eq!(
            pikamux::process::process_record(native_pid)
                .unwrap()
                .generation(),
            native_generation
        );
        eprintln!(
            "PIKA_IOS_APPROVAL_DELIVERED original native tool output synthetic-approved and final TUI response, same UUID {thread}, providerPID {owner}, nativePID {native_pid}"
        );
        let deadline = Instant::now() + Duration::from_secs(seconds);
        while !stop.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        return;
    }
    let operation = uuid::Uuid::new_v4().to_string();
    let decision = "accept";
    let payload = json!({"identity":identity,"requestId":replayed["requestId"],"turnId":replayed["params"]["turnId"],"itemId":replayed["params"]["itemId"],"decision":decision});
    let approved = request_fixed(
        &mut input,
        &mut output,
        &operation,
        "conversation/approve",
        payload.clone(),
    );
    assert_eq!(approved["result"]["state"], "submitted", "{approved}");
    continuation.recv_timeout(Duration::from_secs(10)).unwrap();
    let repeated = request_fixed(
        &mut input,
        &mut output,
        &operation,
        "conversation/approve",
        payload,
    );
    assert_eq!(
        repeated["result"], approved["result"],
        "Stable operation retry must not send another native response"
    );
    model.join().unwrap();
    let removed = fixture
        .command(env!("CARGO_BIN_EXE_pika"))
        .args(["untrack", &format!("codex:{thread}")])
        .output()
        .unwrap();
    assert!(
        removed.status.success(),
        "{}",
        String::from_utf8_lossy(&removed.stderr)
    );
    assert!(
        !store
            .list_sessions()
            .unwrap()
            .iter()
            .any(|s| s.provider_thread_id() == thread)
    );
    let candidates = request(
        &mut input,
        &mut output,
        "conversation/candidates",
        json!({"nodeId":node}),
    );
    assert!(
        candidates["result"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["identity"] == identity),
        "{candidates}"
    );
    let before = store
        .list_untracked_sessions()
        .unwrap()
        .into_iter()
        .find(|s| s.provider_thread_id() == thread)
        .unwrap()
        .unread;
    let added = request(
        &mut input,
        &mut output,
        "conversation/adopt",
        json!({"identity":identity}),
    );
    assert_eq!(added["result"]["state"], "added", "{added}");
    let restored = store
        .list_sessions()
        .unwrap()
        .into_iter()
        .find(|s| s.provider_thread_id() == thread)
        .unwrap();
    assert_eq!(
        restored.unread, before,
        "Adding must not acknowledge existing unread state"
    );
    assert_eq!(loaded(&socket), vec![json!(thread)]);
    assert_eq!(fixture.provider.as_ref().unwrap().id(), owner);
    assert_eq!(
        pikamux::process::process_record(native_pid)
            .unwrap()
            .generation(),
        native_generation
    );
    eprintln!(
        "Actual native {method}: original pending request replayed with full details to late mobile client, one-time {decision} changed original native tool output, stable-ID retry did not resubmit, same UUID/provider/native TUI retained."
    );
}

#[test]
#[ignore = "Held actual phone creation fixture; explicitly selected provider and bounded isolated inference"]
fn held_phone_creation_and_first_reply_same_native_desktop() {
    let seconds = std::env::var("PIKA_IOS_CREATION_FIXTURE_SECONDS")
        .expect("Explicit bounded fixture duration")
        .parse::<u64>()
        .unwrap();
    assert!((30..=900).contains(&seconds));
    let codex = std::fs::canonicalize(
        std::env::var_os("PIKA_IOS_CODEX").expect("Explicit installed provider"),
    )
    .unwrap();
    let mut fixture = Fixture {
        root: tempfile::Builder::new()
            .prefix("pv-")
            .tempdir_in("/tmp")
            .unwrap(),
        provider: None,
        mobile: None,
        socket: format!("creation-{}", uuid::Uuid::new_v4().simple()),
    };
    for name in [
        "home",
        "home/app-server-control",
        "config",
        "state",
        "xdg-config",
        "xdg-data",
        "xdg-state",
        "tmp",
        "tmux",
    ] {
        std::fs::create_dir_all(fixture.root.path().join(name)).unwrap();
    }
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    std::fs::set_permissions(
        fixture.root.path().join("state"),
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .unwrap();
    std::fs::write(fixture.root.path().join("home/config.toml"),format!("model='isolated-fake'\nmodel_provider='isolated'\napproval_policy='never'\nsandbox_mode='read-only'\n[model_providers.isolated]\nname='Isolated phone creation'\nbase_url='{url}'\nwire_api='responses'\nrequires_openai_auth=false\nsupports_websockets=false\n[analytics]\nenabled=false\n")).unwrap();
    std::fs::write(fixture.root.path().join("config/config.json"),json!({"provider_executables":{"codex":codex},"provider_runtime_path":"/opt/homebrew/bin:/usr/bin:/bin"}).to_string()).unwrap();
    let socket = fixture
        .root
        .path()
        .join("home/app-server-control/app-server-control.sock");
    fixture.provider = Some(
        fixture
            .command(&codex)
            .args([
                "app-server",
                "--listen",
                &format!("unix://{}", socket.display()),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    let owner = fixture.provider.as_ref().unwrap().id();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !socket.exists() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    // The selectable project comes from a real native existing UUID, not a
    // made-up board binding or a freshly invented folder.
    let seed = native_rpc(
        &socket,
        "thread/start",
        json!({"cwd":fixture.root.path(),"model":"isolated-fake"}),
    )["thread"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    native_rpc(&socket, "thread/archive", json!({"threadId":seed}));
    native_rpc(&socket, "thread/unarchive", json!({"threadId":seed}));
    native_rpc(
        &socket,
        "thread/resume",
        json!({"threadId":seed,"excludeTurns":true}),
    );
    native_rpc(
        &socket,
        "thread/name/set",
        json!({"threadId":seed,"name":"Creation Project"}),
    );
    let store = pikamux::store::Store::at(fixture.root.path().join("pika.db"));
    store.initialize().unwrap();
    let candidate = pikamux::model::Candidate {
        provider: pikamux::model::Provider::Codex,
        session_id: seed.clone(),
        name: Some("Creation Project".into()),
        cwd: Some(fixture.root.path().to_string_lossy().into_owned()),
        branch: None,
        transcript_path: None,
        model: Some("isolated-fake".into()),
        updated_at: 1.0,
        live: true,
        pid: None,
        source: "native-provider-verified-fixture".into(),
        parent_session_id: None,
        created_at: 1.0,
        lifecycle_status: None,
    };
    store
        .adopt_session(&pikamux::core::session_from_candidate(&candidate))
        .unwrap();
    let node = store.ensure_local_node_id().unwrap();
    assert_eq!(loaded(&socket), vec![json!(seed)]);
    let stop = fixture.root.path().join("stop");
    let worker_stop = stop.clone();
    let (delivered, delivery) = std::sync::mpsc::channel();
    let model = std::thread::spawn(move || {
        let Some((mut stream, body)) = fake_http_until(&listener, Some(&worker_stop), seconds)
        else {
            return;
        };
        assert!(
            body["input"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["role"] == "user"
                    && item["content"].as_array().is_some_and(|content| content
                        .iter()
                        .any(|part| part["text"] == "First actual phone-created reply")))
        );
        fake_output(
            &mut stream,
            json!({"type":"message","id":"phone-created-output","role":"assistant","status":"completed","content":[{"type":"output_text","text":"Actual phone-created reply received.","annotations":[]}]}),
            "phone-created-inference",
        );
        delivered.send(()).unwrap();
    });
    let marker = fixture.root.path().join("created-identity");
    let auto = std::env::var_os("PIKA_IOS_CREATION_FIXTURE_AUTO").is_some();
    let precreate = std::env::var_os("PIKA_IOS_CREATION_FIXTURE_PRECREATE").is_some();
    let feed_check = std::env::var_os("PIKA_IOS_CREATION_FEED_CHECK").is_some();
    let mut board_connection = None;
    let before_creation = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();
    if auto || precreate {
        let cwd = fixture.root.path().to_owned();
        let (mut input, mut output) = fixture.endpoint();
        if feed_check {
            let subscribed = request(&mut input, &mut output, "board/subscribe", json!({}));
            assert_eq!(subscribed["result"]["subscribed"], true);
        }
        let created = request(
            &mut input,
            &mut output,
            "conversation/create",
            json!({"nodeId":node,"provider":"codex","clientOperationId":uuid::Uuid::new_v4().to_string(),"name":"Actual Phone Creation","projectId":cwd}),
        );
        assert_eq!(created["result"]["state"], "created", "{created}");
        let identity = created["result"]["identity"].clone();
        std::fs::write(&marker, identity["threadId"].as_str().unwrap()).unwrap();
        let sent = request(
            &mut input,
            &mut output,
            "conversation/send",
            json!({"identity":identity,"clientMessageId":uuid::Uuid::new_v4().to_string(),"text":"First actual phone-created reply"}),
        );
        assert_eq!(sent["result"]["state"], "accepted", "{sent}");
        if feed_check {
            board_connection = Some((input, output));
        }
    }
    eprintln!(
        "PIKA_IOS_CREATION_FIXTURE {}",
        json!({"root":fixture.root.path(),"nodeId":node,"seedThreadId":seed,"providerPid":owner,"tmuxSocket":fixture.socket,"projectLabel":fixture.root.path().file_name().unwrap().to_string_lossy(),"creationName":"Actual Phone Creation","reply":"First actual phone-created reply","expectedResponse":"Actual phone-created reply received.","createdIdentityPath":marker,"stopMarker":stop,"adoptReadyPath":fixture.root.path().join("adopt-ready"),"adoptRemovedPath":fixture.root.path().join("adopt-removed"),"beforeCreationReadyPath":fixture.root.path().join("creation-ready"),"beforeCreationProceedPath":fixture.root.path().join("creation-proceed"),"binary":env!("CARGO_BIN_EXE_pika")})
    );
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let mut proved = false;
    let mut removed = false;
    let mut native_generation = None;
    while !stop.exists() && Instant::now() < deadline {
        if !proved && marker.exists() && delivery.try_recv().is_ok() {
            let thread = std::fs::read_to_string(&marker).unwrap();
            let thread = thread.trim();
            uuid::Uuid::parse_str(thread).unwrap();
            assert_ne!(thread, seed);
            let loaded = loaded(&socket);
            // The configured empty seed has no native TUI owner and may
            // unload normally during phone onboarding. It must remain the
            // same persisted project, not an artificially pinned runtime.
            assert!(loaded.contains(&json!(thread)));
            assert!(
                loaded
                    .iter()
                    .all(|id| id == &json!(thread) || id == &json!(seed))
            );
            let turns = native_rpc(
                &socket,
                "thread/turns/list",
                json!({"threadId":thread,"limit":10,"itemsView":"full"}),
            );
            assert!(
                turns
                    .to_string()
                    .contains("First actual phone-created reply")
            );
            let session = store
                .list_sessions()
                .unwrap()
                .into_iter()
                .find(|row| row.provider_thread_id() == thread)
                .unwrap();
            let pid = session.root_pid.unwrap();
            let process = pikamux::process::process_record(pid).unwrap();
            assert!(process.argv.iter().any(|arg| arg == thread));
            let pane = session.tmux_pane.unwrap();
            let capture_deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let captured = fixture
                    .command("/opt/homebrew/bin/tmux")
                    .args(["-L", &fixture.socket, "capture-pane", "-p", "-t", &pane])
                    .output()
                    .unwrap();
                if String::from_utf8_lossy(&captured.stdout)
                    .contains("Actual phone-created reply received.")
                {
                    break;
                }
                assert!(
                    Instant::now() < capture_deadline,
                    "Actual new native TUI did not render its response"
                );
                std::thread::sleep(Duration::from_millis(25));
            }
            assert_eq!(
                pikamux::process::process_record(pid).unwrap().generation(),
                process.generation()
            );
            assert_eq!(fixture.provider.as_ref().unwrap().id(), owner);
            native_generation = Some((pid, process.generation()));
            let seed_read = native_rpc(
                &socket,
                "thread/read",
                json!({"threadId":seed,"includeTurns":false}),
            );
            assert_eq!(seed_read["thread"]["id"], seed);
            eprintln!(
                "PIKA_IOS_CREATION_DELIVERED actual mobile-endpoint-created UUID {thread} nativePID {pid}: original user input entered actual provider and final response rendered same native desktop; existing persisted seed unchanged, no unexpected loaded UUIDs"
            );
            proved = true;
            if auto {
                if let Some((_, output)) = &mut board_connection {
                    let started = Instant::now();
                    let deadline = started + Duration::from_secs(45);
                    loop {
                        let frame = read_frame_before(output, deadline);
                        if frame["event"] != "board/snapshot" {
                            continue;
                        }
                        for item in frame["params"]["items"].as_array().unwrap() {
                            if item["identity"]["threadId"] == thread {
                                eprintln!(
                                    "New UUID feed frame: revision={} status={} stale={} observedAt={}",
                                    frame["params"]["revision"],
                                    item["status"],
                                    item["stale"],
                                    frame["params"]["observedAt"]
                                );
                            }
                        }
                        let ready = frame["params"]["items"].as_array().is_some_and(|items| {
                            items.iter().any(|item| {
                                item["identity"]["threadId"] == thread
                                    && item["status"] == "READY"
                                    && item["stale"] == false
                            })
                        });
                        let fresh = frame["params"]["observedAt"]
                            .as_f64()
                            .is_some_and(|observed| observed >= before_creation);
                        if ready && fresh {
                            eprintln!(
                                "PIKA_IOS_CREATION_FEED_READY exact new UUID {thread} became READY with fresh observation on existing shared feed after {:.2}s",
                                started.elapsed().as_secs_f64()
                            );
                            break;
                        }
                    }
                }
                break;
            }
        }
        if proved && !removed && fixture.root.path().join("adopt-ready").exists() {
            let thread = std::fs::read_to_string(&marker).unwrap();
            let result = fixture
                .command(env!("CARGO_BIN_EXE_pika"))
                .args(["untrack", &format!("codex:{}", thread.trim())])
                .output()
                .unwrap();
            assert!(
                result.status.success(),
                "Actual desktop untrack refused: {}",
                String::from_utf8_lossy(&result.stderr)
            );
            let (pid, generation) = native_generation.as_ref().unwrap();
            assert_eq!(
                &pikamux::process::process_record(*pid).unwrap().generation(),
                generation
            );
            assert_eq!(fixture.provider.as_ref().unwrap().id(), owner);
            std::fs::write(fixture.root.path().join("adopt-removed"), thread.trim()).unwrap();
            eprintln!(
                "PIKA_IOS_CREATION_REMOVED normal desktop CLI removed exact new UUID {} without changing original provider/native TUI",
                thread.trim()
            );
            removed = true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    std::fs::write(&stop, b"stop").unwrap();
    model.join().unwrap();
    assert!(
        proved,
        "No original phone-created/native desktop delivery proof"
    );
}
