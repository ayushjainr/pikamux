//! Installed OpenCode only with disposable homes and synthetic loopback inference.
#![cfg(unix)]
use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

struct Fixture {
    server: Child,
    tmux: PathBuf,
    socket: PathBuf,
    stop: Arc<AtomicBool>,
    model: Option<std::thread::JoinHandle<()>>,
    logs: Arc<Mutex<Vec<String>>>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!(
                "Synthetic native diagnostics: {:?}",
                self.logs.lock().unwrap()
            );
            let screen = Command::new(&self.tmux)
                .args([
                    "-S",
                    self.socket.to_str().unwrap(),
                    "capture-pane",
                    "-p",
                    "-t",
                    "native-fixture:0.0",
                    "-S",
                    "-200",
                ])
                .output();
            eprintln!(
                "Synthetic native screen: {:?}",
                screen.map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
            );
        }
        let _ = Command::new(&self.tmux)
            .args(["-S", self.socket.to_str().unwrap(), "kill-server"])
            .output();
        let _ = self.server.kill();
        let _ = self.server.wait();
        self.stop.store(true, Ordering::Relaxed);
        if let Some(worker) = self.model.take() {
            let _ = worker.join();
        }
    }
}

fn http(
    address: SocketAddr,
    password: &str,
    method: &str,
    path: &str,
    body: Value,
) -> (u16, Value) {
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(3)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let data = if method == "GET" {
        String::new()
    } else {
        body.to_string()
    };
    let auth = basic(&format!("opencode:{password}"));
    write!(stream,"{method} {path} HTTP/1.1\r\nHost: {address}\r\nAuthorization: Basic {auth}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{data}",data.len()).unwrap();
    let mut bytes = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let mut chunk = [0; 8192];
        let read = stream.read(&mut chunk).unwrap_or_else(|error| {
            panic!(
                "HTTP {method} {path} failed {error}: {}",
                String::from_utf8_lossy(&bytes)
            )
        });
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..read]);
        assert!(bytes.len() < 16 * 1024 * 1024);
        assert!(Instant::now() < deadline);
        if let Some(split) = bytes.windows(4).position(|b| b == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&bytes[..split]);
            if head
                .lines()
                .next()
                .is_some_and(|line| line.contains(" 204 "))
            {
                break;
            }
            if let Some(length) = head.lines().find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .map(|s| s.trim().parse::<usize>().unwrap())
            }) {
                if bytes.len() >= split + 4 + length {
                    break;
                }
            }
            if head
                .to_ascii_lowercase()
                .contains("transfer-encoding: chunked")
                && bytes.ends_with(b"0\r\n\r\n")
            {
                break;
            }
        }
    }
    let split = bytes.windows(4).position(|b| b == b"\r\n\r\n").unwrap();
    let head = String::from_utf8_lossy(&bytes[..split]);
    let code = head
        .lines()
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    let body = &bytes[split + 4..];
    let body = if head
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        chunks(body)
    } else {
        body.to_vec()
    };
    let value = serde_json::from_slice(&body)
        .unwrap_or_else(|_| json!({"raw":String::from_utf8_lossy(&body)}));
    if path.ends_with("/message?limit=1") || path.contains("/message?limit=1&") {
        let cursor = head.lines().find_map(|line| {
            line.split_once(':')
                .filter(|(key, _)| key.eq_ignore_ascii_case("x-next-cursor"))
                .map(|(_, value)| value.trim())
        });
        return (code, json!({"data":value,"cursor":cursor}));
    }
    (code, value)
}
fn basic(value: &str) -> String {
    const ABC: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for b in value.as_bytes().chunks(3) {
        let word = (u32::from(b[0]) << 16)
            | (u32::from(*b.get(1).unwrap_or(&0)) << 8)
            | u32::from(*b.get(2).unwrap_or(&0));
        for shift in [18, 12] {
            out.push(ABC[((word >> shift) & 63) as usize] as char);
        }
        out.push(if b.len() > 1 {
            ABC[((word >> 6) & 63) as usize] as char
        } else {
            '='
        });
        out.push(if b.len() > 2 {
            ABC[(word & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}
fn chunks(mut bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let end = bytes.windows(2).position(|b| b == b"\r\n").unwrap();
        let size = usize::from_str_radix(
            std::str::from_utf8(&bytes[..end])
                .unwrap()
                .split(';')
                .next()
                .unwrap(),
            16,
        )
        .unwrap();
        if size == 0 {
            return out;
        }
        bytes = &bytes[end + 2..];
        out.extend_from_slice(&bytes[..size]);
        bytes = &bytes[size + 2..];
    }
}

fn environment(root: &Path, config: &Value, tmux: &Path) -> Vec<(String, String)> {
    let private_bin = root.join("bin");
    fs::create_dir(&private_bin).unwrap();
    std::os::unix::fs::symlink(fs::canonicalize(tmux).unwrap(), private_bin.join("tmux")).unwrap();
    let mut values = [
        ("HOME", "home"),
        ("XDG_CONFIG_HOME", "config"),
        ("XDG_DATA_HOME", "data"),
        ("XDG_STATE_HOME", "state"),
        ("XDG_CACHE_HOME", "cache"),
        ("TMPDIR", "tmp"),
        ("TMUX_TMPDIR", "tmp"),
        ("OPENCODE_CONFIG_DIR", "config/opencode"),
        ("OPENCODE_DATA_HOME", "data/opencode"),
    ]
    .into_iter()
    .map(|(key, folder)| {
        let path = root.join(folder);
        fs::create_dir_all(&path).unwrap();
        (key.into(), path.to_str().unwrap().into())
    })
    .collect::<Vec<_>>();
    values.extend(
        [
            ("TERM", "xterm-256color"),
            ("OPENCODE_SERVER_PASSWORD", "disposable-shared-proof"),
            ("OPENCODE_SERVER_USERNAME", "opencode"),
            ("OPENCODE_DISABLE_MODELS_FETCH", "1"),
            ("OPENCODE_DISABLE_AUTOUPDATE", "1"),
            ("OPENCODE_DISABLE_DEFAULT_PLUGINS", "1"),
            ("OPENCODE_DISABLE_PROJECT_CONFIG", "1"),
            ("OPENCODE_DISABLE_EXTERNAL_SKILLS", "1"),
            ("OPENCODE_DISABLE_LSP_DOWNLOAD", "1"),
        ]
        .map(|(k, v)| (k.into(), v.into())),
    );
    values.push((
        "PATH".into(),
        format!("{}:/usr/bin:/bin", private_bin.display()),
    ));
    values.push(("OPENCODE_CONFIG_CONTENT".into(), config.to_string()));
    values
}

fn fake_model(listener: TcpListener, stop: Arc<AtomicBool>, requests: Arc<Mutex<Vec<Value>>>) {
    listener.set_nonblocking(true).unwrap();
    while !stop.load(Ordering::Relaxed) {
        let Ok((mut stream, _)) = listener.accept() else {
            std::thread::sleep(Duration::from_millis(5));
            continue;
        };
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            let mut b = [0];
            if stream.read_exact(&mut b).is_err() {
                break;
            }
            head.push(b[0]);
            assert!(head.len() < 65536);
        }
        let head = String::from_utf8_lossy(&head);
        let length = head
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .map(|s| s.trim().parse::<usize>().unwrap())
            })
            .unwrap_or(0);
        let mut data = vec![0; length];
        stream.read_exact(&mut data).unwrap();
        let body: Value = serde_json::from_slice(&data).unwrap();
        let mobile = body.to_string().contains("Exact synthetic mobile reply");
        requests.lock().unwrap().push(body);
        let text = if mobile {
            "Fixture mobile reply on original owner."
        } else {
            "Fixture original response."
        };
        let chunks = [
            json!({"id":"fixture-completion","object":"chat.completion.chunk","created":1,"model":"fixture","choices":[{"index":0,"delta":{"role":"assistant","content":text},"finish_reason":null}]}),
            json!({"id":"fixture-completion","object":"chat.completion.chunk","created":1,"model":"fixture","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}),
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

fn wait_messages(address: SocketAddr, id: &str, text: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let (status, page) = http(
            address,
            "disposable-shared-proof",
            "GET",
            &format!("/session/{id}/message?limit=100"),
            Value::Null,
        );
        assert_eq!(status, 200, "{page}");
        if page.to_string().contains(text) {
            let data=page.as_array().unwrap().iter().map(|entry| json!({"id":entry["info"]["id"],"type":entry["info"]["role"],"text":entry["parts"].as_array().unwrap().iter().filter(|part|part["type"]=="text").filter_map(|part|part["text"].as_str()).collect::<String>()})).collect::<Vec<_>>();
            return json!({"data":data});
        }
        assert!(Instant::now() < deadline, "native reply missing: {page}");
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[test]
#[ignore = "Explicit installed native OpenCode and tmux; only disposable roots and fake loopback model"]
fn existing_native_terminal_and_second_client_share_exact_session() {
    let executable = PathBuf::from(
        std::env::var_os("PIKA_IOS_OPENCODE").expect("Explicit installed native binary"),
    );
    let tmux = PathBuf::from(std::env::var_os("PIKA_IOS_TMUX").expect("Explicit tmux binary"));
    let temp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(temp.path()).unwrap();
    let model_listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let model_url = format!("http://{}/v1", model_listener.local_addr().unwrap());
    let config = json!({"model":"fixture/fixture","small_model":"fixture/fixture","enabled_providers":["fixture"],"provider":{"fixture":{"name":"Synthetic loopback","npm":"@ai-sdk/openai-compatible","options":{"baseURL":model_url,"apiKey":"fixture-only"},"models":{"fixture":{"name":"Fixture","limit":{"context":131072,"output":4096}}}}},"permission":"deny","share":"disabled","plugin":[]});
    let env = environment(&root, &config, &tmux);
    let stop = Arc::new(AtomicBool::new(false));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let model = {
        let stop = stop.clone();
        let requests = requests.clone();
        std::thread::spawn(move || fake_model(model_listener, stop, requests))
    };
    let mut server = Command::new(&executable)
        .env_clear()
        .envs(env.clone())
        .current_dir(&root)
        .args(["--pure", "serve", "--hostname", "127.0.0.1", "--port", "0"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = server.stdout.take().unwrap();
    let stderr = server.stderr.take().unwrap();
    let logs = Arc::new(Mutex::new(Vec::new()));
    let logcopy = logs.clone();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines() {
            let Ok(line) = line else { break };
            let mut out = logcopy.lock().unwrap();
            if out.len() < 1000 {
                out.push(line);
            }
        }
    });
    let (sender, reader) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if sender.send(line).is_err() {
                break;
            }
        }
    });
    let socket = root.join("tmp/native-tmux.sock");
    let mut fixture = Fixture {
        server,
        tmux: tmux.clone(),
        socket: socket.clone(),
        stop,
        model: Some(model),
        logs,
    };
    let deadline = Instant::now() + Duration::from_secs(15);
    let address = loop {
        let line = reader
            .recv_timeout(Duration::from_millis(500))
            .unwrap_or_default();
        if let Some(url) = line.strip_prefix("opencode server listening on ") {
            break url
                .trim()
                .strip_prefix("http://")
                .unwrap()
                .parse::<SocketAddr>()
                .unwrap();
        }
        if fixture.server.try_wait().unwrap().is_some() {
            panic!("native server exited: {:?}", fixture.logs.lock().unwrap());
        }
        assert!(Instant::now() < deadline, "native server never ready");
    };
    let (unauthorized, _) = http(address, "wrong", "GET", "/api/session", Value::Null);
    assert_eq!(unauthorized, 401);
    let (status, created) = http(
        address,
        "disposable-shared-proof",
        "POST",
        "/session",
        json!({"title":"Original synthetic session"}),
    );
    assert_eq!(status, 200, "{created}");
    let id = created["id"].as_str().unwrap().to_owned();
    let mut argv = vec!["/usr/bin/env".to_owned(), "-i".into()];
    argv.extend(env.iter().map(|(k, v)| format!("{k}={v}")));
    argv.extend([
        executable.to_str().unwrap().into(),
        "--pure".into(),
        "attach".into(),
        format!("http://{address}"),
        "--session".into(),
        id.clone(),
        "--dir".into(),
        root.to_str().unwrap().into(),
    ]);
    let shell = shell_words::join(argv.iter());
    let out = Command::new(&tmux)
        .args([
            "-S",
            socket.to_str().unwrap(),
            "-f",
            "/dev/null",
            "new-session",
            "-d",
            "-s",
            "native-fixture",
            "-x",
            "120",
            "-y",
            "40",
            "-c",
            root.to_str().unwrap(),
            &shell,
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let owner = Command::new(&tmux)
        .args([
            "-S",
            socket.to_str().unwrap(),
            "display-message",
            "-p",
            "-t",
            "native-fixture:0.0",
            "#{pane_pid}",
        ])
        .output()
        .unwrap();
    let owner = String::from_utf8(owner.stdout).unwrap();
    let owner = owner.trim().parse::<i64>().unwrap();
    let native_generation = pikamux::process::process_generation(owner).unwrap();
    let server_generation =
        pikamux::process::process_generation(i64::from(fixture.server.id())).unwrap();
    let ready = Instant::now() + Duration::from_secs(10);
    loop {
        let screen = Command::new(&tmux)
            .args([
                "-S",
                socket.to_str().unwrap(),
                "capture-pane",
                "-p",
                "-t",
                "native-fixture:0.0",
            ])
            .output()
            .unwrap();
        if String::from_utf8_lossy(&screen.stdout).contains("ctrl") {
            break;
        };
        assert!(
            Instant::now() < ready,
            "native composer did not become ready"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        Command::new(&tmux)
            .args([
                "-S",
                socket.to_str().unwrap(),
                "send-keys",
                "-t",
                "native-fixture:0.0",
                "-l",
                "Original synthetic context to retain."
            ])
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new(&tmux)
            .args([
                "-S",
                socket.to_str().unwrap(),
                "send-keys",
                "-t",
                "native-fixture:0.0",
                "Enter"
            ])
            .status()
            .unwrap()
            .success()
    );
    wait_messages(address, &id, "Fixture original response.");
    let (status, second) = http(
        address,
        "disposable-shared-proof",
        "POST",
        "/session",
        json!({"title":"Other untouched synthetic session"}),
    );
    assert_eq!(status, 200);
    let second = second["id"].as_str().unwrap().to_owned();
    let (status, _) = http(
        address,
        "disposable-shared-proof",
        "POST",
        &format!("/session/{second}/prompt_async"),
        json!({"noReply":true,"parts":[{"type":"text","text":"Other session untouched marker"}]}),
    );
    assert_eq!(status, 204);
    wait_messages(address, &second, "Other session untouched marker");
    let (status, _) = http(
        address,
        "disposable-shared-proof",
        "POST",
        "/tui/select-session",
        json!({"sessionID":second}),
    );
    assert_eq!(status, 200);
    let switch_deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let screen = Command::new(&tmux)
            .args([
                "-S",
                socket.to_str().unwrap(),
                "capture-pane",
                "-p",
                "-t",
                "native-fixture:0.0",
            ])
            .output()
            .unwrap();
        if String::from_utf8_lossy(&screen.stdout).contains("Other session untouched marker") {
            break;
        };
        assert!(
            Instant::now() < switch_deadline,
            "native session switch not visible"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let (_, second_before) = http(
        address,
        "disposable-shared-proof",
        "GET",
        &format!("/session/{second}/message?limit=100"),
        Value::Null,
    );
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis()
        << 12;
    let operation = format!("msg_{:012x}FixturePhone01", timestamp & 0xffffffffffff);
    let text = "Exact synthetic mobile reply\nsecond line";
    let body = json!({"messageID":operation,"parts":[{"type":"text","text":text}]});
    let (status, admitted) = http(
        address,
        "disposable-shared-proof",
        "POST",
        &format!("/session/{id}/prompt_async"),
        body.clone(),
    );
    assert_eq!(status, 204, "{admitted}");
    let page = wait_messages(address, &id, "Fixture mobile reply on original owner.");
    assert_eq!(
        page["data"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|entry| entry["id"] == operation)
            .count(),
        1
    );
    let (_, second_after) = http(
        address,
        "disposable-shared-proof",
        "GET",
        &format!("/session/{second}/message?limit=100"),
        Value::Null,
    );
    assert_eq!(
        second_before, second_after,
        "phone changed another selected native session"
    );
    let (status, _) = http(
        address,
        "disposable-shared-proof",
        "POST",
        "/tui/select-session",
        json!({"sessionID":id}),
    );
    assert_eq!(status, 200);
    let captured = loop {
        let out = Command::new(&tmux)
            .args([
                "-S",
                socket.to_str().unwrap(),
                "capture-pane",
                "-p",
                "-t",
                "native-fixture:0.0",
                "-S",
                "-200",
            ])
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        if text.contains("Fixture mobile reply on original owner.") {
            break text;
        }
        assert!(
            Instant::now() < deadline + Duration::from_secs(20),
            "native TUI did not show original response: {text}"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(captured.contains("Exact synthetic mobile reply"));
    assert_eq!(
        pikamux::process::process_generation(owner),
        Some(native_generation)
    );
    assert_eq!(
        pikamux::process::process_generation(i64::from(fixture.server.id())),
        Some(server_generation)
    );
    let observed = requests.lock().unwrap();
    assert!(observed.iter().any(|body| {
        let raw = body.to_string();
        raw.contains("Exact synthetic mobile reply")
            && raw.contains("Original synthetic context to retain.")
            && raw.contains("Fixture original response.")
    }));
    let (code, sessions) = http(
        address,
        "disposable-shared-proof",
        "GET",
        "/session",
        Value::Null,
    );
    assert_eq!(code, 200);
    assert_eq!(sessions.as_array().unwrap().len(), 2);
    let (code, newest) = http(
        address,
        "disposable-shared-proof",
        "GET",
        &format!("/session/{id}/message?limit=1"),
        Value::Null,
    );
    assert_eq!(code, 200);
    let boundary = newest["cursor"]
        .as_str()
        .expect("native next cursor header missing")
        .to_owned();
    let newest = newest["data"].as_array().unwrap();
    assert_eq!(newest.len(), 1, "native ignored bounded page limit");
    let (code, older) = http(
        address,
        "disposable-shared-proof",
        "GET",
        &format!("/session/{id}/message?limit=1&before={boundary}"),
        Value::Null,
    );
    assert_eq!(code, 200, "native before cursor error: {older}");
    let older = older["data"].as_array().unwrap();
    assert_eq!(older.len(), 1, "native older page unavailable");
    assert_ne!(
        older[0]["info"]["id"], newest[0]["info"]["id"],
        "native ignored before cursor"
    );
    println!(
        "PIKA_OPENCODE_SHARED_PROOF terminal-typed context retained, original server/native generation preserved through native view switch; exact phone input reached original session, second session untouched, original reopened visibly {id}"
    );
    drop(observed);
    // A separate opt-in diagnostic retains the measured unsupported TUI-port
    // failure without conflating it with the verified serve+attach route.
    if std::env::var_os("PIKA_IOS_OPENCODE_TUI_PORT_PROBE").is_none() {
        return;
    }
    assert!(
        Command::new(&tmux)
            .args(["-S", socket.to_str().unwrap(), "kill-server"])
            .status()
            .unwrap()
            .success()
    );
    fixture.server.kill().unwrap();
    fixture.server.wait().unwrap();
    let reserved = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = reserved.local_addr().unwrap();
    drop(reserved);
    // Explicit future-launch probe only: a new native TUI resumes this disposable
    // saved identity and owns its own authenticated server. No live user is migrated.
    let mut argv = vec!["/usr/bin/env".to_owned(), "-i".into()];
    argv.extend(env.iter().map(|(k, v)| format!("{k}={v}")));
    argv.extend([
        executable.to_str().unwrap().into(),
        "--pure".into(),
        "--hostname".into(),
        "127.0.0.1".into(),
        "--port".into(),
        address.port().to_string(),
        "--session".into(),
        id.clone(),
        root.to_str().unwrap().into(),
    ]);
    let shell = shell_words::join(argv.iter());
    let out = Command::new(&tmux)
        .args([
            "-S",
            socket.to_str().unwrap(),
            "-f",
            "/dev/null",
            "new-session",
            "-d",
            "-s",
            "native-fixture",
            "-x",
            "120",
            "-y",
            "40",
            "-c",
            root.to_str().unwrap(),
            &shell,
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while TcpStream::connect_timeout(&address, Duration::from_millis(100)).is_err() {
        assert!(
            Instant::now() < deadline,
            "native TUI did not expose authenticated server"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let owner = Command::new(&tmux)
        .args([
            "-S",
            socket.to_str().unwrap(),
            "display-message",
            "-p",
            "-t",
            "native-fixture:0.0",
            "#{pane_pid}",
        ])
        .output()
        .unwrap();
    let owner = String::from_utf8(owner.stdout).unwrap();
    let owner = owner.trim().parse::<i64>().unwrap();
    let generation = pikamux::process::process_generation(owner).unwrap();
    let (status, existing) = http(
        address,
        "disposable-shared-proof",
        "GET",
        &format!("/session/{id}"),
        Value::Null,
    );
    assert_eq!(status, 200);
    assert_eq!(existing["id"], id);
    let (status, response) = http(
        address,
        "disposable-shared-proof",
        "POST",
        &format!("/session/{id}/prompt_async"),
        json!({"messageID":"msg_native_tui_owner","parts":[{"type":"text","text":"Exact synthetic mobile reply from new original TUI owner\nsecond line"}]}),
    );
    assert_eq!(status, 204, "{response}");
    let page = wait_messages(address, &id, "msg_native_tui_owner");
    assert_eq!(
        page["data"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|entry| entry["id"] == "msg_native_tui_owner")
            .count(),
        1
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let out = Command::new(&tmux)
            .args([
                "-S",
                socket.to_str().unwrap(),
                "capture-pane",
                "-p",
                "-t",
                "native-fixture:0.0",
                "-S",
                "-200",
            ])
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        if text.contains("Exact synthetic mobile reply from new original TUI owner") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "new original TUI did not receive exact phone text: {text}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        pikamux::process::process_generation(owner),
        Some(generation)
    );
    println!(
        "PIKA_OPENCODE_TUI_SERVER_PROOF native TUI owns authenticated listener and exact resumed original identity {id}; no supervisor needed"
    );
}
