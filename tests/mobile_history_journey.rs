//! Actual native mobile endpoint, only synthetic provider stores and denied executables.
#![cfg(unix)]
use pikamux::{
    config::Config, core::Pika, model::Provider, paths::Paths, providers::Providers, store::Store,
    tmux::Tmux,
};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Child, Command, Stdio},
    sync::mpsc::{self, Receiver},
    time::{Duration, Instant},
};

struct Endpoint {
    child: Child,
    output: Receiver<Value>,
}
impl Endpoint {
    fn request(&mut self, method: &str, params: Value) -> Value {
        // Match SSHWire's real 20-second request deadline, including decoding.
        // The old per-frame 10-second wait both rejected slower debug builds
        // and could reset indefinitely on unrelated events. Production timing
        // is measured separately through the optimized binary and actual app.
        let started = Instant::now();
        let deadline = started + Duration::from_secs(20);
        let id = uuid::Uuid::new_v4().to_string();
        writeln!(
            self.child.stdin.as_mut().unwrap(),
            "{}",
            json!({"v":1,"id":id,"method":method,"params":params})
        )
        .unwrap();
        loop {
            let frame = self
                .output
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .expect("mobile response timeout");
            if frame["id"] == id {
                eprintln!("native journey {method}: {:?}", started.elapsed());
                return frame;
            }
        }
    }
}
impl Drop for Endpoint {
    fn drop(&mut self) {
        self.child.stdin.take();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn fixture_paths(root: &Path) -> Paths {
    Paths {
        config_dir: root.join("config/pika"),
        config: root.join("config/pika/config.json"),
        state_dir: root.join("state"),
        database: root.join("state/pika.db"),
        codex_home: root.join("codex"),
        claude_home: root.join("claude"),
        opencode_data_home: root.join("data/opencode"),
        opencode_config_home: root.join("config/opencode"),
        muse_data_home: root.join("data/muse"),
        muse_config_home: root.join("config/muse"),
    }
}

fn fixture(root: &Path, provider: Provider, id: &str) -> (Store, String) {
    let paths = fixture_paths(root);
    for directory in [
        &paths.state_dir,
        &paths.config_dir,
        &paths.claude_home,
        &paths.opencode_data_home,
        &paths.muse_data_home,
        &root.join("home"),
        &root.join("tmp"),
        &root.join("cache"),
        &root.join("bin"),
    ] {
        fs::create_dir_all(directory).unwrap();
    }
    let deny = root.join("bin/denied");
    fs::write(&deny, "#!/bin/sh\nexit 97\n").unwrap();
    fs::set_permissions(&deny, fs::Permissions::from_mode(0o700)).unwrap();
    for command in ["codex", "claude", "opencode", "muse", "ssh", "tmux", "curl"] {
        std::os::unix::fs::symlink(&deny, root.join("bin").join(command)).unwrap();
    }
    match provider {
        Provider::Claude => {
            let folder = paths.claude_home.join("projects/fixture");
            fs::create_dir_all(&folder).unwrap();
            let mut file = fs::File::create(folder.join(format!("{id}.jsonl"))).unwrap();
            writeln!(
                file,
                "{}",
                json!({"type":"custom-title","customTitle":"Synthetic Claude","sessionId":id})
            )
            .unwrap();
            for index in 0..85 {
                writeln!(file,"{}",json!({"type":if index%2==0{"user"}else{"assistant"},"uuid":format!("item-{index:03}"),"parentUuid":if index==0{Value::Null}else{json!(format!("item-{:03}",index-1))},"sessionId":id,"isSidechain":false,"entrypoint":"cli","message":{"role":if index%2==0{"user"}else{"assistant"},"content":[{"type":"text","text":format!("literal {index}\nsecond line")}]}})).unwrap();
            }
        }
        Provider::Muse => {
            let folder = paths.muse_data_home.join("sessions/2026/10/03").join(id);
            fs::create_dir_all(&folder).unwrap();
            let mut file = fs::File::create(folder.join("session.jsonl")).unwrap();
            writeln!(file,"{}",json!({"schema_version":1,"stream":{"kind":"session","id":id},"payload_type":"runtime.session.metadata","payload":{"record":{"workspace_root":root,"provider_id":"echo","model_id":"fixture"}}})).unwrap();
            for index in 0..85 {
                writeln!(file,"{}",json!({"schema_version":1,"id":format!("item-{index:03}"),"stream":{"kind":"session","id":id},"payload_type":"runtime.session","payload":{"event":if index%2==0{json!({"kind":"started","prompt":format!("literal {index}\nsecond line")})}else{json!({"kind":"assistant_message_committed","text":format!("literal {index}\nsecond line")})}}})).unwrap();
            }
        }
        Provider::Opencode => {
            let db = Connection::open(paths.opencode_data_home.join("opencode.db")).unwrap();
            db.execute_batch("CREATE TABLE session(id TEXT PRIMARY KEY,title TEXT,directory TEXT,parent_id TEXT,time_created INTEGER,time_updated INTEGER); CREATE TABLE message(id TEXT PRIMARY KEY,session_id TEXT,time_created INTEGER,data TEXT); CREATE TABLE part(id TEXT PRIMARY KEY,session_id TEXT,message_id TEXT,time_created INTEGER,data TEXT);").unwrap();
            db.execute(
                "INSERT INTO session VALUES(?,'Synthetic OpenCode',?,NULL,1,2)",
                params![id, root.to_str().unwrap()],
            )
            .unwrap();
            for index in 0..85 {
                let message_id = format!("item-{index:03}");
                db.execute(
                    "INSERT INTO message VALUES(?,?,?,?)",
                    params![
                        message_id,
                        id,
                        index,
                        json!({"role":if index%2==0{"user"}else{"assistant"}}).to_string()
                    ],
                )
                .unwrap();
                db.execute(
                    "INSERT INTO part VALUES(?,?,?,?,?)",
                    params![
                        format!("part-{index}"),
                        id,
                        message_id,
                        index,
                        json!({"type":"text","text":format!("literal {index}\nsecond line")})
                            .to_string()
                    ],
                )
                .unwrap();
            }
        }
        Provider::Codex => unreachable!(),
    }
    let store = Store::at(&paths.database);
    store.initialize().unwrap();
    let node = store.ensure_local_node_id().unwrap();
    let config = Config::default();
    let candidate = Providers::new(&paths, &config)
        .find(provider, id)
        .into_iter()
        .find(|c| c.session_id == id)
        .expect("native exact fixture candidate");
    let pika = Pika::with_components(
        paths,
        config,
        store.clone(),
        Tmux::with_executable(
            root.join("bin/denied").to_string_lossy(),
            Some("mobile-history-fixture".into()),
        ),
    );
    pika.adopt_candidate(&candidate).unwrap();
    (store, node)
}

fn endpoint(root: &Path) -> Endpoint {
    let mut command = Command::new(env!("CARGO_BIN_EXE_pika"));
    command
        .env_clear()
        .env("HOME", root.join("home"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("PIKA_STATE_HOME", root.join("state"))
        .env("PIKA_CONFIG_HOME", root.join("config/pika"))
        .env("PIKA_DB_PATH", root.join("state/pika.db"))
        .env("CODEX_HOME", root.join("codex"))
        .env("CLAUDE_CONFIG_DIR", root.join("claude"))
        .env("OPENCODE_DATA_HOME", root.join("data/opencode"))
        .env("TMPDIR", root.join("tmp"))
        .env("TMUX_TMPDIR", root.join("tmp"))
        .env("PIKA_TMUX_SOCKET", "mobile-history-fixture")
        .env(
            "PATH",
            format!("{}:/usr/bin:/bin", root.join("bin").display()),
        )
        .env("PIKA_UPDATE_CHECK", "0")
        .current_dir(root)
        .arg("_mobile")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = command.spawn().unwrap();
    let stdout = child.stdout.take().unwrap();
    let (sender, output) = mpsc::channel();
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
    Endpoint { child, output }
}

fn history_roundtrip(provider: Provider, id: &str) {
    let temp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(temp.path()).unwrap();
    let (store, node) = fixture(&root, provider, id);
    let mut original = store.get_session(provider, id).unwrap().unwrap();
    original.unread = true;
    store.upsert_session(&original, false).unwrap();
    let mut endpoint = endpoint(&root);
    let identity = json!({"nodeId":node,"provider":provider,"threadId":id});
    let opened = endpoint.request("conversation/open", json!({"identity":identity}));
    assert!(opened["error"].is_null(), "{opened}");
    assert_eq!(opened["result"]["identity"], identity);
    assert_eq!(opened["result"]["capabilities"]["send"], false);
    assert_eq!(opened["result"]["turns"]["order"], "chronological");
    let mut pages = vec![
        opened["result"]["turns"]["data"]
            .as_array()
            .unwrap()
            .clone(),
    ];
    let mut cursor = opened["result"]["turns"]["nextCursor"].clone();
    while cursor.is_string() {
        let older = endpoint.request(
            "conversation/history",
            json!({"identity":identity,"cursor":cursor}),
        );
        assert!(older["error"].is_null(), "{older}");
        pages.insert(
            0,
            older["result"]["turns"]["data"].as_array().unwrap().clone(),
        );
        cursor = older["result"]["turns"]["nextCursor"].clone();
    }
    let messages = pages.into_iter().flatten().collect::<Vec<_>>();
    assert_eq!(messages.len(), 85);
    for (index, turn) in messages.iter().enumerate() {
        assert_eq!(turn["items"][0]["id"], format!("item-{index:03}"));
        assert_eq!(
            turn["items"][0]["text"],
            format!("literal {index}\nsecond line")
        );
    }
    let send=endpoint.request("conversation/send",json!({"identity":identity,"text":"must not dispatch","clientMessageId":uuid::Uuid::new_v4().to_string()}));
    assert!(send["error"].is_object(), "{send}");
    assert_eq!(send["error"]["code"], "rejected_before_dispatch", "{send}");
    assert!(store.get_session(provider, id).unwrap().unwrap().unread);
    store.untrack_session(provider, id).unwrap();
    let denied = endpoint.request("conversation/open", json!({"identity":identity}));
    assert!(denied["error"].is_object(), "{denied}");
}

#[test]
fn successful_mobile_read_acknowledges_only_its_exact_ready_event() {
    let temp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(temp.path()).unwrap();
    let id = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    let (store, node) = fixture(&root, Provider::Claude, id);
    let mut session = store.get_session(Provider::Claude, id).unwrap().unwrap();
    session.status = pikamux::model::Status::Ready;
    session.unread = true;
    session.last_event_at += 100.0;
    store.upsert_session(&session, false).unwrap();
    let mut endpoint = endpoint(&root);
    let identity = json!({"nodeId":node,"provider":"claude","threadId":id});
    let opened = endpoint.request("conversation/open", json!({"identity":identity}));
    assert!(opened["error"].is_null(), "{opened}");
    let token = opened["result"]["readAcknowledgement"].clone();
    assert!(token.is_string(), "{opened}");
    let wrong_identity = json!({"nodeId":node,"provider":"claude","threadId":"bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb"});
    let wrong_owner = endpoint.request(
        "conversation/acknowledge",
        json!({"identity":wrong_identity,"readAcknowledgement":token}),
    );
    assert!(wrong_owner["error"].is_object());
    // Reading/paging alone does not mutate unread; only rendering acknowledgement does.
    assert!(
        store
            .get_session(Provider::Claude, id)
            .unwrap()
            .unwrap()
            .unread
    );
    let wrong = endpoint.request(
        "conversation/acknowledge",
        json!({"identity":identity,"readAcknowledgement":"wrong"}),
    );
    assert!(wrong["error"].is_object());
    assert!(
        store
            .get_session(Provider::Claude, id)
            .unwrap()
            .unwrap()
            .unread
    );
    session.last_event_at += 1.0;
    store.upsert_session(&session, false).unwrap();
    let newer = endpoint.request(
        "conversation/acknowledge",
        json!({"identity":identity,"readAcknowledgement":token}),
    );
    assert_eq!(newer["result"]["acknowledged"], false, "{newer}");
    assert!(
        store
            .get_session(Provider::Claude, id)
            .unwrap()
            .unwrap()
            .unread
    );
    let reopened = endpoint.request("conversation/open", json!({"identity":identity}));
    let fresh = reopened["result"]["readAcknowledgement"].clone();
    let stale = endpoint.request(
        "conversation/acknowledge",
        json!({"identity":identity,"readAcknowledgement":token}),
    );
    assert!(stale["error"].is_object());
    let ack = endpoint.request(
        "conversation/acknowledge",
        json!({"identity":identity,"readAcknowledgement":fresh}),
    );
    assert_eq!(ack["result"]["acknowledged"], true, "{ack}");
    assert!(
        !store
            .get_session(Provider::Claude, id)
            .unwrap()
            .unwrap()
            .unread
    );
    // This is persisted server state, not an app-side badge override.
    drop(endpoint);
    let mut endpoint = self::endpoint(&root);
    let reopened = endpoint.request("conversation/open", json!({"identity":identity}));
    assert!(reopened["result"]["readAcknowledgement"].is_null());
    session.unread = true;
    for status in [
        pikamux::model::Status::Working,
        pikamux::model::Status::Error,
        pikamux::model::Status::NeedsYou,
    ] {
        session.last_event_at += 1.0;
        session.status = status;
        store.upsert_session(&session, false).unwrap();
        let opened = endpoint.request("conversation/open", json!({"identity":identity}));
        assert!(opened["error"].is_null(), "{status}: {opened}");
        assert!(
            opened["result"]["readAcknowledgement"].is_null(),
            "{status}: {opened}"
        );
        assert!(
            store
                .get_session(Provider::Claude, id)
                .unwrap()
                .unwrap()
                .unread,
            "{status}"
        );
    }
    session.status = pikamux::model::Status::Ready;
    session.last_event_at += 1.0;
    store.upsert_session(&session, false).unwrap();
    let path = root
        .join("claude/projects/fixture")
        .join(format!("{id}.jsonl"));
    fs::remove_file(path).unwrap();
    let failed = endpoint.request("conversation/open", json!({"identity":identity}));
    assert!(failed["error"].is_object(), "{failed}");
    let denied = endpoint.request(
        "conversation/acknowledge",
        json!({"identity":identity,"readAcknowledgement":fresh}),
    );
    assert!(denied["error"].is_object());
    assert!(
        store
            .get_session(Provider::Claude, id)
            .unwrap()
            .unwrap()
            .unread
    );
}

#[test]
fn claude_original_history_mobile_roundtrip() {
    history_roundtrip(Provider::Claude, "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa");
}

#[test]
fn claude_channel_history_reopens_only_native_correlated_content() {
    let temp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(temp.path()).unwrap();
    let thread = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    let (store, node) = fixture(&root, Provider::Claude, thread);
    let token = uuid::Uuid::new_v4();
    let operation = uuid::Uuid::new_v4().to_string();
    store
        .set_meta(&format!("claude-channel:{token}:thread"), thread)
        .unwrap();
    store.set_meta(&format!("claude-channel:{token}:operation:{operation}"), &json!({
        "text":"  phone message\nwith literal whitespace  ","released":true,"reply":"Native channel reply",
        "attestation":null,"fetch_tool_id":"toolu_fetch","reply_tool_id":"toolu_reply"
    }).to_string()).unwrap();
    let server = format!("mcp__pika_{}", token.simple());
    let records = [
        (
            "assistant",
            json!([{"type":"tool_use","id":"toolu_fetch","name":format!("{server}__fetch_message"),"input":{"operation_id":operation}}]),
        ),
        (
            "user",
            json!([{"type":"tool_result","tool_use_id":"toolu_fetch","content":[{"type":"text","text":"  phone message\nwith literal whitespace  "}]}]),
        ),
        (
            "assistant",
            json!([{"type":"tool_use","id":"toolu_reply","name":format!("{server}__reply"),"input":{"operation_id":operation,"text":"Native channel reply"}}]),
        ),
        (
            "user",
            json!([{"type":"tool_result","tool_use_id":"toolu_reply","content":"Reply recorded for the original Pika operation."}]),
        ),
        (
            "assistant",
            json!([{"type":"tool_use","id":"toolu_forged","name":format!("{server}__reply"),"input":{"operation_id":operation,"text":"Must not be shown"}}]),
        ),
    ];
    let path = root
        .join("claude/projects/fixture")
        .join(format!("{thread}.jsonl"));
    let mut file = fs::OpenOptions::new().append(true).open(path).unwrap();
    for (offset, (role, content)) in records.into_iter().enumerate() {
        let index = offset + 85;
        let mut parent = format!("item-{:03}", index - 1);
        if offset == 1 {
            // Observed native hook attachment siblings are not alternate turns.
            writeln!(file,"{}",json!({"type":"attachment","uuid":"hook-leaf","parentUuid":parent,"sessionId":thread,"isSidechain":false})).unwrap();
            writeln!(file,"{}",json!({"type":"attachment","uuid":"hook-leaf-child","parentUuid":"hook-leaf","sessionId":thread,"isSidechain":false})).unwrap();
        }
        if offset == 2 {
            // An attachment on the actual message ancestry must be retained.
            writeln!(file,"{}",json!({"type":"attachment","uuid":"hook-ancestor","parentUuid":parent,"sessionId":thread,"isSidechain":false})).unwrap();
            parent = "hook-ancestor".into();
        }
        writeln!(file,"{}",json!({"type":role,"uuid":format!("item-{index:03}"),"parentUuid":parent,"sessionId":thread,"isSidechain":false,"message":{"role":role,"content":content}})).unwrap();
    }
    writeln!(file,"{}",json!({"type":"user","uuid":"internal-wake","parentUuid":"item-089","sessionId":thread,"isSidechain":false,"isMeta":true,"message":{"role":"user","content":"INTERNAL_PROTOCOL_WAKE"}})).unwrap();
    let identity = json!({"nodeId":node,"provider":"claude","threadId":thread});
    // Reopening uses a fresh mobile process and the native source, not live journal overlays.
    let key = format!("claude-channel:{token}:operation:{operation}");
    let complete = store.get_meta(&key).unwrap().unwrap();
    let mut unsettled: Value = serde_json::from_str(&complete).unwrap();
    unsettled["reply"] = Value::Null;
    unsettled["reply_tool_id"] = Value::Null;
    store.set_meta(&key, &unsettled.to_string()).unwrap();
    let mut before_settlement = endpoint(&root);
    let before = before_settlement.request("conversation/open", json!({"identity":identity}));
    assert!(before["error"].is_null(), "{before}");
    store.set_meta(&key, &complete).unwrap();
    let changed = before_settlement.request(
        "conversation/history",
        json!({"identity":identity,"cursor":before["result"]["turns"]["nextCursor"]}),
    );
    assert!(
        changed["error"].to_string().contains("projection changed"),
        "{changed}"
    );
    drop(before_settlement);
    for _ in 0..2 {
        let mut mobile = endpoint(&root);
        let opened = mobile.request("conversation/open", json!({"identity":identity}));
        assert!(opened["error"].is_null(), "{opened}");
        let turns = opened["result"]["turns"]["data"].as_array().unwrap();
        let entries: Vec<_> = turns
            .iter()
            .flat_map(|turn| turn["items"].as_array().unwrap())
            .collect();
        assert_eq!(
            entries[entries.len() - 2]["id"],
            "claude-channel-user:toolu_fetch"
        );
        assert_eq!(
            entries[entries.len() - 2]["text"],
            "  phone message\nwith literal whitespace  "
        );
        assert_eq!(
            entries.last().unwrap()["id"],
            "claude-channel-reply:toolu_reply"
        );
        assert_eq!(entries.last().unwrap()["text"], "Native channel reply");
        assert!(!opened.to_string().contains("Must not be shown"));
        assert!(!opened.to_string().contains("Reply recorded"));
        assert!(!opened.to_string().contains("INTERNAL_PROTOCOL_WAKE"));
    }
    // Native compaction preserves the real result but removes its source call.
    // The reader must reverify that source call without reviving its old reply.
    writeln!(file,"{}",json!({"type":"system","subtype":"compact_boundary","uuid":"compact","parentUuid":null,"sessionId":thread,"isSidechain":false,"compactMetadata":{"preservedMessages":{"anchorUuid":"summary","uuids":["item-086"]}}})).unwrap();
    writeln!(file,"{}",json!({"type":"user","uuid":"summary","parentUuid":"compact","sessionId":thread,"isSidechain":false,"message":{"role":"user","content":"Native summary"}})).unwrap();
    writeln!(file,"{}",json!({"type":"assistant","uuid":"latest","parentUuid":"item-086","sessionId":thread,"isSidechain":false,"message":{"role":"assistant","content":"Latest after compaction"}})).unwrap();
    for _ in 0..2 {
        let mut mobile = endpoint(&root);
        let opened = mobile.request("conversation/open", json!({"identity":identity}));
        assert!(opened["error"].is_null(), "{opened}");
        let entries: Vec<_> = opened["result"]["turns"]["data"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|turn| turn["items"].as_array().unwrap())
            .collect();
        assert_eq!(entries.len(), 3, "{opened}");
        assert_eq!(entries[1]["id"], "claude-channel-user:toolu_fetch");
        assert_eq!(
            entries[1]["text"],
            "  phone message\nwith literal whitespace  "
        );
        assert_eq!(entries[2]["text"], "Latest after compaction");
        assert!(!opened.to_string().contains("Native channel reply"));
    }
}
#[test]
fn opencode_original_history_mobile_roundtrip() {
    history_roundtrip(Provider::Opencode, "ses_exactfixture");
}
#[test]
fn muse_original_history_mobile_roundtrip() {
    history_roundtrip(Provider::Muse, "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb");
}

fn claude_ambiguous_history_is_explicitly_unavailable(record: Value, reason: &str) {
    let temp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(temp.path()).unwrap();
    let id = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    let (_, node) = fixture(&root, Provider::Claude, id);
    let path = root
        .join("claude/projects/fixture")
        .join(format!("{id}.jsonl"));
    let mut file = fs::OpenOptions::new().append(true).open(path).unwrap();
    writeln!(file, "{record}").unwrap();
    let mut endpoint = endpoint(&root);
    let response = endpoint.request(
        "conversation/open",
        json!({"identity":{"nodeId":node,"provider":"claude","threadId":id}}),
    );
    assert!(
        response["error"]["message"]
            .as_str()
            .unwrap()
            .contains(reason),
        "{response}"
    );
    assert!(response["result"].is_null());
}

#[test]
fn claude_rewind_is_unavailable_not_abandoned_branch_history() {
    claude_ambiguous_history_is_explicitly_unavailable(
        json!({"uuid":"rewound-C","parentUuid":"item-000","sessionId":"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa","type":"user","message":{"role":"user","content":"rewound request"}}),
        "branching ambiguity",
    );
}

#[test]
fn claude_unknown_compaction_schema_is_unavailable() {
    claude_ambiguous_history_is_explicitly_unavailable(
        json!({"uuid":"compacted","parentUuid":"item-084","sessionId":"aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa","type":"system","subtype":"compact_boundary","compactMetadata":{"preservedMessages":["item-000"]}}),
        "native relinking",
    );
}

#[test]
fn claude_native_compaction_pages_reconstructed_order_and_freezes_cursor() {
    let temp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(temp.path()).unwrap();
    let thread = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    let (_store, node) = fixture(&root, Provider::Claude, thread);
    let source = root.join(format!("claude/projects/fixture/{thread}.jsonl"));
    append_large_hidden_records(&source, thread);
    let mut file = fs::OpenOptions::new().append(true).open(&source).unwrap();
    // Native 2.1.274 iOs/aOs: exact preserved UUID list is relocated AFTER its
    // summary anchor, irrespective of where those records were appended.
    let kept: Vec<_> = (20..85).map(|index| format!("item-{index:03}")).collect();
    writeln!(file,"{}",json!({"type":"system","subtype":"compact_boundary","uuid":"boundary","parentUuid":null,"logicalParentUuid":"item-084","sessionId":thread,"compactMetadata":{"trigger":"auto","preTokens":100,"preservedMessages":{"anchorUuid":"summary","uuids":kept,"allUuids":kept}}})).unwrap();
    writeln!(file,"{}",json!({"type":"user","uuid":"summary","parentUuid":"boundary","sessionId":thread,"isCompactSummary":true,"message":{"role":"user","content":"Native compact summary"}})).unwrap();
    writeln!(file,"{}",json!({"type":"assistant","uuid":"latest","parentUuid":"summary","sessionId":thread,"message":{"role":"assistant","content":"Latest after compact"}})).unwrap();
    let identity = json!({"nodeId":node,"provider":"claude","threadId":thread});
    let mut phone = endpoint(&root);
    let opened = phone.request("conversation/open", json!({"identity":identity}));
    assert!(opened["error"].is_null(), "{opened}");
    let mut all = opened["result"]["turns"]["data"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(all.last().unwrap()["id"], "latest");
    let cursor = opened["result"]["turns"]["nextCursor"].clone();
    assert!(cursor.is_string());
    // An append cannot move the older-page snapshot or inject a new message.
    writeln!(file,"{}",json!({"type":"user","uuid":"append","parentUuid":"latest","sessionId":thread,"message":{"role":"user","content":"New append"}})).unwrap();
    let older = phone.request(
        "conversation/history",
        json!({"identity":identity,"cursor":cursor}),
    );
    assert!(older["error"].is_null(), "{older}");
    let mut prefix = older["result"]["turns"]["data"].as_array().unwrap().clone();
    assert!(older["result"]["turns"]["nextCursor"].is_null());
    prefix.append(&mut all);
    let ids: Vec<_> = prefix
        .iter()
        .map(|turn| turn["id"].as_str().unwrap())
        .collect();
    let mut expected = vec!["summary".to_owned()];
    expected.extend((20..85).map(|index| format!("item-{index:03}")));
    expected.push("latest".into());
    assert_eq!(ids, expected);
    drop(phone);
    let mut reopened = endpoint(&root);
    let latest = reopened.request("conversation/open", json!({"identity":identity}));
    assert!(latest["error"].is_null(), "{latest}");
    assert_eq!(
        latest["result"]["turns"]["data"]
            .as_array()
            .unwrap()
            .last()
            .unwrap()["id"],
        "append"
    );
}

fn append_large_hidden_records(source: &Path, thread: &str) {
    let mut file = fs::OpenOptions::new().append(true).open(source).unwrap();
    let payload = "x".repeat(2 * 1024 * 1024);
    let mut parent = "item-084".to_owned();
    for index in 0..12 {
        let id = format!("hidden-{index}");
        if matches!(index % 4, 0 | 3) {
            let call = format!("hidden-call-{index}");
            writeln!(file,"{}",json!({"type":"assistant","uuid":call,"parentUuid":parent,"sessionId":thread,"message":{"role":"assistant","content":[{"type":"tool_use","id":format!("hidden-tool-{index}"),"name":"synthetic","input":{}}]}})).unwrap();
            parent = call;
        }
        let (role, block) = match index % 4 {
            0 => (
                "user",
                json!({"type":"tool_result","tool_use_id":format!("hidden-tool-{index}"),"content":payload}),
            ),
            1 => (
                "assistant",
                json!({"type":"thinking","thinking":payload,"signature":"synthetic"}),
            ),
            2 => (
                "user",
                json!({"type":"image","source":{"type":"base64","media_type":"image/png","data":payload}}),
            ),
            _ => (
                "user",
                json!({"type":"tool_result","tool_use_id":format!("hidden-tool-{index}"),"content":[{"type":"text","text":payload}]}),
            ),
        };
        let record = json!({"type":role,"uuid":id,"parentUuid":parent,"sessionId":thread,"message":{"role":role,"content":[block]}});
        let bytes = serde_json::to_vec(&record).unwrap();
        assert!(bytes.len() > 2 * 1024 * 1024);
        writeln!(file, "{record}").unwrap();
        parent = id;
    }
    assert!(fs::metadata(source).unwrap().len() > 16 * 1024 * 1024);
}

#[test]
fn claude_five_thousand_small_nodes_remain_supported() {
    let temp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(temp.path()).unwrap();
    let thread = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    let (_store, node) = fixture(&root, Provider::Claude, thread);
    let source = root.join(format!("claude/projects/fixture/{thread}.jsonl"));
    let mut file = fs::OpenOptions::new().append(true).open(&source).unwrap();
    for index in 85..5000 {
        writeln!(file,"{}",json!({"type":"user","uuid":format!("item-{index:03}"),"parentUuid":format!("item-{:03}",index-1),"sessionId":thread,"message":{"role":"user","content":format!("literal-{index}")}})).unwrap();
    }
    let bytes = fs::metadata(source).unwrap().len();
    assert!(bytes < 16 * 1024 * 1024);
    let identity = json!({"nodeId":node,"provider":"claude","threadId":thread});
    let mut phone = endpoint(&root);
    let opened = phone.request("conversation/open", json!({"identity":identity}));
    assert!(opened["error"].is_null(), "{opened}");
    assert_eq!(opened["result"]["turns"]["data"][39]["id"], "item-4999");
    let older = phone.request(
        "conversation/history",
        json!({"identity":identity,"cursor":opened["result"]["turns"]["nextCursor"]}),
    );
    assert!(older["error"].is_null(), "{older}");
    assert_eq!(older["result"]["turns"]["data"][39]["id"], "item-4959");
    eprintln!(
        "small native endpoint: {bytes} source bytes, 5000 ancestry nodes, latest and older page supported"
    );
}

#[test]
fn claude_multimib_hidden_and_visible_records_page_exactly_and_reopen() {
    let temp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(temp.path()).unwrap();
    let thread = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    let (_, node) = fixture(&root, Provider::Claude, thread);
    let source = root.join(format!("claude/projects/fixture/{thread}.jsonl"));
    append_large_hidden_records(&source, thread);
    let mut expected: Vec<(String, String)> = (0..85)
        .map(|i| (format!("item-{i:03}"), format!("literal {i}\nsecond line")))
        .collect();
    let mut file = fs::OpenOptions::new().append(true).open(&source).unwrap();
    let mut parent = "hidden-11".to_owned();
    for index in 0..12 {
        let id = format!("visible-large-{index}");
        let text = format!(
            "Large {index}\n```rust\n{}\n```\n",
            "雪🦀\\\"\t\nlet value = 42;\n".repeat(45_000)
        );
        assert!(serde_json::to_vec(&text).unwrap().len() > 256 * 1024);
        writeln!(file,"{}",json!({"type":"assistant","uuid":id,"parentUuid":parent,"sessionId":thread,"message":{"role":"assistant","content":[{"type":"text","text":text}]}})).unwrap();
        parent = id.clone();
        expected.push((id, text));
    }
    let identity = json!({"nodeId":node,"provider":"claude","threadId":thread});
    for _ in 0..2 {
        let mut phone = endpoint(&root);
        let mut response = phone.request("conversation/open", json!({"identity":identity}));
        let mut pages = Vec::new();
        loop {
            assert!(response["error"].is_null(), "{}", response["error"]);
            let turns = &response["result"]["turns"];
            let page = turns["data"].as_array().unwrap().clone();
            assert!(!page.is_empty());
            assert!(page.len() <= 40);
            assert!(serde_json::to_vec(&page).unwrap().len() <= 8 * 1024 * 1024);
            pages.insert(0, page);
            let cursor = turns["nextCursor"].clone();
            if cursor.is_null() {
                break;
            }
            assert!(pages.len() < 20);
            response = phone.request(
                "conversation/history",
                json!({"identity":identity,"cursor":cursor}),
            );
        }
        assert!(
            pages.len() > 3,
            "Byte budget must split twelve multi-MiB messages"
        );
        let all: Vec<_> = pages.into_iter().flatten().collect();
        assert_eq!(all.len(), expected.len());
        for (turn, (id, text)) in all.iter().zip(&expected) {
            assert_eq!(turn["items"][0]["id"], *id);
            let actual = turn["items"][0]["text"].as_str().unwrap();
            assert_eq!(actual, text);
            use sha2::{Digest, Sha256};
            assert_eq!(
                Sha256::digest(actual.as_bytes()),
                Sha256::digest(text.as_bytes())
            );
        }
    }
}

#[test]
fn claude_source_unicode_escapes_are_bounded_by_outgoing_text_not_raw_record() {
    let temp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(temp.path()).unwrap();
    let thread = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    let (_, node) = fixture(&root, Provider::Claude, thread);
    let source = root.join(format!("claude/projects/fixture/{thread}.jsonl"));
    let mut file = fs::OpenOptions::new().append(true).open(source).unwrap();
    let escaped = "\\u0061".repeat(2 * 1024 * 1024);
    assert_eq!(escaped.len(), 12 * 1024 * 1024);
    // Native-style insertion ordering puts message before the outer type, and
    // text before its block discriminator. Classification cannot depend on order.
    writeln!(file, "{{\"message\":{{\"role\":\"assistant\",\"content\":[{{\"text\":\"{escaped}\",\"type\":\"text\"}}]}},\"sessionId\":\"{thread}\",\"parentUuid\":\"item-084\",\"uuid\":\"escaped-large\",\"type\":\"assistant\"}}").unwrap();
    writeln!(file, "{{\"type\":\"user\",\"uuid\":\"escaped-small\",\"parentUuid\":\"escaped-large\",\"sessionId\":\"{thread}\",\"message\":{{\"role\":\"user\",\"content\":\"\\ud83e\\udd80\\u96ea\\u0061\"}}}}").unwrap();
    let identity = json!({"nodeId":node,"provider":"claude","threadId":thread});
    for _ in 0..2 {
        let mut phone = endpoint(&root);
        let response = phone.request("conversation/open", json!({"identity":identity}));
        assert!(response["error"].is_null(), "{}", response["error"]);
        let page = response["result"]["turns"]["data"].as_array().unwrap();
        let large = &page[page.len() - 2]["items"][0];
        assert_eq!(large["id"], "escaped-large");
        assert_eq!(large["text"].as_str().unwrap(), "a".repeat(2 * 1024 * 1024));
        assert_eq!(page.last().unwrap()["items"][0]["text"], "🦀雪a");
    }
}

#[test]
fn claude_visible_record_over_encoded_page_budget_is_explicitly_unavailable() {
    let temp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(temp.path()).unwrap();
    let thread = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    let (_, node) = fixture(&root, Provider::Claude, thread);
    let source = root.join(format!("claude/projects/fixture/{thread}.jsonl"));
    let mut file = fs::OpenOptions::new().append(true).open(source).unwrap();
    // Decoded text is only 5 MiB, but JSON escaping exceeds the 8 MiB wire budget.
    let text = "\n".repeat(5 * 1024 * 1024);
    assert!(serde_json::to_vec(&text).unwrap().len() > 8 * 1024 * 1024);
    writeln!(file,"{}",json!({"type":"user","uuid":"too-large","parentUuid":"item-084","sessionId":thread,"message":{"role":"user","content":text}})).unwrap();
    let mut phone = endpoint(&root);
    let response = phone.request(
        "conversation/open",
        json!({"identity":{"nodeId":node,"provider":"claude","threadId":thread}}),
    );
    assert!(response["result"].is_null());
    assert!(
        response["error"].to_string().contains("8 MiB"),
        "{response}"
    );
}

#[test]
fn claude_malformed_multimib_hidden_record_fails_closed() {
    let temp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(temp.path()).unwrap();
    let thread = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    let (_, node) = fixture(&root, Provider::Claude, thread);
    let source = root.join(format!("claude/projects/fixture/{thread}.jsonl"));
    let mut file = fs::OpenOptions::new().append(true).open(source).unwrap();
    writeln!(
        file,
        "{{\"type\":\"file-history-snapshot\",\"padding\":\"{}\",\"broken\":]}}",
        "x".repeat(3 * 1024 * 1024)
    )
    .unwrap();
    let mut phone = endpoint(&root);
    let response = phone.request(
        "conversation/open",
        json!({"identity":{"nodeId":node,"provider":"claude","threadId":thread}}),
    );
    assert!(response["result"].is_null());
    assert!(response["error"].is_object(), "{response}");
}

#[test]
fn claude_large_history_opens_pages_reopens_and_refuses_nonprefix_rewrite() {
    let temp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(temp.path()).unwrap();
    let thread = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    let (_store, node) = fixture(&root, Provider::Claude, thread);
    let source = root.join(format!("claude/projects/fixture/{thread}.jsonl"));
    append_large_hidden_records(&source, thread);
    let identity = json!({"nodeId":node,"provider":"claude","threadId":thread});
    for _ in 0..2 {
        let mut phone = endpoint(&root);
        let opened = phone.request("conversation/open", json!({"identity":identity}));
        assert!(opened["error"].is_null(), "{opened}");
        let mut all = opened["result"]["turns"]["data"]
            .as_array()
            .unwrap()
            .clone();
        let mut cursor = opened["result"]["turns"]["nextCursor"].clone();
        while cursor.is_string() {
            let older = phone.request(
                "conversation/history",
                json!({"identity":identity,"cursor":cursor}),
            );
            assert!(older["error"].is_null(), "{older}");
            let mut prefix = older["result"]["turns"]["data"].as_array().unwrap().clone();
            prefix.append(&mut all);
            all = prefix;
            cursor = older["result"]["turns"]["nextCursor"].clone();
        }
        assert_eq!(all.len(), 85);
        for (index, turn) in all.iter().enumerate() {
            assert_eq!(turn["id"], format!("item-{index:03}"));
        }
    }
    let mut phone = endpoint(&root);
    let opened = phone.request("conversation/open", json!({"identity":identity}));
    let cursor = opened["result"]["turns"]["nextCursor"].clone();
    // Same inode/length and unchanged first 4096 bytes must not hide a rewrite.
    use std::io::{Read, Seek, SeekFrom};
    let mut file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&source)
        .unwrap();
    file.seek(SeekFrom::Start(50_000)).unwrap();
    let mut byte = [0];
    file.read_exact(&mut byte).unwrap();
    assert_eq!(byte[0], b'x');
    file.seek(SeekFrom::Start(50_000)).unwrap();
    file.write_all(b"y").unwrap();
    let rejected = phone.request(
        "conversation/history",
        json!({"identity":identity,"cursor":cursor}),
    );
    assert!(
        rejected["error"]
            .to_string()
            .contains("frozen history changed"),
        "{rejected}"
    );
    eprintln!(
        "large native endpoint: {} source bytes, 97 ancestry nodes, complete latest/paging/reopen and full-prefix mutation refusal",
        fs::metadata(source).unwrap().len()
    );
}
