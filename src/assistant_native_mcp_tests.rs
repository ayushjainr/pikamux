//! Explicit native, protocol-only integration proof. Everything is disposable;
//! inference points at closed loopback port 9 and no model turn is submitted.
use super::*;
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader},
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc::{Receiver, channel},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

struct Rpc {
    child: Child,
    input: ChildStdin,
    output: Receiver<Value>,
    next: u64,
}

impl Rpc {
    fn start(command: &mut Command) -> Self {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (sender, output) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                let Ok(value) = serde_json::from_str(&line) else {
                    continue;
                };
                if sender.send(value).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            input,
            output,
            next: 0,
        }
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let reply = self.request_frame(method, params);
        assert!(reply.get("error").is_none(), "{method}: {reply}");
        reply["result"].clone()
    }

    fn request_frame(&mut self, method: &str, params: Value) -> Value {
        self.next += 1;
        let id = self.next;
        writeln!(
            self.input,
            "{}",
            json!({"id":id,"method":method,"params":params})
        )
        .unwrap();
        self.input.flush().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        loop {
            let reply = self
                .output
                .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
                .expect("native RPC timed out");
            if reply["id"] == id {
                return reply;
            }
        }
    }
}

impl Drop for Rpc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
#[ignore = "set PIKA_NATIVE_CODEX and PIKA_NATIVE_PIKA for a no-model native MCP proof"]
fn native_codex_connects_pika_and_reads_only_shared_board_rows() {
    let codex = PathBuf::from(std::env::var_os("PIKA_NATIVE_CODEX").unwrap())
        .canonicalize()
        .unwrap();
    let pika = PathBuf::from(std::env::var_os("PIKA_NATIVE_PIKA").unwrap())
        .canonicalize()
        .unwrap();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("private");
    let mut control = crate::assistant_control::Controller::open(&root).unwrap();
    let profile = crate::assistant_memory::Store::open(root.join("memory.sqlite"))
        .unwrap()
        .profile_id()
        .to_owned();
    let mut binding = bind_scope(&root, &profile, "personal").unwrap();
    let remembered = seed_memory(&root);
    crate::assistant_storage::directory(&root.join("native-registry")).unwrap();
    let registry = crate::store::Store::at(root.join("native-registry/pika.db"));
    registry.initialize().unwrap();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let shared = crate::assistant_observation::Row {
        identity: crate::assistant_observation::Identity {
            node: "fixture-node".into(),
            provider: "codex".into(),
            conversation: uuid::Uuid::new_v4().to_string(),
        },
        name: "Fixture research".into(),
        status: "working".into(),
        stale: false,
        event_id: None,
        occurrence: None,
    };
    let feed = temp.path().join("activity-feed");
    crate::assistant_observation::publish_rows(&feed, vec![shared.clone()], false, now).unwrap();
    let preview = control.share_board("personal", None).unwrap();
    control
        .share_board("personal", preview["confirmation"].as_str())
        .unwrap();
    let hidden = crate::assistant_observation::Row {
        name: "Unshared private task".into(),
        identity: crate::assistant_observation::Identity {
            conversation: uuid::Uuid::new_v4().to_string(),
            ..shared.identity.clone()
        },
        ..shared.clone()
    };
    crate::assistant_observation::publish_rows(&feed, vec![shared, hidden], false, now).unwrap();
    drop(control);

    // Selected authority is outside the ordinary installation's state root.
    // A parent-directory decoy must never become its shared board feed.
    let state = temp.path().join("ordinary-state");
    let config_home = temp.path().join("ordinary-config");
    crate::assistant_storage::directory(&config_home).unwrap();
    crate::assistant_startup::save(
        &state.join("assistant-startup/selection.json"),
        &crate::assistant_startup::Selection {
            profile_root: root.clone(),
            profile_id: profile.clone(),
            scope: "personal".into(),
            executable: codex.clone(),
            max_calls: crate::assistant_policy::NO_CALL_LIMIT,
        },
    )
    .unwrap();
    let shared_rows = crate::assistant_observation::load(&feed, now)
        .unwrap()
        .unwrap();
    crate::assistant_observation::publish_rows(
        &state.join("activity-feed"),
        shared_rows.rows,
        false,
        now,
    )
    .unwrap();
    crate::assistant_observation::publish_rows(&feed, vec![], false, now).unwrap();

    let launch =
        crate::assistant_native_profile::prepare(&root, &profile, "personal", &pika, &codex)
            .unwrap();
    // app-server has no --profile support. Replay the generated private
    // profile's exact MCP and permissions layers, without inference.
    let config: toml_edit::DocumentMut =
        std::fs::read_to_string(root.join("provider-home/pika-assistant.config.toml"))
            .unwrap()
            .parse()
            .unwrap();
    let server = config["mcp_servers"]["pika"].as_inline_table().unwrap();
    let mut broken = server.clone();
    broken.remove("env_vars");
    let mut command = native_command(
        &codex,
        &launch,
        &binding,
        &state,
        &config_home,
        &root,
        &broken,
    );
    let mut disconnected = initialized_rpc(&mut command);
    let rejected = disconnected.request_frame("thread/start", json!({"model":"gpt-6-luna","modelProvider":"pika-no-inference","cwd":launch.cwd,"ephemeral":true}));
    assert!(
        rejected["error"]["message"]
            .as_str()
            .is_some_and(
                |message| message.contains("required MCP servers failed to initialize")
                    && message.contains("pika")
            ),
        "missing-generation startup must fail visibly, not open a blind assistant: {rejected}"
    );
    drop(disconnected);
    let mut command = native_command(
        &codex,
        &launch,
        &binding,
        &state,
        &config_home,
        &root,
        server,
    );
    let mut rpc = initialized_rpc(&mut command);
    let thread = rpc.request("thread/start", json!({"model":"gpt-6-luna","modelProvider":"pika-no-inference","cwd":launch.cwd,"ephemeral":false}));
    let id = thread["thread"]["id"].as_str().unwrap();
    binding.thread_id = Some(id.to_owned());
    save_binding(&root, &binding).unwrap();
    crate::assistant_native_recovery::record_context(&root, &profile, "personal", id).unwrap();
    let inventory = rpc.request(
        "mcpServerStatus/list",
        json!({"threadId":id,"detail":"toolsAndAuthOnly"}),
    );
    assert!(inventory.to_string().contains("pika_state"), "{inventory}");
    let response = rpc.request(
        "mcpServer/tool/call",
        json!({"threadId":id,"server":"pika","tool":"pika_state","arguments":{}}),
    );
    assert_ne!(response["isError"], true, "{response}");
    let text = response.to_string();
    assert!(text.contains("Fixture research"), "{response}");
    assert!(!text.contains("Unshared private task"), "{response}");
    let recalled = rpc.request("mcpServer/tool/call", json!({"threadId":id,"server":"pika","tool":"pika_memory_search","arguments":{"query":"ZephyrMemory"}}));
    assert_ne!(recalled["isError"], true, "{recalled}");
    assert!(recalled.to_string().contains(&remembered), "{recalled}");
    assert!(
        !recalled.to_string().contains("Other-scope secret"),
        "{recalled}"
    );
    let version = crate::assistant_memory::Store::open(root.join("memory.sqlite"))
        .unwrap()
        .source_version(&remembered)
        .unwrap()
        .unwrap();
    let learning = rpc.request("mcpServer/tool/call", json!({"threadId":id,"server":"pika","tool":"pika_save_learning","arguments":{
        "request_id":"synthetic-native-learning", "body":"Source-backed interpretation",
        "candidates":[{"kind":"fact","body":"ZephyrLearned one shared feed avoids duplicated work","sources":[{"id":remembered,"revision":version}]}]
    }}));
    assert_ne!(learning["isError"], true, "{learning}");
    let receipt: Value =
        serde_json::from_str(learning["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(receipt["origin"], "Worker");
    assert!(!receipt["saved"].as_array().unwrap().is_empty());
    let mut persisted = crate::assistant_memory::Store::open(root.join("memory.sqlite")).unwrap();
    let learned = persisted
        .search_bm25(
            &crate::assistant::scope("personal").unwrap(),
            "ZephyrLearned",
            20,
        )
        .unwrap();
    assert_eq!(learned.len(), 1);
    assert_eq!(learned[0].origin, crate::assistant_memory::Origin::Worker);
    assert!(learned[0].dependencies.contains(&remembered));
    let wrong_scope = rpc.request_frame(
        "mcpServer/tool/call",
        json!({"threadId":id,"server":"pika","tool":"pika_state","arguments":{"scope":"other"}}),
    );
    assert!(
        wrong_scope["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("unknown field `scope`")),
        "{wrong_scope}"
    );
    persisted.forget(&remembered).unwrap();
    assert!(
        persisted
            .search_bm25(
                &crate::assistant::scope("personal").unwrap(),
                "ZephyrLearned",
                20
            )
            .unwrap()
            .is_empty()
    );
    let invalidated = rpc.request("mcpServer/tool/call", json!({"threadId":id,"server":"pika","tool":"pika_memory_search","arguments":{"query":"ZephyrMemory"}}));
    assert_eq!(
        invalidated["isError"], true,
        "forgotten context must not remain readable: {invalidated}"
    );
    drop(rpc);
    let mut reopened = initialized_rpc(&mut command);
    // Codex does not retain a rollout before its first model turn. This
    // protocol-only fixture must not invent a replacement when resume fails.
    // Actual same-UUID TUI reopen is separately measured in the alpha smoke.
    let resumed = reopened.request_frame("thread/resume", json!({"threadId":id,"model":"gpt-6-luna","modelProvider":"pika-no-inference","cwd":launch.cwd}));
    assert!(
        resumed["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("no rollout found")),
        "{resumed}"
    );
    assert_eq!(
        bind_scope(&root, &profile, "personal")
            .unwrap()
            .thread_id
            .as_deref(),
        Some(id)
    );
    assert_eq!(registry.list_sessions().unwrap().len(), 0);
    assert_eq!(registry.list_pending().unwrap().len(), 0);
}

fn seed_memory(root: &Path) -> String {
    use crate::assistant_memory::{NewRecord, Origin, RecordKind, Store};
    let mut memory = Store::open(root.join("memory.sqlite")).unwrap();
    let mut record = NewRecord {
        kind: RecordKind::Finding,
        origin: Origin::Human,
        scope: crate::assistant::scope("personal").unwrap(),
        body: "ZephyrMemory retained rationale: reuse one feed instead of rescanning every view"
            .into(),
        provenance: "synthetic native acceptance source".into(),
        timestamp: 1,
        supersedes: None,
        dependencies: vec![],
        decision_state: None,
        protected_policy: false,
    };
    let remembered = memory.append(record.clone()).unwrap().id;
    record.scope = crate::assistant::scope("other").unwrap();
    record.body = "ZephyrMemory Other-scope secret".into();
    memory.append(record.clone()).unwrap();
    record.scope = crate::assistant::scope("personal").unwrap();
    for timestamp in 2..42 {
        record.timestamp = timestamp;
        record.body = format!("Unrelated recent noise {timestamp}");
        memory.append(record.clone()).unwrap();
    }
    remembered
}

fn native_command(
    codex: &Path,
    launch: &crate::assistant_native_profile::LaunchProfile,
    binding: &Binding,
    state: &Path,
    config_home: &Path,
    root: &Path,
    server: &toml_edit::InlineTable,
) -> Command {
    let mut command = Command::new(codex);
    command
        .args(["-c", "model_provider=\"pika-no-inference\"", "-c", "model_providers.pika-no-inference={name=\"No inference\",base_url=\"http://127.0.0.1:9\",wire_api=\"responses\",requires_openai_auth=false}"])
        .arg("-c").arg(format!("mcp_servers.pika={server}"))
        .current_dir(&launch.cwd)
        .envs(&launch.environment)
        .env("PIKA_ASSISTANT_NATIVE_LAUNCH_TOKEN", &binding.launch_token)
        .env("PIKA_ASSISTANT_BOARD_DB_PATH", state.join("board.sqlite"))
        .env("PIKA_STATE_HOME", state)
        .env("PIKA_CONFIG_HOME", config_home)
        .env("PIKA_DB_PATH", root.join("native-registry/pika.db"));
    for pair in launch.argv.windows(2).filter(|pair| pair[0] == "--config") {
        command.args(pair);
    }
    command.arg("app-server");
    command
}

fn initialized_rpc(command: &mut Command) -> Rpc {
    let mut rpc = Rpc::start(command);
    rpc.request("initialize", json!({"clientInfo":{"name":"pika-native-mcp-test","version":"1"},"capabilities":{"experimentalApi":true}}));
    writeln!(rpc.input, "{}", json!({"method":"initialized","params":{}})).unwrap();
    rpc.input.flush().unwrap();
    rpc
}
