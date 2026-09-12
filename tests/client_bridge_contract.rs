use pikamux::client_bridge::{
    BRIDGE_PROTOCOL, BRIDGE_VERSION, ClientBridgeError, ClientBridgeErrorKind,
    ClientBridgeTransport, ClientConfig, ClientLaunchBridge, ClientWindowOutcome, LoopbackEndpoint,
    MAX_BRIDGE_MESSAGE_BYTES, PairReceipt, WindowLauncher, accept_pairing, bind_client_bridge,
    generate_pairing_token, install_client_pairing, make_launch_request, make_pair_request,
    read_bridge_message, request_client_launch, route_client_window, serve_client_bridge_once,
    validate_launch_request, validate_pair_request, validate_pairing_hello,
    windows_terminal_command,
};
use pikamux::fleet::{CAPABILITIES, PROTOCOL_NAME, PROTOCOL_VERSION};
use pikamux::model::Provider;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::io::Cursor;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use uuid::Uuid;

const SOURCE_TOKEN: &str = "abababababababababababababababababababababababababababababababab";
const TARGET_TOKEN: &str = "cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd";

fn ids() -> (String, String, String, String) {
    (
        Uuid::new_v4().to_string(),
        Uuid::new_v4().to_string(),
        Uuid::new_v4().to_string(),
        Uuid::new_v4().to_string(),
    )
}

fn config(client_id: &str, source_id: &str, target_id: &str) -> ClientConfig {
    let mut nodes = serde_json::Map::new();
    nodes.insert(
        source_id.to_owned(),
        json!({
            "alias": "devbox",
            "ssh_target": "developer@devbox",
            "token": SOURCE_TOKEN,
            "remote_port": 49000,
        }),
    );
    nodes.insert(
        target_id.to_owned(),
        json!({
            "alias": "gpu-box",
            "ssh_target": "developer@gpu-box",
            "token": TARGET_TOKEN,
        }),
    );
    ClientConfig::from_value(&json!({
        "version": 1,
        "client_id": client_id,
        "nodes": nodes,
    }))
    .unwrap()
}

#[derive(Clone, Default)]
struct FakeLauncher {
    launched: Arc<Mutex<Vec<Vec<String>>>>,
}

impl WindowLauncher for FakeLauncher {
    fn launch(&mut self, argv: &[String]) -> Result<(), ClientBridgeError> {
        self.launched.lock().unwrap().push(argv.to_vec());
        Ok(())
    }
}

#[derive(Default)]
struct FakeTransport {
    responses: VecDeque<Result<Value, ClientBridgeError>>,
    requests: Vec<(LoopbackEndpoint, Value)>,
}

impl ClientBridgeTransport for FakeTransport {
    fn exchange(
        &mut self,
        endpoint: &LoopbackEndpoint,
        request: &Value,
    ) -> Result<Value, ClientBridgeError> {
        self.requests.push((endpoint.clone(), request.clone()));
        self.responses.pop_front().expect("fake response")
    }
}

#[test]
fn pairing_uses_authoritative_fleet_v2_and_strict_envelopes() {
    assert_eq!(BRIDGE_VERSION, 2);
    assert_eq!(PROTOCOL_VERSION, 2);
    let node_id = Uuid::new_v4().to_string();
    let hello = json!({
        "type": "hello",
        "protocol": PROTOCOL_NAME,
        "version": PROTOCOL_VERSION,
        "node_id": node_id,
        "machine": "devbox",
        "package_version": "0.6.0-alpha.1",
        "capabilities": CAPABILITIES,
    });
    let validated = validate_pairing_hello(&hello).unwrap();
    assert_eq!(validated.node_id, node_id);

    let mut stale = hello.clone();
    stale["version"] = json!(1);
    let error = validate_pairing_hello(&stale).unwrap_err();
    assert_eq!(error.kind, ClientBridgeErrorKind::Incompatible);

    let mut additive = hello;
    additive["claim"] = json!("native-windows-host");
    assert!(validate_pairing_hello(&additive).is_err());
}

#[test]
fn pairing_request_requires_exact_protocol_ids_secret_and_fields() {
    let (client_id, node_id, _, _) = ids();
    let request =
        make_pair_request(&node_id, &client_id, "my-laptop", SOURCE_TOKEN, 49_000).unwrap();
    assert_eq!(request.version, 2);
    assert_eq!(request.protocol, BRIDGE_PROTOCOL);
    assert_eq!(request.token, SOURCE_TOKEN);

    let mut extra = serde_json::to_value(request).unwrap();
    extra["native_hosting"] = json!(true);
    assert!(validate_pair_request(&extra).is_err());
    extra.as_object_mut().unwrap().remove("native_hosting");
    extra["expected_node_id"] = json!("not-a-uuid");
    assert!(validate_pair_request(&extra).is_err());
    extra["expected_node_id"] = json!(node_id);
    extra["token"] = json!("short");
    assert!(validate_pair_request(&extra).is_err());
}

