//! Explicitly approved live acceptance. Never runs in the default test suite.
//! All conversations/state are synthetic and disposable; authentication alone
//! is copied from the explicitly selected existing sign-in, never printed.
use super::*;
#[path = "assistant_native_live_dream.rs"]
mod live_dream;

#[test]
#[ignore = "explicit native provider protocol probe; no authentication or inference"]
fn native_maintenance_preflight_without_inference() {
    use crate::assistant_provider::RpcTransport;
    let codex = PathBuf::from(std::env::var_os("PIKA_NATIVE_CODEX").unwrap())
        .canonicalize()
        .unwrap();
    let pika = PathBuf::from(std::env::var_os("PIKA_NATIVE_PIKA").unwrap())
        .canonicalize()
        .unwrap();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap().join("profile");
    let _control = crate::assistant_control::Controller::open(&root).unwrap();
    let profile = crate::assistant_memory::Store::open(root.join("memory.sqlite"))
        .unwrap()
        .profile_id()
        .to_owned();
    crate::assistant_native_profile::prepare(&root, &profile, "personal", &pika, &codex).unwrap();
    std::fs::copy(
        root.join("provider-home/pika-assistant.config.toml"),
        root.join("provider-home/config.toml"),
    )
    .unwrap();
    let config_path = root.join("provider-home/config.toml");
    let mut config: toml_edit::DocumentMut = std::fs::read_to_string(&config_path)
        .unwrap()
        .parse()
        .unwrap();
    let marker = root.join("mcp-must-not-start");
    config["mcp_servers"]["pika"]["command"] = toml_edit::value("/bin/sh");
    config["mcp_servers"]["pika"]["args"] = toml_edit::value(toml_edit::Array::from_iter([
        "-c",
        "touch \"$1\"; exit 19",
        "pika-marker",
        marker.to_str().unwrap(),
    ]));
    std::fs::write(&config_path, config.to_string()).unwrap();
    let scratch = root.join("scratch");
    crate::assistant_storage::directory(&scratch).unwrap();
    let mut transport = crate::assistant_transport::CodexTransport::spawn(
        crate::assistant_transport::TransportConfig {
            executable: codex,
            codex_home: root.join("provider-home"),
            scratch,
        },
    )
    .unwrap();
    transport
        .request(
            "initialize",
            json!({"clientInfo":{"name":"pika-maintenance-probe","version":"1"},"capabilities":{"experimentalApi":true}}),
        )
        .unwrap();
    transport.notify("initialized", json!({})).unwrap();
    let effective = transport
        .request("config/read", json!({"includeLayers":false}))
        .unwrap();
    assert_eq!(effective["config"]["mcp_servers"]["pika"]["enabled"], false);
    transport
        .request(
            "thread/start",
            json!({"model":"gpt-6-luna","approvalPolicy":"never","permissions":"pika-assistant"}),
        )
        .unwrap();
    drop(transport);
    assert!(
        !marker.exists(),
        "disabled MCP was started by the native provider"
    );
}

