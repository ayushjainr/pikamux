//! Experimental Windows client window routing.
//!
//! This module does not host agents on Windows.  A trusted macOS/Linux Pika
//! node can ask a paired, loopback-only client bridge to start Windows
//! Terminal with an exact `_fleet-open` node/provider/conversation route.
//! The bridge confirms only that the window process was launched; the new
//! terminal owns the later SSH connection and remote attach.

use crate::fleet::{PROTOCOL_NAME as FLEET_PROTOCOL, PROTOCOL_VERSION as FLEET_VERSION};
use crate::model::Provider;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
#[cfg(windows)]
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use uuid::Uuid;

pub const BRIDGE_PROTOCOL: &str = "pikamux-client-launch";
pub const BRIDGE_VERSION: u32 = 2;
pub const CLIENT_CONFIG_VERSION: u32 = 1;
pub const DEFAULT_LOCAL_PORT: u16 = 47_653;
pub const DEFAULT_REMOTE_PORT: u16 = 47_654;
pub const MAX_BRIDGE_MESSAGE_BYTES: usize = 16 * 1024;
pub const RECEIPT_CACHE_TTL: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClientBridgeErrorKind {
    Invalid,
    Incompatible,
    Rejected,
    Unavailable,
    OutcomeUnknown,
    Io,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClientBridgeError {
    pub kind: ClientBridgeErrorKind,
    pub message: String,
}

impl ClientBridgeError {
    fn new(kind: ClientBridgeErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: sanitized_message(&message.into(), 500),
        }
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(ClientBridgeErrorKind::Invalid, message)
    }

    pub fn incompatible(message: impl Into<String>) -> Self {
        Self::new(ClientBridgeErrorKind::Incompatible, message)
    }

    pub fn rejected(message: impl Into<String>) -> Self {
        Self::new(ClientBridgeErrorKind::Rejected, message)
    }

    fn io(message: impl Into<String>) -> Self {
        Self::new(ClientBridgeErrorKind::Io, message)
    }

    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::new(ClientBridgeErrorKind::Unavailable, message)
    }

    pub fn outcome_unknown(message: impl Into<String>) -> Self {
        Self::new(ClientBridgeErrorKind::OutcomeUnknown, message)
    }
}

impl fmt::Display for ClientBridgeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ClientBridgeError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClientNode {
    pub node_id: String,
    pub alias: String,
    pub ssh_target: String,
    pub token: String,
    pub remote_port: Option<u16>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClientConfig {
    pub version: u32,
    pub client_id: String,
    pub nodes: BTreeMap<String, ClientNode>,
}

impl ClientConfig {
    pub fn empty() -> Self {
        Self {
            version: CLIENT_CONFIG_VERSION,
            client_id: Uuid::new_v4().to_string(),
            nodes: BTreeMap::new(),
        }
    }

    pub fn from_value(value: &Value) -> Result<Self, ClientBridgeError> {
        let object = value
            .as_object()
            .ok_or_else(|| ClientBridgeError::invalid("Unsupported Pika client configuration"))?;
        if object.get("version").and_then(Value::as_u64) != Some(u64::from(CLIENT_CONFIG_VERSION)) {
            return Err(ClientBridgeError::invalid(
                "Unsupported Pika client configuration",
            ));
        }
        let client_id = canonical_uuid(object.get("client_id"), "Invalid client identity")?;
        let raw_nodes = object
            .get("nodes")
            .and_then(Value::as_object)
            .ok_or_else(|| {
                ClientBridgeError::invalid("Pika client node configuration is malformed")
            })?;
        let mut nodes = BTreeMap::new();
        for (raw_node_id, raw_node) in raw_nodes {
            let node_id = canonical_uuid(
                Some(&Value::String(raw_node_id.clone())),
                "Invalid node identity",
            )?;
            let node = parse_client_node(&node_id, raw_node)?;
            nodes.insert(node_id, node);
        }
        Ok(Self {
            version: CLIENT_CONFIG_VERSION,
            client_id,
            nodes,
        })
    }

