//! Opt-in installed-provider probe; uses a synthetic loopback model, never user data or quota.
#![cfg(unix)]
use serde_json::{Value, json};
use std::{
    io::{Read, Write},
    net::TcpListener,
    os::unix::net::UnixStream,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

struct Server(std::process::Child);
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
struct Client {
    socket: tungstenite::WebSocket<UnixStream>,
    unsolicited: Vec<Value>,
}
struct Endpoint {
    process: Server,
    input: std::process::ChildStdin,
    frames: std::sync::mpsc::Receiver<Value>,
    events: Vec<Value>,
}
impl Endpoint {
    fn spawn(root: &std::path::Path, home: &std::path::Path, db: &std::path::Path) -> Self {
        let mut process = Server(
            Command::new(env!("CARGO_BIN_EXE_pika"))
                .arg("_mobile")
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env("HOME", home)
                .env("CODEX_HOME", home)
                .env("PIKA_DB_PATH", db)
                .env("PIKA_STATE_HOME", root.join("pika-state"))
                .env("PIKA_CONFIG_HOME", root.join("pika-config"))
                .env("XDG_CONFIG_HOME", root.join("config"))
                .env("XDG_DATA_HOME", root.join("data"))
                .env("XDG_STATE_HOME", root.join("state"))
                .env("TMPDIR", root)
                .env("TMUX_TMPDIR", root.join("tmux"))
                .env("PIKA_TMUX_SOCKET", "ios-isolated")
                .env("PIKA_UPDATE_CHECK", "0")
                .current_dir(root)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()
                .unwrap(),
        );
        let input = process.0.stdin.take().unwrap();
        let output = process.0.stdout.take().unwrap();
        let (sender, frames) = std::sync::mpsc::sync_channel(32);
        std::thread::spawn(move || {
            use std::io::BufRead;
            let mut output = std::io::BufReader::new(output);
            loop {
                let mut line = String::new();
                match output.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        let value = serde_json::from_str(&line).unwrap();
                        if sender.send(value).is_err() {
                            break;
                        }
                    }
                }
            }
        });
        Self {
            process,
            input,
            frames,
            events: Vec::new(),
        }
    }
    fn submit(&mut self, method: &str, params: Value) -> String {
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
        let id = self.submit(method, params);
        loop {
            let frame = self.frames.recv_timeout(Duration::from_secs(15)).unwrap();
            if frame["id"] == id {
                return frame;
            }
            self.events.push(frame);
        }
    }
    fn question(&mut self) -> Value {
        loop {
            if let Some(index) = self.events.iter().position(|event| {
                event["event"] == "conversation/event"
                    && event["params"]["method"] == "item/tool/requestUserInput"
            }) {
                return self.events.remove(index)["params"].clone();
            }
            self.events
                .push(self.frames.recv_timeout(Duration::from_secs(15)).unwrap());
        }
    }
}
#[allow(clippy::too_many_arguments)]
fn run_endpoint_probe(
    root: &std::path::Path,
    home: &std::path::Path,
    db: &std::path::Path,
    node: &str,
    thread: &str,
    turn: &str,
    question: &Value,
    desktop: &mut Client,
    answer_received: std::sync::mpsc::Receiver<()>,
    release: std::sync::mpsc::Sender<()>,
    delivery_received: std::sync::mpsc::Receiver<()>,
) {
    let identity = json!({"nodeId":node,"provider":"codex","threadId":thread});
    if std::env::var_os("PIKA_IOS_LARGE_HISTORY").is_some() {
        let socket =
            UnixStream::connect(home.join("app-server-control/app-server-control.sock")).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let config = tungstenite::protocol::WebSocketConfig::default()
            .max_message_size(Some(1024 * 1024))
            .max_frame_size(Some(1024 * 1024));
        let (wire, _) =
            tungstenite::client::client_with_config("ws://localhost/", socket, Some(config))
                .unwrap();
        let mut old = Client {
            socket: wire,
            unsolicited: Vec::new(),
        };
        old.rpc(1, "initialize", json!({"clientInfo":{"name":"old-limit-regression","version":"1"},"capabilities":{"experimentalApi":true}}));
        old.send(json!({"id":2,"method":"thread/turns/list","params":{"threadId":thread,"limit":10,"itemsView":"full"}}));
        loop {
            match old.socket.read() {
                Err(tungstenite::Error::Capacity(_)) => {
                    eprintln!(
                        "Reproduced original 1 MiB provider limit failure on the same history"
                    );
                    break;
                }
                Ok(_) => {}
                Err(error) => panic!("Unexpected baseline error: {error}"),
            }
        }
    }
    let mut phone = Endpoint::spawn(root, home, db);
    assert_eq!(phone.rpc("hello", json!({}))["result"]["nodeId"], node);
    assert!(
        phone
            .rpc("board/subscribe", json!({}))
            .get("error")
            .is_none()
    );
    let opened = phone.rpc("conversation/open", json!({"identity":identity}));
    assert!(opened.get("error").is_none(), "{opened}");
    assert_eq!(opened["result"]["identity"], identity);
    if std::env::var_os("PIKA_IOS_LARGE_HISTORY").is_some() {
        let size = serde_json::to_vec(&opened).unwrap().len();
        assert!(
            size > 3 * 1024 * 1024,
            "Oversized fixture did not reach the real mobile endpoint"
        );
        eprintln!("Actual mobile open response: {size} bytes, original identity preserved");
    }
    assert_eq!(opened["result"]["activeTurnId"], turn);
    let pending = phone.question();
    assert_eq!(pending["requestId"], question["id"]);
    assert_eq!(pending["params"], question["params"]);
    let status_params = json!({"identity":identity,"requestId":pending["requestId"],"turnId":turn,"itemId":pending["params"]["itemId"]});
    assert_eq!(
        phone.rpc("conversation/requestStatus", status_params.clone())["result"]["state"],
        "pending"
    );
    let answered=phone.rpc("conversation/answer",json!({"identity":identity,"requestId":pending["requestId"],"turnId":turn,"itemId":pending["params"]["itemId"],"answers":{"probe":{"answers":["Proceed"]}}}));
    assert_eq!(answered["result"]["state"], "submitted", "{answered}");
    answer_received
        .recv_timeout(Duration::from_secs(10))
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let status = phone.rpc("conversation/requestStatus", status_params.clone());
        if status["result"]["state"] == "resolved" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "Observed native resolution was lost: {status}"
        );
    }
    let message = uuid::Uuid::new_v4().to_string();
    let payload = json!({"identity":identity,"clientMessageId":message,"expectedTurnId":turn,"text":"Exact synthetic mobile reply\nsecond line"});
    phone.submit("conversation/send", payload.clone());
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let ledger = rusqlite::Connection::open(db).unwrap();
        let result = ledger.query_row(
            "SELECT outcome FROM mobile_deliveries WHERE id=?",
            [&message],
            |r| r.get::<_, String>(0),
        );
        if result
            .ok()
            .is_some_and(|s| serde_json::from_str::<Value>(&s).unwrap()["state"] == "accepted")
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "Provider did not accept original phone message"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    // Lose the client before it consumes the send receipt; the provider survives.
    phone.process.0.kill().unwrap();
    phone.process.0.wait().unwrap();
    drop(phone);
    release.send(()).unwrap();
    delivery_received
        .recv_timeout(Duration::from_secs(10))
        .unwrap();
    let mut recovered = Endpoint::spawn(root, home, db);
    let opened = recovered.rpc("conversation/open", json!({"identity":identity}));
    assert!(opened.get("error").is_none(), "{opened}");
    assert_eq!(
        recovered.rpc("conversation/requestStatus", status_params)["result"]["state"],
        "unknown",
        "Absence on reconnect is not resolution proof"
    );
    let receipt = recovered.rpc(
        "conversation/receipt",
        json!({"identity":identity,"clientMessageId":message}),
    );
    assert_eq!(receipt["result"]["state"], "delivered", "{receipt}");
    let repeated = recovered.rpc("conversation/send", payload.clone());
    assert_eq!(repeated["result"]["state"], "delivered", "{repeated}");
    let conflicting=recovered.rpc("conversation/send",json!({"identity":identity,"clientMessageId":message,"expectedTurnId":turn,"text":"different text"}));
    assert!(conflicting.get("error").is_some(), "{conflicting}");
    let history = desktop.rpc(
        40,
        "thread/turns/list",
        json!({"threadId":thread,"limit":10,"itemsView":"full"}),
    );
    let count = history["result"]["data"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|turn| turn["items"].as_array().unwrap())
        .filter(|item| item["type"] == "userMessage" && item["clientId"] == message)
        .count();
    assert_eq!(
        count, 1,
        "Same durable message must appear exactly once: {history}"
    );
    eprintln!(
        "Actual Pika JSONL endpoint: exact question answered, lost multiline receipt reconciled from native original history, same-ID replay caused zero extra messages."
    );
}
impl Client {
    fn connect(path: &std::path::Path) -> Self {
        let socket = UnixStream::connect(path).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let (socket, _) = tungstenite::client("ws://localhost/", socket).unwrap();
        let mut client = Self {
            socket,
            unsolicited: Vec::new(),
        };
        let response = client.rpc(1, "initialize", json!({"clientInfo":{"name":"pika_ios_isolated_spike","version":"1"},"capabilities":{"experimentalApi":true}}));
        assert!(response.get("error").is_none(), "{response}");
        client.send(json!({"method":"initialized","params":{}}));
        client
    }
    fn rpc(&mut self, id: u64, method: &str, params: Value) -> Value {
        self.send(json!({"id":id,"method":method,"params":params}));
        loop {
            let value = self.receive();
            if value["id"] == id && (value.get("result").is_some() || value.get("error").is_some())
            {
                return value;
            }
            self.unsolicited.push(value);
        }
    }
    fn send(&mut self, value: Value) {
        self.socket
            .send(tungstenite::Message::Text(value.to_string().into()))
            .unwrap();
    }
    fn pending_question(&mut self) -> Value {
        if let Some(index) = self
            .unsolicited
            .iter()
            .position(|v| v["method"] == "item/tool/requestUserInput")
        {
            return self.unsolicited.remove(index);
        }
        loop {
            let value = self.receive();
            if value["method"] == "item/tool/requestUserInput" {
                return value;
            }
            self.unsolicited.push(value);
        }
    }
    fn receive(&mut self) -> Value {
        loop {
            match self.socket.read().unwrap() {
                tungstenite::Message::Text(text) => return serde_json::from_str(&text).unwrap(),
                tungstenite::Message::Ping(_) | tungstenite::Message::Pong(_) => {
                    self.socket.flush().unwrap()
                }
                frame => panic!("Unexpected provider frame: {frame:?}"),
            }
        }
    }
}

