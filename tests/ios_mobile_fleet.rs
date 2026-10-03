//! Two real Pika processes with a disposable SSH-command substitute. This proves
//! fleet routing/identity, not network authentication or provider delivery.
#![cfg(unix)]
use pikamux::{model::FleetNode, store::Store};
use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::mpsc::{self, Receiver},
    time::Duration,
};

struct Peer {
    child: Child,
    responses: Receiver<Value>,
}
impl Peer {
    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = uuid::Uuid::new_v4().to_string();
        writeln!(
            self.child.stdin.as_mut().unwrap(),
            "{}",
            json!({"v":1,"id":id,"method":method,"params":params})
        )
        .unwrap();
        loop {
            let frame = self
                .responses
                .recv_timeout(Duration::from_secs(20))
                .expect("native endpoint response timeout");
            if frame["id"] == id {
                return frame;
            }
        }
    }
}
impl Drop for Peer {
    fn drop(&mut self) {
        self.child.stdin.take();
        for _ in 0..100 {
            if self.child.try_wait().unwrap().is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}
fn without_refresh_clock(mut rows: Vec<pikamux::model::Session>) -> Vec<pikamux::model::Session> {
    // Name resolution legitimately refreshes observation time. Identity,
    // unread state, ownership and all other fields must remain unchanged.
    for row in &mut rows {
        row.updated_at = 0.0;
    }
    rows
}
fn isolated(root: &Path) -> Vec<(String, PathBuf)> {
    [
        ("HOME", "home"),
        ("XDG_CONFIG_HOME", "config"),
        ("XDG_STATE_HOME", "state"),
        ("XDG_DATA_HOME", "data"),
        ("XDG_CACHE_HOME", "cache"),
        ("PIKA_CONFIG_HOME", "config/pika"),
        ("PIKA_STATE_HOME", "state"),
        ("PIKA_DB_PATH", "board.sqlite"),
        ("CODEX_HOME", "codex"),
        ("CLAUDE_CONFIG_DIR", "claude"),
        ("OPENCODE_DATA_HOME", "opencode"),
        ("TMPDIR", "tmp"),
        ("TMUX_TMPDIR", "tmux"),
    ]
    .into_iter()
    .map(|(name, path)| (name.to_owned(), root.join(path)))
    .collect()
}
fn setup(root: &Path) -> Store {
    for (_, path) in isolated(root) {
        if path.extension().is_none() {
            fs::create_dir_all(path).unwrap();
        }
    }
    let store = Store::at(root.join("board.sqlite"));
    store.initialize().unwrap();
    store.ensure_local_node_id().unwrap();
    store
}
fn spawn(root: &Path, bin: &Path) -> Peer {
    let mut command = Command::new(env!("CARGO_BIN_EXE_pika"));
    command
        .env_clear()
        .envs(isolated(root))
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .env("PIKA_UPDATE_CHECK", "0")
        .env("PIKA_TMUX_SOCKET", "disposable-ios-fleet")
        .current_dir(root)
        .arg("_mobile")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = command.spawn().unwrap();
    let stdout = child.stdout.take().unwrap();
    let (send, responses) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else {
                break;
            };
            let Ok(frame) = serde_json::from_str(&line) else {
                break;
            };
            if send.send(frame).is_err() {
                break;
            }
        }
    });
    Peer { child, responses }
}