#[test]
#[ignore = "requires explicit PIKA_LIVE_ACCEPTANCE=approved and selected auth/provider/binary"]
fn live_native_conversation_creates_source_backed_memory() {
    assert_eq!(
        std::env::var("PIKA_LIVE_ACCEPTANCE").as_deref(),
        Ok("approved")
    );
    let codex = PathBuf::from(std::env::var_os("PIKA_NATIVE_CODEX").unwrap())
        .canonicalize()
        .unwrap();
    let pika = PathBuf::from(std::env::var_os("PIKA_NATIVE_PIKA").unwrap())
        .canonicalize()
        .unwrap();
    let auth = PathBuf::from(std::env::var_os("PIKA_LIVE_AUTH_FILE").unwrap());
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap().join("profile");
    let _control = crate::assistant_control::Controller::open(&root).unwrap();
    // Native turns can contain multiple model requests. Bound this explicitly
    // approved trial by a fixed journey and per-turn wall time, not a false call cap.
    let mut policy =
        crate::assistant_policy::AssistantPolicy::open(root.join("policy.sqlite")).unwrap();
    let mut limits = policy.config().unwrap();
    limits.max_total_calls = crate::assistant_policy::NO_CALL_LIMIT;
    policy.configure(&limits).unwrap();
    let profile = crate::assistant_memory::Store::open(root.join("memory.sqlite"))
        .unwrap()
        .profile_id()
        .to_owned();
    let mut binding = bind_scope(&root, &profile, "personal").unwrap();
    let registry = crate::store::Store::at(root.join("native-registry/pika.db"));
    registry.initialize().unwrap();
    let launch =
        crate::assistant_native_profile::prepare(&root, &profile, "personal", &pika, &codex)
            .unwrap();
    let destination = root.join("provider-home/auth.json");
    crate::assistant_storage::file(&destination).unwrap();
    std::fs::copy(auth, &destination).unwrap();
    let mut scoped: toml_edit::DocumentMut =
        std::fs::read_to_string(root.join("provider-home/pika-assistant.config.toml"))
            .unwrap()
            .parse()
            .unwrap();
    scoped["cli_auth_credentials_store"] = toml_edit::value("file");
    scoped["projects"][launch.cwd.to_str().unwrap()]["trust_level"] = toml_edit::value("trusted");
    let config_path = root.join("provider-home/config.toml");
    crate::assistant_storage::file(&config_path).unwrap();
    std::fs::write(&config_path, scoped.to_string()).unwrap();
    let mut command = Command::new(&codex);
    command
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("LANG", "en_US.UTF-8");
    for (variable, directory) in [
        ("HOME", "process-home"),
        ("XDG_CONFIG_HOME", "process-config"),
        ("XDG_CACHE_HOME", "process-cache"),
        ("XDG_DATA_HOME", "process-data"),
        ("XDG_STATE_HOME", "process-state"),
        ("TMPDIR", "process-tmp"),
    ] {
        let directory = root.join(directory);
        crate::assistant_storage::directory(&directory).unwrap();
        command.env(variable, directory);
    }
    command
        .current_dir(&launch.cwd)
        .envs(&launch.environment)
        .env("PIKA_ASSISTANT_NATIVE_LAUNCH_TOKEN", &binding.launch_token)
        .env("PIKA_DB_PATH", root.join("native-registry/pika.db"))
        .env(
            "PIKA_ASSISTANT_BOARD_DB_PATH",
            root.join("empty-board.sqlite"),
        )
        .env("PIKA_STATE_HOME", temp.path().join("state"))
        .env("PIKA_CONFIG_HOME", temp.path().join("config"));
    command.args(["-c", "cli_auth_credentials_store=\"file\"", "app-server"]);
    let mut rpc = initialized_rpc(&mut command);
    let hooks = rpc.request("hooks/list", json!({"cwds":[launch.cwd]}));
    let hooks = hooks["data"][0]["hooks"].as_array().unwrap();
    assert_eq!(
        hooks.len(),
        6,
        "only the six generated private Pika hooks may be trusted"
    );
    let expected: Value = serde_json::from_str(
        &std::fs::read_to_string(root.join("provider-home/hooks.json")).unwrap(),
    )
    .unwrap();
    let mut edits = Vec::new();
    for hook in hooks {
        assert_eq!(
            hook["sourcePath"].as_str().unwrap(),
            root.join("provider-home/hooks.json").to_str().unwrap()
        );
        let command = hook["command"].as_str().unwrap();
        assert!(
            expected["hooks"]
                .as_object()
                .unwrap()
                .values()
                .any(|entries| entries
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|entry| entry["hooks"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|candidate| candidate["command"] == command)))
        );
        edits.push(json!({"keyPath":format!("hooks.state.{}.trusted_hash", serde_json::to_string(hook["key"].as_str().unwrap()).unwrap()),"value":hook["currentHash"],"mergeStrategy":"replace"}));
    }
    rpc.request(
        "config/batchWrite",
        json!({"filePath":config_path,"edits":edits,"reloadUserConfig":true}),
    );
    let hooks = rpc.request("hooks/list", json!({"cwds":[launch.cwd]}));
    assert!(
        hooks["data"][0]["hooks"]
            .as_array()
            .unwrap()
            .iter()
            .all(|hook| hook["trustStatus"] == "trusted")
    );
    let thread = rpc.request(
        "thread/start",
        json!({"model":"gpt-6-luna","cwd":launch.cwd,"approvalPolicy":"never","ephemeral":false}),
    );
    let id = thread["thread"]["id"].as_str().unwrap().to_owned();
    binding.thread_id = Some(id.clone());
    save_binding(&root, &binding).unwrap();
    crate::assistant_native_recovery::record_context(&root, &profile, "personal", &id).unwrap();
    let candidate = crate::model::Candidate {
        provider: crate::model::Provider::Codex,
        session_id: id.clone(),
        name: Some("Disposable continuity acceptance".into()),
        cwd: Some(launch.cwd.display().to_string()),
        branch: None,
        transcript_path: None,
        model: None,
        updated_at: 1.0,
        live: false,
        pid: None,
        source: "explicit-live-acceptance".into(),
        parent_session_id: None,
        created_at: 1.0,
        lifecycle_status: None,
    };
    registry
        .upsert_session(&crate::core::session_from_candidate(&candidate), false)
        .unwrap();
    registry
        .bind_launch(&binding.launch_token, crate::model::Provider::Codex, &id)
        .unwrap();
    live_turn(
        &mut rpc,
        &id,
        "For this synthetic project, I deliberately parked Cedar until its accessibility review is complete. Do not nag me merely because it is quiet. Going forward, when I ask about priorities, start with the decision that unblocks work, not a list of busy agents. Keep this in mind for our future conversations.",
    );
    let memory = crate::assistant_memory::Store::open(root.join("memory.sqlite")).unwrap();
    let records = memory
        .working_set(&crate::assistant::scope("personal").unwrap(), 64)
        .unwrap();
    assert!(
        records.iter().any(
            |record| record.origin == crate::assistant_memory::Origin::Human
                && record.body.contains("Cedar")
        ),
        "native Human hook did not persist the actual prompt"
    );
    let human = records
        .iter()
        .find(|record| {
            record.origin == crate::assistant_memory::Origin::Human && record.body.contains("Cedar")
        })
        .unwrap();
    assert!(
        records.iter().any(
            |record| record.origin == crate::assistant_memory::Origin::Worker
                && record.dependencies.contains(&human.id)
                && (record.body.contains("Cedar") || record.body.contains("unblock"))
        ),
        "model did not save relevant learning linked to actual Human prompt"
    );
    drop(memory);
    live_dream::run(&root, &codex);
    drop(rpc);
    crate::assistant_native::fresh_context(
        &root,
        &profile,
        "personal",
        &uuid::Uuid::new_v4().to_string(),
    )
    .unwrap();
    binding = bind_scope(&root, &profile, "personal").unwrap();
    command.env("PIKA_ASSISTANT_NATIVE_LAUNCH_TOKEN", &binding.launch_token);
    let mut rpc = initialized_rpc(&mut command);
    let fresh = rpc.request(
        "thread/start",
        json!({"model":"gpt-6-luna","cwd":launch.cwd,"approvalPolicy":"never","ephemeral":false}),
    );
    let fresh_id = fresh["thread"]["id"].as_str().unwrap().to_owned();
    assert_ne!(fresh_id, id);
    binding.thread_id = Some(fresh_id.clone());
    save_binding(&root, &binding).unwrap();
    crate::assistant_native_recovery::record_context(&root, &profile, "personal", &fresh_id)
        .unwrap();
    let fresh_candidate = crate::model::Candidate {
        session_id: fresh_id.clone(),
        ..candidate
    };
    registry
        .upsert_session(
            &crate::core::session_from_candidate(&fresh_candidate),
            false,
        )
        .unwrap();
    registry
        .bind_launch(
            &binding.launch_token,
            crate::model::Provider::Codex,
            &fresh_id,
        )
        .unwrap();
    let reply = live_turn(
        &mut rpc,
        &fresh_id,
        "I'm back. What did we decide about Cedar, and how should you help me choose what to focus on? Don't invent current progress.",
    );
    assert!(
        reply.to_lowercase().contains("accessibility"),
        "fresh context did not recall the actual condition: {reply}"
    );
    assert!(
        reply.to_lowercase().contains("unblock"),
        "fresh context missed priority preference: {reply}"
    );
    live_turn(
        &mut rpc,
        &fresh_id,
        "Correction for future conversations: the accessibility review is complete. Cedar is no longer parked. Prioritize Cedar now. Also replace my previous priorities preference: lead with the highest-risk unresolved decision, not simply whatever unblocks the most work. Save this correction.",
    );
    drop(rpc);
    let (mut rpc, corrected_id) = fresh_live_context(&root, &profile, &launch.cwd, &mut command);
    let corrected = live_turn(
        &mut rpc,
        &corrected_id,
        "I'm back again. What is the latest decision about Cedar, and what principle should you use to choose my focus? Consult current guidance, not just historical notes.",
    );
    assert!(
        corrected.to_lowercase().contains("risk"),
        "correction did not survive fresh context: {corrected}"
    );
    assert!(
        corrected.to_lowercase().contains("complete"),
        "Cedar correction not recalled: {corrected}"
    );
    println!("LIVE_RETURN_AND_CORRECTION_COMPLETED {corrected}");
    let memory = crate::assistant_memory::Store::open(root.join("memory.sqlite")).unwrap();
    let sources: Vec<String> = memory
        .working_set(&crate::assistant::scope("personal").unwrap(), 64)
        .unwrap()
        .into_iter()
        .filter(|record| {
            record.origin == crate::assistant_memory::Origin::Human && record.body.contains("Cedar")
        })
        .map(|record| record.id)
        .collect();
    assert!(sources.len() >= 4);
    drop(memory);
    for source in sources {
        let reply = run_live_turn(
            &mut rpc,
            &corrected_id,
            &format!("$pika-control forget {source}"),
            true,
        );
        assert!(
            reply.contains("removed"),
            "forget control did not confirm removal: {reply}"
        );
    }
    drop(rpc);
    let (mut rpc, forgotten_id) = fresh_live_context(&root, &profile, &launch.cwd, &mut command);
    let reply = live_turn(
        &mut rpc,
        &forgotten_id,
        "What do you remember about Cedar? If nothing is available, say so; do not guess.",
    );
    assert!(
        !reply.to_lowercase().contains("accessibility"),
        "forgotten detail resurfaced: {reply}"
    );
    assert!(
        !reply.to_lowercase().contains("highest-risk"),
        "forgotten preference resurfaced: {reply}"
    );
    println!("LIVE_FORGET_REPLY {reply}");
}