#[test]
fn re_pairing_rotates_the_server_and_client_secret_only_after_exact_receipt() {
    let (client_id, node_id, _, _) = ids();
    let request = make_pair_request(&node_id, &client_id, "laptop", SOURCE_TOKEN, 49_000).unwrap();
    let old_token = "ef".repeat(32);
    let existing = vec![json!({
        "client_id": client_id,
        "label": "laptop",
        "host": "127.0.0.1",
        "port": 49000,
        "token": old_token,
        "timeout": 0.35,
        "enabled": true,
    })];
    let (routes, receipt) =
        accept_pairing(&existing, &node_id, &serde_json::to_value(request).unwrap()).unwrap();
    assert_eq!(routes.len(), 1);
    assert_eq!(routes[0]["token"], SOURCE_TOKEN);

    let mut client = ClientConfig::empty();
    client.client_id = client_id.clone();
    let wrong_receipt = PairReceipt {
        node_id: Uuid::new_v4().to_string(),
        ..receipt.clone()
    };
    assert!(
        install_client_pairing(
            &mut client,
            &node_id,
            "devbox",
            "devbox",
            SOURCE_TOKEN,
            49_000,
            &serde_json::to_value(wrong_receipt).unwrap(),
        )
        .is_err()
    );
    assert!(client.nodes.is_empty());

    install_client_pairing(
        &mut client,
        &node_id,
        "devbox",
        "devbox",
        SOURCE_TOKEN,
        49_000,
        &serde_json::to_value(receipt).unwrap(),
    )
    .unwrap();
    assert_eq!(client.nodes[&node_id].token, SOURCE_TOKEN);
    let generated = generate_pairing_token();
    assert_eq!(generated.len(), 64);
    assert!(generated.bytes().all(|byte| byte.is_ascii_hexdigit()));
}

#[test]
fn terminal_command_is_argv_only_and_pins_node_provider_and_conversation() {
    let (client_id, source_id, target_id, session_id) = ids();
    let config = config(&client_id, &source_id, &target_id);
    let node = &config.nodes[&target_id];
    let command =
        windows_terminal_command(node, Provider::Claude, &session_id, "wt.exe", "ssh.exe").unwrap();
    assert_eq!(&command[..4], ["wt.exe", "-w", "new", "new-tab"]);
    assert!(
        command
            .iter()
            .any(|value| value == "ClearAllForwardings=yes")
    );
    assert!(command.iter().any(|value| value == &target_id));
    assert!(command.iter().any(|value| value == "claude"));
    assert!(command.iter().any(|value| value == &session_id));
    assert!(!command.iter().any(|value| value == "--continue"));
    assert!(!command.iter().any(|value| value == "--last"));

    let opencode = windows_terminal_command(
        node,
        Provider::Opencode,
        "ses_fdd613642ffeZLuODNNxjAL3h7",
        "wt.exe",
        "ssh.exe",
    )
    .unwrap();
    assert!(
        opencode
            .iter()
            .any(|value| value == "ses_fdd613642ffeZLuODNNxjAL3h7")
    );
    assert!(
        windows_terminal_command(
            node,
            Provider::Opencode,
            "ses_../../bad",
            "wt.exe",
            "ssh.exe"
        )
        .is_err()
    );
}

#[test]
fn authenticated_launch_is_deduplicated_and_request_id_is_identity_bound() {
    let (client_id, source_id, target_id, session_id) = ids();
    let launcher = FakeLauncher::default();
    let launched = launcher.launched.clone();
    let mut bridge = ClientLaunchBridge::new(
        config(&client_id, &source_id, &target_id),
        launcher,
        "wt.exe",
        "ssh.exe",
    )
    .unwrap();
    let request_id = Uuid::new_v4().to_string();
    let request = make_launch_request(
        &client_id,
        SOURCE_TOKEN,
        &source_id,
        &target_id,
        Provider::Codex,
        &session_id,
        Some(&request_id),
    )
    .unwrap();
    let value = serde_json::to_value(&request).unwrap();
    let now = Instant::now();
    let first = bridge.handle_at(&value, now).unwrap();
    let second = bridge
        .handle_at(&value, now + Duration::from_secs(1))
        .unwrap();
    assert_eq!(first, second);
    assert_eq!(launched.lock().unwrap().len(), 1);
    assert_eq!(first["type"], "launched");

    let changed = make_launch_request(
        &client_id,
        SOURCE_TOKEN,
        &source_id,
        &target_id,
        Provider::Claude,
        &session_id,
        Some(&request_id),
    )
    .unwrap();
    let error = bridge
        .handle_at(
            &serde_json::to_value(changed).unwrap(),
            now + Duration::from_secs(2),
        )
        .unwrap_err();
    assert_eq!(error.kind, ClientBridgeErrorKind::Rejected);
    assert_eq!(launched.lock().unwrap().len(), 1);
}

