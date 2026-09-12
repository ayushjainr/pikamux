use pikamux::consult::CancellationToken;
use pikamux::fleet::{
    CAPABILITIES, ConsultationPolicy, ConsultationTimeouts, FleetError, FleetErrorKind,
    FleetManager, FleetService, FleetSession, FleetTransport, MAX_CACHED_FLEET_BYTES,
    MAX_CACHED_FLEET_NODES, MAX_CACHED_FLEET_ROWS, MAX_SNAPSHOT_SESSIONS, NodeCandidate,
    PROTOCOL_NAME, PROTOCOL_VERSION, RemoteConsultation, SshTransport,
    bound_snapshot_for_transport, discover_node_candidates, discover_ssh_candidates,
    handle_fleet_stdio, next_remote_node, session_to_wire, validate_snapshot,
};
use pikamux::model::{Candidate, FleetNode, Provider, Session, Status};
use pikamux::store::Store;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::fs;
use std::io::Cursor;
use std::path::Path;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tempfile::TempDir;
use uuid::Uuid;

const CODEX_THREAD_ID: &str = "11111111-1111-4111-8111-111111111111";
const CODEX_PARENT_ID: &str = "22222222-2222-4222-8222-222222222222";
const CODEX_ACTIVE_ID: &str = "33333333-3333-4333-8333-333333333333";
const CODEX_OTHER_ID: &str = "44444444-4444-4444-8444-444444444444";
const CODEX_CHILD_ID: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
const CLAUDE_THREAD_ID: &str = "55555555-5555-4555-8555-555555555555";

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
        // This is the canonical value emitted by native core reconciliation;
        // `session_to_wire` translates it to the protocol's exact_home bit.
        home_state: "exact".to_owned(),
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

fn opened_receipt(parent_id: &str, workstream_id: &str) -> Value {
    json!({
        "type":"opened", "receipt_version":2, "ephemeral":true,
        "provider":"codex", "parent_id":parent_id, "workstream_id":workstream_id,
        "child_id":CODEX_CHILD_ID,
        "isolation":{
            "version":1, "mechanism":"codex_thread_fork", "evidence":"provider_confirmed",
            "distinct_child":true, "ephemeral_confirmed":true,
            "parent_turns_excluded":true, "approval_policy":"never", "sandbox":"read-only"
        },
        "receipt":{
            "receipt_version":2, "stage":"prepare", "delivery":"not_sent",
            "cleanup":"pending", "turn":0, "answers_received":0
        },
        "consultation_mode":"default", "model":"gpt-test", "effort":"low"
    })
}

fn provider_opened_receipt(
    provider: Provider,
    parent_id: &str,
    workstream_id: &str,
    child_id: Option<&str>,
) -> Value {
    let isolation = match provider {
        Provider::Codex => opened_receipt(parent_id, workstream_id)["isolation"].clone(),
        Provider::Claude => json!({
            "version":1, "mechanism":"claude_fork_process", "evidence":"launch_arguments",
            "fork_session":true, "no_session_persistence":true, "tools_disabled":true
        }),
        Provider::Opencode => json!({
            "version":1, "mechanism":"opencode_session_fork",
            "evidence":"provider_issued_child_and_isolated_runtime", "distinct_child":true,
            "ephemeral_process":true, "readonly_agent":true, "cleanup":"exact_child_delete"
        }),
    };
    json!({
        "type":"opened", "receipt_version":2, "ephemeral":true,
        "provider":provider, "parent_id":parent_id, "workstream_id":workstream_id,
        "child_id":child_id, "isolation":isolation,
        "receipt":{
            "receipt_version":2, "stage":"prepare", "delivery":"not_sent",
            "cleanup":"pending", "turn":0, "answers_received":0
        },
        "consultation_mode": if provider == Provider::Codex { "default" } else { "provider-native" },
        "model": if provider == Provider::Codex { "gpt-test" } else { "" },
        "effort": if provider == Provider::Codex { "low" } else { "" }
    })
}