fn fresh_live_context(
    root: &Path,
    profile: &str,
    cwd: &Path,
    command: &mut Command,
) -> (Rpc, String) {
    crate::assistant_native::fresh_context(
        root,
        profile,
        "personal",
        &uuid::Uuid::new_v4().to_string(),
    )
    .unwrap();
    let mut binding = bind_scope(root, profile, "personal").unwrap();
    command.env("PIKA_ASSISTANT_NATIVE_LAUNCH_TOKEN", &binding.launch_token);
    let mut rpc = initialized_rpc(command);
    let thread = rpc.request(
        "thread/start",
        json!({"model":"gpt-6-luna","cwd":cwd,"approvalPolicy":"never","ephemeral":false}),
    );
    let id = thread["thread"]["id"].as_str().unwrap().to_owned();
    binding.thread_id = Some(id.clone());
    save_binding(root, &binding).unwrap();
    crate::assistant_native_recovery::record_context(root, profile, "personal", &id).unwrap();
    let registry = crate::store::Store::at(root.join("native-registry/pika.db"));
    let candidate = crate::model::Candidate {
        provider: crate::model::Provider::Codex,
        session_id: id.clone(),
        name: Some("Disposable continuity acceptance".into()),
        cwd: Some(cwd.display().to_string()),
        branch: None,
        transcript_path: None,
        model: None,
        updated_at: 1.0,
        live: false,
        pid: None,
        source: "explicit-live-acceptance".into(),
        parent_session_id: None,
        created_at: 1.0,
        lifecycle_status: None,
    };
    registry
        .upsert_session(&crate::core::session_from_candidate(&candidate), false)
        .unwrap();
    registry
        .bind_launch(&binding.launch_token, crate::model::Provider::Codex, &id)
        .unwrap();
    (rpc, id)
}