#[test]
#[ignore = "Requires explicitly selected installed Codex: PIKA_IOS_CODEX; fake loopback model only"]
fn two_clients_read_same_loaded_thread_and_reject_stale_steering() {
    run_probe(false);
}
#[test]
#[ignore = "Actual Pika JSONL endpoint and installed Codex; isolated fake loopback model"]
fn mobile_handler_resolves_question_and_recovers_lost_multiline_receipt() {
    run_probe(true);
}
fn run_probe(use_endpoint: bool) {
    let executable =
        std::env::var_os("PIKA_IOS_CODEX").expect("Select a provider executable explicitly");
    let root = tempfile::Builder::new()
        .prefix("pika-ios-")
        .tempdir()
        .unwrap();
    let home = root.path().join("home");
    std::fs::create_dir(&home).unwrap();
    let socket_dir = home.join("app-server-control");
    std::fs::create_dir(&socket_dir).unwrap();
    let socket = socket_dir.join("app-server-control.sock");
    let fixture_seconds = std::env::var("PIKA_IOS_FIXTURE_SECONDS")
        .ok()
        .map(|v| v.parse::<u64>().unwrap().clamp(30, 900));
    let fake = TcpListener::bind("127.0.0.1:0").unwrap();
    fake.set_nonblocking(true).unwrap();
    let fake_url = format!("http://{}/v1", fake.local_addr().unwrap());
    let (release, wait) = std::sync::mpsc::channel();
    let (ready, received) = std::sync::mpsc::channel();
    let (answered, answer_received) = std::sync::mpsc::channel();
    let (delivered, delivery_received) = std::sync::mpsc::channel();
    let large_history = std::env::var_os("PIKA_IOS_LARGE_HISTORY").is_some();
    let fake_worker = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(fixture_seconds.unwrap_or(10));
        let mut stream = loop {
            match fake.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "No fake model request arrived");
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
        let length: usize = header
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .map(|v| v.trim().parse().unwrap())
            })
            .unwrap();
        assert!(length < 16 * 1024 * 1024);
        let mut body = vec![0; length];
        stream.read_exact(&mut body).unwrap();
        let request: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(request["model"], "isolated-fake");
        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\nevent: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"fake-running-response\",\"status\":\"in_progress\",\"output\":[]}}\n\n").unwrap();
        let call = json!({"type":"function_call","id":"fc-isolated","call_id":"call-isolated","name":"request_user_input","arguments":json!({"questions":[{"header":"Probe","id":"probe","question":"Choose the isolated path","options":[{"label":"Proceed","description":"Continue synthetic probe"},{"label":"Stop","description":"Stop synthetic probe"}]}]}).to_string()});
        let mut items = vec![call.clone()];
        if large_history {
            let large = json!({"type":"reasoning","id":"large-history-reasoning","summary":[{"type":"summary_text","text":"x".repeat(3 * 1024 * 1024)}]});
            for kind in ["response.output_item.added", "response.output_item.done"] {
                writeln!(
                    stream,
                    "event: {kind}\ndata: {}\n",
                    json!({"type":kind,"output_index":1,"item":large})
                )
                .unwrap();
            }
            items.push(large);
        }
        for event in [
            json!({"type":"response.output_item.added","output_index":0,"item":call}),
            json!({"type":"response.output_item.done","output_index":0,"item":call}),
            json!({"type":"response.completed","response":{"id":"fake-running-response","status":"completed","output":items,"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}),
        ] {
            writeln!(
                stream,
                "event: {}\ndata: {}\n",
                event["type"].as_str().unwrap(),
                event
            )
            .unwrap();
        }
        ready.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(fixture_seconds.unwrap_or(10));
        let mut continuation = loop {
            match fake.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "Answered turn did not continue");
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(error) => panic!("{error}"),
            }
        };
        continuation.set_nonblocking(false).unwrap();
        continuation
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut header = Vec::new();
        while !header.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            continuation.read_exact(&mut byte).unwrap();
            header.push(byte[0]);
            assert!(header.len() < 65536);
        }
        let header = String::from_utf8(header).unwrap();
        let length: usize = header
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .map(|v| v.trim().parse().unwrap())
            })
            .unwrap();
        assert!(length < 16 * 1024 * 1024);
        let mut body = vec![0; length];
        continuation.read_exact(&mut body).unwrap();
        let continued: Value = serde_json::from_slice(&body).unwrap();
        let tool_output = continued["input"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["type"] == "function_call_output" && v["call_id"] == "call-isolated")
            .expect("Original tool call must receive the phone answer");
        assert!(
            tool_output["output"].as_str().unwrap().contains("Proceed"),
            "{tool_output}"
        );
        continuation.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\nevent: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"fake-continuation\",\"status\":\"in_progress\",\"output\":[]}}\n\n").unwrap();
        answered.send(()).unwrap();
        wait.recv_timeout(Duration::from_secs(fixture_seconds.unwrap_or(15)))
            .unwrap();
        writeln!(continuation,"event: response.completed\ndata: {}\n",json!({"type":"response.completed","response":{"id":"fake-continuation","status":"completed","output":[],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}})).unwrap();
        let deadline = Instant::now() + Duration::from_secs(fixture_seconds.unwrap_or(10));
        let mut next = loop {
            match fake.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "Steered turn did not continue");
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(error) => panic!("{error}"),
            }
        };
        next.set_nonblocking(false).unwrap();
        next.set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut header = Vec::new();
        while !header.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            next.read_exact(&mut byte).unwrap();
            header.push(byte[0]);
            assert!(header.len() < 65536);
        }
        let header = String::from_utf8(header).unwrap();
        let length: usize = header
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .map(|v| v.trim().parse().unwrap())
            })
            .unwrap();
        assert!(length < 16 * 1024 * 1024);
        let mut body = vec![0; length];
        next.read_exact(&mut body).unwrap();
        let next_input: Value = serde_json::from_slice(&body).unwrap();
        fn has_text(value: &Value) -> bool {
            match value {
                Value::String(s) => s == "Exact synthetic mobile reply\nsecond line",
                Value::Array(a) => a.iter().any(has_text),
                Value::Object(o) => o.values().any(has_text),
                _ => false,
            }
        }
        assert!(
            has_text(&next_input["input"]),
            "Exact steered text absent from existing turn continuation: {next_input}"
        );
        delivered.send(()).unwrap();
        if fixture_seconds.is_some() {
            next.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            )
            .unwrap();
            let message = json!({"type":"message","id":"fixture-result","role":"assistant","status":"completed","content":[{"type":"output_text","text":"Fixture received the exact multiline reply.","annotations":[]}]});
            for event in [
                json!({"type":"response.created","response":{"id":"fixture-final","status":"in_progress","output":[]}}),
                json!({"type":"response.output_item.added","output_index":0,"item":message}),
                json!({"type":"response.output_item.done","output_index":0,"item":message}),
                json!({"type":"response.completed","response":{"id":"fixture-final","status":"completed","output":[message],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}),
            ] {
                writeln!(
                    next,
                    "event: {}\ndata: {}\n",
                    event["type"].as_str().unwrap(),
                    event
                )
                .unwrap();
            }
        }
    });
    let mut server = Server(
        Command::new(&executable)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &home)
            .env("CODEX_HOME", &home)
            .env("XDG_CONFIG_HOME", root.path().join("config"))
            .env("XDG_DATA_HOME", root.path().join("data"))
            .env("XDG_STATE_HOME", root.path().join("state"))
            .env("TMPDIR", root.path())
            .current_dir(root.path())
            .args([
                "-c",
                "model_provider=\"isolated\"",
                "-c",
                "model_providers.isolated.name=\"Isolated unreachable fake\"",
                "-c",
                &format!("model_providers.isolated.base_url=\"{fake_url}\""),
                "-c",
                "model_providers.isolated.wire_api=\"responses\"",
                "-c",
                "model_providers.isolated.requires_openai_auth=false",
                "-c",
                "model_providers.isolated.supports_websockets=false",
                "-c",
                "analytics.enabled=false",
                "app-server",
                "--listen",
            ])
            .arg(format!("unix://{}", socket.display()))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    let until = Instant::now() + Duration::from_secs(10);
    while !socket.exists() {
        assert!(Instant::now() < until, "Provider socket did not appear");
        assert!(server.0.try_wait().unwrap().is_none(), "Provider exited");
        std::thread::sleep(Duration::from_millis(25));
    }
    let mut desktop = Client::connect(&socket);
    let created = desktop.rpc(2, "thread/start", json!({"cwd":root.path(),"model":"isolated-fake","approvalPolicy":"never","sandbox":"read-only"}));
    assert!(created.get("error").is_none(), "{created}");
    let identity = created["result"]["thread"]["id"].as_str().unwrap();
    let started=desktop.rpc(3,"turn/start",json!({"threadId":identity,"collaborationMode":{"mode":"plan","settings":{"model":"isolated-fake","reasoning_effort":"medium","developer_instructions":null}},"input":[{"type":"text","text":"Synthetic isolated provider transport probe"}]}));
    assert!(started.get("error").is_none(), "{started}");
    let active = started["result"]["turn"]["id"].as_str().unwrap();
    received
        .recv_timeout(Duration::from_secs(10))
        .expect("Actual provider must reach synthetic model");
    let original_question = desktop.pending_question();
    assert_eq!(original_question["params"]["threadId"], identity);
    assert_eq!(original_question["params"]["turnId"], active);
    if fixture_seconds.is_some() || use_endpoint {
        let named = desktop.rpc(
            90,
            "thread/name/set",
            json!({"threadId":identity,"name":"Mobile Synthetic"}),
        );
        assert!(named.get("error").is_none(), "{named}");
        let db = root.path().join("pika.db");
        let store = pikamux::store::Store::at(&db);
        store.initialize().unwrap();
        let candidate = pikamux::model::Candidate {
            provider: pikamux::model::Provider::Codex,
            session_id: identity.into(),
            name: Some("Mobile Synthetic".into()),
            cwd: Some(root.path().to_string_lossy().into_owned()),
            branch: None,
            transcript_path: None,
            model: Some("isolated-fake".into()),
            updated_at: 1.0,
            live: true,
            pid: Some(server.0.id() as i64),
            source: "ios-synthetic-selected".into(),
            parent_session_id: None,
            created_at: 1.0,
            lifecycle_status: Some(pikamux::model::Status::NeedsYou),
        };
        store
            .adopt_session(&pikamux::core::session_from_candidate(&candidate))
            .unwrap();
        let node = store.ensure_local_node_id().unwrap();
        if use_endpoint {
            run_endpoint_probe(
                root.path(),
                &home,
                &db,
                &node,
                identity,
                active,
                &original_question,
                &mut desktop,
                answer_received,
                release,
                delivery_received,
            );
            fake_worker.join().unwrap();
            return;
        }
        eprintln!(
            "PIKA_IOS_FIXTURE {}",
            json!({"root":root.path(),"home":home,"codexHome":home,"database":db,"threadId":identity,"nodeId":node,"socket":socket,"providerPid":server.0.id(),"binary":env!("CARGO_BIN_EXE_pika"),"turnId":active,"question":original_question,"stopMarker":root.path().join("stop"),"adoptReadyPath":root.path().join("adopt-ready"),"adoptRemovedPath":root.path().join("adopt-removed")})
        );
        let deadline = Instant::now() + Duration::from_secs(fixture_seconds.unwrap());
        let mut advanced = false;
        let mut proved_delivery = false;
        let mut removed = false;
        while Instant::now() < deadline && !root.path().join("stop").exists() {
            if !advanced && answer_received.try_recv().is_ok() {
                let _ = release.send(());
                advanced = true;
            }
            if !proved_delivery && delivery_received.try_recv().is_ok() {
                eprintln!(
                    "PIKA_IOS_FIXTURE_DELIVERED exact multiline input reached the original provider inference for {identity}"
                );
                proved_delivery = true;
            }
            if proved_delivery && !removed && root.path().join("adopt-ready").exists() {
                let result = Command::new(env!("CARGO_BIN_EXE_pika"))
                    .args(["untrack", &format!("codex:{identity}")])
                    .env_clear()
                    .env("PATH", "/usr/bin:/bin")
                    .env("HOME", &home)
                    .env("CODEX_HOME", &home)
                    .env("PIKA_DB_PATH", &db)
                    .env("PIKA_STATE_HOME", root.path().join("pika-state"))
                    .env("PIKA_CONFIG_HOME", root.path().join("pika-config"))
                    .env("XDG_CONFIG_HOME", root.path().join("config"))
                    .env("XDG_DATA_HOME", root.path().join("data"))
                    .env("XDG_STATE_HOME", root.path().join("state"))
                    .env("TMPDIR", root.path())
                    .env("TMUX_TMPDIR", root.path().join("tmux"))
                    .env("PIKA_TMUX_SOCKET", "ios-isolated")
                    .env("PIKA_UPDATE_CHECK", "0")
                    .current_dir(root.path())
                    .output()
                    .unwrap();
                assert!(
                    result.status.success(),
                    "Desktop untrack failed: {} {}",
                    String::from_utf8_lossy(&result.stdout),
                    String::from_utf8_lossy(&result.stderr)
                );
                assert_eq!(unsafe { libc::kill(server.0.id() as i32, 0) }, 0);
                std::fs::write(root.path().join("adopt-removed"), identity).unwrap();
                eprintln!(
                    "PIKA_IOS_FIXTURE_REMOVED exact desktop untrack retained original provider PID and UUID {identity}"
                );
                removed = true;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        drop(server);
        return;
    }
    let mut mobile = Client::connect(&socket);
    let read = mobile.rpc(
        2,
        "thread/read",
        json!({"threadId":identity,"includeTurns":false}),
    );
    assert_eq!(read["result"]["thread"]["id"], identity, "{read}");
    let resumed = mobile.rpc(3, "thread/resume", json!({"threadId":identity}));
    assert_eq!(resumed["result"]["thread"]["id"], identity, "{resumed}");
    let replayed = mobile.pending_question();
    assert_eq!(
        replayed, original_question,
        "Late attachment must replay exact pending request"
    );
    mobile
        .send(json!({"id":replayed["id"],"result":{"answers":{"probe":{"answers":["Proceed"]}}}}));
    loop {
        let frame = desktop.receive();
        if frame["method"] == "serverRequest/resolved" {
            assert_eq!(frame["params"]["threadId"], identity);
            assert_eq!(frame["params"]["requestId"], replayed["id"]);
            break;
        }
        desktop.unsolicited.push(frame);
    }
    answer_received
        .recv_timeout(Duration::from_secs(10))
        .expect("Phone answer must enter the existing provider turn continuation");
    let accepted=mobile.rpc(7,"turn/steer",json!({"threadId":identity,"expectedTurnId":active,"clientUserMessageId":"isolated-valid-message","input":[{"type":"text","text":"Exact synthetic mobile reply\nsecond line"}]}));
    assert!(accepted.get("error").is_none(), "{accepted}");
    assert_eq!(accepted["result"]["turnId"], active, "{accepted}");
    let _ = release.send(());
    delivery_received
        .recv_timeout(Duration::from_secs(10))
        .expect("Accepted steer must enter original provider inference");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let history = desktop.rpc(
            8,
            "thread/turns/list",
            json!({"threadId":identity,"limit":2,"itemsView":"full"}),
        );
        assert!(history.get("error").is_none(), "{history}");
        let found = history["result"]["data"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|turn| turn["items"].as_array().unwrap())
            .any(|item| {
                item["type"] == "userMessage"
                    && item["content"].as_array().is_some_and(|parts| {
                        parts
                            .iter()
                            .any(|part| part["text"] == "Exact synthetic mobile reply\nsecond line")
                    })
            });
        if found {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "Accepted multiline reply missing from original provider history: {history}"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    let rejected = mobile.rpc(4, "turn/steer", json!({"threadId":identity,"expectedTurnId":"not-an-active-turn","clientUserMessageId":"isolated-message","input":[{"type":"text","text":"must not be delivered"}]}));
    assert!(rejected.get("error").is_some(), "{rejected}");
    let original = desktop.rpc(
        5,
        "thread/read",
        json!({"threadId":identity,"includeTurns":false}),
    );
    assert_eq!(original["result"]["thread"]["id"], identity);
    let stopped = desktop.rpc(
        6,
        "turn/interrupt",
        json!({"threadId":identity,"turnId":active}),
    );
    assert!(stopped.get("error").is_none(), "{stopped}");
    fake_worker.join().unwrap();
    eprintln!(
        "Actual installed provider: two clients, same server PID {}, same running thread {}; exact question replayed and answered; valid steering accepted and stale steering rejected. Only synthetic loopback model accessed.",
        server.0.id(),
        identity
    );
}