fn snapshot(id: &str, session_id: &str) -> Value {
    let session_id = Uuid::parse_str(session_id)
        .map(|value| value.to_string())
        .unwrap_or_else(|_| CODEX_THREAD_ID.to_owned());
    json!({
        "type":"snapshot", "protocol":PROTOCOL_NAME, "version":PROTOCOL_VERSION,
        "node_id":id, "machine":"atlas", "captured_at":now(),
        "sessions":[session_to_wire(&session(Provider::Codex, &session_id, "remote-work"), false)],
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

struct SupersedingAddTransport {
    store: Store,
    node_id: String,
}

impl FleetTransport for &SupersedingAddTransport {
    fn request(
        &self,
        _target: &str,
        payload: &Value,
        _mutating: bool,
    ) -> Result<Value, FleetError> {
        match payload.get("op").and_then(Value::as_str) {
            Some("hello") => Ok(json!({
                "type":"hello", "protocol":PROTOCOL_NAME, "version":PROTOCOL_VERSION,
                "node_id":self.node_id, "machine":"atlas", "package_version":"0.6.0",
                "capabilities":CAPABILITIES,
            })),
            Some("snapshot") => {
                // Simulate a second add beginning while the first add waits on
                // its snapshot. The first response must not become trusted.
                self.store.claim_fleet_onboarding(&self.node_id).unwrap();
                Ok(snapshot(&self.node_id, CODEX_THREAD_ID))
            }
            operation => panic!("unexpected fake fleet operation: {operation:?}"),
        }
    }

    fn run_exact(
        &self,
        _node: &FleetNode,
        _arguments: &[String],
        _tty: bool,
    ) -> Result<i32, FleetError> {
        unreachable!("add does not run an exact command")
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

fn fake_process_guard() -> MutexGuard<'static, ()> {
    // Several fixtures exercise process-group termination. Keep those signals
    // from overlapping another shell-backed fixture's spawn inside this one
    // libtest process; the rest of the fleet contracts remain parallel.
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[test]
fn ssh_config_discovery_is_file_only_and_configured_hosts_rank_first() {
    let _process = fake_process_guard();
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
    let wire = session_to_wire(&session(Provider::Codex, "thread-id", "work"), false);
    assert_eq!(wire["exact_home"], true);
    let encoded = wire.to_string();
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
fn strict_snapshot_rejects_more_than_two_thousand_sessions() {
    let node_id = Uuid::new_v4().to_string();
    let sessions = (0..=MAX_SNAPSHOT_SESSIONS)
        .map(|index| {
            let session_id = format!("00000000-0000-4000-8000-{index:012x}");
            session_to_wire(
                &session(Provider::Codex, &session_id, "bounded-remote"),
                false,
            )
        })
        .collect::<Vec<_>>();
    let payload = json!({
        "type":"snapshot", "protocol":PROTOCOL_NAME, "version":PROTOCOL_VERSION,
        "node_id":node_id, "machine":"atlas", "captured_at":now(),
        "sessions":sessions, "profiles":[], "cards":[]
    });
    let error = validate_snapshot(&payload, Some(&node_id)).unwrap_err();
    assert_eq!(error.kind, FleetErrorKind::Incompatible);
    assert!(error.message.contains("safety limit"));
}

#[test]
fn sender_reserves_untracked_expert_inside_the_two_thousand_row_wire_bound() {
    let node_id = Uuid::new_v4().to_string();
    let watched = (0..MAX_SNAPSHOT_SESSIONS)
        .map(|index| {
            let session_id = format!("00000000-0000-4000-8000-{index:012x}");
            session_to_wire(&session(Provider::Codex, &session_id, "watched"), false)
        })
        .collect::<Vec<_>>();
    let expert_id = "ffffffff-ffff-4fff-8fff-ffffffffffff";
    let expert = session_to_wire(
        &session(Provider::Codex, expert_id, "untracked-expert"),
        true,
    );
    let payload = json!({
        "type":"snapshot", "protocol":PROTOCOL_NAME, "version":PROTOCOL_VERSION,
        "node_id":node_id, "machine":"atlas", "captured_at":now(),
        "sessions":watched,
        "expert_sessions":[expert],
        "profiles":[{
            "provider":"codex", "session_id":expert_id,
            "scope":"Owns needle infrastructure", "current_state":"shipping",
            "topics":["needle"], "artifacts":[], "updated_at":10.0,
            "source":"interview", "scope_updated_at":10.0,
            "current_state_updated_at":10.0
        }],
        "cards":[{
            "provider":"codex", "session_id":expert_id,
            "status":"CURRENT", "detail":"matches remote source",
            "watched":false, "availability":"source-available",
            "current_state_status":"CURRENT"
        }]
    });
    let bounded = bound_snapshot_for_transport(payload).unwrap();
    let validated = validate_snapshot(&bounded, Some(&node_id)).unwrap();
    assert_eq!(
        validated["sessions"].as_array().unwrap().len()
            + validated["expert_sessions"].as_array().unwrap().len(),
        MAX_SNAPSHOT_SESSIONS
    );
    assert_eq!(validated["expert_sessions"][0]["session_id"], expert_id);
    assert_eq!(
        validated["directory_notices"][0]["kind"],
        "snapshot-incomplete"
    );
    assert_eq!(validated["directory_notices"][0]["omitted_rows"], 1);
}

#[test]
fn sender_bounds_large_expert_profiles_without_rejecting_the_whole_snapshot() {
    let node_id = Uuid::new_v4().to_string();
    let large = "x".repeat(8_192);
    let mut expert_sessions = Vec::new();
    let mut profiles = Vec::new();
    let mut cards = Vec::new();
    for index in 0..6 {
        let session_id = format!("eeeeeeee-eeee-4eee-8eee-{index:012x}");
        expert_sessions.push(session_to_wire(
            &session(Provider::Codex, &session_id, "large-expert"),
            true,
        ));
        profiles.push(json!({
            "provider":"codex", "session_id":session_id,
            "scope":large, "current_state":large,
            "topics":vec![large.clone(); 64], "artifacts":vec![large.clone(); 64],
            "updated_at":10.0, "source":"interview",
            "scope_updated_at":10.0, "current_state_updated_at":10.0
        }));
        cards.push(json!({
            "provider":"codex", "session_id":session_id,
            "status":"CURRENT", "detail":"matches remote source",
            "watched":false, "availability":"source-available",
            "current_state_status":"CURRENT"
        }));
    }
    let bounded = bound_snapshot_for_transport(json!({
        "type":"snapshot", "protocol":PROTOCOL_NAME, "version":PROTOCOL_VERSION,
        "node_id":node_id, "machine":"atlas", "captured_at":now(),
        "sessions":[], "expert_sessions":expert_sessions,
        "profiles":profiles, "cards":cards
    }))
    .unwrap();
    assert!(serde_json::to_vec(&bounded).unwrap().len() <= pikamux::fleet::MAX_MESSAGE_BYTES);
    let validated = validate_snapshot(&bounded, Some(&node_id)).unwrap();
    assert!(validated["expert_sessions"].as_array().unwrap().len() < 6);
    assert_eq!(
        validated["directory_notices"][0]["kind"],
        "expert-directory-incomplete"
    );
    assert!(
        validated["directory_notices"][0]["omitted_rows"]
            .as_u64()
            .unwrap()
            > 0
    );
}

#[test]
fn cached_fleet_accepts_twenty_small_nodes_and_fairly_slices_aggregate_rows() {
    let temp = TempDir::new().unwrap();
    let store = initialized_store(&temp, "state.db");
    for index in 0..20 {
        let node_id = Uuid::new_v4().to_string();
        store
            .upsert_fleet_node(&node(&node_id, &format!("node-{index}")))
            .unwrap();
        store
            .put_remote_snapshot(&node_id, &snapshot(&node_id, CODEX_THREAD_ID), now())
            .unwrap();
    }
    assert_eq!(
        FleetManager::new(&store, &FakeTransport::default())
            .cached_sessions(None, false)
            .unwrap()
            .len(),
        20
    );

    let temp = TempDir::new().unwrap();
    let store = initialized_store(&temp, "amplified.db");
    let rows_per_node = (MAX_CACHED_FLEET_ROWS / 5) + 1;
    let mut node_ids = Vec::new();
    for node_index in 0..5 {
        let node_id = Uuid::new_v4().to_string();
        node_ids.push(node_id.clone());
        store
            .upsert_fleet_node(&node(&node_id, &format!("large-{node_index}")))
            .unwrap();
        let sessions = (0..rows_per_node)
            .map(|row_index| {
                let sequence = node_index * rows_per_node + row_index;
                let session_id = format!("00000000-0000-4000-8000-{sequence:012x}");
                session_to_wire(
                    &session(Provider::Codex, &session_id, "bounded-remote"),
                    false,
                )
            })
            .collect::<Vec<_>>();
        let payload = json!({
            "type":"snapshot", "protocol":PROTOCOL_NAME, "version":PROTOCOL_VERSION,
            "node_id":node_id, "machine":format!("large-{node_index}"),
            "captured_at":now(), "sessions":sessions, "profiles":[], "cards":[]
        });
        store
            .put_remote_snapshot(&node_id, &payload, now())
            .unwrap();
    }
    let fake = FakeTransport::default();
    let manager = FleetManager::new(&store, &fake);
    let cached = manager.cached_sessions_with_notices(None, false).unwrap();
    assert_eq!(cached.sessions.len(), MAX_CACHED_FLEET_ROWS);
    assert_eq!(cached.notices.len(), 5);
    for node_index in 0..5 {
        let alias = format!("large-{node_index}");
        assert_eq!(
            cached
                .sessions
                .iter()
                .filter(|session| session.node_name == alias)
                .count(),
            MAX_CACHED_FLEET_ROWS / 5
        );
        let notice = cached
            .notices
            .iter()
            .find(|notice| notice.node_name == alias)
            .unwrap();
        assert_eq!(notice.omitted_rows, 1);
        assert!(notice.message.contains("1 cached conversation"));
    }
    assert_eq!(
        manager
            .cached_sessions(Some(&node_ids[0]), false)
            .unwrap()
            .len(),
        rows_per_node,
        "exact-machine reads must not inherit the aggregate slice"
    );
}

#[test]
fn cached_fleet_twenty_heavy_nodes_parses_only_the_fair_prepaint_projection() {
    let temp = TempDir::new().unwrap();
    let store = initialized_store(&temp, "twenty-heavy.db");
    let mut node_ids = Vec::new();
    for node_index in 0..20 {
        let node_id = Uuid::new_v4().to_string();
        node_ids.push(node_id.clone());
        store
            .upsert_fleet_node(&node(&node_id, &format!("heavy-{node_index}")))
            .unwrap();
        let sessions = (0..MAX_SNAPSHOT_SESSIONS)
            .map(|row_index| {
                let sequence = node_index * MAX_SNAPSHOT_SESSIONS + row_index;
                let session_id = format!("00000000-0000-4000-8000-{sequence:012x}");
                session_to_wire(
                    &session(
                        Provider::Codex,
                        &session_id,
                        &format!("heavy-{node_index}-{}", "x".repeat(768)),
                    ),
                    false,
                )
            })
            .collect::<Vec<_>>();
        let captured_at = now();
        let payload = json!({
            "type":"snapshot", "protocol":PROTOCOL_NAME, "version":PROTOCOL_VERSION,
            "node_id":node_id, "machine":format!("heavy-{node_index}"),
            "captured_at":captured_at, "sessions":sessions, "profiles":[], "cards":[]
        });
        store
            .put_remote_snapshot(&node_id, &payload, captured_at)
            .unwrap();
    }

    let per_node_bytes = MAX_CACHED_FLEET_BYTES / node_ids.len();
    let per_node_rows = MAX_CACHED_FLEET_ROWS / node_ids.len();
    let mut parsed_projection_bytes = 0_usize;
    let mut authoritative_snapshot_bytes = 0_usize;
    for node_id in &node_ids {
        let projection = store
            .get_remote_board_projection(node_id, per_node_bytes, per_node_rows)
            .unwrap()
            .unwrap();
        assert_eq!(projection.rows.len(), per_node_rows);
        parsed_projection_bytes += projection.input_bytes;
        authoritative_snapshot_bytes += projection.source_encoded_bytes;
    }
    assert!(authoritative_snapshot_bytes > MAX_CACHED_FLEET_BYTES);
    assert!(parsed_projection_bytes <= MAX_CACHED_FLEET_BYTES);

    let fake = FakeTransport::default();
    let manager = FleetManager::new(&store, &fake);
    let started = Instant::now();
    let cached = manager.cached_sessions_with_notices(None, false).unwrap();
    let elapsed = started.elapsed();
    assert_eq!(cached.sessions.len(), MAX_CACHED_FLEET_ROWS);
    assert_eq!(cached.notices.len(), node_ids.len());
    for node_index in 0..20 {
        let alias = format!("heavy-{node_index}");
        assert_eq!(
            cached
                .sessions
                .iter()
                .filter(|session| session.node_name == alias)
                .count(),
            per_node_rows
        );
    }
    eprintln!(
        "20-heavy-node cached board parsed {parsed_projection_bytes} of {authoritative_snapshot_bytes} source bytes in {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "bounded cached first frame took {elapsed:?}"
    );
}

#[test]
fn sixty_four_dense_nodes_each_contribute_with_bounded_board_and_expert_input() {
    let temp = TempDir::new().unwrap();
    let store = initialized_store(&temp, "sixty-four-dense.db");
    let captured_at = now();
    let mut node_ids = Vec::new();
    for node_index in 0..MAX_CACHED_FLEET_NODES {
        let node_id = Uuid::new_v4().to_string();
        node_ids.push(node_id.clone());
        store
            .upsert_fleet_node(&node(&node_id, &format!("dense-{node_index}")))
            .unwrap();
        let mut sessions = Vec::with_capacity(MAX_SNAPSHOT_SESSIONS);
        let mut profiles = Vec::with_capacity(MAX_SNAPSHOT_SESSIONS);
        for row_index in 0..MAX_SNAPSHOT_SESSIONS {
            let sequence = node_index * MAX_SNAPSHOT_SESSIONS + row_index;
            let identity = format!("00000000-0000-4000-8000-{sequence:012x}");
            sessions.push(session_to_wire(
                &session(Provider::Codex, &identity, "dense-worker"),
                false,
            ));
            profiles.push(json!({
                "provider":"codex", "session_id":identity,
                "scope":"needle", "current_state":"working",
                "topics":[], "artifacts":[], "updated_at":captured_at,
                "source":"fixture", "scope_updated_at":captured_at,
                "current_state_updated_at":captured_at
            }));
        }
        let payload = json!({
            "type":"snapshot", "protocol":PROTOCOL_NAME, "version":PROTOCOL_VERSION,
            "node_id":node_id, "machine":format!("dense-{node_index}"),
            "captured_at":captured_at, "sessions":sessions,
            "profiles":profiles, "cards":[]
        });
        store
            .put_remote_snapshot(&node_id, &payload, captured_at)
            .unwrap();
    }

    let per_node_bytes = MAX_CACHED_FLEET_BYTES / MAX_CACHED_FLEET_NODES;
    let per_node_rows = MAX_CACHED_FLEET_ROWS / MAX_CACHED_FLEET_NODES;
    let mut board_input = 0_usize;
    let mut expert_input = 0_usize;
    for node_id in &node_ids {
        let board = store
            .get_remote_board_projection(node_id, per_node_bytes, per_node_rows)
            .unwrap()
            .unwrap();
        let expert = store
            .get_remote_expert_projection(node_id, per_node_bytes, per_node_rows)
            .unwrap()
            .unwrap();
        assert!(!board.rows.is_empty());
        assert!(!expert.rows.is_empty());
        board_input += board.input_bytes;
        expert_input += expert.input_bytes;
    }
    assert!(board_input <= MAX_CACHED_FLEET_BYTES);
    assert!(expert_input <= MAX_CACHED_FLEET_BYTES);

    let transport = FakeTransport::default();
    let manager = FleetManager::new(&store, &transport);
    let started = Instant::now();
    let board = manager.cached_sessions_with_notices(None, false).unwrap();
    let experts = manager.expert_directory("needle").unwrap();
    assert!(started.elapsed() < Duration::from_secs(5));
    for node_index in 0..MAX_CACHED_FLEET_NODES {
        let alias = format!("dense-{node_index}");
        assert!(board.sessions.iter().any(|row| row.node_name == alias));
        assert!(
            experts
                .matches
                .iter()
                .any(|row| row.machine.as_deref() == Some(alias.as_str()))
        );
    }
}

#[test]
fn remote_name_resolution_exposes_exact_candidates_and_error_routes() {
    let temp = TempDir::new().unwrap();
    let store = initialized_store(&temp, "resolve-candidates.db");
    let node_id = Uuid::new_v4().to_string();
    store.upsert_fleet_node(&node(&node_id, "atlas")).unwrap();
    let mut payload = snapshot(&node_id, CODEX_THREAD_ID);
    payload["sessions"] = json!([
        session_to_wire(
            &session(Provider::Codex, CODEX_THREAD_ID, "same-name"),
            false
        ),
        session_to_wire(
            &session(Provider::Claude, CLAUDE_THREAD_ID, "same-name"),
            false
        ),
    ]);
    store
        .put_remote_snapshot(&node_id, &payload, now())
        .unwrap();
    let fake = FakeTransport::default();
    let manager = FleetManager::new(&store, &fake);

    let candidates = manager
        .resolve_candidates("same-name@atlas", false, false)
        .unwrap()
        .unwrap();
    assert_eq!(candidates.len(), 2);
    let error = manager
        .resolve("same-name@atlas", false, false)
        .unwrap_err();
    assert_eq!(error.kind, FleetErrorKind::InvalidRequest);
    assert!(error.message.contains(&format!("{CODEX_THREAD_ID}@atlas")));
    assert!(error.message.contains(&format!("{CLAUDE_THREAD_ID}@atlas")));
    assert!(!error.message.contains("codex:"));
    assert!(!error.message.contains("claude:"));
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
fn stale_add_response_cannot_trust_a_node_after_a_newer_add_claim() {
    let temp = TempDir::new().unwrap();
    let store = initialized_store(&temp, "state.db");
    let remote = Uuid::new_v4().to_string();
    let transport = SupersedingAddTransport {
        store: store.clone(),
        node_id: remote,
    };
    let candidate = NodeCandidate {
        alias: "atlas".into(),
        ssh_target: "atlas".into(),
        sources: vec!["explicit".into()],
        hostname: None,
        online: None,
        os_name: None,
    };
    let error = FleetManager::new(&store, &transport)
        .add(&candidate, None)
        .unwrap_err();
    assert!(error.message.contains("superseded"));
    assert!(store.list_nodes().unwrap().is_empty());
}

#[test]
fn intentional_refresh_cancellation_preserves_ready_node_and_cached_snapshot() {
    let temp = TempDir::new().unwrap();
    let store = initialized_store(&temp, "state.db");
    let remote = Uuid::new_v4().to_string();
    store.upsert_fleet_node(&node(&remote, "atlas")).unwrap();
    let original = snapshot(&remote, CODEX_THREAD_ID);
    store
        .put_remote_snapshot(&remote, &original, now())
        .unwrap();

    let cancellation = CancellationToken::default();
    cancellation.cancel();
    let error = FleetManager::new(&store, &FakeTransport::default())
        .refresh_node_cancellable(&remote, &cancellation)
        .unwrap_err();
    assert_eq!(error.kind, FleetErrorKind::Unreachable);

    let retained = store.get_fleet_node(&remote).unwrap().unwrap();
    assert_eq!(retained.status, "ready");
    assert_eq!(retained.last_error, None);
    assert_eq!(
        store.get_remote_snapshot(&remote).unwrap().unwrap().payload,
        original
    );
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
fn snapshot_notice_field_is_sent_only_to_capable_nodes() {
    for supports_notices in [false, true] {
        let temp = TempDir::new().unwrap();
        let store = initialized_store(&temp, "notice-negotiation.db");
        let remote = Uuid::new_v4().to_string();
        let mut remote_node = node(&remote, "atlas");
        if !supports_notices {
            remote_node
                .capabilities
                .retain(|capability| capability != "expert-directory-notices-v1");
        }
        store.upsert_fleet_node(&remote_node).unwrap();
        let fake = FakeTransport::with(vec![Ok(snapshot(&remote, CODEX_THREAD_ID))]);
        FleetManager::new(&store, &fake)
            .refresh_node(&remote)
            .unwrap();
        let requests = fake.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0]
                .1
                .get("directory_notices")
                .and_then(Value::as_bool),
            supports_notices.then_some(true)
        );
    }
}

#[test]
fn remote_acknowledgement_request_carries_the_selected_event() {
    let temp = TempDir::new().unwrap();
    let store = initialized_store(&temp, "state.db");
    let remote = Uuid::new_v4().to_string();
    let mut cached_node = node(&remote, "atlas");
    cached_node
        .capabilities
        .retain(|capability| capability != "ack-event-bound-v1");
    store.upsert_fleet_node(&cached_node).unwrap();
    store
        .put_remote_snapshot(&remote, &snapshot(&remote, "thread"), now())
        .unwrap();
    let selected = FleetManager::new(&store, &FakeTransport::default())
        .cached_sessions(Some(&remote), false)
        .unwrap()
        .pop()
        .unwrap();
    let fake = FakeTransport::with(vec![
        Ok(json!({
            "type":"hello", "protocol":PROTOCOL_NAME, "version":PROTOCOL_VERSION,
            "node_id":remote, "machine":"atlas", "package_version":"0.6.0",
            "capabilities":CAPABILITIES,
        })),
        Ok(json!({
            "type":"acknowledged", "node_id":remote, "acknowledged":true,
        })),
    ]);
    assert!(
        FleetManager::new(&store, &fake)
            .acknowledge(&selected)
            .unwrap()
    );
    let requests = fake.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(!requests[0].2);
    assert!(requests[1].2);
    assert_eq!(requests[1].1["expected_last_event_at"], 9.0);
}

#[test]
fn remote_clock_skew_does_not_control_local_cache_age_or_polling() {
    for offset in [-3600.0, 3600.0] {
        let temp = TempDir::new().unwrap();
        let store = initialized_store(&temp, "state.db");
        let remote = Uuid::new_v4().to_string();
        let mut source = snapshot(&remote, "thread");
        source["captured_at"] = json!(now() + offset);
        let remote_capture = source["captured_at"].clone();
        let hello = json!({
            "type":"hello", "protocol":PROTOCOL_NAME, "version":PROTOCOL_VERSION,
            "node_id":remote, "machine":"atlas", "package_version":"0.6.0",
            "capabilities":CAPABILITIES,
        });
        let fake = FakeTransport::with(vec![Ok(hello), Ok(source.clone()), Ok(source)]);
        let manager = FleetManager::new(&store, &fake);
        let candidate = NodeCandidate {
            alias: "atlas".into(),
            ssh_target: "atlas".into(),
            sources: vec!["explicit".into()],
            hostname: None,
            online: None,
            os_name: None,
        };
        let before = now();
        manager.add(&candidate, None).unwrap();
        for refresh in [false, true] {
            if refresh {
                manager.refresh_node(&remote).unwrap();
            }
            let after = now();
            let stored = store.get_remote_snapshot(&remote).unwrap().unwrap();
            assert_eq!(stored.payload["captured_at"], remote_capture);
            assert!((before..=after).contains(&stored.captured_at));
            let node = store.get_fleet_node(&remote).unwrap().unwrap();
            assert_eq!(node.last_attempt_at, stored.captured_at);
            assert_eq!(node.last_seen, stored.captured_at);
            assert!(!manager.cached_sessions(Some(&remote), false).unwrap()[0].stale);
            let nodes = [node];
            assert!(next_remote_node(&nodes, None, stored.captured_at + 14.0, false).is_none());
            assert!(next_remote_node(&nodes, None, stored.captured_at + 16.0, false).is_some());
        }
        let payload = store.get_remote_snapshot(&remote).unwrap().unwrap().payload;
        store
            .put_remote_snapshot(&remote, &payload, now() - 46.0)
            .unwrap();
        assert!(manager.cached_sessions(Some(&remote), false).unwrap()[0].stale);
        // Legacy versions stored a remote capture clock in these local fields.
        // An existing future entry must be stale and immediately refreshable.
        store
            .put_remote_snapshot(&remote, &payload, now() + 3600.0)
            .unwrap();
        assert!(manager.cached_sessions(Some(&remote), false).unwrap()[0].stale);
        let mut normal_due = node(&Uuid::new_v4().to_string(), "normal-due");
        normal_due.last_attempt_at = now() - 60.0;
        let nodes = [normal_due, store.get_fleet_node(&remote).unwrap().unwrap()];
        assert_eq!(
            next_remote_node(&nodes, None, now(), false)
                .unwrap()
                .node_id,
            remote,
            "legacy future entries must not starve behind continuously due nodes"
        );
    }
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
fn attach_ignores_reassigned_display_alias_and_uses_immutable_node_id() {
    let temp = TempDir::new().unwrap();
    let store = initialized_store(&temp, "state.db");
    let original_id = Uuid::new_v4().to_string();
    let replacement_id = Uuid::new_v4().to_string();
    let thread = "11111111-1111-4111-8111-111111111111";
    store
        .upsert_fleet_node(&node(&original_id, "atlas"))
        .unwrap();
    store
        .put_remote_snapshot(&original_id, &snapshot(&original_id, thread), now())
        .unwrap();
    let selected = FleetManager::new(&store, &FakeTransport::default())
        .cached_sessions(None, false)
        .unwrap()
        .remove(0);

    let mut renamed = node(&original_id, "atlas-renamed");
    renamed.ssh_target = "original-route".into();
    store.upsert_fleet_node(&renamed).unwrap();
    store
        .upsert_fleet_node(&node(&replacement_id, "atlas"))
        .unwrap();

    let fake = FakeTransport::with(vec![Ok(snapshot(&original_id, thread))]);
    assert_eq!(
        FleetManager::new(&store, &fake).attach(&selected).unwrap(),
        0
    );
    let calls = fake.exact.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, original_id);
}

#[test]
fn attach_fails_closed_when_immutable_node_was_deleted() {
    let temp = TempDir::new().unwrap();
    let store = initialized_store(&temp, "state.db");
    let node_id = Uuid::new_v4().to_string();
    let thread = "11111111-1111-4111-8111-111111111111";
    store.upsert_fleet_node(&node(&node_id, "atlas")).unwrap();
    store
        .put_remote_snapshot(&node_id, &snapshot(&node_id, thread), now())
        .unwrap();
    let selected = FleetManager::new(&store, &FakeTransport::default())
        .cached_sessions(None, false)
        .unwrap()
        .remove(0);
    store.delete_fleet_node(&node_id).unwrap();
    let fake = FakeTransport::default();
    let error = FleetManager::new(&store, &fake)
        .attach(&selected)
        .unwrap_err();
    assert_eq!(error.kind, FleetErrorKind::NotFound);
    assert!(fake.exact.lock().unwrap().is_empty());
}

struct RouteReplacingTransport<'a> {
    store: &'a Store,
    response: Value,
    exact: Mutex<Vec<String>>,
}

impl FleetTransport for &RouteReplacingTransport<'_> {
    fn request(
        &self,
        _target: &str,
        _payload: &Value,
        _mutating: bool,
    ) -> Result<Value, FleetError> {
        let node_id = self.response["node_id"].as_str().unwrap();
        let mut replacement = node(node_id, "atlas");
        replacement.ssh_target = "concurrently-replaced-route".into();
        self.store.upsert_fleet_node(&replacement).unwrap();
        Ok(self.response.clone())
    }

    fn run_exact(
        &self,
        node: &FleetNode,
        _arguments: &[String],
        _tty: bool,
    ) -> Result<i32, FleetError> {
        self.exact.lock().unwrap().push(node.ssh_target.clone());
        Ok(0)
    }
}

#[test]
fn attach_fails_closed_when_route_is_replaced_during_refresh() {
    let temp = TempDir::new().unwrap();
    let store = initialized_store(&temp, "state.db");
    let node_id = Uuid::new_v4().to_string();
    let thread = "11111111-1111-4111-8111-111111111111";
    store.upsert_fleet_node(&node(&node_id, "atlas")).unwrap();
    store
        .put_remote_snapshot(&node_id, &snapshot(&node_id, thread), now())
        .unwrap();
    let selected = FleetManager::new(&store, &FakeTransport::default())
        .cached_sessions(None, false)
        .unwrap()
        .remove(0);
    let transport = RouteReplacingTransport {
        store: &store,
        response: snapshot(&node_id, thread),
        exact: Mutex::new(Vec::new()),
    };
    let error = FleetManager::new(&store, &transport)
        .attach(&selected)
        .unwrap_err();
    assert_eq!(error.kind, FleetErrorKind::Quarantined);
    assert!(error.message.contains("route changed"));
    assert!(transport.exact.lock().unwrap().is_empty());
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
    current_event_at: f64,
    acknowledgement_events: Vec<f64>,
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
    fn acknowledge(
        &mut self,
        _provider: Provider,
        _id: &str,
        expected_last_event_at: f64,
    ) -> Result<bool, FleetError> {
        self.acknowledgement_events.push(expected_last_event_at);
        Ok(self.current_event_at == expected_last_event_at)
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
        "expected_node_id":node_id, "provider":"claude", "session_id":CLAUDE_THREAD_ID,
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
fn remote_acknowledgement_is_bound_to_the_observed_event() {
    let temp = TempDir::new().unwrap();
    let store = initialized_store(&temp, "state.db");
    let node_id = store.ensure_local_node_id().unwrap();
    let request = json!({
        "op":"acknowledge", "protocol":PROTOCOL_NAME, "version":PROTOCOL_VERSION,
        "expected_node_id":node_id, "provider":"codex", "session_id":CODEX_THREAD_ID,
        "expected_last_event_at":10.0,
    });
    let mut output = Vec::new();
    let mut service = Service {
        current_event_at: 20.0,
        ..Service::default()
    };
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
    assert_eq!(value["acknowledged"], false);
    assert_eq!(service.acknowledgement_events, [10.0]);
}

#[test]
fn remote_acknowledgement_rejects_a_missing_event_without_service_action() {
    let temp = TempDir::new().unwrap();
    let store = initialized_store(&temp, "state.db");
    let node_id = store.ensure_local_node_id().unwrap();
    let request = json!({
        "op":"acknowledge", "protocol":PROTOCOL_NAME, "version":PROTOCOL_VERSION,
        "expected_node_id":node_id, "provider":"codex", "session_id":CODEX_THREAD_ID,
    });
    let mut output = Vec::new();
    let mut service = Service {
        current_event_at: 20.0,
        ..Service::default()
    };
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
    assert_eq!(value["kind"], "incompatible");
    assert!(service.acknowledgement_events.is_empty());
}

#[test]
fn ssh_payload_stays_on_stdin_and_remote_argv_is_fixed() {
    let _process = fake_process_guard();
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
    let _process = fake_process_guard();
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
fn snapshot_request_cancellation_joins_and_kills_owned_descendants() {
    let _process = fake_process_guard();
    let temp = TempDir::new().unwrap();
    let fake = temp.path().join("ssh");
    let owned_pid = temp.path().join("snapshot-child.pid");
    executable(
        &fake,
        &format!(
            "sleep 30 &\nprintf '%s' \"$!\" > '{}'\nwait",
            owned_pid.display()
        ),
    );
    let transport = SshTransport::new(&fake, Duration::from_secs(1), Duration::from_secs(30));
    let cancellation = CancellationToken::default();
    let worker_token = cancellation.clone();
    let worker = std::thread::spawn(move || {
        transport.request_cancellable("atlas", &json!({"op":"snapshot"}), false, &worker_token)
    });
    // Process creation can be delayed on loaded CI hosts; this only waits for
    // the fixture to exist and does not weaken the cancellation deadline below.
    for _ in 0..500 {
        if owned_pid.is_file() {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(owned_pid.is_file(), "fake SSH descendant never started");
    let cancelled_at = std::time::Instant::now();
    cancellation.cancel();
    let error = worker.join().unwrap().unwrap_err();
    assert_eq!(error.kind, FleetErrorKind::Unreachable);
    assert!(cancelled_at.elapsed() < Duration::from_secs(1));

    let pid: i32 = fs::read_to_string(&owned_pid).unwrap().parse().unwrap();
    // A killed grandchild may briefly remain as an init-owned zombie. Two
    // seconds still distinguishes cleanup from the fixture's 30-second sleep.
    for _ in 0..200 {
        // SAFETY: signal zero only probes the exact disposable fixture descendant.
        if unsafe { libc::kill(pid, 0) } != 0 {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("owned snapshot descendant {pid} survived cancellation");
}

#[test]
fn remote_consultation_reuses_one_connection_and_requires_v2_cleanup() {
    let _process = fake_process_guard();
    let temp = TempDir::new().unwrap();
    let fake = temp.path().join("ssh");
    let opened = opened_receipt(CODEX_PARENT_ID, CODEX_PARENT_ID);
    let body = format!(
        "printf '%s\\n' '{opened}'\nwhile IFS= read -r line; do\n case \"$line\" in\n *\\\"close\\\"*) printf '%s\\n' '{{\"type\":\"closed\",\"receipt_version\":2,\"discarded\":true,\"cleanup\":\"complete\"}}'; exit 0 ;;\n *) printf '%s\\n' '{{\"type\":\"answer\",\"text\":\"from remote expert\"}}' ;;\n esac\ndone"
    );
    executable(&fake, &body);
    let node_id = Uuid::new_v4().to_string();
    let trusted = node(&node_id, "atlas");
    let remote = FleetSession {
        node_id: node_id.clone(),
        node_name: "atlas".into(),
        session: session(Provider::Codex, CODEX_PARENT_ID, "expert"),
        stale: false,
        remote_error: None,
        seen_at: now(),
        card_status: None,
        card_detail: None,
        watched: true,
        availability: Some("source-available".into()),
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
    assert_eq!(side.opening_receipt().receipt_version, 2);
    assert!(side.opening_receipt().ephemeral);
    assert_eq!(
        side.opening_receipt().child_id.as_deref(),
        Some(CODEX_CHILD_ID)
    );
    assert_eq!(side.ask("first").unwrap(), "from remote expert");
    assert_eq!(side.ask("follow up").unwrap(), "from remote expert");
    let receipt = side.close().unwrap();
    assert_eq!(receipt.answers_received, Some(2));
    assert_eq!(receipt.cleanup.as_deref(), Some("complete"));
}

#[test]
fn remote_opening_requires_v2_ephemeral_exact_child_and_provider_isolation() {
    let _process = fake_process_guard();
    let temp = TempDir::new().unwrap();
    let node_id = Uuid::new_v4().to_string();
    let remote = FleetSession {
        node_id: node_id.clone(),
        node_name: "atlas".into(),
        session: session(Provider::Codex, CODEX_PARENT_ID, "expert"),
        stale: false,
        remote_error: None,
        seen_at: now(),
        card_status: None,
        card_detail: None,
        watched: true,
        availability: Some("source-available".into()),
        scope_updated_at: None,
        current_state_updated_at: None,
        current_state_status: None,
    };
    let mut invalid = Vec::new();
    let mut old = opened_receipt(CODEX_PARENT_ID, CODEX_PARENT_ID);
    old["receipt_version"] = json!(1);
    invalid.push(old);
    let mut persistent = opened_receipt(CODEX_PARENT_ID, CODEX_PARENT_ID);
    persistent["ephemeral"] = json!(false);
    invalid.push(persistent);
    let mut same_child = opened_receipt(CODEX_PARENT_ID, CODEX_PARENT_ID);
    same_child["child_id"] = json!(CODEX_PARENT_ID);
    invalid.push(same_child);
    let mut weak = opened_receipt(CODEX_PARENT_ID, CODEX_PARENT_ID);
    weak["isolation"]["sandbox"] = json!("workspace-write");
    invalid.push(weak);

    for (index, opened) in invalid.into_iter().enumerate() {
        let fake = temp.path().join(format!("invalid-open-{index}"));
        executable(&fake, &format!("printf '%s\\n' '{opened}'\nsleep 5"));
        let error = RemoteConsultation::open(
            &SshTransport::new(&fake, Duration::from_secs(1), Duration::from_secs(1)),
            node(&node_id, "atlas"),
            remote.clone(),
            ConsultationPolicy {
                consultation_mode: "default".into(),
                model: "gpt-test".into(),
                effort: "low".into(),
            },
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::from_secs(1),
        )
        .err()
        .expect("unproved opening must be quarantined");
        assert_eq!(error.kind, FleetErrorKind::Quarantined);
        assert!(error.message.contains("no question was sent"));
    }
}

#[test]
fn remote_opening_validates_claude_flags_and_opencode_exact_child() {
    let _process = fake_process_guard();
    let temp = TempDir::new().unwrap();
    for (index, provider, parent, workstream, child) in [
        (
            0,
            Provider::Claude,
            CLAUDE_THREAD_ID,
            CLAUDE_THREAD_ID,
            None,
        ),
        (
            1,
            Provider::Opencode,
            "ses_parent123",
            "ses_parent123",
            Some("ses_child123"),
        ),
    ] {
        let node_id = Uuid::new_v4().to_string();
        let fake = temp.path().join(format!("provider-open-{index}"));
        let opened = provider_opened_receipt(provider, parent, workstream, child);
        executable(
            &fake,
            &format!(
                "printf '%s\\n' '{opened}'\nIFS= read -r line\nprintf '%s\\n' '{{\"type\":\"closed\",\"receipt_version\":2,\"discarded\":true,\"cleanup\":\"complete\"}}'"
            ),
        );
        let remote = FleetSession {
            node_id: node_id.clone(),
            node_name: "atlas".into(),
            session: session(provider, workstream, "expert"),
            stale: false,
            remote_error: None,
            seen_at: now(),
            card_status: None,
            card_detail: None,
            watched: true,
            availability: Some("source-available".into()),
            scope_updated_at: None,
            current_state_updated_at: None,
            current_state_status: None,
        };
        let mut side = RemoteConsultation::open(
            &SshTransport::new(&fake, Duration::from_secs(1), Duration::from_secs(1)),
            node(&node_id, "atlas"),
            remote,
            ConsultationPolicy {
                consultation_mode: "provider-native".into(),
                model: String::new(),
                effort: String::new(),
            },
            Duration::from_secs(1),
            Duration::from_secs(1),
            Duration::from_secs(1),
        )
        .unwrap();
        assert_eq!(side.opening_receipt().child_id.as_deref(), child);
        side.close().unwrap();
    }
}

#[test]
fn remote_consultation_cancellation_kills_owned_transport_promptly() {
    let _process = fake_process_guard();
    let temp = TempDir::new().unwrap();
    let fake = temp.path().join("ssh");
    let owned_pid = temp.path().join("owned.pid");
    executable(
        &fake,
        &format!(
            "printf '%s\\n' '{}'\nIFS= read -r line\nsleep 30 &\nprintf '%s' \"$!\" > '{}'\nwait",
            opened_receipt(CODEX_PARENT_ID, CODEX_PARENT_ID),
            owned_pid.display()
        ),
    );
    let node_id = Uuid::new_v4().to_string();
    let trusted = node(&node_id, "atlas");
    let remote = FleetSession {
        node_id,
        node_name: "atlas".into(),
        session: session(Provider::Codex, CODEX_PARENT_ID, "expert"),
        stale: false,
        remote_error: None,
        seen_at: now(),
        card_status: None,
        card_detail: None,
        watched: true,
        availability: Some("source-available".into()),
        scope_updated_at: None,
        current_state_updated_at: None,
        current_state_status: None,
    };
    let policy = ConsultationPolicy {
        consultation_mode: "default".into(),
        model: "gpt-test".into(),
        effort: "low".into(),
    };
    let cancellation = CancellationToken::default();
    let transport = SshTransport::new(&fake, Duration::from_secs(1), Duration::from_secs(1));
    let mut side = RemoteConsultation::open_cancellable(
        &transport,
        trusted,
        remote,
        policy,
        ConsultationTimeouts {
            open: Duration::from_secs(1),
            event: Duration::from_secs(30),
            cleanup: Duration::from_secs(1),
        },
        cancellation.clone(),
    )
    .unwrap();
    let worker = std::thread::spawn(move || side.ask("one exact question"));
    for _ in 0..100 {
        if owned_pid.is_file() {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    let cancelled_at = std::time::Instant::now();
    cancellation.cancel();
    let error = worker.join().unwrap().unwrap_err();
    assert_eq!(error.kind, FleetErrorKind::OutcomeUnknown);
    assert_eq!(
        error
            .receipt
            .as_ref()
            .and_then(|value| value.cleanup.as_deref()),
        Some("unknown")
    );
    assert!(cancelled_at.elapsed() < Duration::from_secs(1));
    let pid: i32 = fs::read_to_string(owned_pid).unwrap().parse().unwrap();
    for _ in 0..50 {
        // SAFETY: signal zero only probes the exact fixture descendant.
        if unsafe { libc::kill(pid, 0) } != 0 {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("owned remote transport descendant {pid} survived cancellation");
}

#[test]
fn remote_consultation_refuses_wrong_leaf_before_sending_question() {
    let _process = fake_process_guard();
    let temp = TempDir::new().unwrap();
    let fake = temp.path().join("ssh");
    executable(
        &fake,
        &format!(
            "printf '%s\\n' '{}'",
            opened_receipt(CODEX_OTHER_ID, CODEX_PARENT_ID)
        ),
    );
    let node_id = Uuid::new_v4().to_string();
    let trusted = node(&node_id, "atlas");
    let mut base = session(Provider::Codex, CODEX_PARENT_ID, "expert");
    base.active_thread_id = Some(CODEX_ACTIVE_ID.to_owned());
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
        availability: Some("source-available".into()),
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
        Duration::from_secs(3),
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
    let _process = fake_process_guard();
    let temp = TempDir::new().unwrap();
    let node_id = Uuid::new_v4().to_string();
    let remote = FleetSession {
        node_id: node_id.clone(),
        node_name: "atlas".into(),
        session: session(Provider::Codex, CODEX_PARENT_ID, "expert"),
        stale: false,
        remote_error: None,
        seen_at: now(),
        card_status: None,
        card_detail: None,
        watched: true,
        availability: Some("source-available".into()),
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
    let opened = opened_receipt(CODEX_PARENT_ID, CODEX_PARENT_ID);
    let body = format!(
        "printf '%s\\n' '{opened}'\nwhile IFS= read -r line; do\n case \"$line\" in\n *\\\"close\\\"*) printf '%s\\n' '{{\"type\":\"closed\",\"discarded\":true}}'; exit 0 ;;\n *) printf '%s\\n' '{{\"type\":\"answer\",\"text\":\"useful\"}}' ;;\n esac\ndone"
    );
    executable(&bad_cleanup, &body);
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
