use pikamux::fleet::{
    CAPABILITIES, ConsultationPolicy, FleetError, FleetErrorKind, FleetManager, FleetService,
    FleetSession, FleetTransport, NodeCandidate, PROTOCOL_NAME, PROTOCOL_VERSION,
    RemoteConsultation, SshTransport, discover_node_candidates, discover_ssh_candidates,
    handle_fleet_stdio, next_remote_node, session_to_wire, validate_snapshot,
};
use pikamux::model::{Candidate, FleetNode, Provider, Session, Status};
use pikamux::store::Store;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::fs;
use std::io::Cursor;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tempfile::TempDir;
use uuid::Uuid;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
}

fn session(provider: Provider, id: &str, name: &str) -> Session {
    Session {
        provider,
        session_id: id.to_owned(),
        name: Some(name.to_owned()),
        cwd: Some("/remote/project".to_owned()),
        branch: Some("main".to_owned()),
        transcript_path: Some("/secret/provider.jsonl".to_owned()),
        tmux_session: Some("pika-c-1".to_owned()),
        tmux_pane: Some("%9".to_owned()),
        root_pid: Some(999),
        status: Status::Working,
        unread: false,
        model: Some("model".to_owned()),
        source: "managed".to_owned(),
        managed: true,
        error: None,
        attention_reason: None,
        created_at: 1.0,
        updated_at: 10.0,
        last_event_at: 9.0,
        last_activity_at: 9.0,
        live: true,
        attached: false,
        home_state: "exact-live".to_owned(),
        cpu_percent: Some(0.5),
        rss_kb: Some(1024),
        input_tokens: None,
        output_tokens: None,
        cached_input_tokens: None,
        cache_write_tokens: None,
        total_tokens: None,
        estimated_cost_usd: None,
        active_thread_id: None,
    }
}

fn node(id: &str, alias: &str) -> FleetNode {
    FleetNode {
        node_id: id.to_owned(),
        alias: alias.to_owned(),
        ssh_target: alias.to_owned(),
        sources: vec!["explicit".to_owned()],
        status: "ready".to_owned(),
        protocol_version: Some(PROTOCOL_VERSION),
        package_version: Some("0.6.0".to_owned()),
        capabilities: CAPABILITIES.iter().map(|v| (*v).to_owned()).collect(),
        last_seen: 0.0,
        last_attempt_at: 0.0,
        last_error: None,
        created_at: 1.0,
        updated_at: 1.0,
    }
}

fn snapshot(id: &str, session_id: &str) -> Value {
    json!({
        "type":"snapshot", "protocol":PROTOCOL_NAME, "version":PROTOCOL_VERSION,
        "node_id":id, "machine":"atlas", "captured_at":now(),
        "sessions":[session_to_wire(&session(Provider::Codex, session_id, "remote-work"), false)],
        "profiles":[{
            "provider":"codex", "session_id":session_id,
            "scope":"Owns pricing infrastructure", "current_state":"Validating rollout",
            "topics":["pricing"], "artifacts":["/remote/result.md"],
            "updated_at":10.0, "source":"interview"
        }],
        "cards":[{
            "provider":"codex", "session_id":session_id,
            "status":"CURRENT", "detail":"matches remote source"
        }]
    })
}

#[derive(Default)]
struct FakeTransport {
    responses: Mutex<VecDeque<Result<Value, FleetError>>>,
    requests: Mutex<Vec<(String, Value, bool)>>,
    exact: Mutex<Vec<(String, Vec<String>, bool)>>,
}

impl FakeTransport {
    fn with(responses: Vec<Result<Value, FleetError>>) -> Self {
        Self {
            responses: Mutex::new(responses.into()),
            ..Self::default()
        }
    }
}

impl FleetTransport for &FakeTransport {
    fn request(&self, target: &str, payload: &Value, mutating: bool) -> Result<Value, FleetError> {
        self.requests
            .lock()
            .unwrap()
            .push((target.to_owned(), payload.clone(), mutating));
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .expect("fake response")
    }