fn live_turn(rpc: &mut Rpc, id: &str, prompt: &str) -> String {
    run_live_turn(rpc, id, prompt, false)
}

fn run_live_turn(rpc: &mut Rpc, id: &str, prompt: &str, control: bool) -> String {
    let started = rpc.request(
        "turn/start",
        json!({"threadId":id,"input":[{"type":"text","text":prompt,"text_elements":[]}]}),
    );
    let turn = started["turn"]["id"].as_str().unwrap().to_owned();
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    let mut completed = false;
    let mut reply = String::new();
    while std::time::Instant::now() < deadline {
        let event = match rpc.output.recv_timeout(Duration::from_secs(5)) {
            Ok(event) => event,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(error) => panic!("provider stream ended: {error}"),
        };
        capture_live_item(&event, &mut reply);
        if event["method"] == "hook/completed" {
            let hook = &event["params"]["run"];
            println!("LIVE_HOOK {} {}", hook["eventName"], hook["status"]);
            if control && hook["status"] == "blocked" {
                reply.push_str(&hook["entries"].to_string());
            } else {
                assert_ne!(hook["status"], "blocked", "native hook blocked: {hook}");
            }
        }
        if event["method"] == "turn/completed" && event["params"]["turn"]["id"] == turn {
            println!("\nLIVE_TURN_STATUS {}", event["params"]["turn"]["status"]);
            assert_eq!(event["params"]["turn"]["status"], "completed");
            completed = true;
            break;
        }
        if event.get("id").is_some() && event.get("method").is_some() {
            panic!("unexpected approval request: {}", event["method"]);
        }
    }
    if !completed {
        let _ = rpc.request_frame("turn/interrupt", json!({"threadId":id,"turnId":turn}));
        panic!("live trial exceeded 120-second budget; interrupted without automatic retry");
    }
    assert!(
        !reply.trim().is_empty(),
        "completed status without actual final reply is not success"
    );
    reply
}

fn capture_live_item(event: &Value, reply: &mut String) {
    if event["method"] == "item/agentMessage/delta" {
        if let Some(delta) = event["params"]["delta"].as_str() {
            print!("{delta}");
        }
    }
    if event["method"] == "item/completed" {
        let item = &event["params"]["item"];
        if item["type"] == "agentMessage" && item["phase"] == "final_answer" {
            reply.push_str(item["text"].as_str().unwrap_or(""));
        }
        if item["type"] == "mcpToolCall" {
            println!("LIVE_TOOL {} {}", item["tool"], item["status"]);
        }
    }
}
