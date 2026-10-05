use super::*;
use crate::{
    config::Config,
    model::{Candidate, Provider},
    paths::Paths,
    store::Store,
    tmux::Tmux,
};

fn fixture() -> (tempfile::TempDir, Pika, Binding, String) {
    let root = tempfile::tempdir().unwrap();
    let p = root.path();
    let paths = Paths {
        config_dir: p.join("config"),
        state_dir: p.join("state"),
        config: p.join("config/config.json"),
        database: p.join("state/pika.db"),
        codex_home: p.join("codex"),
        claude_home: p.join("claude"),
        opencode_data_home: p.join("oc"),
        opencode_config_home: p.join("oc-config"),
        muse_data_home: p.join("muse"),
        muse_config_home: p.join("muse-config"),
    };
    let store = Store::from_paths(&paths);
    store.initialize().unwrap();
    let binding = Binding {
        version: 1,
        token: uuid::Uuid::new_v4().to_string(),
        thread: uuid::Uuid::new_v4().to_string(),
        cwd: p.to_string_lossy().into(),
        native_pid: 42,
        native_start: 123,
        mcp_pid: 43,
        mcp_start: 124,
        endpoint: p.join("unused.sock").to_string_lossy().into(),
        server_name: "pika_fixture".into(),
        ready: true,
    };
    let candidate = Candidate {
        provider: Provider::Claude,
        session_id: binding.thread.clone(),
        name: Some("Synthetic".into()),
        cwd: Some(binding.cwd.clone()),
        branch: None,
        transcript_path: None,
        model: None,
        updated_at: 1.0,
        live: false,
        pid: None,
        source: "fixture".into(),
        parent_session_id: None,
        created_at: 1.0,
        lifecycle_status: None,
    };
    store
        .upsert_session(&crate::core::session_from_candidate(&candidate), false)
        .unwrap();
    store
        .bind_launch(&binding.token, Provider::Claude, &binding.thread)
        .unwrap();
    assert!(
        store
            .certify_launch(&binding.token, Provider::Claude, &binding.thread, 42, 123)
            .unwrap()
    );
    let pika = Pika::with_components(
        paths,
        Config::default(),
        store,
        Tmux::with_executable("/usr/bin/true", Some("unused".into())),
    );
    change_route(&pika, &binding, |r| {
        r.initialized = true;
        r.started = true;
    })
    .unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    (root, pika, binding, id)
}

fn proof(binding: &Binding, id: &str, short: &str, tool: &str, text: Option<&str>) -> Value {
    let mut args = json!({"operation_id":id});
    if let Some(text) = text {
        args["text"] = json!(text);
    }
    json!({"hook_event_name":"PreToolUse","session_id":binding.thread,"tool_name":tool_name(binding,short),"tool_use_id":tool,"tool_input":args})
}

#[test]
fn attested_literal_release_is_one_use_and_reply_is_exact_receipt() {
    let (_root, pika, binding, id) = fixture();
    let text = "  literal\nrequest  ";
    assert!(admit(&pika, &binding, &id, text).unwrap());
    assert!(!admit(&pika, &binding, &id, text).unwrap());
    assert!(admit(&pika, &binding, &id, "changed").is_err());
    let fetch = json!({"operation_id":id});
    assert!(execute_tool(&pika, &binding, "fetch_message", &fetch, "toolu_fetch").is_err());
    install_attestation(
        &pika,
        &binding,
        &proof(&binding, &id, "fetch_message", "toolu_fetch", None),
    )
    .unwrap();
    assert!(execute_tool(&pika, &binding, "fetch_message", &fetch, "toolu_wrong").is_err());
    assert_eq!(
        execute_tool(&pika, &binding, "fetch_message", &fetch, "toolu_fetch").unwrap(),
        text
    );
    assert!(execute_tool(&pika, &binding, "fetch_message", &fetch, "toolu_fetch").is_err());
    assert_eq!(
        receipt(&pika, &binding, &id).unwrap().unwrap()["state"],
        "unknown"
    );
    let answer = "  native reply\n ";
    install_attestation(
        &pika,
        &binding,
        &proof(&binding, &id, "reply", "toolu_reply", Some(answer)),
    )
    .unwrap();
    execute_tool(
        &pika,
        &binding,
        "reply",
        &json!({"operation_id":id,"text":answer}),
        "toolu_reply",
    )
    .unwrap();
    assert_eq!(
        receipt(&pika, &binding, &id).unwrap().unwrap()["state"],
        "delivered"
    );
    let archived = history_operation(&pika.paths, &binding.thread, &binding.token, &id)
        .unwrap()
        .unwrap();
    assert_eq!(archived.fetch_tool_id.as_deref(), Some("toolu_fetch"));
    assert_eq!(archived.reply_tool_id.as_deref(), Some("toolu_reply"));
}