    fn run_exact(
        &self,
        node: &FleetNode,
        arguments: &[String],
        tty: bool,
    ) -> Result<i32, FleetError> {
        self.exact
            .lock()
            .unwrap()
            .push((node.node_id.clone(), arguments.to_vec(), tty));
        Ok(0)
    }
}

fn initialized_store(temp: &TempDir, name: &str) -> Store {
    let store = Store::at(temp.path().join(name));
    store.initialize().unwrap();
    store
}

fn executable(path: &Path, body: &str) {
    fs::write(path, format!("#!/bin/sh\nset -eu\n{body}\n")).unwrap();
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

#[test]
fn ssh_config_discovery_is_file_only_and_configured_hosts_rank_first() {
    let temp = TempDir::new().unwrap();
    let ssh = temp.path().join("ssh");
    fs::create_dir_all(ssh.join("conf.d")).unwrap();
    fs::write(
        ssh.join("config"),
        "Host z-work\nHost *.wild !blocked\nInclude conf.d/*.conf\n",
    )
    .unwrap();
    fs::write(ssh.join("conf.d/gpu.conf"), "Host gpu-box\n").unwrap();
    assert_eq!(
        discover_ssh_candidates(&ssh)
            .iter()
            .map(|v| v.alias.as_str())
            .collect::<Vec<_>>(),
        ["gpu-box", "z-work"]
    );

    let tailscale = temp.path().join("tailscale");
    executable(
        &tailscale,
        &format!(
            "printf '%s' '{}'",
            include_str!("fixtures/fleet/tailscale.json").replace('\'', "'\"'\"'")
        ),
    );
    let store = initialized_store(&temp, "state.db");
    let report =
        discover_node_candidates(&store, &ssh, &tailscale, Duration::from_secs(1)).unwrap();
    assert_eq!(report.excluded_unsupported_os, 1);
    assert_eq!(report.candidates[0].sources, ["ssh-config"]);
    assert!(report.candidates.iter().any(|v| v.alias == "laptop"));
}

#[test]
fn wire_never_exports_transcript_tmux_or_process_identity() {
    let encoded =
        session_to_wire(&session(Provider::Codex, "thread-id", "work"), false).to_string();
    for secret in ["transcript", "tmux_pane", "tmux_session", "root_pid"] {
        assert!(!encoded.contains(secret), "leaked {secret}");
    }
}

#[test]
fn strict_snapshot_rejects_identity_changes_duplicates_and_extensions() {
    let expected = Uuid::new_v4().to_string();
    let other = Uuid::new_v4().to_string();
    assert_eq!(
        validate_snapshot(&snapshot(&other, "thread"), Some(&expected))
            .unwrap_err()
            .kind,
        FleetErrorKind::Quarantined
    );
    let mut duplicate = snapshot(&expected, "thread");
    duplicate["sessions"] = json!([
        duplicate["sessions"][0].clone(),
        duplicate["sessions"][0].clone()
    ]);
    assert!(
        validate_snapshot(&duplicate, Some(&expected))
            .unwrap_err()
            .message
            .contains("repeats")
    );
    let mut extension = snapshot(&expected, "thread");
    extension["profiles"][0]["transcript_path"] = json!("/secret");
    assert!(validate_snapshot(&extension, Some(&expected)).is_err());
}

#[test]
fn failed_initial_snapshot_trusts_nothing() {
    let temp = TempDir::new().unwrap();
    let store = initialized_store(&temp, "state.db");
    let remote = Uuid::new_v4().to_string();
    let hello = json!({
        "type":"hello", "protocol":PROTOCOL_NAME, "version":PROTOCOL_VERSION,
        "node_id":remote, "machine":"atlas", "package_version":"0.6.0",
        "capabilities":CAPABILITIES,
    });
    let fake = FakeTransport::with(vec![
        Ok(hello),
        Err(FleetError::new(
            FleetErrorKind::Incompatible,
            "truncated snapshot",
        )),
    ]);
    let manager = FleetManager::new(&store, &fake);
    let candidate = NodeCandidate {
        alias: "atlas".into(),
        ssh_target: "atlas".into(),
        sources: vec!["explicit".into()],
        hostname: None,
        online: None,
        os_name: None,
    };
    assert!(manager.add(&candidate, None).is_err());
    assert!(store.list_nodes().unwrap().is_empty());
}

#[test]
fn cached_remote_identity_includes_node_and_stale_cache_cannot_need_attention() {
    let temp = TempDir::new().unwrap();
    let store = initialized_store(&temp, "state.db");
    let thread = "same-thread";
    for alias in ["atlas", "gpu"] {
        let id = Uuid::new_v4().to_string();
        store.upsert_fleet_node(&node(&id, alias)).unwrap();
        store
            .put_remote_snapshot(&id, &snapshot(&id, thread), now())
            .unwrap();
    }
    let fake = FakeTransport::default();
    let manager = FleetManager::new(&store, &fake);
    let values = manager.cached_sessions(None, false).unwrap();
    assert_eq!(values.len(), 2);
    assert_ne!(values[0].key(), values[1].key());
    store
        .mark_fleet_node_error(&values[0].node_id, "unreachable", "timeout")
        .unwrap();
    let stale = manager
        .cached_sessions(Some(&values[0].node_id), false)
        .unwrap();
    assert!(stale[0].stale);
    assert!(!stale[0].needs_attention());
}

#[test]
fn refresh_scheduler_is_bounded_oldest_first_and_manual_selection_only() {
    let mut selected = node(&Uuid::new_v4().to_string(), "selected");
    selected.last_attempt_at = 90.0;
    let mut oldest = node(&Uuid::new_v4().to_string(), "oldest");
    oldest.last_attempt_at = 10.0;
    let mut middle = node(&Uuid::new_v4().to_string(), "middle");
    middle.last_attempt_at = 50.0;
    let nodes = vec![selected.clone(), oldest.clone(), middle];
    assert_eq!(
        next_remote_node(&nodes, Some(&selected.node_id), 200.0, false)
            .unwrap()
            .node_id,
        oldest.node_id
    );
    assert_eq!(
        next_remote_node(&nodes, Some(&selected.node_id), 200.0, true)
            .unwrap()
            .node_id,
        selected.node_id
    );
}

#[test]
fn attach_refreshes_and_routes_only_exact_node_provider_uuid() {
    let temp = TempDir::new().unwrap();
    let store = initialized_store(&temp, "state.db");
    let remote = Uuid::new_v4().to_string();
    let thread = "11111111-1111-4111-8111-111111111111";
    store.upsert_fleet_node(&node(&remote, "atlas")).unwrap();
    store
        .put_remote_snapshot(&remote, &snapshot(&remote, thread), now())
        .unwrap();
    let fake = FakeTransport::with(vec![Ok(snapshot(&remote, thread))]);
    let manager = FleetManager::new(&store, &fake);
    let selected = manager.cached_sessions(None, false).unwrap().remove(0);
    assert_eq!(manager.attach(&selected).unwrap(), 0);
    let calls = fake.exact.lock().unwrap();
    assert!(calls[0].2);
    assert!(calls[0].1.contains(&thread.to_owned()));
    assert!(!calls[0].1.contains(&"remote-work".to_owned()));
}

#[test]
fn mutation_timeout_reuses_durable_idempotency_key() {
    let temp = TempDir::new().unwrap();
    let store = initialized_store(&temp, "state.db");
    let remote = Uuid::new_v4().to_string();
    store.upsert_fleet_node(&node(&remote, "atlas")).unwrap();
    let fake = FakeTransport::with(vec![Err(FleetError::new(
        FleetErrorKind::OutcomeUnknown,
        "lost",
    ))]);
    let manager = FleetManager::new(&store, &fake);
    let candidate = Candidate {
        provider: Provider::Codex,
        session_id: "thread".into(),
        name: Some("work".into()),
        cwd: None,
        branch: None,
        transcript_path: None,
        model: None,
        updated_at: 0.0,
        live: false,
        pid: None,
        source: "remote".into(),
        parent_session_id: None,
        created_at: 0.0,
        lifecycle_status: None,
    };
    let error = manager
        .adopt(&node(&remote, "atlas"), &candidate, None)
        .unwrap_err();
    assert_eq!(error.kind, FleetErrorKind::OutcomeUnknown);
    let saved = store
        .get_meta(&format!("fleet:pending-adopt:{remote}:codex:thread"))
        .unwrap()
        .unwrap();
    assert!(Uuid::parse_str(&saved).is_ok());
}

#[derive(Default)]
struct Service {
    untracks: usize,
}
impl FleetService for Service {
    fn snapshot(&mut self, _extended: bool) -> Result<Value, FleetError> {
        unreachable!()
    }
    fn candidates(&mut self, _include: bool) -> Result<Vec<Candidate>, FleetError> {
        Ok(Vec::new())
    }
    fn adopt(&mut self, provider: Provider, id: &str) -> Result<Session, FleetError> {
        Ok(session(provider, id, "adopted"))
    }
    fn peek(
        &mut self,
        _provider: Provider,
        _id: &str,
        _lines: usize,
    ) -> Result<String, FleetError> {
        Ok("tail".to_owned())
    }
    fn acknowledge(&mut self, _provider: Provider, _id: &str) -> Result<bool, FleetError> {
        Ok(true)
    }
    fn untrack(&mut self, _provider: Provider, _id: &str) -> Result<(i64, bool), FleetError> {
        self.untracks += 1;
        Ok((1, true))
    }
}

#[test]
fn server_validates_exact_route_and_replays_mutation_receipt_once() {
    let temp = TempDir::new().unwrap();
    let store = initialized_store(&temp, "state.db");
    let node_id = store.ensure_local_node_id().unwrap();
    let request_id = Uuid::new_v4().to_string();
    let request = json!({
        "op":"untrack", "protocol":PROTOCOL_NAME, "version":PROTOCOL_VERSION,
        "expected_node_id":node_id, "provider":"claude", "session_id":"exact-thread",
        "request_id":request_id,
    });
    let bytes = format!("{request}\n{request}\n");
    let mut output = Vec::new();
    let mut service = Service::default();
    handle_fleet_stdio(
        &store,
        "atlas",
        "0.6.0",
        &mut service,
        Cursor::new(bytes),
        &mut output,
    )
    .unwrap();
    let receipts: Vec<Value> = String::from_utf8(output)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(receipts[0], receipts[1]);
    assert_eq!(service.untracks, 1);
}

#[test]
fn server_rejects_changed_node_without_service_action() {
    let temp = TempDir::new().unwrap();
    let store = initialized_store(&temp, "state.db");
    let request = json!({
        "op":"acknowledge", "protocol":PROTOCOL_NAME, "version":PROTOCOL_VERSION,
        "expected_node_id":Uuid::new_v4().to_string(), "provider":"codex", "session_id":"exact",
    });
    let mut output = Vec::new();
    let mut service = Service::default();
    handle_fleet_stdio(
        &store,
        "atlas",
        "0.6.0",
        &mut service,
        Cursor::new(format!("{request}\n")),
        &mut output,
    )
    .unwrap();
    let value: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(value["kind"], "quarantined");
}

#[test]
fn ssh_payload_stays_on_stdin_and_remote_argv_is_fixed() {
    let temp = TempDir::new().unwrap();
    let fake = temp.path().join("ssh");
    let log = temp.path().join("argv");
    executable(
        &fake,
        &format!(
            "printf '%s\\n' \"$@\" > '{}'; printf '%s\\n' '{{\"type\":\"ok\"}}'",
            log.display()
        ),
    );
    let transport = SshTransport::new(&fake, Duration::from_secs(1), Duration::from_secs(1));
    let dangerous = "$(touch /tmp/pika-must-not-exist); rm -rf nope";
    let response = transport
        .request(
            "developer@atlas",
            &json!({"op":"hello", "question":dangerous}),
            false,
        )
        .unwrap();
    assert_eq!(response["type"], "ok");
    let argv = fs::read_to_string(log).unwrap();
    assert!(!argv.contains("touch"));
    assert!(argv.contains("'_fleet' '--stdio'"));
    assert!(!Path::new("/tmp/pika-must-not-exist").exists());
}

#[test]
fn ssh_timeout_is_bounded_and_mutation_becomes_outcome_unknown() {
    let temp = TempDir::new().unwrap();
    let fake = temp.path().join("ssh");
    executable(&fake, "sleep 2");
    let transport = SshTransport::new(&fake, Duration::from_secs(1), Duration::from_millis(30));
    let started = std::time::Instant::now();
    let error = transport
        .request("atlas", &json!({"op":"x"}), true)
        .unwrap_err();
    assert_eq!(error.kind, FleetErrorKind::OutcomeUnknown);
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[test]
fn remote_consultation_reuses_one_connection_and_requires_v2_cleanup() {
    let temp = TempDir::new().unwrap();
    let fake = temp.path().join("ssh");
    let body = "printf '%s\\n' '{\"type\":\"opened\",\"provider\":\"codex\",\"parent_id\":\"parent\",\"workstream_id\":\"parent\",\"consultation_mode\":\"default\",\"model\":\"gpt-test\",\"effort\":\"low\"}'\nwhile IFS= read -r line; do\n case \"$line\" in\n *\\\"close\\\"*) printf '%s\\n' '{\"type\":\"closed\",\"receipt_version\":2,\"discarded\":true,\"cleanup\":\"complete\"}'; exit 0 ;;\n *) printf '%s\\n' '{\"type\":\"answer\",\"text\":\"from remote expert\"}' ;;\n esac\ndone";
    executable(&fake, body);
    let node_id = Uuid::new_v4().to_string();
    let trusted = node(&node_id, "atlas");
    let remote = FleetSession {
        node_id: node_id.clone(),
        node_name: "atlas".into(),
        session: session(Provider::Codex, "parent", "expert"),
        stale: false,
        remote_error: None,
        seen_at: now(),
        card_status: None,
        card_detail: None,
        watched: true,
        availability: None,
        scope_updated_at: None,
        current_state_updated_at: None,
        current_state_status: None,
    };
    let policy = ConsultationPolicy {
        consultation_mode: "default".into(),
        model: "gpt-test".into(),
        effort: "low".into(),
    };
    let transport = SshTransport::new(&fake, Duration::from_secs(1), Duration::from_secs(1));
    let mut side = RemoteConsultation::open(
        &transport,
        trusted,
        remote,
        policy,
        Duration::from_secs(1),
        Duration::from_secs(1),
        Duration::from_secs(1),
    )
    .unwrap();
    assert_eq!(side.ask("first").unwrap(), "from remote expert");
    assert_eq!(side.ask("follow up").unwrap(), "from remote expert");
    let receipt = side.close().unwrap();
    assert_eq!(receipt.answers_received, Some(2));
    assert_eq!(receipt.cleanup.as_deref(), Some("complete"));
}

#[test]
fn remote_consultation_refuses_wrong_leaf_before_sending_question() {
    let temp = TempDir::new().unwrap();
    let fake = temp.path().join("ssh");
    executable(
        &fake,
        "printf '%s\\n' '{\"type\":\"opened\",\"provider\":\"codex\",\"parent_id\":\"old-parent\",\"consultation_mode\":\"default\",\"model\":\"gpt-test\",\"effort\":\"low\"}'; sleep 2",
    );
    let node_id = Uuid::new_v4().to_string();
    let trusted = node(&node_id, "atlas");
    let mut base = session(Provider::Codex, "parent", "expert");
    base.active_thread_id = Some("active-leaf".to_owned());
    let remote = FleetSession {
        node_id,
        node_name: "atlas".into(),
        session: base,
        stale: false,
        remote_error: None,
        seen_at: now(),
        card_status: None,
        card_detail: None,
        watched: true,
        availability: None,
        scope_updated_at: None,
        current_state_updated_at: None,
        current_state_status: None,
    };
    let policy = ConsultationPolicy {
        consultation_mode: "default".into(),
        model: "gpt-test".into(),
        effort: "low".into(),
    };
    let transport = SshTransport::new(&fake, Duration::from_secs(1), Duration::from_secs(1));
    let error = RemoteConsultation::open(
        &transport,
        trusted,
        remote,
        policy,
        Duration::from_secs(1),
        Duration::from_secs(1),
        Duration::from_secs(1),
    )
    .err()
    .expect("wrong identity rejected");
    assert_eq!(error.kind, FleetErrorKind::Quarantined);
    assert!(error.message.contains("no question was sent"));
}

#[test]
fn remote_consultation_rejects_partial_frames_and_unverified_cleanup() {
    let temp = TempDir::new().unwrap();
    let node_id = Uuid::new_v4().to_string();
    let remote = FleetSession {
        node_id: node_id.clone(),
        node_name: "atlas".into(),
        session: session(Provider::Codex, "parent", "expert"),
        stale: false,
        remote_error: None,
        seen_at: now(),
        card_status: None,
        card_detail: None,
        watched: true,
        availability: None,
        scope_updated_at: None,
        current_state_updated_at: None,
        current_state_status: None,
    };
    let policy = ConsultationPolicy {
        consultation_mode: "default".into(),
        model: "gpt-test".into(),
        effort: "low".into(),
    };

    let partial = temp.path().join("partial-ssh");
    executable(&partial, "printf '%s' '{\"type\":\"opened\"}'");
    let transport = SshTransport::new(&partial, Duration::from_secs(1), Duration::from_secs(1));
    let error = RemoteConsultation::open(
        &transport,
        node(&node_id, "atlas"),
        remote.clone(),
        policy.clone(),
        Duration::from_secs(1),
        Duration::from_secs(1),
        Duration::from_secs(1),
    )
    .err()
    .unwrap();
    assert!(error.message.contains("partial JSONL"));

    let bad_cleanup = temp.path().join("bad-cleanup-ssh");
    let body = "printf '%s\\n' '{\"type\":\"opened\",\"provider\":\"codex\",\"parent_id\":\"parent\",\"workstream_id\":\"parent\",\"consultation_mode\":\"default\",\"model\":\"gpt-test\",\"effort\":\"low\"}'\nwhile IFS= read -r line; do\n case \"$line\" in\n *\\\"close\\\"*) printf '%s\\n' '{\"type\":\"closed\",\"discarded\":true}'; exit 0 ;;\n *) printf '%s\\n' '{\"type\":\"answer\",\"text\":\"useful\"}' ;;\n esac\ndone";
    executable(&bad_cleanup, body);
    let transport = SshTransport::new(&bad_cleanup, Duration::from_secs(1), Duration::from_secs(1));
    let mut side = RemoteConsultation::open(
        &transport,
        node(&node_id, "atlas"),
        remote,
        policy,
        Duration::from_secs(1),
        Duration::from_secs(1),
        Duration::from_secs(1),
    )
    .unwrap();
    assert_eq!(side.ask("question").unwrap(), "useful");
    let error = side.close().unwrap_err();
    assert_eq!(error.kind, FleetErrorKind::OutcomeUnknown);
    assert_eq!(error.receipt.unwrap().cleanup.as_deref(), Some("unknown"));
}