#[test]
fn configured_fleet_routes_to_real_native_endpoint_and_revocation_stops_access() {
    let temp = tempfile::tempdir().unwrap();
    let local = temp.path().join("local");
    let remote = temp.path().join("remote");
    let local_store = setup(&local);
    let remote_store = setup(&remote);
    let remote_id = remote_store.local_node_id().unwrap().unwrap();
    let bin = temp.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let log = temp.path().join("ssh-invocations");
    let ssh = bin.join("ssh");
    let env = isolated(&remote)
        .into_iter()
        .map(|(name, value)| format!("{}={}", name, quote(&value)))
        .collect::<Vec<_>>()
        .join(" ");
    fs::write(&ssh, format!("#!/bin/sh\nprintf '%s\\n' invoked >> {}\nexec /usr/bin/env -i {} PATH=/usr/bin:/bin PIKA_UPDATE_CHECK=0 {} _mobile\n",quote(&log),env,quote(Path::new(env!("CARGO_BIN_EXE_pika"))))).unwrap();
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();
    let node = FleetNode {
        node_id: remote_id.clone(),
        alias: "same-name".into(),
        ssh_target: "fixture-only".into(),
        sources: vec!["explicit".into()],
        status: "ready".into(),
        protocol_version: Some(2),
        package_version: None,
        capabilities: vec![],
        last_seen: 0.0,
        last_attempt_at: 0.0,
        last_error: None,
        created_at: 1.0,
        updated_at: 1.0,
    };
    local_store.upsert_fleet_node(&node).unwrap();
    let mut peer = spawn(&local, &bin);
    let hello = peer.request("hello", json!({}));
    assert_eq!(
        hello["result"]["nodeId"],
        local_store.local_node_id().unwrap().unwrap()
    );
    let projects = peer.request("projects/list", json!({"nodeId":remote_id}));
    assert!(projects.get("error").is_none(), "{projects}");
    assert!(projects["result"]["items"].is_array(), "{projects}");
    assert_eq!(
        fs::read_to_string(&log).unwrap().lines().count(),
        1,
        "exactly one selected remote connection"
    );
    // Seed only a disposable board row so this is a permitted remote project.
    // A failed creation before provider dispatch has no identity, not an
    // uncertain new conversation. Exercise both actual endpoint processes.
    rusqlite::Connection::open(remote_store.path()).unwrap().execute(
        "INSERT INTO sessions(provider,session_id,name,cwd,created_at,updated_at,last_event_at,last_activity_at) VALUES('codex',?1,'Existing fixture',?2,1,1,1,1)",
        rusqlite::params![uuid::Uuid::new_v4().to_string(), remote.to_string_lossy().as_ref()],
    ).unwrap();
    let seeded_rows = without_refresh_clock(remote_store.list_sessions().unwrap());
    let operation = uuid::Uuid::new_v4().to_string();
    let creation = peer.request("conversation/create", json!({"nodeId":remote_id,"provider":"codex","name":"Existing fixture","projectId":remote,"clientOperationId":operation}));
    assert_eq!(
        creation["result"]["state"], "rejected",
        "definite pre-dispatch rejection must not become unknown: {creation}"
    );
    assert_eq!(creation["result"]["clientOperationId"], operation);
    assert!(creation["result"]["identity"].is_null());
    assert_eq!(
        without_refresh_clock(remote_store.list_sessions().unwrap()),
        seeded_rows
    );
    let refused = peer.request("conversation/send", json!({"identity":{"nodeId":remote_id,"provider":"codex","threadId":uuid::Uuid::new_v4().to_string()},"clientMessageId":uuid::Uuid::new_v4().to_string(),"text":"No selected conversation"}));
    assert_eq!(
        refused["error"]["code"], "rejected_before_dispatch",
        "a verified remote refusal is not an unknown send: {refused}"
    );
    local_store.delete_fleet_node(&node.node_id).unwrap();
    let revoked = peer.request("projects/list", json!({"nodeId":remote_id}));
    assert!(
        revoked.get("error").is_some(),
        "removed route must not remain usable: {revoked}"
    );
    assert_eq!(
        fs::read_to_string(&log).unwrap().lines().count(),
        1,
        "no reconnect to revoked route"
    );
    // Point the trusted UUID at a different native Pika identity. The SSH
    // substitute stays reachable, so refusal must come from Pika identity.
    let mut wrong = node.clone();
    wrong.node_id = uuid::Uuid::new_v4().to_string();
    local_store.upsert_fleet_node(&wrong).unwrap();
    let mismatch = peer.request("projects/list", json!({"nodeId":wrong.node_id}));
    assert!(
        mismatch["error"]["message"]
            .as_str()
            .unwrap()
            .contains("identity changed"),
        "{mismatch}"
    );
    assert!(local_store.list_sessions().unwrap().is_empty());
    assert_eq!(
        without_refresh_clock(remote_store.list_sessions().unwrap()),
        seeded_rows,
        "routing retained only the seeded row; it did not launch or adopt a provider"
    );

    drop(peer);
    local_store.delete_fleet_node(&wrong.node_id).unwrap();
    local_store.upsert_fleet_node(&node).unwrap();
    // Let the real owning endpoint receive a mutation, but drop its response.
    // Even when the peer happened to reject it, the relay cannot infer that
    // unseen fact and offer a new operation as if non-delivery were proven.
    fs::write(&ssh, format!("#!/bin/sh\n/usr/bin/env -i {} PATH=/usr/bin:/bin PIKA_UPDATE_CHECK=0 {} _mobile | /usr/bin/awk 'NR == 1 {{ print; fflush(); }}'\n",env,quote(Path::new(env!("CARGO_BIN_EXE_pika"))))).unwrap();
    let mut lossy = spawn(&local, &bin);
    let message_id = uuid::Uuid::new_v4().to_string();
    let lost = lossy.request("conversation/send",json!({"identity":{"nodeId":remote_id,"provider":"codex","threadId":uuid::Uuid::new_v4().to_string()},"clientMessageId":message_id,"text":"disposable delivery probe"}));
    assert_eq!(
        lost["result"]["state"], "unknown",
        "a lost response is not proof of rejection: {lost}"
    );
    assert_eq!(lost["result"]["clientMessageId"], message_id);
}
