//! Explicit real-provider, model-free assistant launch proof. Private homes,
//! tmux socket and synthetic provider config; never user sessions or credentials.
#![cfg(unix)]
use serde_json::Value;
use serde_json::json;
use std::os::unix::net::UnixStream;
use std::{
    fs,
    io::{Read, Write},
    os::unix::{fs::PermissionsExt, io::FromRawFd, process::CommandExt},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

struct Fixture {
    root: tempfile::TempDir,
    codex: PathBuf,
    socket: String,
}

struct Rpc {
    wire: tungstenite::WebSocket<UnixStream>,
    next: u64,
}
impl Rpc {
    fn connect(path: &Path) -> Self {
        let stream = UnixStream::connect(path).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let (wire, _) = tungstenite::client("ws://localhost/", stream).unwrap();
        let mut client = Self { wire, next: 0 };
        client.request("initialize",json!({"clientInfo":{"name":"pika_assistant_isolated_acceptance","version":"1"},"capabilities":{"experimentalApi":true}}));
        client
            .wire
            .send(tungstenite::Message::text(
                json!({"method":"initialized","params":{}}).to_string(),
            ))
            .unwrap();
        client
    }
    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next += 1;
        let id = self.next;
        self.wire
            .send(tungstenite::Message::text(
                json!({"id":id,"method":method,"params":params}).to_string(),
            ))
            .unwrap();
        for _ in 0..256 {
            let frame = self.wire.read().unwrap();
            if let tungstenite::Message::Text(text) = frame {
                let value: Value = serde_json::from_str(&text).unwrap();
                if value["id"] == id
                    && (value.get("result").is_some() || value.get("error").is_some())
                {
                    assert!(value.get("error").is_none(), "{method}: {value}");
                    return value["result"].clone();
                }
            }
        }
        panic!("No bounded provider response for {method}");
    }
}
impl Fixture {
    fn command(&self, executable: &Path) -> Command {
        let root = self.root.path();
        let mut command = Command::new(executable);
        command
            .env_clear()
            .current_dir(root)
            .env("PATH", "/opt/homebrew/bin:/usr/bin:/bin")
            .env("HOME", root.join("home"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_STATE_HOME", root.join("state"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env("PIKA_CONFIG_HOME", root.join("config/pika"))
            .env("PIKA_STATE_HOME", root.join("state"))
            .env("PIKA_DB_PATH", root.join("board.sqlite"))
            .env("PIKA_UPDATE_CHECK", "0")
            .env("CODEX_HOME", self.profile().join("provider-home"))
            .env("CLAUDE_CONFIG_DIR", root.join("claude"))
            .env("OPENCODE_DATA_HOME", root.join("opencode"))
            .env("TMPDIR", root.join("tmp"))
            .env("TMUX_TMPDIR", root.join("tmux"))
            .env("PIKA_TMUX_SOCKET", &self.socket)
            .env("TERM", "xterm-256color")
            .env("SHELL", "/bin/sh");
        command
    }
    fn profile(&self) -> PathBuf {
        self.root.path().join("state/assistant")
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self
            .command(Path::new("/opt/homebrew/bin/tmux"))
            .args(["-L", &self.socket, "kill-server"])
            .output();
        let _ = self
            .command(&self.codex)
            .args(["app-server", "daemon", "stop"])
            .output();
    }
}
struct Terminal {
    child: Child,
    master: fs::File,
    output: Vec<u8>,
}
impl Terminal {
    fn spawn(command: &mut Command) -> Self {
        let mut master = -1;
        let mut slave = -1;
        let mut size = libc::winsize {
            ws_row: 40,
            ws_col: 120,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &mut size,
                )
            },
            0
        );
        let master = unsafe { fs::File::from_raw_fd(master) };
        let slave = unsafe { fs::File::from_raw_fd(slave) };
        let child = command
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave))
            .process_group(0)
            .spawn()
            .unwrap();
        use std::os::unix::io::AsRawFd;
        unsafe {
            libc::fcntl(master.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK);
        }
        Self {
            child,
            master,
            output: Vec::new(),
        }
    }
    fn collect(&mut self) {
        let mut bytes = [0; 8192];
        while let Ok(count) = self.master.read(&mut bytes) {
            if count == 0 {
                break;
            }
            self.output.extend_from_slice(&bytes[..count]);
            assert!(self.output.len() < 1024 * 1024);
        }
    }
}
impl Drop for Terminal {
    fn drop(&mut self) {
        unsafe {
            libc::kill(-(self.child.id() as i32), libc::SIGKILL);
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
#[ignore = "PIKA_IOS_CODEX selects installed provider; disposable homes/tmux, no model turn"]
fn fresh_assistant_reserves_one_shared_uuid_and_reopens_same_live_terminal() {
    let codex =
        PathBuf::from(std::env::var_os("PIKA_IOS_CODEX").expect("Explicit provider required"))
            .canonicalize()
            .unwrap();
    let fixture = Fixture {
        root: tempfile::Builder::new()
            .prefix("pa-")
            .tempdir_in("/tmp")
            .unwrap(),
        codex,
        socket: format!("assistant-{}", uuid::Uuid::new_v4().simple()),
    };
    for name in [
        "home", "config", "state", "data", "cache", "tmp", "tmux", "claude", "opencode",
    ] {
        fs::create_dir(fixture.root.path().join(name)).unwrap();
        fs::set_permissions(
            fixture.root.path().join(name),
            fs::Permissions::from_mode(0o700),
        )
        .unwrap();
    }
    let pika = Path::new(env!("CARGO_BIN_EXE_pika"));
    let initial = fixture
        .command(pika)
        .args([
            "pika",
            "--scope",
            "personal",
            "--remember",
            "Keep my synthetic preference",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        initial.status.success(),
        "{}",
        String::from_utf8_lossy(&initial.stderr)
    );
    let state: Value = serde_json::from_slice(&initial.stdout).unwrap();
    let profile_id = state["profile_id"].as_str().unwrap();
    let provider_home = fixture.profile().join("provider-home");
    fs::create_dir_all(&provider_home).unwrap();
    fs::set_permissions(&provider_home, fs::Permissions::from_mode(0o700)).unwrap();
    let held_seconds = std::env::var("PIKA_IOS_ASSISTANT_FIXTURE_SECONDS")
        .ok()
        .map(|value| value.parse::<u64>().unwrap());
    if let Some(seconds) = held_seconds {
        assert!((30..=900).contains(&seconds));
    }
    let listener = held_seconds.map(|_| std::net::TcpListener::bind("127.0.0.1:0").unwrap());
    let url = listener
        .as_ref()
        .map(|listener| format!("http://{}/v1", listener.local_addr().unwrap()))
        .unwrap_or_else(|| "http://127.0.0.1:9/v1".into());
    fs::write(provider_home.join("config.toml"),format!("model_provider='synthetic'\n[model_providers.synthetic]\nname='Isolated synthetic inference'\nbase_url='{url}'\nwire_api='responses'\nrequires_openai_auth=false\nsupports_websockets=false\n[analytics]\nenabled=false\n")).unwrap();
    fs::set_permissions(
        provider_home.join("config.toml"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let mut launch = fixture.command(pika);
    launch.args([
        "pika",
        "--scope",
        "personal",
        "--profile-root",
        fixture.profile().to_str().unwrap(),
        "--expected-profile-id",
        profile_id,
        "--enable-codex",
        fixture.codex.to_str().unwrap(),
        "--no-call-limit",
        "--set-default",
    ]);
    let mut first = Terminal::spawn(&mut launch);
    let deadline = Instant::now() + Duration::from_secs(30);
    let (binding, session) = loop {
        first.collect();
        if let Ok(bytes) = fs::read(fixture.profile().join("native-binding.json")) {
            let binding: Value = serde_json::from_slice(&bytes).unwrap();
            if let Some(thread) = binding["thread_id"].as_str() {
                let store =
                    pikamux::store::Store::at(fixture.profile().join("native-registry/pika.db"));
                if let Some(session) = store
                    .get_session(pikamux::model::Provider::Codex, thread)
                    .unwrap()
                {
                    if session.root_pid.is_some() {
                        break (binding, session);
                    }
                }
            }
        }
        assert!(
            first.child.try_wait().unwrap().is_none(),
            "Assistant exited: {}",
            String::from_utf8_lossy(&first.output)
        );
        assert!(
            Instant::now() < deadline,
            "No exact assistant home: {}",
            String::from_utf8_lossy(&first.output)
        );
        std::thread::sleep(Duration::from_millis(25));
    };
    assert_eq!(binding["profile_id"], profile_id);
    assert_eq!(binding["memory_epoch"], 0);
    assert!(binding["shared_creation"].as_bool().unwrap());
    assert!(binding["shared_ready"].as_bool().unwrap());
    assert_eq!(session.session_id, binding["thread_id"]);
    assert_eq!(session.source, "private-shared-provider");
    let mut provider =
        Rpc::connect(&provider_home.join("app-server-control/app-server-control.sock"));
    let history = provider.request(
        "thread/read",
        json!({"threadId":session.session_id,"includeTurns":true}),
    );
    assert_eq!(history["thread"]["id"], session.session_id);
    assert!(
        history["thread"]["turns"].as_array().unwrap().is_empty(),
        "Initialization must not create a model turn"
    );
    let loaded = provider.request("thread/loaded/list", json!({"limit":64}));
    assert_eq!(loaded["data"], json!([session.session_id]));
    let inventory = provider.request(
        "mcpServerStatus/list",
        json!({"threadId":session.session_id,"detail":"toolsAndAuthOnly"}),
    );
    assert!(
        inventory.to_string().contains("pika_memory_search"),
        "{inventory}"
    );
    let recall=provider.request("mcpServer/tool/call",json!({"threadId":session.session_id,"server":"pika","tool":"pika_memory_search","arguments":{"query":"synthetic preference"}}));
    assert_ne!(recall["isError"], true, "{recall}");
    assert!(
        recall.to_string().contains("Keep my synthetic preference"),
        "{recall}"
    );
    let skills = provider.request(
        "skills/list",
        json!({"cwds":[fixture.profile().join("native-assistant")]}),
    );
    for name in [
        "pika-control",
        "pika-memory",
        "pika-self-awareness",
        "pika-reflection",
        "pika-consolidation",
        "pika-user-feedback",
        "pika-small-council-grasp",
    ] {
        assert!(
            skills.to_string().contains(name),
            "Missing {name}: {skills}"
        );
    }
    // The phone endpoint must attach through the actual saved selection and
    // private generation fence, not through a synthetic board binding.
    let open_id = uuid::Uuid::new_v4().to_string();
    let mut mobile = fixture
        .command(pika)
        .arg("_mobile")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    {
        let mut input = mobile.stdin.take().unwrap();
        writeln!(
            input,
            "{}",
            json!({"v":1,"id":open_id,"method":"assistant/open","params":{}})
        )
        .unwrap();
    }
    let mobile = mobile.wait_with_output().unwrap();
    assert!(
        mobile.status.success(),
        "{}",
        String::from_utf8_lossy(&mobile.stderr)
    );
    let opened: Value = String::from_utf8(mobile.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .find(|frame| frame["id"] == open_id)
        .unwrap();
    assert!(opened.get("error").is_none(), "{opened}");
    assert_eq!(opened["result"]["identity"]["threadId"], session.session_id);
    assert_eq!(opened["result"]["assistant"]["profileId"], profile_id);
    assert_eq!(opened["result"]["assistant"]["scope"], "personal");
    assert_eq!(
        opened["result"]["assistant"]["memoryEpoch"],
        binding["memory_epoch"]
    );
    assert_eq!(
        provider.request("thread/loaded/list", json!({"limit":64}))["data"],
        json!([session.session_id])
    );
    let recalled=provider.request("mcpServer/tool/call",json!({"threadId":session.session_id,"server":"pika","tool":"pika_memory_search","arguments":{"query":"synthetic preference"}}));
    assert!(
        recalled
            .to_string()
            .contains("Keep my synthetic preference"),
        "{recalled}"
    );
    assert_eq!(
        serde_json::from_slice::<Value>(
            &fs::read(fixture.profile().join("native-binding.json")).unwrap()
        )
        .unwrap(),
        binding
    );
    let pid = session.root_pid.unwrap();
    assert_eq!(unsafe { libc::kill(pid as i32, 0) }, 0);
    let mut second = Terminal::spawn(&mut launch);
    std::thread::sleep(Duration::from_millis(800));
    second.collect();
    assert!(
        second.child.try_wait().unwrap().is_none(),
        "Reopen failed: {}",
        String::from_utf8_lossy(&second.output)
    );
    let store = pikamux::store::Store::at(fixture.profile().join("native-registry/pika.db"));
    let same = store
        .get_session(pikamux::model::Provider::Codex, &session.session_id)
        .unwrap()
        .unwrap();
    assert_eq!(same.root_pid, Some(pid));
    assert_eq!(store.list_sessions().unwrap().len(), 1);
    let reopened: Value =
        serde_json::from_slice(&fs::read(fixture.profile().join("native-binding.json")).unwrap())
            .unwrap();
    assert_eq!(reopened, binding);
    drop(second);
    drop(first);
    assert!(
        fixture
            .command(Path::new("/opt/homebrew/bin/tmux"))
            .args(["-L", &fixture.socket, "kill-server"])
            .status()
            .unwrap()
            .success()
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while unsafe { libc::kill(pid as i32, 0) } == 0 {
        assert!(
            Instant::now() < deadline,
            "Old TUI survived fixture terminal exit"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let mut third = Terminal::spawn(&mut launch);
    let deadline = Instant::now() + Duration::from_secs(15);
    let resumed = loop {
        third.collect();
        let resumed = store
            .get_session(pikamux::model::Provider::Codex, &session.session_id)
            .unwrap()
            .unwrap();
        if resumed.root_pid.is_some_and(|current| current != pid) {
            break resumed;
        }
        assert!(
            third.child.try_wait().unwrap().is_none(),
            "Same-thread relaunch failed: {}",
            String::from_utf8_lossy(&third.output)
        );
        assert!(
            Instant::now() < deadline,
            "No same-thread relaunch: {}",
            String::from_utf8_lossy(&third.output)
        );
        std::thread::sleep(Duration::from_millis(25));
    };
    assert_eq!(resumed.session_id, session.session_id);
    assert_eq!(
        provider.request("thread/loaded/list", json!({"limit":64}))["data"],
        json!([session.session_id])
    );
    assert_eq!(store.list_sessions().unwrap().len(), 1);
    if let (Some(seconds), Some(listener)) = (held_seconds, listener) {
        let native_generation = pikamux::process::process_record(resumed.root_pid.unwrap())
            .unwrap()
            .generation();
        let stop = fixture.root.path().join("stop");
        let stop_worker = stop.clone();
        let worker =
            std::thread::spawn(move || assistant_inference(listener, &stop_worker, seconds));
        let board = pikamux::store::Store::at(fixture.root.path().join("board.sqlite"));
        board.initialize().unwrap();
        let node = board.ensure_local_node_id().unwrap();
        let auto = std::env::var_os("PIKA_IOS_ASSISTANT_FIXTURE_AUTO").is_some();
        if auto {
            let mut mobile = fixture
                .command(pika)
                .arg("_mobile")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            let input = mobile.stdin.as_mut().unwrap();
            let identity = json!({"nodeId":node,"provider":"codex","threadId":session.session_id});
            for (method, params) in [
                ("assistant/open", json!({})),
                (
                    "conversation/send",
                    json!({"identity":identity,"clientMessageId":uuid::Uuid::new_v4().to_string(),"text":"Keep my synthetic preference"}),
                ),
            ] {
                writeln!(input,"{}",json!({"v":1,"id":uuid::Uuid::new_v4().to_string(),"method":method,"params":params})).unwrap();
            }
            drop(mobile.stdin.take());
            let output = mobile.wait_with_output().unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                String::from_utf8_lossy(&output.stdout).contains("accepted"),
                "{}",
                String::from_utf8_lossy(&output.stdout)
            );
        }
        eprintln!(
            "PIKA_IOS_ASSISTANT_FIXTURE {}",
            json!({"root":fixture.root.path(),"nodeId":node,"profileRoot":fixture.profile(),"profileId":profile_id,"threadId":session.session_id,"nativePid":resumed.root_pid,"socket":fixture.socket,"stopMarker":stop,"binary":pika,"reply":"Keep my synthetic preference","expectedResponse":"Original assistant received the exact mobile reply."})
        );
        let deadline = Instant::now() + Duration::from_secs(seconds);
        while !stop.exists() && Instant::now() < deadline {
            third.collect();
            if auto && fixture.root.path().join("assistant-delivered").exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        fs::write(&stop, b"stop").unwrap();
        if auto {
            assert!(fixture.root.path().join("assistant-delivered").exists());
        }
        let retained=provider.request("mcpServer/tool/call",json!({"threadId":session.session_id,"server":"pika","tool":"pika_memory_search","arguments":{"query":"synthetic preference"}}));
        assert!(
            retained
                .to_string()
                .contains("Keep my synthetic preference"),
            "{retained}"
        );
        eprintln!(
            "PIKA_IOS_ASSISTANT_MEMORY_RETAINED original real Pika MCP still recalls the preference after mobile interaction; not a model-invoked memory claim"
        );
        worker.join().unwrap();
        assert_eq!(
            provider.request("thread/loaded/list", json!({"limit":64}))["data"],
            json!([session.session_id])
        );
        assert_eq!(
            store
                .get_session(pikamux::model::Provider::Codex, &session.session_id)
                .unwrap()
                .unwrap()
                .root_pid,
            resumed.root_pid
        );
        assert_eq!(
            pikamux::process::process_record(resumed.root_pid.unwrap())
                .unwrap()
                .generation(),
            native_generation
        );
    }
}

fn assistant_inference(listener: std::net::TcpListener, stop: &Path, seconds: u64) {
    listener.set_nonblocking(true).unwrap();
    let deadline = Instant::now() + Duration::from_secs(seconds);
    while !stop.exists() && Instant::now() < deadline {
        let (mut stream, _) = match listener.accept() {
            Ok(connection) => connection,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20));
                continue;
            }
            Err(error) => panic!("{error}"),
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
                    .map(|value| value.trim().parse::<usize>().unwrap())
            })
            .unwrap();
        assert!(length < 8 * 1024 * 1024);
        let mut body = vec![0; length];
        stream.read_exact(&mut body).unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        if let Some(tools) = body["input"]
            .as_array()
            .and_then(|items| items.iter().find(|item| item["type"] == "additional_tools"))
            .and_then(|item| item.get("tools"))
        {
            fs::write(
                stop.parent().unwrap().join("native-tools.json"),
                serde_json::to_vec_pretty(tools).unwrap(),
            )
            .unwrap();
        }
        eprintln!(
            "Synthetic assistant request path: {}; tool choice: {}; input types: {:?}",
            header.lines().next().unwrap(),
            body["tool_choice"],
            body["input"].as_array().map(|items| items
                .iter()
                .map(|item| item["type"].clone())
                .collect::<Vec<_>>())
        );
        eprintln!(
            "Synthetic assistant inference keys: {:?}; tool names: {:?}",
            body.as_object().unwrap().keys().collect::<Vec<_>>(),
            body["tools"].as_array().map(|tools| tools
                .iter()
                .filter_map(|tool| tool["name"].as_str())
                .collect::<Vec<_>>())
        );
        let original = body["input"].as_array().unwrap().iter().any(|item| {
            item["role"] == "user"
                && item["content"].as_array().is_some_and(|content| {
                    content
                        .iter()
                        .any(|part| part["text"] == "Keep my synthetic preference")
                })
        });
        let delivered = stop.parent().unwrap().join("assistant-delivered");
        assert!(
            original || delivered.exists(),
            "First inference must contain the exact actual user input"
        );
        if original {
            eprintln!(
                "PIKA_IOS_ASSISTANT_DELIVERED exact actual mobile user input reached original private assistant inference"
            );
        } else {
            eprintln!(
                "Synthetic background maintenance inference answered separately; not mobile delivery evidence"
            );
        }
        let item = json!({"type":"message","id":"assistant-mobile-final","role":"assistant","status":"completed","content":[{"type":"output_text","text":"Original assistant received the exact mobile reply.","annotations":[]}]});
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            )
            .unwrap();
        for event in [
            json!({"type":"response.created","response":{"id":"assistant-synthetic","status":"in_progress","output":[]}}),
            json!({"type":"response.output_item.added","output_index":0,"item":item}),
            json!({"type":"response.output_item.done","output_index":0,"item":item}),
            json!({"type":"response.completed","response":{"id":"assistant-synthetic","status":"completed","output":[item],"usage":{"input_tokens":1,"output_tokens":1,"total_tokens":2}}}),
        ] {
            writeln!(
                stream,
                "event: {}\ndata: {}\n",
                event["type"].as_str().unwrap(),
                event
            )
            .unwrap();
        }
        if original {
            fs::write(delivered, b"delivered").unwrap();
        }
    }
}