    pub fn to_value(&self) -> Value {
        let nodes = self
            .nodes
            .iter()
            .map(|(node_id, node)| {
                let mut value = json!({
                    "alias": node.alias,
                    "ssh_target": node.ssh_target,
                    "token": node.token,
                });
                if let Some(port) = node.remote_port {
                    value
                        .as_object_mut()
                        .expect("node is an object")
                        .insert("remote_port".to_owned(), json!(port));
                }
                (node_id.clone(), value)
            })
            .collect::<Map<_, _>>();
        json!({
            "version": CLIENT_CONFIG_VERSION,
            "client_id": self.client_id,
            "nodes": nodes,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PairRequest {
    #[serde(rename = "type")]
    pub message_type: String,
    pub protocol: String,
    pub version: u32,
    pub expected_node_id: String,
    pub client_id: String,
    pub client_label: String,
    pub token: String,
    pub port: u16,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PairReceipt {
    #[serde(rename = "type")]
    pub message_type: String,
    pub protocol: String,
    pub version: u32,
    pub node_id: String,
    pub client_id: String,
    pub port: u16,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchRequest {
    #[serde(rename = "type")]
    pub message_type: String,
    pub protocol: String,
    pub version: u32,
    pub request_id: String,
    pub client_id: String,
    pub token: String,
    pub source_node_id: String,
    pub target_node_id: String,
    pub provider: Provider,
    pub session_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientLaunchReceipt {
    #[serde(rename = "type")]
    pub message_type: String,
    pub protocol: String,
    pub version: u32,
    pub request_id: String,
    pub target_node_id: String,
    pub provider: Provider,
    pub session_id: String,
    pub detail: String,
}

/// The only positive bridge receipt.  It deliberately does not mean that the
/// newly launched terminal has completed its remote `_fleet-open` attach.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ClientWindowOutcome {
    ContinueWithExistingAttach,
    WindowLaunched(ClientLaunchReceipt),
}

#[derive(Clone, Debug, PartialEq)]
pub struct ServerBridgeRoute {
    pub client_id: String,
    pub label: String,
    pub host: String,
    pub port: u16,
    pub token: String,
    pub timeout: Duration,
    pub enabled: bool,
}

impl ServerBridgeRoute {
    pub fn from_value(value: &Value) -> Result<Self, ClientBridgeError> {
        let object = value
            .as_object()
            .ok_or_else(|| ClientBridgeError::invalid("Client bridge route is malformed"))?;
        let enabled = match object.get("enabled") {
            None => true,
            Some(Value::Bool(value)) => *value,
            Some(_) => {
                return Err(ClientBridgeError::invalid(
                    "Client bridge enabled flag is invalid",
                ));
            }
        };
        let host = match object.get("host") {
            None | Some(Value::Null) => "127.0.0.1".to_owned(),
            Some(Value::String(value)) if !value.is_empty() => value.clone(),
            Some(_) => {
                return Err(ClientBridgeError::invalid(
                    "Client bridge endpoint is invalid",
                ));
            }
        };
        validate_loopback_host(&host, "Client bridge endpoint must stay on server loopback")?;
        let port = optional_port(
            object.get("port"),
            DEFAULT_REMOTE_PORT,
            "Client bridge port",
        )?;
        let timeout = match object.get("timeout") {
            None | Some(Value::Null) => Duration::from_millis(350),
            Some(Value::Number(value)) => {
                let seconds = value
                    .as_f64()
                    .filter(|value| value.is_finite())
                    .ok_or_else(|| {
                        ClientBridgeError::invalid("Client bridge timeout is invalid")
                    })?;
                Duration::from_secs_f64(seconds.clamp(0.05, 2.0))
            }
            Some(_) => {
                return Err(ClientBridgeError::invalid(
                    "Client bridge timeout is invalid",
                ));
            }
        };
        let label = optional_text(object.get("label"), "pika-client", 63, "client label")?;
        Ok(Self {
            client_id: canonical_uuid(object.get("client_id"), "Invalid client identity")?,
            label,
            host,
            port,
            token: bridge_token(object.get("token"))?,
            timeout,
            enabled,
        })
    }

    pub fn to_value(&self) -> Value {
        json!({
            "client_id": self.client_id,
            "label": self.label,
            "host": self.host,
            "port": self.port,
            "token": self.token,
            "timeout": self.timeout.as_secs_f64(),
            "enabled": self.enabled,
        })
    }

    pub fn endpoint(&self) -> Result<LoopbackEndpoint, ClientBridgeError> {
        LoopbackEndpoint::new(&self.host, self.port, self.timeout)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PairingHello {
    pub node_id: String,
    pub machine: String,
    pub package_version: String,
    pub capabilities: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LoopbackEndpoint {
    pub host: String,
    pub port: u16,
    pub timeout: Duration,
}

impl LoopbackEndpoint {
    pub fn new(host: &str, port: u16, timeout: Duration) -> Result<Self, ClientBridgeError> {
        validate_loopback_host(host, "The Pika client bridge may use only loopback")?;
        if port < 1024 {
            return Err(ClientBridgeError::invalid(
                "Client bridge port must be between 1024 and 65535",
            ));
        }
        if timeout.is_zero() {
            return Err(ClientBridgeError::invalid(
                "Client bridge timeout must be positive",
            ));
        }
        Ok(Self {
            host: host.to_owned(),
            port,
            timeout,
        })
    }

    fn socket_addr(&self) -> SocketAddr {
        let ip = if self.host == "::1" {
            IpAddr::V6(Ipv6Addr::LOCALHOST)
        } else {
            // `localhost` is resolved explicitly, so this bridge never consults
            // DNS or accidentally follows a non-loopback hosts-file entry.
            IpAddr::V4(Ipv4Addr::LOCALHOST)
        };
        SocketAddr::new(ip, self.port)
    }
}

pub fn generate_pairing_token() -> String {
    // `Uuid::new_v4` uses the operating system CSPRNG. Hash three UUIDs so the
    // 64-hex-character pairing token has more than 256 bits of input entropy
    // despite the fixed UUID version/variant bits.
    let mut hasher = Sha256::new();
    for _ in 0..3 {
        hasher.update(Uuid::new_v4().as_bytes());
    }
    format!("{:x}", hasher.finalize())
}

pub fn make_pair_request(
    expected_node_id: &str,
    client_id: &str,
    client_label: &str,
    token: &str,
    port: u16,
) -> Result<PairRequest, ClientBridgeError> {
    validate_pair_request(
        &serde_json::to_value(PairRequest {
            message_type: "pair".to_owned(),
            protocol: BRIDGE_PROTOCOL.to_owned(),
            version: BRIDGE_VERSION,
            expected_node_id: expected_node_id.to_owned(),
            client_id: client_id.to_owned(),
            client_label: client_label.to_owned(),
            token: token.to_owned(),
            port,
        })
        .expect("pair request is serializable"),
    )
}

pub fn validate_pair_request(value: &Value) -> Result<PairRequest, ClientBridgeError> {
    let mut request: PairRequest = serde_json::from_value(value.clone())
        .map_err(|_| ClientBridgeError::invalid("Malformed client pairing request"))?;
    if request.message_type != "pair"
        || request.protocol != BRIDGE_PROTOCOL
        || request.version != BRIDGE_VERSION
    {
        return Err(ClientBridgeError::incompatible(
            "Incompatible client pairing request",
        ));
    }
    request.expected_node_id =
        canonical_uuid_str(&request.expected_node_id, "Invalid node identity")?;
    request.client_id = canonical_uuid_str(&request.client_id, "Invalid client identity")?;
    request.client_label = client_label(&request.client_label)?;
    request.token = bridge_token_str(&request.token)?;
    if request.port < 1024 {
        return Err(ClientBridgeError::invalid("Invalid reverse bridge port"));
    }
    Ok(request)
}

/// Validate the fleet hello used before pairing.  Version 2 is intentionally
/// taken from the authoritative fleet module; the frozen Python client CLI's
/// version-1 hello was incompatible with both Python and Rust fleet servers.
pub fn validate_pairing_hello(value: &Value) -> Result<PairingHello, ClientBridgeError> {
    let object = exact_object(
        value,
        &[
            "type",
            "protocol",
            "version",
            "node_id",
            "machine",
            "package_version",
            "capabilities",
        ],
        "Remote Pika returned a malformed fleet hello",
    )?;
    if object.get("type").and_then(Value::as_str) != Some("hello")
        || object.get("protocol").and_then(Value::as_str) != Some(FLEET_PROTOCOL)
        || object.get("version").and_then(Value::as_i64) != Some(FLEET_VERSION)
    {
        return Err(ClientBridgeError::incompatible(
            "Remote machine did not prove a compatible Pika node",
        ));
    }
    let node_id = canonical_uuid(
        object.get("node_id"),
        "Remote Pika returned an invalid node identity",
    )?;
    let machine = required_printable_text(
        object.get("machine"),
        63,
        "Remote Pika returned an invalid machine name",
    )?;
    let package_version = required_printable_text(
        object.get("package_version"),
        64,
        "Remote Pika returned an invalid package version",
    )?;
    let capabilities = object
        .get("capabilities")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            ClientBridgeError::incompatible("Remote Pika returned invalid capabilities")
        })?
        .iter()
        .map(|value| {
            required_printable_text(Some(value), 64, "Remote Pika returned invalid capabilities")
        })
        .collect::<Result<Vec<_>, _>>()?;
    if capabilities.len() > 64
        || capabilities.iter().collect::<BTreeSet<_>>().len() != capabilities.len()
        || !capabilities.iter().any(|value| value == "attach")
    {
        return Err(ClientBridgeError::incompatible(
            "Remote Pika lacks the exact attach capability",
        ));
    }
    Ok(PairingHello {
        node_id,
        machine,
        package_version,
        capabilities,
    })
}

/// Rotate or add the server-side secret for one client.  Callers persist the
/// returned route list atomically only after this function succeeds.
pub fn accept_pairing(
    existing: &[Value],
    local_node_id: &str,
    raw_request: &Value,
) -> Result<(Vec<Value>, PairReceipt), ClientBridgeError> {
    let request = validate_pair_request(raw_request)?;
    let local_node_id = canonical_uuid_str(local_node_id, "Invalid local node identity")?;
    if request.expected_node_id != local_node_id {
        return Err(ClientBridgeError::rejected(format!(
            "NODE IDENTITY CHANGED: expected {}, received {}",
            prefix(&request.expected_node_id, 8),
            prefix(&local_node_id, 8)
        )));
    }
    let route = ServerBridgeRoute {
        client_id: request.client_id.clone(),
        label: request.client_label.clone(),
        host: "127.0.0.1".to_owned(),
        port: request.port,
        token: request.token.clone(),
        timeout: Duration::from_millis(350),
        enabled: true,
    };
    let mut routes = existing
        .iter()
        .filter(|value| value.is_object())
        .filter(|value| {
            value.get("client_id").and_then(Value::as_str) != Some(request.client_id.as_str())
        })
        .cloned()
        .collect::<Vec<_>>();
    routes.push(route.to_value());
    let receipt = PairReceipt {
        message_type: "paired".to_owned(),
        protocol: BRIDGE_PROTOCOL.to_owned(),
        version: BRIDGE_VERSION,
        node_id: local_node_id,
        client_id: request.client_id,
        port: request.port,
    };
    Ok((routes, receipt))
}

pub fn validate_pair_receipt(
    value: &Value,
    expected_node_id: &str,
    expected_client_id: &str,
    expected_port: u16,
) -> Result<PairReceipt, ClientBridgeError> {
    let mut receipt: PairReceipt = serde_json::from_value(value.clone())
        .map_err(|_| ClientBridgeError::invalid("Malformed client pairing receipt"))?;
    if receipt.message_type != "paired"
        || receipt.protocol != BRIDGE_PROTOCOL
        || receipt.version != BRIDGE_VERSION
    {
        return Err(ClientBridgeError::incompatible(
            "Incompatible client pairing receipt",
        ));
    }
    receipt.node_id = canonical_uuid_str(&receipt.node_id, "Invalid node identity")?;
    receipt.client_id = canonical_uuid_str(&receipt.client_id, "Invalid client identity")?;
    let expected_node_id = canonical_uuid_str(expected_node_id, "Invalid node identity")?;
    let expected_client_id = canonical_uuid_str(expected_client_id, "Invalid client identity")?;
    if receipt.node_id != expected_node_id
        || receipt.client_id != expected_client_id
        || receipt.port != expected_port
    {
        return Err(ClientBridgeError::rejected(
            "Remote Pika pairing receipt failed identity validation",
        ));
    }
    Ok(receipt)
}

/// Install a client-side node entry only after the server's exact pairing
/// receipt has been verified. Re-pairing the same node replaces its old token.
pub fn install_client_pairing(
    config: &mut ClientConfig,
    node_id: &str,
    alias: &str,
    ssh_target: &str,
    token: &str,
    remote_port: u16,
    raw_receipt: &Value,
) -> Result<(), ClientBridgeError> {
    let node_id = canonical_uuid_str(node_id, "Invalid node identity")?;
    validate_pair_receipt(raw_receipt, &node_id, &config.client_id, remote_port)?;
    let node = ClientNode {
        node_id: node_id.clone(),
        alias: client_alias(alias)?,
        ssh_target: ssh_target_value(ssh_target)?,
        token: bridge_token_str(token)?,
        remote_port: Some(validate_port(remote_port, "Invalid reverse bridge port")?),
    };
    config.nodes.insert(node_id, node);
    Ok(())
}

pub fn make_launch_request(
    client_id: &str,
    token: &str,
    source_node_id: &str,
    target_node_id: &str,
    provider: Provider,
    session_id: &str,
    request_id: Option<&str>,
) -> Result<LaunchRequest, ClientBridgeError> {
    validate_launch_request(
        &serde_json::to_value(LaunchRequest {
            message_type: "open".to_owned(),
            protocol: BRIDGE_PROTOCOL.to_owned(),
            version: BRIDGE_VERSION,
            request_id: request_id
                .map(str::to_owned)
                .unwrap_or_else(|| Uuid::new_v4().to_string()),
            client_id: client_id.to_owned(),
            token: token.to_owned(),
            source_node_id: source_node_id.to_owned(),
            target_node_id: target_node_id.to_owned(),
            provider,
            session_id: session_id.to_owned(),
        })
        .expect("launch request is serializable"),
    )
}

pub fn validate_launch_request(value: &Value) -> Result<LaunchRequest, ClientBridgeError> {
    let mut request: LaunchRequest = serde_json::from_value(value.clone())
        .map_err(|_| ClientBridgeError::invalid("Malformed client launch request"))?;
    if request.message_type != "open"
        || request.protocol != BRIDGE_PROTOCOL
        || request.version != BRIDGE_VERSION
    {
        return Err(ClientBridgeError::incompatible(
            "Incompatible client launch request",
        ));
    }
    request.request_id = canonical_uuid_str(&request.request_id, "Invalid request identity")?;
    request.client_id = canonical_uuid_str(&request.client_id, "Invalid client identity")?;
    request.token = bridge_token_str(&request.token)?;
    request.source_node_id =
        canonical_uuid_str(&request.source_node_id, "Invalid source node identity")?;
    request.target_node_id =
        canonical_uuid_str(&request.target_node_id, "Invalid target node identity")?;
    request.session_id = conversation_identity(request.provider, &request.session_id)?;
    Ok(request)
}

pub fn windows_terminal_command(
    node: &ClientNode,
    provider: Provider,
    session_id: &str,
    terminal_executable: &str,
    ssh_executable: &str,
) -> Result<Vec<String>, ClientBridgeError> {
    let exact_session_id = conversation_identity(provider, session_id)?;
    let exact_node_id = canonical_uuid_str(&node.node_id, "Invalid node identity")?;
    client_alias(&node.alias)?;
    ssh_target_value(&node.ssh_target)?;
    if terminal_executable.is_empty() || ssh_executable.is_empty() {
        return Err(ClientBridgeError::invalid(
            "Client bridge executable is missing",
        ));
    }
    Ok(vec![
        terminal_executable.to_owned(),
        "-w".to_owned(),
        "new".to_owned(),
        "new-tab".to_owned(),
        "--title".to_owned(),
        format!(
            "Pika · {} · {}-{}",
            node.alias,
            provider,
            prefix(&exact_session_id, 8)
        ),
        ssh_executable.to_owned(),
        "-tt".to_owned(),
        "-o".to_owned(),
        "ClearAllForwardings=yes".to_owned(),
        node.ssh_target.clone(),
        "pika".to_owned(),
        "_fleet-open".to_owned(),
        "--expected-node-id".to_owned(),
        exact_node_id,
        "--provider".to_owned(),
        provider.as_str().to_owned(),
        "--session-id".to_owned(),
        exact_session_id,
    ])
}

pub trait WindowLauncher {
    /// Return only after the OS has accepted creation of the window process.
    /// This does not certify the later SSH connection or remote attach.
    fn launch(&mut self, argv: &[String]) -> Result<(), ClientBridgeError>;
}

#[derive(Default)]
pub struct ProcessWindowLauncher;

impl WindowLauncher for ProcessWindowLauncher {
    fn launch(&mut self, argv: &[String]) -> Result<(), ClientBridgeError> {
        if argv.is_empty() {
            return Err(ClientBridgeError::invalid("Window launch command is empty"));
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
            Command::new(&argv[0])
                .args(&argv[1..])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .creation_flags(CREATE_NEW_PROCESS_GROUP)
                .spawn()
                .map(|_| ())
                .map_err(|error| {
                    ClientBridgeError::io(format!("Could not launch Windows Terminal: {error}"))
                })
        }
        #[cfg(not(windows))]
        {
            let _ = argv;
            Err(ClientBridgeError::io(
                "Windows client window launching is unavailable on this operating system",
            ))
        }
    }
}

#[derive(Clone)]
struct CachedReceipt {
    created_at: Instant,
    identity: LaunchIdentity,
    receipt: ClientLaunchReceipt,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct LaunchIdentity {
    client_id: String,
    source_node_id: String,
    target_node_id: String,
    provider: Provider,
    session_id: String,
}

impl From<&LaunchRequest> for LaunchIdentity {
    fn from(request: &LaunchRequest) -> Self {
        Self {
            client_id: request.client_id.clone(),
            source_node_id: request.source_node_id.clone(),
            target_node_id: request.target_node_id.clone(),
            provider: request.provider,
            session_id: request.session_id.clone(),
        }
    }
}

pub struct ClientLaunchBridge<L> {
    client_id: String,
    pub nodes: BTreeMap<String, ClientNode>,
    launcher: L,
    terminal_executable: String,
    ssh_executable: String,
    receipts: BTreeMap<String, CachedReceipt>,
}

impl<L: WindowLauncher> ClientLaunchBridge<L> {
    pub fn new(
        config: ClientConfig,
        launcher: L,
        terminal_executable: impl Into<String>,
        ssh_executable: impl Into<String>,
    ) -> Result<Self, ClientBridgeError> {
        validate_client_config(&config)?;
        let client_id = config.client_id.clone();
        Ok(Self {
            client_id,
            nodes: config.nodes,
            launcher,
            terminal_executable: terminal_executable.into(),
            ssh_executable: ssh_executable.into(),
            receipts: BTreeMap::new(),
        })
    }

    /// Adopt an atomically replaced pairing file without restarting. The
    /// bridge identity cannot change underneath a running listener.
    pub fn reload(&mut self, config: ClientConfig) -> Result<(), ClientBridgeError> {
        validate_client_config(&config)?;
        let client_id = config.client_id.clone();
        if client_id != self.client_id {
            return Err(ClientBridgeError::rejected(
                "Client identity changed while bridge was running",
            ));
        }
        self.nodes = config.nodes;
        Ok(())
    }

    pub fn handle(&mut self, value: &Value) -> Result<Value, ClientBridgeError> {
        self.handle_at(value, Instant::now())
    }

    pub fn handle_at(&mut self, value: &Value, now: Instant) -> Result<Value, ClientBridgeError> {
        if value
            == &json!({
                "type": "ping",
                "protocol": BRIDGE_PROTOCOL,
                "version": BRIDGE_VERSION,
            })
        {
            return Ok(json!({
                "type": "pong",
                "protocol": BRIDGE_PROTOCOL,
                "version": BRIDGE_VERSION,
            }));
        }
        let request = validate_launch_request(value)?;
        if request.client_id != self.client_id {
            return Err(ClientBridgeError::rejected(
                "Client identity does not match this bridge",
            ));
        }
        let source = self.nodes.get(&request.source_node_id).ok_or_else(|| {
            ClientBridgeError::rejected("Source Pika node is not paired with this bridge")
        })?;
        if !constant_time_eq(source.token.as_bytes(), request.token.as_bytes()) {
            return Err(ClientBridgeError::rejected(
                "Source Pika node is not paired with this bridge",
            ));
        }
        let target = self.nodes.get(&request.target_node_id).ok_or_else(|| {
            ClientBridgeError::rejected("Target Pika node is not paired on this client")
        })?;

        self.receipts.retain(|_, receipt| {
            now.saturating_duration_since(receipt.created_at) < RECEIPT_CACHE_TTL
        });
        if let Some(prior) = self.receipts.get(&request.request_id) {
            if prior.identity != LaunchIdentity::from(&request) {
                return Err(ClientBridgeError::rejected(
                    "Request identity changed while reusing a launch request_id",
                ));
            }
            return serde_json::to_value(&prior.receipt)
                .map_err(|error| ClientBridgeError::io(error.to_string()));
        }

        let command = windows_terminal_command(
            target,
            request.provider,
            &request.session_id,
            &self.terminal_executable,
            &self.ssh_executable,
        )?;
        self.launcher.launch(&command)?;
        let receipt = ClientLaunchReceipt {
            message_type: "launched".to_owned(),
            protocol: BRIDGE_PROTOCOL.to_owned(),
            version: BRIDGE_VERSION,
            request_id: request.request_id.clone(),
            target_node_id: target.node_id.clone(),
            provider: request.provider,
            session_id: request.session_id.clone(),
            detail: format!(
                "WINDOW LAUNCHED · {} · id {}",
                target.alias,
                prefix(&request.session_id, 8)
            ),
        };
        self.receipts.insert(
            request.request_id.clone(),
            CachedReceipt {
                created_at: now,
                identity: LaunchIdentity::from(&request),
                receipt: receipt.clone(),
            },
        );
        serde_json::to_value(receipt).map_err(|error| ClientBridgeError::io(error.to_string()))
    }

    pub fn into_launcher(self) -> L {
        self.launcher
    }
}

pub trait ClientBridgeTransport {
    fn exchange(
        &mut self,
        endpoint: &LoopbackEndpoint,
        request: &Value,
    ) -> Result<Value, ClientBridgeError>;
}

#[derive(Default)]
pub struct TcpClientBridgeTransport;

impl ClientBridgeTransport for TcpClientBridgeTransport {
    fn exchange(
        &mut self,
        endpoint: &LoopbackEndpoint,
        request: &Value,
    ) -> Result<Value, ClientBridgeError> {
        let mut stream = TcpStream::connect_timeout(&endpoint.socket_addr(), endpoint.timeout)
            .map_err(|error| {
                ClientBridgeError::unavailable(format!("Client bridge unavailable: {error}"))
            })?;
        stream
            .set_read_timeout(Some(endpoint.timeout))
            .and_then(|_| stream.set_write_timeout(Some(endpoint.timeout)))
            .map_err(|error| {
                ClientBridgeError::outcome_unknown(format!(
                    "Client bridge outcome is unknown after connecting: {error}"
                ))
            })?;
        write_bridge_message(&mut stream, request).map_err(|error| {
            ClientBridgeError::outcome_unknown(format!(
                "Client bridge outcome is unknown after connecting: {error}"
            ))
        })?;
        read_bridge_message(&mut stream).map_err(|error| {
            ClientBridgeError::outcome_unknown(format!(
                "Client bridge outcome is unknown after submitting the request: {error}"
            ))
        })
    }
}

pub fn request_client_launch<T: ClientBridgeTransport>(
    transport: &mut T,
    endpoint: &LoopbackEndpoint,
    request: &LaunchRequest,
) -> Result<ClientLaunchReceipt, ClientBridgeError> {
    let request_value = serde_json::to_value(request)
        .map_err(|error| ClientBridgeError::invalid(error.to_string()))?;
    let validated = validate_launch_request(&request_value)?;
    let response = transport.exchange(endpoint, &request_value)?;
    if response.get("type").and_then(Value::as_str) == Some("error") {
        let object = exact_object(
            &response,
            &["type", "protocol", "version", "message"],
            "Malformed client bridge rejection",
        )?;
        if object.get("protocol").and_then(Value::as_str) != Some(BRIDGE_PROTOCOL)
            || object.get("version").and_then(Value::as_u64) != Some(u64::from(BRIDGE_VERSION))
        {
            return Err(ClientBridgeError::incompatible(
                "Incompatible client bridge rejection",
            ));
        }
        let message = required_printable_text(
            object.get("message"),
            500,
            "Client bridge rejected the request",
        )?;
        return Err(ClientBridgeError::rejected(message));
    }
    let mut receipt: ClientLaunchReceipt = serde_json::from_value(response)
        .map_err(|_| ClientBridgeError::rejected("Malformed client launch receipt"))?;
    if receipt.message_type != "launched"
        || receipt.protocol != BRIDGE_PROTOCOL
        || receipt.version != BRIDGE_VERSION
    {
        return Err(ClientBridgeError::incompatible(
            "Incompatible client launch receipt",
        ));
    }
    receipt.request_id = canonical_uuid_str(&receipt.request_id, "Invalid request identity")?;
    receipt.target_node_id =
        canonical_uuid_str(&receipt.target_node_id, "Invalid target node identity")?;
    receipt.session_id = conversation_identity(receipt.provider, &receipt.session_id)?;
    if receipt.request_id != validated.request_id
        || receipt.target_node_id != validated.target_node_id
        || receipt.provider != validated.provider
        || receipt.session_id != validated.session_id
    {
        return Err(ClientBridgeError::rejected(
            "Client launch receipt identity mismatch",
        ));
    }
    receipt.detail = required_printable_text(
        Some(&Value::String(receipt.detail)),
        500,
        "Malformed client launch receipt detail",
    )?;
    Ok(receipt)
}

/// CLI/core integration point for Enter on a session. A missing reverse tunnel
/// returns the existing attach path. Any response or post-connect uncertainty
/// fails closed; it must never fall through and create a possible second attach.
#[allow(clippy::too_many_arguments)]
pub fn route_client_window<T: ClientBridgeTransport>(
    configured: &[Value],
    client_context: bool,
    source_node_id: &str,
    target_node_id: &str,
    provider: Provider,
    session_id: &str,
    transport: &mut T,
) -> Result<ClientWindowOutcome, ClientBridgeError> {
    if session_id.starts_with("unbound:") || configured.is_empty() || !client_context {
        return Ok(ClientWindowOutcome::ContinueWithExistingAttach);
    }
    let source_node_id = canonical_uuid_str(source_node_id, "Invalid source node identity")?;
    let target_node_id = canonical_uuid_str(target_node_id, "Invalid target node identity")?;
    let session_id = conversation_identity(provider, session_id)?;
    for raw in configured.iter().filter(|value| value.is_object()) {
        if raw.get("enabled").and_then(Value::as_bool) == Some(false) {
            continue;
        }
        let route = ServerBridgeRoute::from_value(raw)?;
        if !route.enabled {
            continue;
        }
        let request = make_launch_request(
            &route.client_id,
            &route.token,
            &source_node_id,
            &target_node_id,
            provider,
            &session_id,
            None,
        )?;
        match request_client_launch(transport, &route.endpoint()?, &request) {
            Ok(receipt) => return Ok(ClientWindowOutcome::WindowLaunched(receipt)),
            Err(error) if error.kind == ClientBridgeErrorKind::Unavailable => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(ClientWindowOutcome::ContinueWithExistingAttach)
}

pub fn client_bridge_running<T: ClientBridgeTransport>(
    transport: &mut T,
    endpoint: &LoopbackEndpoint,
) -> bool {
    let ping = json!({
        "type": "ping",
        "protocol": BRIDGE_PROTOCOL,
        "version": BRIDGE_VERSION,
    });
    transport.exchange(endpoint, &ping).is_ok_and(|response| {
        response
            == json!({
                "type": "pong",
                "protocol": BRIDGE_PROTOCOL,
                "version": BRIDGE_VERSION,
            })
    })
}

pub fn bind_client_bridge(endpoint: &LoopbackEndpoint) -> Result<TcpListener, ClientBridgeError> {
    TcpListener::bind(endpoint.socket_addr())
        .map_err(|error| ClientBridgeError::io(format!("Cannot bind client bridge: {error}")))
}

pub fn default_client_config_path() -> Result<PathBuf, ClientBridgeError> {
    if let Some(path) = std::env::var_os("PIKA_CLIENT_CONFIG") {
        return Ok(PathBuf::from(path));
    }
    let home = directories::BaseDirs::new()
        .ok_or_else(|| ClientBridgeError::io("Cannot determine the client configuration home"))?;
    let directory = std::env::var_os("PIKA_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("XDG_CONFIG_HOME").map(|path| PathBuf::from(path).join("pika"))
        })
        .unwrap_or_else(|| home.home_dir().join(".config/pika"));
    Ok(directory.join("client.json"))
}

/// Accept and handle one loopback request. Long-running CLI code can reload its
/// atomically replaced `ClientConfig` before each call and then call `reload`.
pub fn serve_client_bridge_once<L: WindowLauncher>(
    listener: &TcpListener,
    bridge: &mut ClientLaunchBridge<L>,
) -> Result<(), ClientBridgeError> {
    let (mut stream, peer) = listener
        .accept()
        .map_err(|error| ClientBridgeError::io(format!("Client bridge accept failed: {error}")))?;
    if !peer.ip().is_loopback() {
        return Err(ClientBridgeError::rejected(
            "Client bridge rejected a non-loopback peer",
        ));
    }
    stream
        .set_read_timeout(Some(Duration::from_secs(1)))
        .and_then(|_| stream.set_write_timeout(Some(Duration::from_secs(1))))
        .map_err(|error| ClientBridgeError::io(format!("Client bridge socket failed: {error}")))?;
    let response = match read_bridge_message(&mut stream).and_then(|value| bridge.handle(&value)) {
        Ok(response) => response,
        Err(error) => error_wire(&error),
    };
    write_bridge_message(&mut stream, &response)
}

/// Handle one connection after reloading the pairing file. This preserves
/// secret rotation for a listener that was already blocked in `accept` when
/// the client configuration was atomically replaced.
pub fn serve_client_bridge_once_reloading<L, F>(
    listener: &TcpListener,
    bridge: &mut ClientLaunchBridge<L>,
    load_config: F,
) -> Result<(), ClientBridgeError>
where
    L: WindowLauncher,
    F: FnOnce() -> Result<ClientConfig, ClientBridgeError>,
{
    let (mut stream, peer) = listener
        .accept()
        .map_err(|error| ClientBridgeError::io(format!("Client bridge accept failed: {error}")))?;
    if !peer.ip().is_loopback() {
        return Err(ClientBridgeError::rejected(
            "Client bridge rejected a non-loopback peer",
        ));
    }
    stream
        .set_read_timeout(Some(Duration::from_secs(1)))
        .and_then(|_| stream.set_write_timeout(Some(Duration::from_secs(1))))
        .map_err(|error| ClientBridgeError::io(format!("Client bridge socket failed: {error}")))?;
    let response = match load_config()
        .and_then(|config| bridge.reload(config))
        .and_then(|()| read_bridge_message(&mut stream))
        .and_then(|value| bridge.handle(&value))
    {
        Ok(response) => response,
        Err(error) => error_wire(&error),
    };
    write_bridge_message(&mut stream, &response)
}

pub fn load_client_config(path: &Path) -> Result<ClientConfig, ClientBridgeError> {
    let contents = match fs::read(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ClientConfig::empty());
        }
        Err(error) => {
            return Err(ClientBridgeError::io(format!(
                "Cannot read Pika client configuration: {error}"
            )));
        }
    };
    let value = serde_json::from_slice(&contents).map_err(|error| {
        ClientBridgeError::invalid(format!("Cannot read Pika client configuration: {error}"))
    })?;
    ClientConfig::from_value(&value)
}

pub fn write_client_config(config: &ClientConfig, path: &Path) -> Result<(), ClientBridgeError> {
    validate_client_config(config)?;
    let parent = path.parent().ok_or_else(|| {
        ClientBridgeError::invalid("Pika client configuration has no parent directory")
    })?;
    fs::create_dir_all(parent).map_err(|error| {
        ClientBridgeError::io(format!("Cannot create Pika client configuration: {error}"))
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700)).map_err(|error| {
            ClientBridgeError::io(format!("Cannot secure Pika client configuration: {error}"))
        })?;
    }
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("client.json");
    let temporary = path.with_file_name(format!(".{name}.{}.tmp", std::process::id()));
    let encoded = serde_json::to_vec_pretty(&config.to_value())
        .map_err(|error| ClientBridgeError::io(error.to_string()))?;
    let mut options = fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| {
        let mut output = options.open(&temporary)?;
        output.write_all(&encoded)?;
        output.write_all(b"\n")?;
        output.sync_all()?;
        fs::rename(&temporary, path)
    })();
    if let Err(error) = result {
        let _ = fs::remove_file(&temporary);
        return Err(ClientBridgeError::io(format!(
            "Cannot replace Pika client configuration: {error}"
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|error| {
            ClientBridgeError::io(format!("Cannot secure Pika client configuration: {error}"))
        })?;
    }
    Ok(())
}

pub fn read_bridge_message<R: Read>(input: &mut R) -> Result<Value, ClientBridgeError> {
    let mut payload = Vec::new();
    let mut chunk = [0_u8; 4096];
    while !payload.contains(&b'\n') {
        let remaining = MAX_BRIDGE_MESSAGE_BYTES + 1 - payload.len();
        if remaining == 0 {
            return Err(ClientBridgeError::invalid(
                "Client bridge request exceeded the safety limit",
            ));
        }
        let read_limit = chunk.len().min(remaining);
        let count = input
            .read(&mut chunk[..read_limit])
            .map_err(|error| ClientBridgeError::io(error.to_string()))?;
        if count == 0 {
            break;
        }
        payload.extend_from_slice(&chunk[..count]);
        if payload.len() > MAX_BRIDGE_MESSAGE_BYTES {
            return Err(ClientBridgeError::invalid(
                "Client bridge request exceeded the safety limit",
            ));
        }
    }
    let Some(newline) = payload.iter().position(|byte| *byte == b'\n') else {
        return Err(ClientBridgeError::invalid(
            "Client bridge requires exactly one JSON line",
        ));
    };
    if payload[newline + 1..]
        .iter()
        .any(|byte| !byte.is_ascii_whitespace())
    {
        return Err(ClientBridgeError::invalid(
            "Client bridge requires exactly one JSON line",
        ));
    }
    serde_json::from_slice::<Value>(&payload[..newline])
        .map_err(|_| ClientBridgeError::invalid("Client bridge received invalid JSON"))
        .and_then(|value| {
            if value.is_object() {
                Ok(value)
            } else {
                Err(ClientBridgeError::invalid(
                    "Client bridge request must be an object",
                ))
            }
        })
}

pub fn write_bridge_message<W: Write>(
    output: &mut W,
    value: &Value,
) -> Result<(), ClientBridgeError> {
    let mut encoded =
        serde_json::to_vec(value).map_err(|error| ClientBridgeError::io(error.to_string()))?;
    encoded.push(b'\n');
    if encoded.len() > MAX_BRIDGE_MESSAGE_BYTES {
        return Err(ClientBridgeError::invalid(
            "Client bridge response exceeded the safety limit",
        ));
    }
    output
        .write_all(&encoded)
        .and_then(|_| output.flush())
        .map_err(|error| ClientBridgeError::io(error.to_string()))
}

fn parse_client_node(node_id: &str, value: &Value) -> Result<ClientNode, ClientBridgeError> {
    let object = value
        .as_object()
        .ok_or_else(|| ClientBridgeError::invalid("Pika client node entry is malformed"))?;
    Ok(ClientNode {
        node_id: node_id.to_owned(),
        alias: client_alias_value(object.get("alias"))?,
        ssh_target: ssh_target(object.get("ssh_target"))?,
        token: bridge_token(object.get("token"))?,
        remote_port: match object.get("remote_port") {
            None | Some(Value::Null) => None,
            value => Some(optional_port(
                value,
                DEFAULT_REMOTE_PORT,
                "reverse bridge port",
            )?),
        },
    })
}

fn validate_client_config(config: &ClientConfig) -> Result<(), ClientBridgeError> {
    if config.version != CLIENT_CONFIG_VERSION {
        return Err(ClientBridgeError::invalid(
            "Unsupported Pika client configuration",
        ));
    }
    let client_id = canonical_uuid_str(&config.client_id, "Invalid client identity")?;
    if client_id != config.client_id {
        return Err(ClientBridgeError::invalid(
            "Client identity must use canonical UUID form",
        ));
    }
    for (raw_key, node) in &config.nodes {
        let key = canonical_uuid_str(raw_key, "Invalid node identity")?;
        let node_id = canonical_uuid_str(&node.node_id, "Invalid node identity")?;
        if key.as_str() != raw_key.as_str()
            || node_id.as_str() != node.node_id.as_str()
            || key != node_id
        {
            return Err(ClientBridgeError::invalid(
                "Pika client node identity is inconsistent",
            ));
        }
        client_alias(&node.alias)?;
        ssh_target_value(&node.ssh_target)?;
        bridge_token_str(&node.token)?;
        if let Some(port) = node.remote_port {
            validate_port(port, "Invalid reverse bridge port")?;
        }
    }
    Ok(())
}

fn validate_loopback_host(host: &str, message: &str) -> Result<(), ClientBridgeError> {
    if matches!(host, "127.0.0.1" | "::1" | "localhost") {
        Ok(())
    } else {
        Err(ClientBridgeError::invalid(message))
    }
}

fn validate_port(port: u16, message: &str) -> Result<u16, ClientBridgeError> {
    if port >= 1024 {
        Ok(port)
    } else {
        Err(ClientBridgeError::invalid(message))
    }
}

fn optional_port(
    value: Option<&Value>,
    default: u16,
    label: &str,
) -> Result<u16, ClientBridgeError> {
    match value {
        None | Some(Value::Null) => Ok(default),
        Some(value) => value
            .as_u64()
            .and_then(|value| u16::try_from(value).ok())
            .filter(|value| *value >= 1024)
            .ok_or_else(|| ClientBridgeError::invalid(format!("{label} is invalid"))),
    }
}

fn canonical_uuid(value: Option<&Value>, label: &str) -> Result<String, ClientBridgeError> {
    canonical_uuid_str(value.and_then(Value::as_str).unwrap_or(""), label)
}

fn canonical_uuid_str(value: &str, label: &str) -> Result<String, ClientBridgeError> {
    Uuid::parse_str(value)
        .map(|value| value.to_string())
        .map_err(|_| ClientBridgeError::invalid(label))
}

fn conversation_identity(provider: Provider, value: &str) -> Result<String, ClientBridgeError> {
    if provider == Provider::Opencode {
        if value.starts_with("ses_")
            && (8..=128).contains(&value.len())
            && value[4..]
                .chars()
                .all(|character| character.is_ascii_alphanumeric())
        {
            Ok(value.to_owned())
        } else {
            Err(ClientBridgeError::invalid("Invalid conversation identity"))
        }
    } else {
        canonical_uuid_str(value, "Invalid conversation identity")
    }
}

fn bridge_token(value: Option<&Value>) -> Result<String, ClientBridgeError> {
    bridge_token_str(value.and_then(Value::as_str).unwrap_or(""))
}

fn bridge_token_str(value: &str) -> Result<String, ClientBridgeError> {
    if value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(value.to_ascii_lowercase())
    } else {
        Err(ClientBridgeError::invalid("Invalid bridge token"))
    }
}

fn client_alias_value(value: Option<&Value>) -> Result<String, ClientBridgeError> {
    client_alias(value.and_then(Value::as_str).unwrap_or(""))
}

fn client_alias(value: &str) -> Result<String, ClientBridgeError> {
    let mut characters = value.chars();
    if !(1..=63).contains(&value.len())
        || !characters
            .next()
            .is_some_and(|ch| ch.is_ascii_alphanumeric())
        || !characters.all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
    {
        Err(ClientBridgeError::invalid(
            "Pika client node alias is malformed",
        ))
    } else {
        Ok(value.to_owned())
    }
}

fn client_label(value: &str) -> Result<String, ClientBridgeError> {
    if value.is_empty() || value.chars().count() > 63 || value.chars().any(char::is_control) {
        Err(ClientBridgeError::invalid("Invalid client label"))
    } else {
        Ok(value.to_owned())
    }
}

fn ssh_target(value: Option<&Value>) -> Result<String, ClientBridgeError> {
    ssh_target_value(value.and_then(Value::as_str).unwrap_or(""))
}

fn ssh_target_value(value: &str) -> Result<String, ClientBridgeError> {
    if value.is_empty()
        || value.len() > 255
        || value.starts_with('-')
        || value.chars().any(|ch| matches!(ch, '\r' | '\n' | '\0'))
    {
        Err(ClientBridgeError::invalid("Invalid SSH target"))
    } else {
        Ok(value.to_owned())
    }
}

pub fn validate_client_ssh_target(value: &str) -> Result<String, ClientBridgeError> {
    ssh_target_value(value)
}

fn optional_text(
    value: Option<&Value>,
    default: &str,
    max: usize,
    label: &str,
) -> Result<String, ClientBridgeError> {
    match value {
        None | Some(Value::Null) => Ok(default.to_owned()),
        value => required_printable_text(value, max, &format!("Invalid {label}")),
    }
}

fn required_printable_text(
    value: Option<&Value>,
    max: usize,
    message: &str,
) -> Result<String, ClientBridgeError> {
    let value = value
        .and_then(Value::as_str)
        .filter(|value| {
            !value.is_empty()
                && value.chars().count() <= max
                && !value.chars().any(char::is_control)
        })
        .ok_or_else(|| ClientBridgeError::invalid(message))?;
    Ok(value.to_owned())
}

fn exact_object<'a>(
    value: &'a Value,
    fields: &[&str],
    message: &str,
) -> Result<&'a Map<String, Value>, ClientBridgeError> {
    let object = value
        .as_object()
        .ok_or_else(|| ClientBridgeError::invalid(message))?;
    if object.len() != fields.len() || object.keys().any(|key| !fields.contains(&key.as_str())) {
        return Err(ClientBridgeError::invalid(message));
    }
    Ok(object)
}

fn error_wire(error: &ClientBridgeError) -> Value {
    json!({
        "type": "error",
        "protocol": BRIDGE_PROTOCOL,
        "version": BRIDGE_VERSION,
        "message": error.message,
    })
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let mut difference = left.len() ^ right.len();
    for index in 0..left.len().max(right.len()) {
        let left = left.get(index).copied().unwrap_or(0);
        let right = right.get(index).copied().unwrap_or(0);
        difference |= usize::from(left ^ right);
    }
    difference == 0
}

fn prefix(value: &str, count: usize) -> &str {
    value.get(..count.min(value.len())).unwrap_or(value)
}

fn sanitized_message(value: &str, max: usize) -> String {
    let mut result = value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .take(max)
        .collect::<String>();
    if result.is_empty() {
        result.push_str("Client bridge error");
    }
    result
}