#[test]
fn validated_dead_binding_selects_history_without_native_control() {
    use std::os::unix::fs::DirBuilderExt;
    let (_root, pika, mut binding, _) = fixture();
    let directory = pika.paths.state_dir.join("claude-shared");
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&directory)
        .unwrap();
    binding.native_pid = i64::from(std::process::id());
    binding.native_start = 0;
    binding.endpoint = super::super::endpoint_path(&pika.paths, &binding.token)
        .unwrap()
        .display()
        .to_string();
    binding.server_name = format!(
        "pika_{}",
        uuid::Uuid::parse_str(&binding.token).unwrap().simple()
    );
    super::super::publish(&pika.paths, &binding).unwrap();
    assert!(
        super::super::read_binding(&pika.paths, &binding.thread)
            .unwrap()
            .is_some()
    );
    assert!(
        super::super::Client::connect(&pika, &binding.thread)
            .unwrap()
            .is_none()
    );
    binding.version = 2;
    super::super::publish(&pika.paths, &binding).unwrap();
    assert!(super::super::Client::connect(&pika, &binding.thread).is_err());
}

#[test]
fn duplicate_owner_after_attestation_withholds_content() {
    let (_root, pika, binding, id) = fixture();
    admit(&pika, &binding, &id, "private sentinel").unwrap();
    install_attestation(
        &pika,
        &binding,
        &proof(&binding, &id, "fetch_message", "toolu_fetch", None),
    )
    .unwrap();
    let mut session = pika
        .store
        .get_session(Provider::Claude, &binding.thread)
        .unwrap()
        .unwrap();
    session.status = crate::model::Status::OpenTwice;
    pika.store.upsert_session(&session, false).unwrap();
    assert!(
        execute_tool(
            &pika,
            &binding,
            "fetch_message",
            &json!({"operation_id":id}),
            "toolu_fetch"
        )
        .is_err()
    );
    let wake = wake_instruction(&binding, &id);
    assert!(wake.contains(&tool_name(&binding, "fetch_message")));
    assert!(wake.contains(&tool_name(&binding, "reply")));
    assert!(wake.contains(&id));
    assert!(!wake.contains("private sentinel"));
}

#[test]
fn attest_then_untrack_or_lifecycle_revoke_withholds_content() {
    for untrack in [true, false] {
        let (_root, pika, binding, id) = fixture();
        admit(&pika, &binding, &id, "private").unwrap();
        install_attestation(
            &pika,
            &binding,
            &proof(&binding, &id, "fetch_message", "toolu_fetch", None),
        )
        .unwrap();
        if untrack {
            pika.store
                .untrack_session(Provider::Claude, &binding.thread)
                .unwrap();
        } else {
            change_route(&pika, &binding, |r| r.revoked = true).unwrap();
        }
        assert!(
            execute_tool(
                &pika,
                &binding,
                "fetch_message",
                &json!({"operation_id":id}),
                "toolu_fetch"
            )
            .is_err()
        );
        assert!(admit(&pika, &binding, &uuid::Uuid::new_v4().to_string(), "new").is_err());
        if untrack {
            assert!(receipt(&pika, &binding, &id).is_err());
        } else {
            assert_eq!(
                receipt(&pika, &binding, &id).unwrap().unwrap()["state"],
                "unknown"
            );
        }
    }
}

#[test]
fn foreign_session_subagent_and_extra_input_cannot_attest() {
    let (_root, pika, binding, id) = fixture();
    admit(&pika, &binding, &id, "private").unwrap();
    for field in ["session_id", "agent_id"] {
        let mut input = proof(&binding, &id, "fetch_message", "toolu_fetch", None);
        input[field] = json!("foreign");
        assert!(install_attestation(&pika, &binding, &input).is_err());
    }
    let mut input = proof(&binding, &id, "fetch_message", "toolu_fetch", None);
    input["tool_input"]["extra"] = json!(true);
    assert!(install_attestation(&pika, &binding, &input).is_err());
    assert!(
        execute_tool(
            &pika,
            &binding,
            "fetch_message",
            &json!({"operation_id":id}),
            "toolu_fetch"
        )
        .is_err()
    );
}