#[test]
fn wrong_client_source_target_or_secret_fails_closed_without_launch() {
    let (client_id, source_id, target_id, session_id) = ids();
    let launcher = FakeLauncher::default();
    let launched = launcher.launched.clone();
    let mut bridge = ClientLaunchBridge::new(
        config(&client_id, &source_id, &target_id),
        launcher,
        "wt.exe",
        "ssh.exe",
    )
    .unwrap();
    for request in [
        make_launch_request(
            &Uuid::new_v4().to_string(),
            SOURCE_TOKEN,
            &source_id,
            &target_id,
            Provider::Codex,
            &session_id,
            None,
        ),
        make_launch_request(
            &client_id,
            TARGET_TOKEN,
            &source_id,
            &target_id,
            Provider::Codex,
            &session_id,
            None,
        ),
        make_launch_request(
            &client_id,
            SOURCE_TOKEN,
            &source_id,
            &Uuid::new_v4().to_string(),
            Provider::Codex,
            &session_id,
            None,
        ),
    ] {
        let error = bridge
            .handle(&serde_json::to_value(request.unwrap()).unwrap())
            .unwrap_err();
        assert_eq!(error.kind, ClientBridgeErrorKind::Rejected);
    }
    assert!(launched.lock().unwrap().is_empty());
}

#[test]
fn secret_rotation_is_adopted_by_reload_without_changing_client_identity() {
    let (client_id, source_id, target_id, session_id) = ids();
    let launcher = FakeLauncher::default();
    let mut bridge = ClientLaunchBridge::new(
        config(&client_id, &source_id, &target_id),
        launcher,
        "wt.exe",
        "ssh.exe",
    )
    .unwrap();
    let new_token = "ef".repeat(32);
    let mut rotated = config(&client_id, &source_id, &target_id);
    rotated.nodes.get_mut(&source_id).unwrap().token = new_token.clone();
    bridge.reload(rotated).unwrap();
    let old = make_launch_request(
        &client_id,
        SOURCE_TOKEN,
        &source_id,
        &target_id,
        Provider::Codex,
        &session_id,
        None,
    )
    .unwrap();
    assert!(bridge.handle(&serde_json::to_value(old).unwrap()).is_err());
    let fresh = make_launch_request(
        &client_id,
        &new_token,
        &source_id,
        &target_id,
        Provider::Codex,
        &session_id,
        None,
    )
    .unwrap();
    assert_eq!(
        bridge
            .handle(&serde_json::to_value(fresh).unwrap())
            .unwrap()["type"],
        "launched"
    );

    let mut changed_identity = config(&Uuid::new_v4().to_string(), &source_id, &target_id);
    changed_identity.nodes.get_mut(&source_id).unwrap().token = new_token;
    assert_eq!(
        bridge.reload(changed_identity).unwrap_err().kind,
        ClientBridgeErrorKind::Rejected
    );
}

#[test]
fn absent_tunnel_falls_through_but_rejection_and_uncertainty_block() {
    let (client_id, source_id, target_id, session_id) = ids();
    let route = json!({
        "client_id": client_id,
        "label": "laptop",
        "host": "127.0.0.1",
        "port": 49000,
        "token": SOURCE_TOKEN,
        "timeout": 0.35,
        "enabled": true,
    });
    let mut absent = FakeTransport {
        responses: VecDeque::from([Err(ClientBridgeError::unavailable("connection refused"))]),
        ..FakeTransport::default()
    };
    assert_eq!(
        route_client_window(
            std::slice::from_ref(&route),
            true,
            &source_id,
            &target_id,
            Provider::Codex,
            &session_id,
            &mut absent,
        )
        .unwrap(),
        ClientWindowOutcome::ContinueWithExistingAttach
    );

    let mut rejected = FakeTransport {
        responses: VecDeque::from([Ok(json!({
            "type": "error",
            "protocol": BRIDGE_PROTOCOL,
            "version": BRIDGE_VERSION,
            "message": "identity mismatch",
        }))]),
        ..FakeTransport::default()
    };
    assert_eq!(
        route_client_window(
            std::slice::from_ref(&route),
            true,
            &source_id,
            &target_id,
            Provider::Codex,
            &session_id,
            &mut rejected,
        )
        .unwrap_err()
        .kind,
        ClientBridgeErrorKind::Rejected
    );

    let mut uncertain = FakeTransport {
        responses: VecDeque::from([Err(ClientBridgeError::outcome_unknown(
            "connection dropped after request",
        ))]),
        ..FakeTransport::default()
    };
    assert_eq!(
        route_client_window(
            &[route],
            true,
            &source_id,
            &target_id,
            Provider::Codex,
            &session_id,
            &mut uncertain,
        )
        .unwrap_err()
        .kind,
        ClientBridgeErrorKind::OutcomeUnknown
    );
}