#[test]
fn ios_uppercase_uuid_maps_to_one_durable_operation_without_rewake() {
    let (_root, pika, binding, id) = fixture();
    let phone_id = id.to_uppercase();
    assert!(admit(&pika, &binding, &phone_id, "literal phone input").unwrap());
    assert!(!admit(&pika, &binding, &id, "literal phone input").unwrap());
    install_attestation(
        &pika,
        &binding,
        &proof(&binding, &id, "fetch_message", "toolu_phone", None),
    )
    .unwrap();
    assert_eq!(
        execute_tool(
            &pika,
            &binding,
            "fetch_message",
            &json!({"operation_id":id}),
            "toolu_phone"
        )
        .unwrap(),
        "literal phone input"
    );
    assert_eq!(
        receipt(&pika, &binding, &phone_id).unwrap().unwrap()["state"],
        "unknown"
    );
    assert!(!admit(&pika, &binding, &phone_id, "literal phone input").unwrap());
}

#[test]
fn expired_attestation_does_not_release_content() {
    let (_root, pika, binding, id) = fixture();
    admit(&pika, &binding, &id, "private").unwrap();
    install_attestation(
        &pika,
        &binding,
        &proof(&binding, &id, "fetch_message", "toolu_expired", None),
    )
    .unwrap();
    pika.store
        .reconcile_transaction(|ledger| {
            let mut op = read_operation(ledger, &binding, &id)?;
            op.attestation.as_mut().unwrap().issued = epoch()? - TTL - 1;
            write_operation(ledger, &binding, &id, &op)
        })
        .unwrap();
    assert!(
        execute_tool(
            &pika,
            &binding,
            "fetch_message",
            &json!({"operation_id":id}),
            "toolu_expired"
        )
        .is_err()
    );
    assert_eq!(
        receipt(&pika, &binding, &id).unwrap().unwrap()["state"],
        "unknown"
    );
}

#[test]
fn framed_native_reply_preserves_worst_case_json_escaping_and_rejects_oversize() {
    let value = json!({"text":"\0".repeat(TEXT)});
    let mut wire = Vec::new();
    write_packet(&mut wire, &value).unwrap();
    assert_eq!(
        read_packet(&mut std::io::Cursor::new(wire))
            .unwrap()
            .unwrap(),
        value
    );
    assert!(read_packet(&mut std::io::Cursor::new(vec![b'x'; FRAME + 1])).is_err());
}

#[test]
fn cold_reopen_recovers_original_receipt_and_never_replays_uncertain_operation() {
    let (_root, pika, old, id) = fixture();
    admit(&pika, &old, &id, "literal").unwrap();
    install_attestation(
        &pika,
        &old,
        &proof(&old, &id, "fetch_message", "toolu_old_fetch", None),
    )
    .unwrap();
    execute_tool(
        &pika,
        &old,
        "fetch_message",
        &json!({"operation_id":id}),
        "toolu_old_fetch",
    )
    .unwrap();
    install_attestation(
        &pika,
        &old,
        &proof(&old, &id, "reply", "toolu_old_reply", Some("answer")),
    )
    .unwrap();
    execute_tool(
        &pika,
        &old,
        "reply",
        &json!({"operation_id":id,"text":"answer"}),
        "toolu_old_reply",
    )
    .unwrap();
    let unknown = uuid::Uuid::new_v4().to_string();
    admit(&pika, &old, &unknown, "uncertain").unwrap();
    let mut new = old.clone();
    new.token = uuid::Uuid::new_v4().to_string();
    pika.store
        .bind_launch(&new.token, Provider::Claude, &new.thread)
        .unwrap();
    pika.store
        .certify_launch(
            &new.token,
            Provider::Claude,
            &new.thread,
            new.native_pid,
            new.native_start as i64,
        )
        .unwrap();
    change_route(&pika, &new, |r| {
        r.started = true;
        r.initialized = true;
    })
    .unwrap();
    assert_eq!(
        receipt(&pika, &new, &id.to_uppercase()).unwrap().unwrap()["state"],
        "delivered"
    );
    assert_eq!(
        receipt(&pika, &new, &unknown).unwrap().unwrap()["state"],
        "unknown"
    );
    assert!(!admit(&pika, &new, &id, "literal").unwrap());
    assert!(!admit(&pika, &new, &unknown, "uncertain").unwrap());
    assert!(admit(&pika, &new, &unknown, "changed").is_err());
    assert!(
        pika.store
            .get_meta(&operation_key(&new, &id).unwrap())
            .unwrap()
            .is_none()
    );
}