#[test]
fn launched_receipt_is_exact_and_never_claims_attach_confirmation() {
    let (client_id, source_id, target_id, session_id) = ids();
    let request = make_launch_request(
        &client_id,
        SOURCE_TOKEN,
        &source_id,
        &target_id,
        Provider::Codex,
        &session_id,
        None,
    )
    .unwrap();
    let response = json!({
        "type": "launched",
        "protocol": BRIDGE_PROTOCOL,
        "version": BRIDGE_VERSION,
        "request_id": request.request_id,
        "target_node_id": target_id,
        "provider": "codex",
        "session_id": session_id,
        "detail": "WINDOW LAUNCHED · gpu-box · id exact",
    });
    let mut transport = FakeTransport {
        responses: VecDeque::from([Ok(response)]),
        ..FakeTransport::default()
    };
    let endpoint = LoopbackEndpoint::new("localhost", 49_000, Duration::from_millis(350)).unwrap();
    let receipt = request_client_launch(&mut transport, &endpoint, &request).unwrap();
    assert_eq!(receipt.message_type, "launched");
    assert!(!receipt.detail.to_ascii_lowercase().contains("attached"));

    let mut false_claim = serde_json::to_value(receipt).unwrap();
    false_claim["attached"] = json!(true);
    transport.responses.push_back(Ok(false_claim));
    assert!(request_client_launch(&mut transport, &endpoint, &request).is_err());
}

#[test]
fn listener_and_endpoint_are_loopback_only_and_wire_is_bounded() {
    assert!(LoopbackEndpoint::new("0.0.0.0", 49_000, Duration::from_secs(1)).is_err());
    assert!(LoopbackEndpoint::new("192.0.2.1", 49_000, Duration::from_secs(1)).is_err());
    let endpoint = LoopbackEndpoint::new("127.0.0.1", 49_001, Duration::from_secs(1)).unwrap();
    let listener = bind_client_bridge(&endpoint).unwrap();
    assert!(listener.local_addr().unwrap().ip().is_loopback());

    let two_lines = br#"{"type":"ping"}
{"type":"ping"}
"#;
    assert!(read_bridge_message(&mut Cursor::new(two_lines)).is_err());
    let oversized = vec![b'x'; MAX_BRIDGE_MESSAGE_BYTES + 1];
    assert!(read_bridge_message(&mut Cursor::new(oversized)).is_err());
}

#[test]
fn loopback_server_launches_once_without_real_windows_or_powershell() {
    let (client_id, source_id, target_id, session_id) = ids();
    let launcher = FakeLauncher::default();
    let launched = launcher.launched.clone();
    let bridge = ClientLaunchBridge::new(
        config(&client_id, &source_id, &target_id),
        launcher,
        "fake-wt.exe",
        "fake-ssh.exe",
    )
    .unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    assert!(port >= 1024);
    let server = thread::spawn(move || {
        let mut bridge = bridge;
        serve_client_bridge_once(&listener, &mut bridge).unwrap();
    });
    let request = make_launch_request(
        &client_id,
        SOURCE_TOKEN,
        &source_id,
        &target_id,
        Provider::Codex,
        &session_id,
        None,
    )
    .unwrap();
    let endpoint = LoopbackEndpoint::new("127.0.0.1", port, Duration::from_secs(1)).unwrap();
    let receipt = request_client_launch(
        &mut pikamux::client_bridge::TcpClientBridgeTransport,
        &endpoint,
        &request,
    )
    .unwrap();
    server.join().unwrap();
    assert_eq!(receipt.target_node_id, target_id);
    let commands = launched.lock().unwrap();
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0][0], "fake-wt.exe");
    assert!(!commands[0].iter().any(|value| {
        matches!(
            value.as_str(),
            "powershell" | "powershell.exe" | "pwsh" | "pwsh.exe"
        )
    }));
}

#[test]
fn launch_request_rejects_non_uuid_provider_identity_and_unknown_fields() {
    let (client_id, source_id, target_id, _) = ids();
    let malformed = json!({
        "type": "open",
        "protocol": BRIDGE_PROTOCOL,
        "version": BRIDGE_VERSION,
        "request_id": Uuid::new_v4().to_string(),
        "client_id": client_id,
        "token": SOURCE_TOKEN,
        "source_node_id": source_id,
        "target_node_id": target_id,
        "provider": "codex",
        "session_id": "$(touch nope)",
    });
    assert!(validate_launch_request(&malformed).is_err());
    let mut unknown_provider = malformed;
    unknown_provider["provider"] = json!("other");
    assert!(validate_launch_request(&unknown_provider).is_err());
}
