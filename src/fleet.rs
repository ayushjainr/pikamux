//! Trusted, bounded federation over the user's existing OpenSSH transport.
//!
//! Discovery is separate from trust: SSH configuration and the local Tailscale
//! status document may suggest addresses, but only a validated handshake and
//! explicit persistence authorize snapshots or actions.

use crate::consult::{
    CancellablePipe, CancellationToken, MAX_QUESTION_BYTES, OwnedChild, owned_child_exited,
    poll_owned_child, terminate_child,
};
use crate::experts::{CardStatus, ExpertMatch, rank_experts, remote_source_availability};
use crate::model::{Candidate, ExpertProfile, FleetNode, Provider, Session, Status};
use crate::providers::Providers;
use crate::store::{MAX_REMOTE_SNAPSHOT_BYTES, Store, StoredExpertProfile};
use crate::update::RemoteInstallBundle;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fmt;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

pub const PROTOCOL_NAME: &str = "pikamux-fleet";
pub const PROTOCOL_VERSION: i64 = 2;
pub const REMOTE_STALE_SECONDS: f64 = 45.0;
pub const MAX_MESSAGE_BYTES: usize = MAX_REMOTE_SNAPSHOT_BYTES;
pub const MAX_STDERR_BYTES: usize = 64 * 1024;
pub const MAX_REMOTE_INSTALL_BYTES: usize = 101 * 1024 * 1024;
pub const MAX_TEXT_CHARS: usize = 8_192;
pub const MAX_EXPERT_ITEMS: usize = 64;
pub const MAX_SNAPSHOT_SESSIONS: usize = 2_000;
pub const MAX_SNAPSHOT_PROFILES: usize = 2_000;
pub const MAX_SNAPSHOT_CARDS: usize = 2_000;
pub const MAX_SNAPSHOT_NOTICES: usize = 16;
pub const MAX_CACHED_FLEET_ROWS: usize = 8_000;
pub const MAX_CACHED_FLEET_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_CACHED_FLEET_NODES: usize = 64;
pub const CAPABILITIES: &[&str] = &[
    "inventory",
    "candidates",
    "adopt",
    "attach",
    "peek",
    "acknowledge",
    "ack-event-bound-v1",
    "untrack",
    "experts",
    "ask-jsonl",
    "provider-opencode",
    "expert-directory-v1",
    "expert-directory-notices-v1",
    "setup-explicit-names-v1",
];
const REQUIRED_CAPABILITIES: &[&str] = &[
    "inventory",
    "candidates",
    "adopt",
    "attach",
    "peek",
    "acknowledge",
    "untrack",
    "experts",
    "ask-jsonl",
    "provider-opencode",
    "expert-directory-v1",
];
const SESSION_FIELDS: &[&str] = &[
    "provider",
    "session_id",
    "name",
    "cwd",
    "branch",
    "status",
    "unread",
    "model",
    "source",
    "managed",
    "error",
    "attention_reason",
    "created_at",
    "updated_at",
    "last_event_at",
    "last_activity_at",
    "live",
    "attached",
    "exact_home",
    "identity_kind",
    "pane_visible",
    "cpu_percent",
    "rss_kb",
];
const CANDIDATE_FIELDS: &[&str] = &[
    "provider",
    "session_id",
    "name",
    "cwd",
    "branch",
    "model",
    "updated_at",
    "live",
    "source",
];
const PROFILE_FIELDS: &[&str] = &[
    "provider",
    "session_id",
    "scope",
    "current_state",
    "topics",
    "artifacts",
    "updated_at",
    "source",
    "scope_updated_at",
    "current_state_updated_at",
];
const CARD_FIELDS: &[&str] = &[
    "provider",
    "session_id",
    "status",
    "detail",
    "watched",
    "availability",
    "current_state_status",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FleetErrorKind {
    InvalidRequest,
    Incompatible,
    Quarantined,
    Unreachable,
    Authentication,
    Missing,
    NotFound,
    OutcomeUnknown,
    Error,
}

impl FleetErrorKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidRequest => "invalid_request",
            Self::Incompatible => "incompatible",
            Self::Quarantined => "quarantined",
            Self::Unreachable => "unreachable",
            Self::Authentication => "auth",
            Self::Missing => "missing",
            Self::NotFound => "not_found",
            Self::OutcomeUnknown => "outcome_unknown",
            Self::Error => "error",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct ConsultationReceipt {
    pub stage: Option<String>,
    pub delivery: Option<String>,
    pub cleanup: Option<String>,
    pub answers_received: Option<u64>,
    pub turn: Option<u64>,
    pub retry_safe: Option<bool>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FleetError {
    pub kind: FleetErrorKind,
    pub message: String,
    pub receipt: Option<Box<ConsultationReceipt>>,
}

impl FleetError {
    pub fn new(kind: FleetErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: sanitize_terminal_text(&message.into()),
            receipt: None,
        }
    }

    fn with_receipt(mut self, value: &Map<String, Value>) -> Self {
        self.receipt = Some(Box::new(ConsultationReceipt {
            stage: value
                .get("stage")
                .and_then(Value::as_str)
                .map(str::to_owned),
            delivery: value
                .get("delivery")
                .and_then(Value::as_str)
                .map(str::to_owned),
            cleanup: value
                .get("cleanup")
                .and_then(Value::as_str)
                .map(str::to_owned),
            answers_received: value.get("answers_received").and_then(Value::as_u64),
            turn: value.get("turn").and_then(Value::as_u64),
            retry_safe: value.get("retry_safe").and_then(Value::as_bool),
        }));
        self
    }
}

impl fmt::Display for FleetError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}
impl std::error::Error for FleetError {}
impl From<anyhow::Error> for FleetError {
    fn from(error: anyhow::Error) -> Self {
        Self::new(FleetErrorKind::Error, error.to_string())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NodeCandidate {
    pub alias: String,
    pub ssh_target: String,
    pub sources: Vec<String>,
    pub hostname: Option<String>,
    pub online: Option<bool>,
    pub os_name: Option<String>,
}
impl NodeCandidate {
    pub fn key(&self) -> String {
        self.ssh_target.to_lowercase()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NodeDiscoveryReport {
    pub candidates: Vec<NodeCandidate>,
    pub ssh_aliases: usize,
    pub ssh_config_files: usize,
    pub tailscale_total: usize,
    pub tailscale_compatible: usize,
    pub excluded_unsupported_os: usize,
    pub excluded_no_target: usize,
    pub tailscale_error: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FleetSession {
    pub node_id: String,
    pub node_name: String,
    pub session: Session,
    pub stale: bool,
    pub remote_error: Option<String>,
    pub seen_at: f64,
    pub card_status: Option<String>,
    pub card_detail: Option<String>,
    pub watched: bool,
    pub availability: Option<String>,
    pub scope_updated_at: Option<f64>,
    pub current_state_updated_at: Option<f64>,
    pub current_state_status: Option<String>,
}

/// A machine-scoped explanation for cached rows omitted from an aggregate
/// view. Exact-machine reads are never truncated by the aggregate budget.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct FleetCacheNotice {
    pub node_id: String,
    pub node_name: String,
    pub kind: String,
    pub omitted_rows: usize,
    pub message: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct FleetExpertDirectory {
    pub matches: Vec<ExpertMatch>,
    pub notices: Vec<FleetCacheNotice>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct CachedFleetSessions {
    pub sessions: Vec<FleetSession>,
    pub notices: Vec<FleetCacheNotice>,
}

impl FleetSession {
    pub fn key(&self) -> (&str, Provider, &str) {
        (
            &self.node_id,
            self.session.provider,
            &self.session.session_id,
        )
    }
    pub fn qualified_name(&self) -> String {
        format!("{}@{}", self.session.display_name(), self.node_name)
    }
    pub fn needs_attention(&self) -> bool {
        !self.stale && self.session.needs_attention()
    }

    pub fn source_availability(&self) -> &str {
        remote_source_availability(self.stale, self.availability.as_deref())
    }

    fn require_consultable(&self) -> Result<(), FleetError> {
        let availability = self.source_availability();
        if crate::experts::source_is_available(availability) {
            Ok(())
        } else {
            Err(FleetError::new(
                FleetErrorKind::InvalidRequest,
                format!(
                    "Cannot consult {}: {availability}. No question was sent; watching is unchanged.",
                    self.qualified_name()
                ),
            ))
        }
    }
}

/// Choose at most one due remote refresh. The board renders cached local state
/// before dispatching this work to its background worker.
pub fn next_remote_node<'a>(
    nodes: &'a [FleetNode],
    selected_node_id: Option<&str>,
    timestamp: f64,
    manual: bool,
) -> Option<&'a FleetNode> {
    let attempt_order = |node: &FleetNode| {
        if node.last_attempt_at > timestamp {
            0
        } else {
            node.last_attempt_at.to_bits()
        }
    };
    nodes
        .iter()
        .filter(|node| {
            manual
                // Recover legacy remote-clock entries and local clock rollback
                // instead of deferring their next observation into the future.
                || node.last_attempt_at > timestamp
                || timestamp - node.last_attempt_at
                    >= if node.status == "ready" { 15.0 } else { 30.0 }
        })
        .min_by(|left, right| {
            let left_key = (
                !(manual && selected_node_id == Some(&left.node_id)),
                attempt_order(left),
                &left.node_id,
            );
            let right_key = (
                !(manual && selected_node_id == Some(&right.node_id)),
                attempt_order(right),
                &right.node_id,
            );
            left_key.cmp(&right_key)
        })
}

pub fn machine_alias(value: &str) -> Result<String, FleetError> {
    let mut result = String::with_capacity(value.len().min(63));
    let mut last_dash = false;
    for ch in value.to_lowercase().chars() {
        let out = if ch.is_ascii_lowercase() || ch.is_ascii_digit() || matches!(ch, '.' | '_' | '-')
        {
            ch
        } else {
            '-'
        };
        if out == '-' && last_dash {
            continue;
        }
        result.push(out);
        last_dash = out == '-';
        if result.len() >= 63 {
            break;
        }
    }
    let result = result.trim_matches(['-', '.', '_']).to_owned();
    if !result.is_empty()
        && result.len() <= 63
        && result.chars().all(|ch| {
            ch.is_ascii_lowercase() || ch.is_ascii_digit() || matches!(ch, '.' | '_' | '-')
        })
    {
        Ok(result)
    } else {
        Err(FleetError::new(
            FleetErrorKind::InvalidRequest,
            format!("Invalid Pika machine alias {value:?}"),
        ))
    }
}

pub fn suggest_alias(target: &str) -> Result<String, FleetError> {
    let host = target.rsplit_once('@').map_or(target, |(_, host)| host);
    let host = if let Some(rest) = host.strip_prefix('[') {
        rest.split_once(']').map_or(rest, |(host, _)| host)
    } else if host.matches(':').count() == 1 {
        host.split_once(':').map_or(host, |(host, _)| host)
    } else {
        host
    };
    machine_alias(host.split('.').next().unwrap_or(host))
}

pub fn validate_ssh_target(value: &str) -> Result<&str, FleetError> {
    if value.is_empty()
        || value.starts_with('-')
        || value.chars().any(|ch| matches!(ch, '\r' | '\n' | '\0'))
    {
        Err(FleetError::new(
            FleetErrorKind::InvalidRequest,
            format!("Unsafe SSH target {value:?}"),
        ))
    } else {
        Ok(value)
    }
}

pub fn discover_ssh_candidates(root: &Path) -> Vec<NodeCandidate> {
    let mut found = BTreeMap::<String, NodeCandidate>::new();
    for path in ssh_config_files(root) {
        let Ok(contents) = read_bounded(&path, 1024 * 1024) else {
            continue;
        };
        for raw in contents.lines() {
            let Ok(parts) = shell_words::split(raw) else {
                continue;
            };
            if parts.len() < 2 || !parts[0].eq_ignore_ascii_case("host") {
                continue;
            }
            for target in &parts[1..] {
                if target.starts_with('!')
                    || target.chars().any(|ch| matches!(ch, '*' | '?' | '[' | ']'))
                {
                    continue;
                }
                let Ok(alias) = machine_alias(target) else {
                    continue;
                };
                if validate_ssh_target(target).is_err() {
                    continue;
                }
                found.insert(
                    target.to_lowercase(),
                    NodeCandidate {
                        alias,
                        ssh_target: target.clone(),
                        sources: vec!["ssh-config".to_owned()],
                        hostname: Some(target.clone()),
                        online: None,
                        os_name: None,
                    },
                );
            }
        }
    }
    found.into_values().collect()
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TailscaleSummary {
    pub total: usize,
    pub compatible: usize,
    pub unsupported_os: usize,
    pub no_target: usize,
    pub error: Option<String>,
}

pub fn discover_tailscale_candidates(
    executable: &Path,
    timeout: Duration,
) -> (Vec<NodeCandidate>, TailscaleSummary) {
    let output = match run_bounded_command(
        Command::new(executable).arg("status").arg("--json"),
        None,
        timeout,
        MAX_MESSAGE_BYTES,
        MAX_STDERR_BYTES,
    ) {
        Ok(output) if output.status.success() => output,
        Ok(output) => {
            return (
                Vec::new(),
                TailscaleSummary {
                    error: Some(sanitize_terminal_text(
                        std::str::from_utf8(&output.stderr).unwrap_or("tailscale status failed"),
                    )),
                    ..TailscaleSummary::default()
                },
            );
        }
        Err(error) => {
            return (
                Vec::new(),
                TailscaleSummary {
                    error: Some(error.message),
                    ..TailscaleSummary::default()
                },
            );
        }
    };
    let Ok(payload) = serde_json::from_slice::<Value>(&output.stdout) else {
        return (
            Vec::new(),
            TailscaleSummary {
                error: Some("invalid Tailscale status JSON".to_owned()),
                ..TailscaleSummary::default()
            },
        );
    };
    let records: Vec<&Value> = match payload.get("Peer") {
        Some(Value::Object(values)) => values.values().collect(),
        Some(Value::Array(values)) => values.iter().collect(),
        _ => Vec::new(),
    };
    let mut summary = TailscaleSummary::default();
    let mut found = BTreeMap::<String, NodeCandidate>::new();
    for raw in records {
        let Some(record) = raw.as_object() else {
            continue;
        };
        summary.total += 1;
        let os_name = record.get("OS").and_then(Value::as_str).unwrap_or("");
        if !os_name.is_empty()
            && !matches!(
                os_name.to_ascii_lowercase().as_str(),
                "linux" | "macos" | "darwin"
            )
        {
            summary.unsupported_os += 1;
            continue;
        }
        let dns = record
            .get("DNSName")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim_end_matches('.');
        let host = record
            .get("HostName")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        let ip = record
            .get("TailscaleIPs")
            .and_then(Value::as_array)
            .and_then(|items| items.first())
            .and_then(Value::as_str)
            .unwrap_or("");
        let target = if dns.is_empty() { ip } else { dns };
        if target.is_empty() {
            summary.no_target += 1;
            continue;
        }
        let Ok(alias) = machine_alias(if host.is_empty() {
            dns.split('.').next().unwrap_or(ip)
        } else {
            host
        }) else {
            continue;
        };
        if validate_ssh_target(target).is_err() {
            continue;
        }
        summary.compatible += 1;
        found.insert(
            target.to_lowercase(),
            NodeCandidate {
                alias,
                ssh_target: target.to_owned(),
                sources: vec!["tailscale".to_owned()],
                hostname: Some(if host.is_empty() { dns } else { host }.to_owned()),
                online: record.get("Online").and_then(Value::as_bool),
                os_name: (!os_name.is_empty()).then(|| os_name.to_owned()),
            },
        );
    }
    (found.into_values().collect(), summary)
}

pub fn discover_node_candidates(
    store: &Store,
    ssh_root: &Path,
    tailscale_executable: &Path,
    tailscale_timeout: Duration,
) -> Result<NodeDiscoveryReport, FleetError> {
    let ssh_files = ssh_config_files(ssh_root);
    let ssh = discover_ssh_candidates(ssh_root);
    let (tailscale, summary) =
        discover_tailscale_candidates(tailscale_executable, tailscale_timeout);
    let existing: HashSet<String> = store
        .list_nodes()?
        .into_iter()
        .map(|node| node.ssh_target.to_lowercase())
        .collect();
    let ignored = store.ignored_node_candidate_keys()?;
    let mut merged = BTreeMap::<String, NodeCandidate>::new();
    for item in ssh.iter().chain(tailscale.iter()) {
        let key = item.key();
        if existing.contains(&key) || ignored.contains(&key) {
            continue;
        }
        if let Some(previous) = merged.get_mut(&key) {
            for source in &item.sources {
                if !previous.sources.contains(source) {
                    previous.sources.push(source.clone());
                }
            }
            if previous.hostname.is_none() {
                previous.hostname.clone_from(&item.hostname);
            }
            if previous.online.is_none() {
                previous.online = item.online;
            }
            if previous.os_name.is_none() {
                previous.os_name.clone_from(&item.os_name);
            }
        } else {
            merged.insert(key, item.clone());
        }
    }
    let mut candidates: Vec<_> = merged.into_values().collect();
    candidates.sort_by_key(|item| {
        (
            !item.sources.iter().any(|source| source == "ssh-config"),
            item.alias.to_lowercase(),
            item.ssh_target.to_lowercase(),
        )
    });
    let mut aliases = BTreeSet::new();
    for item in &mut candidates {
        let base = item.alias.clone();
        let mut suffix = 2;
        while !aliases.insert(item.alias.clone()) {
            let tail = format!("-{suffix}");
            let keep = 63usize.saturating_sub(tail.len()).min(base.len());
            item.alias = format!("{}{}", &base[..keep], tail);
            suffix += 1;
        }
    }
    Ok(NodeDiscoveryReport {
        candidates,
        ssh_aliases: ssh.len(),
        ssh_config_files: ssh_files.len(),
        tailscale_total: summary.total,
        tailscale_compatible: summary.compatible,
        excluded_unsupported_os: summary.unsupported_os,
        excluded_no_target: summary.no_target,
        tailscale_error: summary.error,
    })
}

pub fn session_to_wire(session: &Session, extended: bool) -> Value {
    let mut result = json!({
        "provider": session.provider.as_str(), "session_id": session.session_id,
        "name": session.name, "cwd": session.cwd, "branch": session.branch,
        "status": session.status.as_str(), "unread": session.unread, "model": session.model,
        "source": session.source, "managed": session.managed, "error": session.error,
        "attention_reason": session.attention_reason, "created_at": session.created_at,
        "updated_at": session.updated_at, "last_event_at": session.last_event_at,
        "last_activity_at": session.last_activity_at, "live": session.live,
        "attached": session.attached, "exact_home": session.has_exact_home(),
        "identity_kind": if session.session_id.starts_with("unbound:") { "placeholder" } else { "conversation" },
        "pane_visible": session.tmux_pane.is_some() || session.tmux_session.is_some(),
        "cpu_percent": session.cpu_percent, "rss_kb": session.rss_kb,
    });
    if extended {
        result.as_object_mut().expect("object").insert(
            "active_thread_id".to_owned(),
            session
                .active_thread_id
                .clone()
                .map_or(Value::Null, Value::String),
        );
    }
    result
}

pub fn candidate_to_wire(candidate: &Candidate) -> Value {
    json!({
        "provider": candidate.provider.as_str(), "session_id": candidate.session_id,
        "name": candidate.name, "cwd": candidate.cwd, "branch": candidate.branch,
        "model": candidate.model, "updated_at": candidate.updated_at, "live": candidate.live,
        "source": candidate.source,
    })
}

pub fn profile_to_wire(profile: &ExpertProfile, extended: bool) -> Value {
    let mut value = json!({
        "provider": profile.provider.as_str(), "session_id": profile.session_id,
        "scope": profile.summary, "current_state": profile.current_state,
        "topics": profile.topics, "artifacts": profile.artifacts,
        "updated_at": profile.updated_at, "source": profile.source,
    });
    if extended {
        let object = value.as_object_mut().expect("object");
        object.insert(
            "scope_updated_at".to_owned(),
            json!(profile.scope_updated_at),
        );
        object.insert(
            "current_state_updated_at".to_owned(),
            json!(profile.current_state_updated_at),
        );
    }
    value
}

pub fn validate_snapshot(
    value: &Value,
    expected_node_id: Option<&str>,
) -> Result<Value, FleetError> {
    let object = object(value, "Remote did not return a complete snapshot")?;
    let mut expected = set(&[
        "type",
        "protocol",
        "version",
        "node_id",
        "machine",
        "captured_at",
        "sessions",
        "profiles",
        "cards",
    ]);
    if object.contains_key("expert_sessions") {
        expected.insert("expert_sessions");
    }
    if object.contains_key("directory_notices") {
        expected.insert("directory_notices");
    }
    exact_fields(object, &expected, "Remote snapshot envelope is malformed")?;
    if object.get("type").and_then(Value::as_str) != Some("snapshot")
        || object.get("protocol").and_then(Value::as_str) != Some(PROTOCOL_NAME)
        || object.get("version").and_then(Value::as_i64) != Some(PROTOCOL_VERSION)
    {
        return Err(FleetError::new(
            FleetErrorKind::Incompatible,
            format!("Remote Pika is incompatible with fleet protocol {PROTOCOL_VERSION}"),
        ));
    }
    let node_id = canonical_uuid(
        object.get("node_id"),
        "Remote Pika returned an invalid node identity",
    )?;
    if expected_node_id.is_some_and(|expected| expected != node_id) {
        return Err(FleetError::new(
            FleetErrorKind::Quarantined,
            format!(
                "NODE IDENTITY CHANGED: expected {}, received {}",
                prefix(expected_node_id.unwrap_or_default(), 8),
                prefix(&node_id, 8),
            ),
        ));
    }
    let machine = required_text(
        object.get("machine"),
        "Remote snapshot has an invalid machine name",
        63,
    )?;
    let captured_at = finite_nonnegative(
        object.get("captured_at"),
        "Remote snapshot has an invalid timestamp",
    )?;
    let sessions = validate_session_array(
        object.get("sessions"),
        true,
        "Remote snapshot has no complete session list",
        MAX_SNAPSHOT_SESSIONS,
    )?;
    let experts = if let Some(raw) = object.get("expert_sessions") {
        validate_session_array(
            Some(raw),
            true,
            "Remote expert inventory is malformed",
            MAX_SNAPSHOT_SESSIONS,
        )?
    } else {
        Vec::new()
    };
    if sessions.len().saturating_add(experts.len()) > MAX_SNAPSHOT_SESSIONS {
        return Err(FleetError::new(
            FleetErrorKind::Incompatible,
            format!(
                "Remote session inventories exceed the {MAX_SNAPSHOT_SESSIONS}-record safety limit"
            ),
        ));
    }
    let mut identities = HashSet::new();
    for raw in sessions.iter().chain(experts.iter()) {
        let session = session_from_wire(raw)?;
        if !identities.insert((session.provider, session.session_id)) {
            return Err(FleetError::new(
                FleetErrorKind::Incompatible,
                "Remote snapshot repeats a session identity",
            ));
        }
    }
    let profiles = validate_profiles(object.get("profiles"))?;
    let cards = validate_cards(object.get("cards"))?;
    let directory_notices = validate_directory_notices(object.get("directory_notices"))?;
    let mut normalized = json!({
        "type":"snapshot", "protocol":PROTOCOL_NAME, "version":PROTOCOL_VERSION,
        "node_id":node_id, "machine":machine, "captured_at":captured_at,
        "sessions":sessions, "profiles":profiles, "cards":cards,
    });
    if object.contains_key("expert_sessions") {
        normalized
            .as_object_mut()
            .expect("object")
            .insert("expert_sessions".to_owned(), Value::Array(experts));
    }
    if object.contains_key("directory_notices") {
        normalized.as_object_mut().expect("object").insert(
            "directory_notices".to_owned(),
            Value::Array(directory_notices),
        );
    }
    Ok(normalized)
}

/// Bound a locally generated snapshot before it reaches the fleet transport.
///
/// Expert-bearing rows are admitted before ordinary board rows within the
/// shared 2,000-row contract. The reserved tail leaves room for truthful
/// omission receipts without risking the wire limit.
pub fn bound_snapshot_for_transport(value: Value) -> Result<Value, FleetError> {
    const NOTICE_RESERVE_BYTES: usize = 64 * 1024;
    let object = value.as_object().ok_or_else(|| {
        FleetError::new(
            FleetErrorKind::Error,
            "Local fleet snapshot is not an object",
        )
    })?;
    let had_expert_sessions = object.contains_key("expert_sessions");
    let watched_rows = object
        .get("sessions")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let expert_rows = object
        .get("expert_sessions")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let profiles = keyed_values(object.get("profiles"));
    let cards = keyed_values(object.get("cards"));

    let mut result = Value::Object(
        object
            .iter()
            .filter(|(key, _)| {
                !matches!(
                    key.as_str(),
                    "sessions" | "expert_sessions" | "profiles" | "cards" | "directory_notices"
                )
            })
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
    );
    let result_object = result.as_object_mut().expect("snapshot object");
    result_object.insert("sessions".to_owned(), Value::Array(Vec::new()));
    result_object.insert("profiles".to_owned(), Value::Array(Vec::new()));
    result_object.insert("cards".to_owned(), Value::Array(Vec::new()));
    if had_expert_sessions {
        result_object.insert("expert_sessions".to_owned(), Value::Array(Vec::new()));
    }
    let mut used_bytes = serde_json::to_vec(&result)
        .map_err(|error| FleetError::new(FleetErrorKind::Error, error.to_string()))?
        .len();
    let payload_budget = MAX_MESSAGE_BYTES.saturating_sub(NOTICE_RESERVE_BYTES);
    let value_cost = |value: &Value| -> Result<usize, FleetError> {
        serde_json::to_vec(value)
            .map_err(|error| FleetError::new(FleetErrorKind::Error, error.to_string()))?
            .len()
            .checked_add(1)
            .ok_or_else(|| {
                FleetError::new(
                    FleetErrorKind::Incompatible,
                    "Fleet snapshot size overflowed its safety counter",
                )
            })
    };

    let mut selected_watched = Vec::new();
    let mut selected_experts = Vec::new();
    let mut selected_profiles = Vec::new();
    let mut selected_cards = Vec::new();
    let mut selected_keys = BTreeSet::new();
    let mut selected_card_keys = BTreeSet::new();
    let mut expert_keys = Vec::new();
    let mut omitted_expert_keys = BTreeSet::new();

    // An untracked expert is the easiest row to lose behind a large watched
    // inventory, so it receives the first admission opportunity.
    for (is_untracked, rows) in [(true, &expert_rows), (false, &watched_rows)] {
        for row in rows {
            let Some(key) = wire_value_key(row) else {
                continue;
            };
            let Some(profile) = profiles.get(&key) else {
                continue;
            };
            if selected_keys.contains(&key) {
                continue;
            }
            let row_cost = value_cost(row)?;
            let profile_cost = value_cost(profile)?;
            let card = cards.get(&key);
            let card_cost = card.map(value_cost).transpose()?.unwrap_or(0);
            let fits = selected_watched.len() + selected_experts.len() < MAX_SNAPSHOT_SESSIONS
                && selected_profiles.len() < MAX_SNAPSHOT_PROFILES
                && card.is_none_or(|_| selected_cards.len() < MAX_SNAPSHOT_CARDS)
                && used_bytes
                    .checked_add(row_cost)
                    .and_then(|value| value.checked_add(profile_cost))
                    .and_then(|value| value.checked_add(card_cost))
                    .is_some_and(|value| value <= payload_budget);
            if !fits {
                omitted_expert_keys.insert(key);
                continue;
            }
            if is_untracked {
                selected_experts.push(row.clone());
            } else {
                selected_watched.push(row.clone());
            }
            selected_profiles.push(profile.clone());
            if let Some(card) = card {
                selected_cards.push(card.clone());
                selected_card_keys.insert(key.clone());
            }
            used_bytes += row_cost + profile_cost + card_cost;
            selected_keys.insert(key.clone());
            expert_keys.push(key);
        }
    }

    // Fill remaining watched capacity only after every expert-bearing row has
    // had a chance to claim space.
    for row in &watched_rows {
        let Some(key) = wire_value_key(row) else {
            continue;
        };
        if selected_keys.contains(&key) || profiles.contains_key(&key) {
            continue;
        }
        let cost = value_cost(row)?;
        if selected_watched.len() + selected_experts.len() >= MAX_SNAPSHOT_SESSIONS
            || used_bytes
                .checked_add(cost)
                .is_none_or(|value| value > payload_budget)
        {
            continue;
        }
        selected_watched.push(row.clone());
        selected_keys.insert(key);
        used_bytes += cost;
    }

    // Card state is useful but never outranks the identity+profile pair that
    // makes an expert discoverable. Expert cards are considered first.
    let mut card_keys = expert_keys;
    for key in &selected_keys {
        if !card_keys.contains(key) {
            card_keys.push(key.clone());
        }
    }
    for key in card_keys {
        if selected_card_keys.contains(&key) {
            continue;
        }
        let Some(card) = cards.get(&key) else {
            continue;
        };
        let cost = value_cost(card)?;
        if selected_cards.len() >= MAX_SNAPSHOT_CARDS
            || used_bytes
                .checked_add(cost)
                .is_none_or(|value| value > payload_budget)
        {
            continue;
        }
        selected_cards.push(card.clone());
        used_bytes += cost;
    }

    let retained_rows = selected_watched.len() + selected_experts.len();
    let omitted_rows = watched_rows
        .len()
        .saturating_add(expert_rows.len())
        .saturating_sub(retained_rows);
    let mut notices = Vec::new();
    if !omitted_expert_keys.is_empty() {
        notices.push(json!({
            "kind":"expert-directory-incomplete",
            "omitted_rows":omitted_expert_keys.len(),
            "message":format!(
                "{} expert card(s) omitted by the remote snapshot safety budget",
                omitted_expert_keys.len()
            ),
        }));
    }
    let ordinary_omitted = omitted_rows.saturating_sub(omitted_expert_keys.len());
    if ordinary_omitted > 0 {
        notices.push(json!({
            "kind":"snapshot-incomplete",
            "omitted_rows":ordinary_omitted,
            "message":format!(
                "{ordinary_omitted} watched conversation(s) omitted by the remote snapshot safety budget"
            ),
        }));
    }

    let result_object = result.as_object_mut().expect("snapshot object");
    result_object.insert("sessions".to_owned(), Value::Array(selected_watched));
    result_object.insert("profiles".to_owned(), Value::Array(selected_profiles));
    result_object.insert("cards".to_owned(), Value::Array(selected_cards));
    if had_expert_sessions {
        result_object.insert("expert_sessions".to_owned(), Value::Array(selected_experts));
    }
    if !notices.is_empty() {
        result_object.insert("directory_notices".to_owned(), Value::Array(notices));
    }
    let encoded_len = serde_json::to_vec(&result)
        .map_err(|error| FleetError::new(FleetErrorKind::Error, error.to_string()))?
        .len();
    if encoded_len > MAX_MESSAGE_BYTES {
        return Err(FleetError::new(
            FleetErrorKind::Incompatible,
            "Bounded fleet snapshot still exceeds the transport safety limit",
        ));
    }
    Ok(result)
}

fn wire_value_key(value: &Value) -> Option<(String, String)> {
    let object = value.as_object()?;
    Some((
        object.get("provider")?.as_str()?.to_owned(),
        object.get("session_id")?.as_str()?.to_owned(),
    ))
}

fn validate_directory_notices(value: Option<&Value>) -> Result<Vec<Value>, FleetError> {
    let Some(values) = value.and_then(Value::as_array) else {
        return if value.is_none() {
            Ok(Vec::new())
        } else {
            Err(FleetError::new(
                FleetErrorKind::Incompatible,
                "Remote expert-directory notices are malformed",
            ))
        };
    };
    if values.len() > MAX_SNAPSHOT_NOTICES {
        return Err(FleetError::new(
            FleetErrorKind::Incompatible,
            "Remote expert-directory notices exceed the safety limit",
        ));
    }
    let fields = set(&["kind", "omitted_rows", "message"]);
    for value in values {
        let object = object(value, "Remote expert-directory notice is malformed")?;
        exact_fields(
            object,
            &fields,
            "Remote expert-directory notice is malformed",
        )?;
        if !matches!(
            object.get("kind").and_then(Value::as_str),
            Some("expert-directory-incomplete" | "snapshot-incomplete")
        ) {
            return Err(FleetError::new(
                FleetErrorKind::Incompatible,
                "Remote expert-directory notice has an unsupported kind",
            ));
        }
        let omitted = object
            .get("omitted_rows")
            .and_then(Value::as_u64)
            .filter(|value| *value > 0)
            .ok_or_else(|| {
                FleetError::new(
                    FleetErrorKind::Incompatible,
                    "Remote expert-directory notice has an invalid omitted count",
                )
            })?;
        usize::try_from(omitted).map_err(|_| {
            FleetError::new(
                FleetErrorKind::Incompatible,
                "Remote expert-directory notice count is too large",
            )
        })?;
        required_text(
            object.get("message"),
            "Remote expert-directory notice has an invalid message",
            1_024,
        )?;
    }
    Ok(values.clone())
}

fn session_from_wire(value: &Value) -> Result<Session, FleetError> {
    let object = object(value, "Remote session record is not an object")?;
    let mut allowed = set(SESSION_FIELDS);
    allowed.insert("active_thread_id");
    if object.keys().any(|key| !allowed.contains(key.as_str())) {
        return Err(FleetError::new(
            FleetErrorKind::Incompatible,
            "Remote session contains unsupported fields",
        ));
    }
    let provider = parse_provider(object.get("provider"))?;
    let session_id = valid_session_id(
        object.get("session_id"),
        "Remote session has an invalid conversation identity",
    )?;
    let status = parse_status(object.get("status"))?;
    let active_thread_id = optional_session_id(
        object.get("active_thread_id"),
        "Remote session has an invalid active conversation identity",
    )?;
    let identity_kind = object
        .get("identity_kind")
        .and_then(Value::as_str)
        .unwrap_or(if session_id.starts_with("unbound:") {
            "placeholder"
        } else {
            "conversation"
        });
    if !matches!(identity_kind, "conversation" | "placeholder")
        || (identity_kind == "placeholder") != session_id.starts_with("unbound:")
        || (identity_kind == "placeholder" && status != Status::Unbound)
    {
        return Err(FleetError::new(
            FleetErrorKind::Incompatible,
            "Remote session identity kind contradicts its identifier",
        ));
    }
    if identity_kind == "conversation" && !Providers::valid_id(provider, &session_id) {
        return Err(FleetError::new(
            FleetErrorKind::Incompatible,
            "Remote session has a noncanonical provider conversation identity",
        ));
    }
    if active_thread_id
        .as_deref()
        .is_some_and(|value| !Providers::valid_id(provider, value))
    {
        return Err(FleetError::new(
            FleetErrorKind::Incompatible,
            "Remote session has a noncanonical active conversation identity",
        ));
    }
    let boolean = |field: &str, default: bool| -> Result<bool, FleetError> {
        match object.get(field) {
            None => Ok(default),
            Some(value) => value.as_bool().ok_or_else(|| {
                FleetError::new(
                    FleetErrorKind::Incompatible,
                    format!("Remote session field {field} has the wrong type"),
                )
            }),
        }
    };
    let number = |field: &str| -> Result<f64, FleetError> {
        match object.get(field) {
            None | Some(Value::Null) => Ok(0.0),
            value => finite_nonnegative(
                value,
                &format!("Remote session field {field} has an invalid value"),
            ),
        }
    };
    let rss_kb = match object.get("rss_kb") {
        None | Some(Value::Null) => None,
        Some(Value::Number(value)) => value.as_i64().filter(|value| *value >= 0),
        _ => None,
    };
    if object.get("rss_kb").is_some_and(|value| !value.is_null()) && rss_kb.is_none() {
        return Err(FleetError::new(
            FleetErrorKind::Incompatible,
            "Remote session field rss_kb must be a nonnegative integer",
        ));
    }
    Ok(Session {
        provider,
        session_id,
        name: wire_string(object.get("name"), "name", MAX_TEXT_CHARS)?,
        cwd: wire_string(object.get("cwd"), "cwd", MAX_TEXT_CHARS)?,
        branch: wire_string(object.get("branch"), "branch", MAX_TEXT_CHARS)?,
        transcript_path: None,
        tmux_session: boolean("pane_visible", false)?.then(|| "remote".to_owned()),
        tmux_pane: None,
        root_pid: None,
        status,
        unread: boolean("unread", false)?,
        model: wire_string(object.get("model"), "model", MAX_TEXT_CHARS)?,
        source: wire_string(object.get("source"), "source", MAX_TEXT_CHARS)?
            .unwrap_or_else(|| "remote".to_owned()),
        managed: boolean("managed", true)?,
        error: wire_string(object.get("error"), "error", MAX_TEXT_CHARS)?,
        attention_reason: wire_string(
            object.get("attention_reason"),
            "attention_reason",
            MAX_TEXT_CHARS,
        )?,
        created_at: number("created_at")?,
        updated_at: number("updated_at")?,
        last_event_at: number("last_event_at")?,
        last_activity_at: number("last_activity_at")?,
        live: boolean("live", false)?,
        attached: boolean("attached", false)?,
        home_state: if boolean("exact_home", false)? {
            "exact-live".to_owned()
        } else {
            "unknown".to_owned()
        },
        cpu_percent: optional_nonnegative(object.get("cpu_percent"), "cpu_percent")?,
        rss_kb,
        input_tokens: None,
        output_tokens: None,
        cached_input_tokens: None,
        cache_write_tokens: None,
        total_tokens: None,
        estimated_cost_usd: None,
        active_thread_id,
    })
}

fn validate_profiles(value: Option<&Value>) -> Result<Vec<Value>, FleetError> {
    let values = value.and_then(Value::as_array).ok_or_else(|| {
        FleetError::new(
            FleetErrorKind::Incompatible,
            "Remote expert profiles are malformed",
        )
    })?;
    if values.len() > MAX_SNAPSHOT_PROFILES {
        return Err(FleetError::new(
            FleetErrorKind::Incompatible,
            format!(
                "Remote expert profiles exceed the {MAX_SNAPSHOT_PROFILES}-record safety limit"
            ),
        ));
    }
    let mut seen = HashSet::new();
    for value in values {
        let object = object(value, "Remote expert profile is malformed")?;
        if object
            .keys()
            .any(|key| !PROFILE_FIELDS.contains(&key.as_str()))
        {
            return Err(FleetError::new(
                FleetErrorKind::Incompatible,
                "Remote expert profile is malformed",
            ));
        }
        let provider = parse_provider(object.get("provider"))?;
        let session_id = valid_session_id(
            object.get("session_id"),
            "Remote expert profile has an invalid conversation identity",
        )?;
        if !seen.insert((provider, session_id)) {
            return Err(FleetError::new(
                FleetErrorKind::Incompatible,
                "Remote expert profile is duplicated",
            ));
        }
        expert_text(
            object.get("scope"),
            "Remote expert scope has the wrong type or size",
            MAX_TEXT_CHARS,
        )?;
        expert_text(
            object.get("current_state"),
            "Remote expert current state has the wrong type or size",
            MAX_TEXT_CHARS,
        )?;
        string_array(
            object.get("topics"),
            "Remote expert topics are malformed",
            MAX_EXPERT_ITEMS,
        )?;
        string_array(
            object.get("artifacts"),
            "Remote expert artifacts are malformed",
            MAX_EXPERT_ITEMS,
        )?;
        finite_nonnegative(
            object.get("updated_at"),
            "Remote expert profile has an invalid timestamp",
        )?;
        if let Some(value) = object.get("scope_updated_at") {
            finite_nonnegative(Some(value), "Remote expert clock is invalid")?;
        }
        if let Some(value) = object.get("current_state_updated_at") {
            finite_nonnegative(Some(value), "Remote expert clock is invalid")?;
        }
        expert_text(
            object.get("source"),
            "Remote expert source has the wrong type or size",
            MAX_TEXT_CHARS,
        )?;
    }
    Ok(values.clone())
}

fn validate_cards(value: Option<&Value>) -> Result<Vec<Value>, FleetError> {
    let values = value.and_then(Value::as_array).ok_or_else(|| {
        FleetError::new(
            FleetErrorKind::Incompatible,
            "Remote thread profiles are malformed",
        )
    })?;
    if values.len() > MAX_SNAPSHOT_CARDS {
        return Err(FleetError::new(
            FleetErrorKind::Incompatible,
            format!("Remote thread profiles exceed the {MAX_SNAPSHOT_CARDS}-record safety limit"),
        ));
    }
    let mut seen = HashSet::new();
    for value in values {
        let object = object(value, "Remote thread profile is malformed")?;
        if object
            .keys()
            .any(|key| !CARD_FIELDS.contains(&key.as_str()))
        {
            return Err(FleetError::new(
                FleetErrorKind::Incompatible,
                "Remote thread profile is malformed",
            ));
        }
        let provider = parse_provider(object.get("provider"))?;
        let session_id = valid_session_id(
            object.get("session_id"),
            "Remote thread profile has an invalid conversation identity",
        )?;
        if !seen.insert((provider, session_id)) {
            return Err(FleetError::new(
                FleetErrorKind::Incompatible,
                "Remote thread profile is duplicated",
            ));
        }
        if !matches!(
            object.get("status").and_then(Value::as_str),
            Some("CURRENT" | "STALE" | "MISSING" | "UNKNOWN")
        ) {
            return Err(FleetError::new(
                FleetErrorKind::Incompatible,
                "Remote thread profile has an invalid state",
            ));
        }
        required_text(
            object.get("detail"),
            "Remote thread profile detail has the wrong type or size",
            MAX_TEXT_CHARS,
        )?;
        if object
            .get("watched")
            .is_some_and(|value| !value.is_boolean())
        {
            return Err(FleetError::new(
                FleetErrorKind::Incompatible,
                "Remote watched flag is invalid",
            ));
        }
        if let Some(value) = object.get("availability")
            && !matches!(
                value.as_str(),
                Some(
                    "source-available"
                        | "source-unavailable"
                        | "archived"
                        | "deleted"
                        | "provider-unavailable"
                        | "excluded-worker"
                        | "requires-reconciliation"
                )
            )
        {
            return Err(FleetError::new(
                FleetErrorKind::Incompatible,
                "Remote expert availability is invalid",
            ));
        }
        if let Some(value) = object.get("current_state_status")
            && !matches!(
                value.as_str(),
                Some("CURRENT" | "STALE" | "MISSING" | "UNKNOWN")
            )
        {
            return Err(FleetError::new(
                FleetErrorKind::Incompatible,
                "Remote work freshness is invalid",
            ));
        }
    }
    Ok(values.clone())
}

fn validate_session_array(
    value: Option<&Value>,
    extended: bool,
    message: &str,
    limit: usize,
) -> Result<Vec<Value>, FleetError> {
    let values = value
        .and_then(Value::as_array)
        .ok_or_else(|| FleetError::new(FleetErrorKind::Incompatible, message))?;
    if values.len() > limit {
        return Err(FleetError::new(
            FleetErrorKind::Incompatible,
            format!("{message}: exceeds the {limit}-record safety limit"),
        ));
    }
    for value in values {
        let object = object(value, message)?;
        let base = set(SESSION_FIELDS);
        let mut with_active = base.clone();
        with_active.insert("active_thread_id");
        let valid_size =
            object.len() == base.len() || (extended && object.len() == with_active.len());
        if !valid_size || object.keys().any(|key| !with_active.contains(key.as_str())) {
            return Err(FleetError::new(FleetErrorKind::Incompatible, message));
        }
        session_from_wire(value)?;
    }
    Ok(values.clone())
}

pub trait FleetTransport {
    fn request(&self, target: &str, payload: &Value, mutating: bool) -> Result<Value, FleetError>;
    fn request_cancellable(
        &self,
        target: &str,
        payload: &Value,
        mutating: bool,
        cancellation: &CancellationToken,
    ) -> Result<Value, FleetError> {
        if cancellation.is_cancelled() {
            return Err(FleetError::new(
                FleetErrorKind::Unreachable,
                "Fleet request cancelled before dispatch",
            ));
        }
        self.request(target, payload, mutating)
    }
    fn run_exact(
        &self,
        node: &FleetNode,
        arguments: &[String],
        tty: bool,
    ) -> Result<i32, FleetError>;
}

/// Explicit mutating transport capability. Keeping installation outside
/// `FleetTransport` prevents an inventory-only caller from upgrading a node by
/// accident.
pub trait FleetInstallTransport: FleetTransport {
    fn native_target(&self, target: &str) -> Result<String, FleetError>;
    fn install_bundle(
        &self,
        target: &str,
        bundle: &RemoteInstallBundle,
        expected_node_id: Option<&str>,
    ) -> Result<String, FleetError>;
}

#[derive(Clone, Debug)]
pub struct SshTransport {
    executable: PathBuf,
    connect_timeout: Duration,
    overall_timeout: Duration,
}

impl Default for SshTransport {
    fn default() -> Self {
        Self::new("ssh", Duration::from_secs(5), Duration::from_secs(12))
    }
}

impl SshTransport {
    pub fn new(
        executable: impl Into<PathBuf>,
        connect_timeout: Duration,
        overall_timeout: Duration,
    ) -> Self {
        Self {
            executable: executable.into(),
            connect_timeout,
            overall_timeout,
        }
    }

    pub fn base_args(&self, target: &str, tty: bool) -> Result<Vec<String>, FleetError> {
        validate_ssh_target(target)?;
        Ok(vec![
            if tty { "-tt" } else { "-T" }.to_owned(),
            "-o".to_owned(),
            "BatchMode=yes".to_owned(),
            "-o".to_owned(),
            format!("ConnectTimeout={}", self.connect_timeout.as_secs().max(1)),
            target.to_owned(),
        ])
    }

    fn command(
        &self,
        target: &str,
        arguments: &[String],
        tty: bool,
    ) -> Result<Command, FleetError> {
        let mut command = Command::new(&self.executable);
        command.args(self.base_args(target, tty)?);
        command.arg(remote_pika_command(arguments)?);
        Ok(command)
    }
}

impl FleetInstallTransport for SshTransport {
    fn native_target(&self, target: &str) -> Result<String, FleetError> {
        let mut command = Command::new(&self.executable);
        command.args(self.base_args(target, false)?);
        command.args(["sh", "-c", &shell_quote("uname -s; uname -m")]);
        let output = run_bounded_command(
            &mut command,
            None,
            self.overall_timeout,
            256,
            MAX_STDERR_BYTES,
        )?;
        if !output.status.success() {
            return Err(FleetError::new(
                FleetErrorKind::Unreachable,
                "Could not determine the remote Pika target",
            ));
        }
        let text = std::str::from_utf8(&output.stdout).map_err(|_| {
            FleetError::new(
                FleetErrorKind::Incompatible,
                "Remote machine returned an invalid operating system",
            )
        })?;
        let values: Vec<_> = text.lines().map(str::trim).collect();
        let target = match values.as_slice() {
            ["Darwin", "arm64" | "aarch64"] => "aarch64-apple-darwin",
            ["Darwin", "x86_64"] => "x86_64-apple-darwin",
            ["Linux", "arm64" | "aarch64"] => "aarch64-unknown-linux-musl",
            ["Linux", "x86_64"] => "x86_64-unknown-linux-musl",
            _ => {
                return Err(FleetError::new(
                    FleetErrorKind::Incompatible,
                    "Remote machine is not a supported native Pika host",
                ));
            }
        };
        Ok(target.to_owned())
    }

    fn install_bundle(
        &self,
        target: &str,
        bundle: &RemoteInstallBundle,
        expected_node_id: Option<&str>,
    ) -> Result<String, FleetError> {
        if bundle.is_empty() || bundle.len() > MAX_REMOTE_INSTALL_BYTES {
            return Err(FleetError::new(
                FleetErrorKind::InvalidRequest,
                "Verified remote installation bundle exceeds the safety limit",
            ));
        }
        // A trusted-node upgrade must prove identity on this mutation connection,
        // not merely on a previous SSH connection that may resolve differently.
        // Existing Python nodes already speak this hello protocol. The native
        // coordinator validates it before sending an approval line or any archive
        // bytes; the remote shell cannot stage or install before that approval.
        const SCRIPT: &str = "set -eu; pika_stage=$(mktemp -d /tmp/pika-remote.XXXXXXXX); trap 'rm -rf -- \"$pika_stage\"' EXIT HUP INT TERM; tar -xf - -C \"$pika_stage\"; bash \"$pika_stage/install.sh\" --bundle \"$pika_stage\" --no-setup";
        let script = if let Some(expected) = expected_node_id {
            if Uuid::parse_str(expected).is_err() {
                return Err(FleetError::new(
                    FleetErrorKind::InvalidRequest,
                    "Invalid expected installation node identity",
                ));
            }
            let hello = json!({"op":"hello", "protocol":PROTOCOL_NAME, "version":PROTOCOL_VERSION, "expected_node_id":expected});
            let pika = remote_pika_command(&["_fleet".to_owned(), "--stdio".to_owned()])?;
            format!(
                "set -eu; printf '%s\\n' {} | ({}); IFS= read -r pika_approval; [ \"$pika_approval\" = PIKA-INSTALL-VERIFIED ] || exit 75; {SCRIPT}",
                shell_quote(&hello.to_string()),
                pika
            )
        } else {
            // Explicit bootstrap of a missing, not-yet-trusted node has no prior
            // immutable identity. It is separately approved and trusted afterward.
            SCRIPT.to_owned()
        };
        let mut command = Command::new(&self.executable);
        command.args(self.base_args(target, false)?);
        command.args(["sh", "-c", &shell_quote(&script)]);
        let output = run_bounded_command_with_identity(
            &mut command,
            Some(bundle.as_bytes()),
            Duration::from_secs(900),
            MAX_STDERR_BYTES,
            MAX_STDERR_BYTES,
            expected_node_id.map(|expected| (expected, self.overall_timeout)),
        )
        .map_err(|error| {
            if error.kind == FleetErrorKind::Unreachable {
                FleetError::new(
                    FleetErrorKind::OutcomeUnknown,
                    format!(
                        "Remote install outcome unknown after SSH failure: {}",
                        error.message
                    ),
                )
            } else {
                error
            }
        })?;
        let detail = [output.stdout.as_slice(), output.stderr.as_slice()].concat();
        let detail = sanitize_terminal_text(
            std::str::from_utf8(&detail).unwrap_or("remote installer emitted invalid text"),
        );
        if !output.status.success() {
            let kind = if output.status.code() == Some(255) {
                FleetErrorKind::OutcomeUnknown
            } else {
                FleetErrorKind::Error
            };
            return Err(FleetError::new(
                kind,
                if detail.is_empty() {
                    format!("Remote installer exited {}", output.status)
                } else {
                    prefix_tail(&detail, 2_000)
                },
            ));
        }
        Ok(prefix_tail(&detail, 2_000))
    }
}

impl FleetTransport for SshTransport {
    fn request(&self, target: &str, payload: &Value, mutating: bool) -> Result<Value, FleetError> {
        self.request_cancellable(target, payload, mutating, &CancellationToken::default())
    }

    fn request_cancellable(
        &self,
        target: &str,
        payload: &Value,
        mutating: bool,
        cancellation: &CancellationToken,
    ) -> Result<Value, FleetError> {
        let mut input = serde_json::to_vec(payload)
            .map_err(|error| FleetError::new(FleetErrorKind::InvalidRequest, error.to_string()))?;
        input.push(b'\n');
        if input.len() > MAX_MESSAGE_BYTES {
            return Err(FleetError::new(
                FleetErrorKind::InvalidRequest,
                "Fleet request exceeded the safety limit",
            ));
        }
        let mut command =
            self.command(target, &["_fleet".to_owned(), "--stdio".to_owned()], false)?;
        let output = run_bounded_command_cancellable(
            &mut command,
            Some(&input),
            self.overall_timeout,
            MAX_MESSAGE_BYTES,
            MAX_STDERR_BYTES,
            cancellation,
        )
        .map_err(|error| {
            if error.kind == FleetErrorKind::Unreachable && mutating {
                FleetError::new(FleetErrorKind::OutcomeUnknown, error.message)
            } else {
                error
            }
        })?;
        if !output.status.success() {
            let bytes = if output.stderr.is_empty() {
                &output.stdout
            } else {
                &output.stderr
            };
            let detail = sanitize_terminal_text(
                std::str::from_utf8(bytes).unwrap_or("SSH returned invalid text"),
            );
            let folded = detail.to_lowercase();
            let kind = if output.status.code() == Some(127)
                || folded.contains("pika: not found")
                || folded.contains("pika: command not found")
            {
                FleetErrorKind::Missing
            } else if folded.contains("permission denied")
                || folded.contains("host key verification")
                || folded.contains("no identities")
            {
                FleetErrorKind::Authentication
            } else if mutating {
                FleetErrorKind::OutcomeUnknown
            } else {
                FleetErrorKind::Unreachable
            };
            return Err(FleetError::new(
                kind,
                if detail.is_empty() {
                    format!("SSH exited {}", output.status)
                } else {
                    prefix_tail(&detail, 500)
                },
            ));
        }
        let stdout = std::str::from_utf8(&output.stdout).map_err(|_| {
            FleetError::new(
                FleetErrorKind::Incompatible,
                "Remote Pika emitted non-UTF-8 output",
            )
        })?;
        let lines: Vec<_> = stdout
            .lines()
            .filter(|line| !line.trim().is_empty())
            .collect();
        if lines.len() != 1 {
            return Err(FleetError::new(
                FleetErrorKind::Incompatible,
                "Remote Pika emitted a malformed JSONL response",
            ));
        }
        let response: Value = serde_json::from_str(lines[0]).map_err(|_| {
            FleetError::new(
                FleetErrorKind::Incompatible,
                "Remote Pika emitted non-JSON output",
            )
        })?;
        let object = object(&response, "Remote Pika response is not an object")?;
        if object.get("type").and_then(Value::as_str) == Some("error") {
            return Err(FleetError::new(
                parse_error_kind(object.get("kind").and_then(Value::as_str)),
                object
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("remote Pika error"),
            ));
        }
        Ok(response)
    }

    fn run_exact(
        &self,
        node: &FleetNode,
        arguments: &[String],
        tty: bool,
    ) -> Result<i32, FleetError> {
        let status = self
            .command(&node.ssh_target, arguments, tty)?
            .status()
            .map_err(|error| {
                FleetError::new(
                    FleetErrorKind::Unreachable,
                    format!("Could not start SSH: {error}"),
                )
            })?;
        Ok(status.code().unwrap_or(1))
    }
}

pub struct FleetManager<'a, T: FleetTransport> {
    store: &'a Store,
    transport: T,
}

impl<'a, T: FleetTransport> FleetManager<'a, T> {
    pub fn new(store: &'a Store, transport: T) -> Self {
        Self { store, transport }
    }
    pub fn nodes(&self) -> Result<Vec<FleetNode>, FleetError> {
        self.store.list_nodes().map_err(Into::into)
    }

    fn node_hello(&self, node: &FleetNode) -> Result<Hello, FleetError> {
        let response = self.transport.request(
            &node.ssh_target,
            &json!({
                "op":"hello", "protocol":PROTOCOL_NAME, "version":PROTOCOL_VERSION,
                "expected_node_id":node.node_id,
            }),
            false,
        )?;
        let local_node_id = self.store.ensure_local_node_id()?;
        validate_hello(&response, Some(&local_node_id), Some(&node.node_id))
    }

    pub fn verify_node_identity(&self, node: &FleetNode) -> Result<String, FleetError> {
        Ok(self.node_hello(node)?.package_version)
    }

    /// Preflight an explicit upgrade and return the trusted node plus its
    /// measured native target. The caller uses that target to select one exact
    /// artifact from a verified multi-platform bundle.
    pub fn upgrade_target(&self, value: &str) -> Result<(FleetNode, String), FleetError>
    where
        T: FleetInstallTransport,
    {
        let node = self.store.get_fleet_node(value)?.ok_or_else(|| {
            FleetError::new(
                FleetErrorKind::NotFound,
                format!("Unknown Pika machine {value:?}"),
            )
        })?;
        self.verify_node_identity(&node)?;
        let target = self.transport.native_target(&node.ssh_target)?;
        Ok((node, target))
    }

    /// Install only an already verified, bounded bundle on an already trusted
    /// node. Its immutable ID is proved before, on the mutation connection, and
    /// after installation; reconnects cannot redirect a previously approved write.
    pub fn upgrade_bundle(
        &self,
        value: &str,
        bundle: &RemoteInstallBundle,
    ) -> Result<FleetNode, FleetError>
    where
        T: FleetInstallTransport,
    {
        let mut node = self.store.get_fleet_node(value)?.ok_or_else(|| {
            FleetError::new(
                FleetErrorKind::NotFound,
                format!("Unknown Pika machine {value:?}"),
            )
        })?;
        self.verify_node_identity(&node)?;
        let measured_target = self.transport.native_target(&node.ssh_target)?;
        if measured_target != bundle.target() {
            return Err(FleetError::new(
                FleetErrorKind::Incompatible,
                format!(
                    "Remote machine target changed from {} to {measured_target}; nothing installed",
                    bundle.target()
                ),
            ));
        }
        self.transport
            .install_bundle(&node.ssh_target, bundle, Some(&node.node_id))?;
        let installed_version = self.verify_node_identity(&node).map_err(|error| {
            if matches!(
                error.kind,
                FleetErrorKind::Unreachable | FleetErrorKind::Authentication
            ) {
                FleetError::new(
                    FleetErrorKind::OutcomeUnknown,
                    format!(
                        "Remote install completed but node identity could not be reverified: {}",
                        error.message
                    ),
                )
            } else {
                error
            }
        })?;
        if installed_version != bundle.version() {
            return Err(FleetError::new(
                FleetErrorKind::Incompatible,
                format!(
                    "Remote Pika reported {installed_version} after installing {}",
                    bundle.version()
                ),
            ));
        }
        let timestamp = now();
        node.package_version = Some(installed_version);
        node.status = "ready".to_owned();
        node.last_seen = timestamp;
        node.last_attempt_at = timestamp;
        node.last_error = None;
        node.updated_at = timestamp;
        self.store.upsert_fleet_node(&node)?;
        Ok(node)
    }

    pub fn handshake(
        &self,
        candidate: &NodeCandidate,
        alias: Option<&str>,
    ) -> Result<FleetNode, FleetError> {
        let response = self.transport.request(
            &candidate.ssh_target,
            &json!({
                "op":"hello", "protocol":PROTOCOL_NAME, "version":PROTOCOL_VERSION,
            }),
            false,
        )?;
        let local_node_id = self.store.ensure_local_node_id()?;
        let hello = validate_hello(&response, Some(&local_node_id), None)?;
        let existing = self.store.get_fleet_node(&hello.node_id)?;
        let timestamp = now();
        Ok(FleetNode {
            node_id: hello.node_id,
            alias: existing
                .as_ref()
                .map(|node| node.alias.clone())
                .unwrap_or(machine_alias(alias.unwrap_or(&candidate.alias))?),
            ssh_target: candidate.ssh_target.clone(),
            sources: candidate.sources.clone(),
            status: "ready".to_owned(),
            protocol_version: Some(PROTOCOL_VERSION),
            package_version: Some(hello.package_version),
            capabilities: hello.capabilities,
            last_seen: existing.as_ref().map_or(0.0, |node| node.last_seen),
            last_attempt_at: timestamp,
            last_error: None,
            created_at: existing.as_ref().map_or(timestamp, |node| node.created_at),
            updated_at: timestamp,
        })
    }

    pub fn add(
        &self,
        candidate: &NodeCandidate,
        alias: Option<&str>,
    ) -> Result<FleetNode, FleetError> {
        let node = self.handshake(candidate, alias)?;
        // Joining the same generation stream as refresh makes onboarding
        // last-started-wins without trusting a node before its first complete
        // snapshot has passed validation.
        let generation = self.store.claim_fleet_onboarding(&node.node_id)?;
        let snapshot = validate_snapshot(&self.snapshot_request(&node)?, Some(&node.node_id))?;
        // Scheduling and cache age belong to the receiving machine's clock.
        // Keep the remote capture timestamp only inside the source payload.
        if !self
            .store
            .commit_fleet_onboarding_if_current(&node, &snapshot, now(), generation)?
        {
            return Err(FleetError::new(
                FleetErrorKind::Error,
                "Machine adoption was superseded by a newer request; retry from the current cache",
            ));
        }
        self.store.get_fleet_node(&node.node_id)?.ok_or_else(|| {
            FleetError::new(
                FleetErrorKind::Error,
                "Adopted fleet node was not persisted",
            )
        })
    }

    pub fn refresh_node(&self, value: &str) -> Result<Vec<FleetSession>, FleetError> {
        self.refresh_node_cancellable(value, &CancellationToken::default())
    }

    pub fn refresh_node_cancellable(
        &self,
        value: &str,
        cancellation: &CancellationToken,
    ) -> Result<Vec<FleetSession>, FleetError> {
        let node = self.store.get_fleet_node(value)?.ok_or_else(|| {
            FleetError::new(
                FleetErrorKind::NotFound,
                format!("Unknown Pika machine {value:?}"),
            )
        })?;
        let generation = self.store.claim_fleet_refresh(&node.node_id)?;
        let snapshot = match self
            .snapshot_request_cancellable(&node, cancellation)
            .and_then(|value| validate_snapshot(&value, Some(&node.node_id)))
        {
            Ok(snapshot) => snapshot,
            Err(error) => {
                // Board shutdown is an intentional observer lifecycle event,
                // not evidence that the remote machine became unreachable.
                // The generation claim still invalidates older in-flight work.
                if cancellation.is_cancelled() {
                    return Err(error);
                }
                let status = match error.kind {
                    FleetErrorKind::Unreachable => "unreachable",
                    FleetErrorKind::Authentication => "auth",
                    FleetErrorKind::Incompatible => "incompatible",
                    FleetErrorKind::Quarantined => "quarantined",
                    _ => "error",
                };
                let _ = self.store.mark_fleet_node_error_if_current(
                    &node.node_id,
                    generation,
                    status,
                    &error.message,
                );
                return Err(error);
            }
        };
        if !self.store.put_remote_snapshot_if_current(
            &node.node_id,
            &snapshot,
            now(),
            generation,
        )? {
            return Err(FleetError::new(
                FleetErrorKind::Error,
                "Remote refresh was superseded by a newer request; retry from the current cache",
            ));
        }
        self.cached_sessions(Some(&node.node_id), false)
    }

    pub fn cached_sessions(
        &self,
        node_id: Option<&str>,
        include_experts: bool,
    ) -> Result<Vec<FleetSession>, FleetError> {
        Ok(self
            .cached_sessions_with_notices(node_id, include_experts)?
            .sessions)
    }

    /// Read the retained fleet cache without remote I/O.
    ///
    /// Aggregate views receive an equal deterministic row and byte slice per
    /// selected machine. One noisy machine therefore cannot evict every other
    /// machine from the board. Exact-machine reads retain their full validated
    /// snapshot and are not subject to aggregate slicing.
    pub fn cached_sessions_with_notices(
        &self,
        node_id: Option<&str>,
        include_experts: bool,
    ) -> Result<CachedFleetSessions, FleetError> {
        let timestamp = now();
        let mut result = Vec::new();
        let mut notices = Vec::new();
        let nodes = self.nodes()?;
        let selected_nodes = nodes
            .iter()
            .filter(|node| node_id.is_none_or(|expected| expected == node.node_id))
            .count();
        if selected_nodes > MAX_CACHED_FLEET_NODES {
            return Err(FleetError::new(
                FleetErrorKind::Incompatible,
                format!("Cached fleet exceeds the {MAX_CACHED_FLEET_NODES}-machine safety limit"),
            ));
        }
        let aggregate = node_id.is_none();
        let row_slice = if aggregate && selected_nodes > 0 {
            MAX_CACHED_FLEET_ROWS / selected_nodes
        } else {
            MAX_CACHED_FLEET_ROWS
        };
        let byte_slice = if aggregate && selected_nodes > 0 {
            MAX_CACHED_FLEET_BYTES / selected_nodes
        } else {
            MAX_CACHED_FLEET_BYTES
        };
        for node in nodes {
            if node_id.is_some_and(|expected| expected != node.node_id) {
                continue;
            }
            if aggregate && !include_experts {
                match self
                    .store
                    .get_remote_board_projection(&node.node_id, byte_slice, row_slice)
                {
                    Ok(Some(projection)) => {
                        if projection.protocol != PROTOCOL_NAME
                            || projection.version != PROTOCOL_VERSION
                        {
                            notices.push(cache_input_notice(
                                &node,
                                "cache-incompatible",
                                "cached board projection uses an incompatible fleet protocol",
                            ));
                            continue;
                        }
                        notices.extend(cache_notices_from_values(
                            &projection.directory_notices,
                            &node,
                        ));
                        let projected = projection
                            .rows
                            .into_iter()
                            .map(|row| {
                                let object = row.as_object().ok_or_else(|| {
                                    FleetError::new(
                                        FleetErrorKind::Incompatible,
                                        "cached board projection row is not an object",
                                    )
                                })?;
                                if object.len() != 3
                                    || !object.contains_key("session")
                                    || !object.contains_key("card")
                                    || !object.contains_key("profile")
                                {
                                    return Err(FleetError::new(
                                        FleetErrorKind::Incompatible,
                                        "cached board projection row has unexpected fields",
                                    ));
                                }
                                let raw = object["session"].clone();
                                let session = session_from_wire(&raw)?;
                                Ok((
                                    raw,
                                    session,
                                    object["card"].clone(),
                                    object["profile"].clone(),
                                ))
                            })
                            .collect::<Result<Vec<_>, FleetError>>();
                        let Ok(projected) = projected else {
                            notices.push(cache_input_notice(
                                &node,
                                "cache-invalid",
                                "cached board projection failed validation",
                            ));
                            continue;
                        };
                        let stale = node.status != "ready"
                            || projection.source_captured_at > timestamp
                            || timestamp - projection.source_captured_at > REMOTE_STALE_SECONDS;
                        let node_wire = json!({
                            "node_id": node.node_id,
                            "node_name": node.alias,
                            "remote_error": node.last_error,
                        });
                        let mut retained_rows = 0_usize;
                        let mut retained_bytes = 0_usize;
                        for (raw, session, card_value, profile_value) in projected {
                            let encoded_bytes = [&raw, &card_value, &profile_value, &node_wire]
                                .into_iter()
                                .try_fold(0_usize, |total, value| {
                                    serde_json::to_vec(value)
                                        .map_err(|error| FleetError::new(FleetErrorKind::Error, error.to_string()))?
                                        .len()
                                        .checked_add(total)
                                        .ok_or_else(|| FleetError::new(FleetErrorKind::Incompatible, "Cached fleet row size overflowed its safety counter"))
                                })?;
                            if retained_bytes
                                .checked_add(encoded_bytes)
                                .is_none_or(|bytes| bytes > byte_slice)
                            {
                                break;
                            }
                            result.push(cached_fleet_session(
                                &node,
                                session,
                                stale,
                                projection.source_captured_at,
                                projection.remote_captured_at,
                                card_value.as_object(),
                                profile_value.as_object(),
                            ));
                            retained_rows += 1;
                            retained_bytes += encoded_bytes;
                        }
                        let omitted_rows = projection.source_sessions.saturating_sub(retained_rows);
                        if omitted_rows > 0 {
                            notices.push(FleetCacheNotice {
                                node_id: node.node_id.clone(),
                                node_name: node.alias.clone(),
                                kind: "aggregate-truncated".to_owned(),
                                omitted_rows,
                                message: format!(
                                    "{omitted_rows} cached conversation(s) on {} not shown · fair aggregate limit is {row_slice} rows and {} KiB for this machine",
                                    node.alias,
                                    byte_slice / 1024,
                                ),
                            });
                        }
                        continue;
                    }
                    Err(_) => {
                        notices.push(cache_input_notice(
                            &node,
                            "cache-invalid",
                            "cached board projection failed validation",
                        ));
                        continue;
                    }
                    Ok(None) => {}
                }
            }
            let stored = if aggregate {
                let Some(bounded) = self
                    .store
                    .get_remote_snapshot_bounded(&node.node_id, byte_slice)?
                else {
                    continue;
                };
                let Some(snapshot) = bounded.snapshot else {
                    notices.push(cache_input_notice(
                        &node,
                        "aggregate-input-limited",
                        "cached snapshot exceeds this machine's fair pre-paint budget; it will reappear after its next refresh",
                    ));
                    continue;
                };
                snapshot
            } else {
                let Some(snapshot) = self.store.get_remote_snapshot(&node.node_id)? else {
                    continue;
                };
                snapshot
            };
            let Ok(snapshot) = validate_snapshot(&stored.payload, Some(&node.node_id)) else {
                continue;
            };
            notices.extend(snapshot_cache_notices(&snapshot, &node));
            let remote_captured_at = snapshot
                .get("captured_at")
                .and_then(Value::as_f64)
                .unwrap_or(stored.captured_at);
            let stale = node.status != "ready"
                || stored.captured_at > timestamp
                || timestamp - stored.captured_at > REMOTE_STALE_SECONDS;
            let cards = keyed_values(snapshot.get("cards"));
            let profiles = keyed_values(snapshot.get("profiles"));
            let mut all = snapshot
                .get("sessions")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if include_experts {
                all.extend(
                    snapshot
                        .get("expert_sessions")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default(),
                );
            }
            let available_rows = all.len();
            let mut retained_rows = 0_usize;
            let mut retained_bytes = 0_usize;
            let node_wire = json!({
                "node_id": node.node_id,
                "node_name": node.alias,
                "remote_error": node.last_error,
            });
            for raw in all {
                if aggregate && retained_rows >= row_slice {
                    break;
                }
                let session = session_from_wire(&raw)?;
                let key = (
                    session.provider.as_str().to_owned(),
                    session.session_id.clone(),
                );
                let card_value = cards.get(&key);
                let profile_value = profiles.get(&key);
                let card = card_value.and_then(Value::as_object);
                let profile = profile_value.and_then(Value::as_object);
                // Count each retained row together with the annotations it can
                // carry. Using the complete wire values deliberately
                // overestimates the smaller FleetSession projection.
                let encoded_bytes = [
                    &raw,
                    card_value.unwrap_or(&Value::Null),
                    profile_value.unwrap_or(&Value::Null),
                    &node_wire,
                ]
                .into_iter()
                .try_fold(0_usize, |total, value| {
                    serde_json::to_vec(value)
                        .map_err(|error| FleetError::new(FleetErrorKind::Error, error.to_string()))?
                        .len()
                        .checked_add(total)
                        .ok_or_else(|| {
                            FleetError::new(
                                FleetErrorKind::Incompatible,
                                "Cached fleet row size overflowed its safety counter",
                            )
                        })
                })?;
                if aggregate
                    && retained_bytes
                        .checked_add(encoded_bytes)
                        .is_none_or(|bytes| bytes > byte_slice)
                {
                    break;
                }
                result.push(cached_fleet_session(
                    &node,
                    session,
                    stale,
                    stored.captured_at,
                    remote_captured_at,
                    card,
                    profile,
                ));
                retained_rows += 1;
                retained_bytes += encoded_bytes;
            }
            let omitted_rows = available_rows.saturating_sub(retained_rows);
            if omitted_rows > 0 {
                notices.push(FleetCacheNotice {
                    node_id: node.node_id.clone(),
                    node_name: node.alias.clone(),
                    kind: "aggregate-truncated".to_owned(),
                    omitted_rows,
                    message: format!(
                        "{omitted_rows} cached conversation(s) on {} not shown · fair aggregate limit is {row_slice} rows and {} KiB for this machine",
                        node.alias,
                        byte_slice / 1024,
                    ),
                });
            }
        }
        Ok(CachedFleetSessions {
            sessions: result,
            notices,
        })
    }

    /// Search cached expert cards without contacting any machine.
    ///
    /// Each node ranks its complete bounded expert set before the aggregate
    /// fleet slice is applied. Ordinary board rows therefore cannot hide an
    /// expert card, and one large machine cannot evict every other machine.
    pub fn expert_directory(&self, query: &str) -> Result<FleetExpertDirectory, FleetError> {
        let timestamp = now();
        let mut matches = Vec::new();
        let mut notices = Vec::new();
        let nodes = self.nodes()?;
        if nodes.len() > MAX_CACHED_FLEET_NODES {
            return Err(FleetError::new(
                FleetErrorKind::Incompatible,
                format!("Cached fleet exceeds the {MAX_CACHED_FLEET_NODES}-machine safety limit"),
            ));
        }
        let row_slice = if nodes.is_empty() {
            MAX_CACHED_FLEET_ROWS
        } else {
            MAX_CACHED_FLEET_ROWS / nodes.len()
        };
        let byte_slice = if nodes.is_empty() {
            MAX_CACHED_FLEET_BYTES
        } else {
            MAX_CACHED_FLEET_BYTES / nodes.len()
        };
        for node in nodes {
            let Some(stored) = self.store.get_remote_snapshot(&node.node_id)? else {
                continue;
            };
            let Ok(snapshot) = validate_snapshot(&stored.payload, Some(&node.node_id)) else {
                continue;
            };
            notices.extend(
                snapshot_cache_notices(&snapshot, &node)
                    .into_iter()
                    .filter(|notice| notice.kind == "expert-directory-incomplete"),
            );
            let remote_captured_at = snapshot
                .get("captured_at")
                .and_then(Value::as_f64)
                .unwrap_or(stored.captured_at);
            let stale = node.status != "ready"
                || stored.captured_at > timestamp
                || timestamp - stored.captured_at > REMOTE_STALE_SECONDS;
            let cards = keyed_values(snapshot.get("cards"));
            let profile_values = keyed_values(snapshot.get("profiles"));
            let mut sessions = Vec::new();
            for raw in snapshot
                .get("expert_sessions")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .chain(
                    snapshot
                        .get("sessions")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten(),
                )
            {
                let session = session_from_wire(raw)?;
                let key = (
                    session.provider.as_str().to_owned(),
                    session.session_id.clone(),
                );
                let Some(profile_value) = profile_values.get(&key) else {
                    continue;
                };
                sessions.push(cached_fleet_session(
                    &node,
                    session,
                    stale,
                    stored.captured_at,
                    remote_captured_at,
                    cards.get(&key).and_then(Value::as_object),
                    profile_value.as_object(),
                ));
            }
            let mut profiles = snapshot
                .get("profiles")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(profile_from_wire)
                .collect::<Result<Vec<_>, _>>()?;
            for profile in &mut profiles {
                profile.profile.updated_at = receiver_clock_timestamp(
                    stored.captured_at,
                    remote_captured_at,
                    profile.profile.updated_at,
                );
                profile.profile.scope_updated_at = receiver_clock_timestamp(
                    stored.captured_at,
                    remote_captured_at,
                    profile.profile.scope_updated_at,
                );
                profile.profile.current_state_updated_at = receiver_clock_timestamp(
                    stored.captured_at,
                    remote_captured_at,
                    profile.profile.current_state_updated_at,
                );
            }
            let local_sessions = sessions
                .iter()
                .map(|item| item.session.clone())
                .collect::<Vec<_>>();
            let wrappers = sessions
                .into_iter()
                .map(|item| {
                    (
                        (item.session.provider, item.session.session_id.clone()),
                        item,
                    )
                })
                .collect::<BTreeMap<_, _>>();
            let ranked = rank_experts(&profiles, &local_sessions, query, &BTreeSet::new());
            let available_matches = ranked.len();
            let mut retained_matches = 0_usize;
            let mut retained_bytes = 0_usize;
            for mut found in ranked {
                let Some(remote) = wrappers.get(&(found.provider, found.session_id.clone())) else {
                    continue;
                };
                found.watched = remote.watched;
                found.availability = remote.source_availability().to_owned();
                found.qualified_name = format!("{}@{}", found.session_id, remote.node_name);
                found.machine = Some(remote.node_name.clone());
                found.node_id = Some(remote.node_id.clone());
                found.snapshot_stale = Some(remote.stale);
                found.snapshot_seen_at = Some(remote.seen_at);
                if let Some(status) = remote.card_status.as_deref().and_then(parse_card_status) {
                    found.card_status = status;
                }
                found.card_detail = remote.card_detail.clone();
                found.freshness.scope_updated_at = remote.scope_updated_at;
                found.freshness.scope_age_seconds = remote
                    .scope_updated_at
                    .map(|value| (timestamp - value).max(0.0));
                found.freshness.current_state_updated_at = remote.current_state_updated_at;
                found.freshness.current_state_age_seconds = remote
                    .current_state_updated_at
                    .map(|value| (timestamp - value).max(0.0));
                found.freshness.current_state_status = if remote.stale {
                    CardStatus::Unknown
                } else {
                    remote
                        .current_state_status
                        .as_deref()
                        .and_then(parse_card_status)
                        .unwrap_or(CardStatus::Unknown)
                };
                let encoded_bytes = serde_json::to_vec(&found)
                    .map_err(|error| FleetError::new(FleetErrorKind::Error, error.to_string()))?
                    .len();
                if retained_matches >= row_slice
                    || retained_bytes
                        .checked_add(encoded_bytes)
                        .is_none_or(|bytes| bytes > byte_slice)
                {
                    continue;
                }
                matches.push(found);
                retained_matches += 1;
                retained_bytes += encoded_bytes;
            }
            let omitted_rows = available_matches.saturating_sub(retained_matches);
            if omitted_rows > 0 {
                notices.push(FleetCacheNotice {
                    node_id: node.node_id.clone(),
                    node_name: node.alias.clone(),
                    kind: "expert-directory-incomplete".to_owned(),
                    omitted_rows,
                    message: format!(
                        "{omitted_rows} matching expert card(s) on {} not shown · fair aggregate limit is {row_slice} rows and {} KiB for this machine",
                        node.alias,
                        byte_slice / 1024,
                    ),
                });
            }
        }
        matches.sort_by(|left, right| {
            let left_fresh = left.snapshot_stale != Some(true);
            let right_fresh = right.snapshot_stale != Some(true);
            right_fresh
                .cmp(&left_fresh)
                .then_with(|| right.score.cmp(&left.score))
                .then_with(|| right.live.cmp(&left.live))
                .then_with(|| right.profile_updated_at.total_cmp(&left.profile_updated_at))
                .then_with(|| right.node_id.cmp(&left.node_id))
                .then_with(|| right.session_id.cmp(&left.session_id))
        });
        notices.sort_by(|left, right| {
            left.node_name
                .cmp(&right.node_name)
                .then_with(|| left.kind.cmp(&right.kind))
                .then_with(|| left.message.cmp(&right.message))
        });
        Ok(FleetExpertDirectory { matches, notices })
    }

    pub fn expert_matches(&self, query: &str) -> Result<Vec<ExpertMatch>, FleetError> {
        Ok(self.expert_directory(query)?.matches)
    }

    pub fn resolve(
        &self,
        query: &str,
        fresh: bool,
        include_experts: bool,
    ) -> Result<Option<FleetSession>, FleetError> {
        let Some(mut matches) = self.resolve_candidates(query, fresh, include_experts)? else {
            return Ok(None);
        };
        match matches.len() {
            0 => Err(FleetError::new(
                FleetErrorKind::NotFound,
                format!("NOT FOUND ON FRESH LOOKUP: {query}"),
            )),
            1 => Ok(matches.pop()),
            _ => {
                let routes = matches
                    .iter()
                    .map(|item| format!("{}@{}", item.session.session_id, item.node_name))
                    .collect::<Vec<_>>()
                    .join(", ");
                Err(FleetError::new(
                    FleetErrorKind::InvalidRequest,
                    format!(
                        "Multiple conversations match {query:?}; choose one exact route: {routes}"
                    ),
                ))
            }
        }
    }

    /// Return every usable match for a qualified remote query. Interactive
    /// callers can present these immutable rows without reimplementing cache
    /// freshness, exact UUID, name, and UUID-prefix precedence.
    pub fn resolve_candidates(
        &self,
        query: &str,
        fresh: bool,
        include_experts: bool,
    ) -> Result<Option<Vec<FleetSession>>, FleetError> {
        let Some((thread, alias)) = query.rsplit_once('@') else {
            return Ok(None);
        };
        let Some(node) = self.store.get_fleet_node(alias)? else {
            return Ok(None);
        };
        if fresh {
            self.refresh_node(&node.node_id)?;
        }
        let sessions = self.cached_sessions(Some(&node.node_id), include_experts)?;
        if fresh && sessions.iter().any(|session| session.stale) {
            return Err(FleetError::new(
                FleetErrorKind::Unreachable,
                format!("{alias} is not current; cached metadata was kept but no action was taken"),
            ));
        }
        let exact: Vec<_> = sessions
            .iter()
            .filter(|item| item.session.session_id == thread)
            .cloned()
            .collect();
        let named: Vec<_> = sessions
            .iter()
            .filter(|item| {
                item.session
                    .name
                    .as_ref()
                    .is_some_and(|name| name.eq_ignore_ascii_case(thread))
            })
            .cloned()
            .collect();
        let mut matches = if exact.is_empty() { named } else { exact };
        if matches.is_empty() {
            matches = sessions
                .into_iter()
                .filter(|item| item.session.session_id.starts_with(thread))
                .collect();
        }
        Ok(Some(matches))
    }

    pub fn attach(&self, session: &FleetSession) -> Result<i32, FleetError> {
        // The alias is display-only and can be renamed or reassigned after a
        // board row was selected. Refresh and resolve exclusively through the
        // immutable node/provider/conversation tuple carried by that row.
        let node = self
            .store
            .get_fleet_node(&session.node_id)?
            .ok_or_else(|| {
                FleetError::new(
                    FleetErrorKind::NotFound,
                    "Remote machine is no longer trusted",
                )
            })?;
        validate_exact_route(&node, session)?;
        self.refresh_node(&session.node_id)?;
        let stable_node = self
            .store
            .get_fleet_node(&session.node_id)?
            .ok_or_else(|| {
                FleetError::new(
                    FleetErrorKind::NotFound,
                    "Remote machine was removed before attach",
                )
            })?;
        if stable_node.ssh_target != node.ssh_target {
            return Err(FleetError::new(
                FleetErrorKind::Quarantined,
                "Remote machine route changed during attach; nothing was opened. Retry from a fresh board.",
            ));
        }
        let matches = self
            .cached_sessions(Some(&session.node_id), false)?
            .into_iter()
            .filter(|current| {
                current.session.provider == session.session.provider
                    && current.session.session_id == session.session.session_id
            })
            .collect::<Vec<_>>();
        let current = match matches.as_slice() {
            [current] => current,
            [] => {
                return Err(FleetError::new(
                    FleetErrorKind::NotFound,
                    "Remote session disappeared on fresh exact lookup",
                ));
            }
            _ => {
                return Err(FleetError::new(
                    FleetErrorKind::Quarantined,
                    "Remote snapshot returned duplicate exact conversation identities",
                ));
            }
        };
        validate_exact_route(&stable_node, current)?;
        self.transport.run_exact(
            &stable_node,
            &[
                "_fleet-open".to_owned(),
                "--expected-node-id".to_owned(),
                stable_node.node_id.clone(),
                "--provider".to_owned(),
                current.session.provider.as_str().to_owned(),
                "--session-id".to_owned(),
                current.session.session_id.clone(),
            ],
            true,
        )
    }

    pub fn capture(&self, session: &FleetSession, lines: usize) -> Result<String, FleetError> {
        let response = self.session_request(
            session,
            "peek",
            false,
            Some(json!({"lines":lines.clamp(1, 2000)})),
        )?;
        exact_fields(
            object(&response, "Remote peek receipt is malformed")?,
            &set(&["type", "node_id", "text"]),
            "Remote peek receipt is malformed",
        )?;
        if response.get("type").and_then(Value::as_str) != Some("peek")
            || response.get("node_id").and_then(Value::as_str) != Some(&session.node_id)
        {
            return Err(FleetError::new(
                FleetErrorKind::Quarantined,
                "Remote peek receipt is malformed",
            ));
        }
        wire_string(response.get("text"), "peek text", MAX_MESSAGE_BYTES)?.ok_or_else(|| {
            FleetError::new(
                FleetErrorKind::Quarantined,
                "Remote peek receipt is malformed",
            )
        })
    }

    pub fn acknowledge(&self, session: &FleetSession) -> Result<bool, FleetError> {
        let node = self
            .store
            .get_fleet_node(&session.node_id)?
            .ok_or_else(|| {
                FleetError::new(
                    FleetErrorKind::NotFound,
                    format!("Machine {:?} is no longer adopted", session.node_name),
                )
            })?;
        let hello = self.node_hello(&node)?;
        if !hello
            .capabilities
            .iter()
            .any(|capability| capability == "ack-event-bound-v1")
        {
            return Err(FleetError::new(
                FleetErrorKind::Incompatible,
                format!(
                    "{} needs an updated Pika before it can safely acknowledge remote events",
                    node.alias
                ),
            ));
        }
        let response = self.session_request(
            session,
            "acknowledge",
            true,
            Some(json!({"expected_last_event_at":session.session.last_event_at})),
        )?;
        exact_fields(
            object(&response, "Remote acknowledgement receipt is malformed")?,
            &set(&["type", "node_id", "acknowledged"]),
            "Remote acknowledgement receipt is malformed",
        )?;
        if response.get("type").and_then(Value::as_str) != Some("acknowledged")
            || response.get("node_id").and_then(Value::as_str) != Some(&session.node_id)
        {
            return Err(FleetError::new(
                FleetErrorKind::OutcomeUnknown,
                "Remote acknowledgement receipt is malformed",
            ));
        }
        response
            .get("acknowledged")
            .and_then(Value::as_bool)
            .ok_or_else(|| {
                FleetError::new(
                    FleetErrorKind::OutcomeUnknown,
                    "Remote acknowledgement receipt is malformed",
                )
            })
    }

    pub fn untrack(
        &self,
        session: &FleetSession,
        supplied_id: Option<&str>,
    ) -> Result<i64, FleetError> {
        let pending_key = format!(
            "fleet:pending-untrack:{}:{}:{}",
            session.node_id, session.session.provider, session.session.session_id
        );
        let request_id = mutation_request_id(self.store, &pending_key, supplied_id)?;
        let response = self
            .session_request(
                session,
                "untrack",
                true,
                Some(json!({"request_id":request_id})),
            )
            .map_err(|error| mutation_unknown(error, "UNTRACK", &session.node_name, &request_id))?;
        validate_mutation_receipt(&response, "untracked", &session.node_id, &request_id)?;
        self.store.delete_meta(&pending_key)?;
        let _ = self.refresh_node(&session.node_id);
        response
            .get("panes_cleared")
            .and_then(Value::as_i64)
            .ok_or_else(|| {
                FleetError::new(
                    FleetErrorKind::Quarantined,
                    "Remote untracked receipt has an invalid pane count",
                )
            })
    }

    pub fn adopt(
        &self,
        node: &FleetNode,
        candidate: &Candidate,
        supplied_id: Option<&str>,
    ) -> Result<Session, FleetError> {
        let pending_key = format!(
            "fleet:pending-adopt:{}:{}:{}",
            node.node_id, candidate.provider, candidate.session_id
        );
        let request_id = mutation_request_id(self.store, &pending_key, supplied_id)?;
        let response = self
            .transport
            .request(
                &node.ssh_target,
                &json!({
                    "op":"adopt", "protocol":PROTOCOL_NAME, "version":PROTOCOL_VERSION,
                    "expected_node_id":node.node_id, "provider":candidate.provider.as_str(),
                    "session_id":candidate.session_id, "request_id":request_id,
                }),
                true,
            )
            .map_err(|error| mutation_unknown(error, "ADOPTION", &node.alias, &request_id))?;
        validate_mutation_receipt(&response, "adopted", &node.node_id, &request_id)?;
        let raw = response.get("session").ok_or_else(|| {
            FleetError::new(
                FleetErrorKind::Quarantined,
                "Remote adoption receipt has no session",
            )
        })?;
        exact_fields(
            object(
                raw,
                "Remote adoption receipt has a malformed session envelope",
            )?,
            &set(SESSION_FIELDS),
            "Remote adoption receipt has a malformed session envelope",
        )?;
        let session = session_from_wire(raw)?;
        if session.provider != candidate.provider || session.session_id != candidate.session_id {
            return Err(FleetError::new(
                FleetErrorKind::Quarantined,
                "Remote adoption receipt has the wrong identity",
            ));
        }
        self.store.delete_meta(&pending_key)?;
        let _ = self.refresh_node(&node.node_id);
        Ok(session)
    }

    pub fn remote_candidates(
        &self,
        node: &FleetNode,
        include_unconfirmed: bool,
    ) -> Result<Vec<Candidate>, FleetError> {
        if !include_unconfirmed
            && !node
                .capabilities
                .iter()
                .any(|capability| capability == "setup-explicit-names-v1")
        {
            return Err(FleetError::new(
                FleetErrorKind::Incompatible,
                format!(
                    "{} needs an updated Pika for explicit-name setup filtering",
                    node.alias
                ),
            ));
        }
        let response = self.transport.request(&node.ssh_target, &json!({
            "op":"candidates", "include_unconfirmed":include_unconfirmed,
            "protocol":PROTOCOL_NAME, "version":PROTOCOL_VERSION, "expected_node_id":node.node_id,
        }), false)?;
        let envelope = object(&response, "Remote candidate inventory is malformed")?;
        exact_fields(
            envelope,
            &set(&["type", "node_id", "candidates"]),
            "Remote candidate inventory is malformed",
        )?;
        if envelope.get("type").and_then(Value::as_str) != Some("candidates")
            || envelope.get("node_id").and_then(Value::as_str) != Some(&node.node_id)
        {
            return Err(FleetError::new(
                FleetErrorKind::Incompatible,
                "Remote candidate inventory is malformed",
            ));
        }
        let values = envelope
            .get("candidates")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                FleetError::new(
                    FleetErrorKind::Incompatible,
                    "Remote candidate inventory is malformed",
                )
            })?;
        values
            .iter()
            .map(|raw| {
                exact_fields(
                    object(raw, "Remote candidate is malformed")?,
                    &set(CANDIDATE_FIELDS),
                    "Remote candidate is malformed",
                )?;
                let provider = parse_provider(raw.get("provider"))?;
                let session_id = valid_session_id(
                    raw.get("session_id"),
                    "Remote candidate has an invalid conversation identity",
                )?;
                let live = raw.get("live").and_then(Value::as_bool).ok_or_else(|| {
                    FleetError::new(
                        FleetErrorKind::Incompatible,
                        "Remote candidate is malformed",
                    )
                })?;
                Ok(Candidate {
                    provider,
                    session_id,
                    name: wire_string(raw.get("name"), "candidate name", MAX_TEXT_CHARS)?,
                    cwd: wire_string(raw.get("cwd"), "candidate cwd", MAX_TEXT_CHARS)?,
                    branch: wire_string(raw.get("branch"), "candidate branch", MAX_TEXT_CHARS)?,
                    transcript_path: None,
                    model: wire_string(raw.get("model"), "candidate model", MAX_TEXT_CHARS)?,
                    updated_at: finite_nonnegative(
                        raw.get("updated_at"),
                        "Remote candidate timestamp is invalid",
                    )?,
                    live,
                    pid: None,
                    source: required_text(
                        raw.get("source"),
                        "Remote candidate source is invalid",
                        MAX_TEXT_CHARS,
                    )?,
                    parent_session_id: None,
                    created_at: 0.0,
                    lifecycle_status: Some(if live {
                        Status::Unbound
                    } else {
                        Status::Parked
                    }),
                })
            })
            .collect()
    }

    fn session_request(
        &self,
        session: &FleetSession,
        op: &str,
        mutating: bool,
        extra: Option<Value>,
    ) -> Result<Value, FleetError> {
        let node = self
            .store
            .get_fleet_node(&session.node_id)?
            .ok_or_else(|| {
                FleetError::new(
                    FleetErrorKind::NotFound,
                    format!("Machine {:?} is no longer adopted", session.node_name),
                )
            })?;
        let mut payload = json!({
            "op":op, "protocol":PROTOCOL_NAME, "version":PROTOCOL_VERSION,
            "expected_node_id":node.node_id, "provider":session.session.provider.as_str(),
            "session_id":session.session.session_id,
        });
        if let (Some(target), Some(source)) = (
            payload.as_object_mut(),
            extra.and_then(|value| value.as_object().cloned()),
        ) {
            target.extend(source);
        }
        self.transport.request(&node.ssh_target, &payload, mutating)
    }

    fn snapshot_request(&self, node: &FleetNode) -> Result<Value, FleetError> {
        let mut request = json!({
            "op":"snapshot", "expert_directory":true, "protocol":PROTOCOL_NAME,
            "version":PROTOCOL_VERSION, "expected_node_id":node.node_id,
        });
        if node
            .capabilities
            .iter()
            .any(|capability| capability == "expert-directory-notices-v1")
        {
            request
                .as_object_mut()
                .expect("snapshot request")
                .insert("directory_notices".to_owned(), Value::Bool(true));
        }
        self.transport.request(&node.ssh_target, &request, false)
    }

    fn snapshot_request_cancellable(
        &self,
        node: &FleetNode,
        cancellation: &CancellationToken,
    ) -> Result<Value, FleetError> {
        let mut request = json!({
            "op":"snapshot", "expert_directory":true, "protocol":PROTOCOL_NAME,
            "version":PROTOCOL_VERSION, "expected_node_id":node.node_id,
        });
        if node
            .capabilities
            .iter()
            .any(|capability| capability == "expert-directory-notices-v1")
        {
            request
                .as_object_mut()
                .expect("snapshot request")
                .insert("directory_notices".to_owned(), Value::Bool(true));
        }
        self.transport
            .request_cancellable(&node.ssh_target, &request, false, cancellation)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ConsultationPolicy {
    pub consultation_mode: String,
    pub model: String,
    pub effort: String,
}

struct AsyncChildInput {
    requests: Option<mpsc::Sender<FleetWriteRequest>>,
}

struct FleetWriteRequest {
    bytes: Vec<u8>,
    result: SyncSender<std::result::Result<(), String>>,
}

impl AsyncChildInput {
    fn new(mut input: impl Write + Send + 'static) -> (Self, thread::JoinHandle<()>) {
        let (requests, receiver) = mpsc::channel::<FleetWriteRequest>();
        let worker = thread::spawn(move || {
            for request in receiver {
                let result = input
                    .write_all(&request.bytes)
                    .and_then(|_| input.flush())
                    .map_err(|error| error.to_string());
                let failed = result.is_err();
                let _ = request.result.send(result);
                if failed {
                    break;
                }
            }
        });
        (
            Self {
                requests: Some(requests),
            },
            worker,
        )
    }

    fn send(
        &self,
        bytes: Vec<u8>,
        deadline: Instant,
        cancellation: &CancellationToken,
    ) -> Result<(), FleetError> {
        if bytes.len() > MAX_MESSAGE_BYTES {
            return Err(FleetError::new(
                FleetErrorKind::InvalidRequest,
                "Remote side input exceeded the safety limit",
            ));
        }
        let (sender, result) = mpsc::sync_channel(1);
        self.requests
            .as_ref()
            .ok_or_else(|| {
                FleetError::new(
                    FleetErrorKind::Unreachable,
                    "Remote side input writer stopped",
                )
            })?
            .send(FleetWriteRequest {
                bytes,
                result: sender,
            })
            .map_err(|_| {
                FleetError::new(
                    FleetErrorKind::Unreachable,
                    "Remote side input writer stopped",
                )
            })?;
        loop {
            if cancellation.is_cancelled() {
                return Err(FleetError::new(
                    FleetErrorKind::OutcomeUnknown,
                    "Remote side input cancelled",
                ));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(FleetError::new(
                    FleetErrorKind::Unreachable,
                    "Remote side input timed out",
                ));
            }
            match result.recv_timeout(remaining.min(Duration::from_millis(25))) {
                Ok(Ok(())) => return Ok(()),
                Ok(Err(error)) => {
                    return Err(FleetError::new(
                        FleetErrorKind::Unreachable,
                        format!("Remote side input write failed: {error}"),
                    ));
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(FleetError::new(
                        FleetErrorKind::Unreachable,
                        "Remote side input writer stopped",
                    ));
                }
            }
        }
    }

    fn close(&mut self) {
        self.requests.take();
    }
}

pub struct RemoteConsultation {
    node: FleetNode,
    session: FleetSession,
    policy: ConsultationPolicy,
    child: OwnedChild,
    input: Option<AsyncChildInput>,
    events: Option<Receiver<Result<Value, FleetError>>>,
    stderr: Arc<Mutex<Vec<u8>>>,
    pipe_stop: CancellationToken,
    workers: Vec<thread::JoinHandle<()>>,
    cleanup_confirmed: bool,
    transport_aborted: bool,
    answers_received: u64,
    turn: u64,
    event_timeout: Duration,
    cleanup_timeout: Duration,
    cancellation: CancellationToken,
}

#[derive(Clone, Copy, Debug)]
pub struct ConsultationTimeouts {
    pub open: Duration,
    pub event: Duration,
    pub cleanup: Duration,
}

impl RemoteConsultation {
    pub fn open(
        transport: &SshTransport,
        node: FleetNode,
        session: FleetSession,
        policy: ConsultationPolicy,
        open_timeout: Duration,
        event_timeout: Duration,
        cleanup_timeout: Duration,
    ) -> Result<Self, FleetError> {
        Self::open_cancellable(
            transport,
            node,
            session,
            policy,
            ConsultationTimeouts {
                open: open_timeout,
                event: event_timeout,
                cleanup: cleanup_timeout,
            },
            CancellationToken::default(),
        )
    }

    pub fn open_cancellable(
        transport: &SshTransport,
        node: FleetNode,
        session: FleetSession,
        policy: ConsultationPolicy,
        timeouts: ConsultationTimeouts,
        cancellation: CancellationToken,
    ) -> Result<Self, FleetError> {
        validate_exact_route(&node, &session)?;
        session.require_consultable()?;
        let args = vec![
            "_fleet-ask".to_owned(),
            "--expected-node-id".to_owned(),
            node.node_id.clone(),
            "--provider".to_owned(),
            session.session.provider.as_str().to_owned(),
            "--session-id".to_owned(),
            session.session.session_id.clone(),
            "--consultation-mode".to_owned(),
            policy.consultation_mode.clone(),
            "--model".to_owned(),
            policy.model.clone(),
            "--effort".to_owned(),
            policy.effort.clone(),
        ];
        let mut command = transport.command(&node.ssh_target, &args, false)?;
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = OwnedChild::spawn(&mut command).map_err(|error| {
            FleetError::new(
                FleetErrorKind::Unreachable,
                format!("Could not start remote side channel: {error}"),
            )
        })?;
        let raw_pipes = (|| {
            Some((
                child.stdin.take()?,
                child.stdout.take()?,
                child.stderr.take()?,
            ))
        })();
        let Some((input, output, errors)) = raw_pipes else {
            kill_reap(&mut child);
            return Err(FleetError::new(
                FleetErrorKind::Error,
                "Remote side channel has incomplete process pipes",
            ));
        };
        let pipe_stop = CancellationToken::default();
        let cancellable_pipes = (|| -> std::io::Result<_> {
            Ok((
                CancellablePipe::new(input, pipe_stop.clone())?,
                CancellablePipe::new(output, pipe_stop.clone())?,
                CancellablePipe::new(errors, pipe_stop.clone())?,
            ))
        })();
        let (input, output, errors) = match cancellable_pipes {
            Ok(pipes) => pipes,
            Err(error) => {
                kill_reap(&mut child);
                return Err(FleetError::new(
                    FleetErrorKind::Unreachable,
                    format!("Could not prepare cancellable remote side pipes: {error}"),
                ));
            }
        };
        let (input, input_worker) = AsyncChildInput::new(input);
        let (sender, events) = mpsc::sync_channel(8);
        let output_worker = spawn_jsonl_reader(output, sender, MAX_MESSAGE_BYTES);
        let stderr = Arc::new(Mutex::new(Vec::new()));
        let stderr_worker = spawn_bounded_stderr(errors, stderr.clone(), MAX_STDERR_BYTES);
        let mut side = Self {
            node,
            session,
            policy,
            child,
            input: Some(input),
            events: Some(events),
            stderr,
            pipe_stop,
            workers: vec![input_worker, output_worker, stderr_worker],
            cleanup_confirmed: false,
            transport_aborted: false,
            answers_received: 0,
            turn: 0,
            event_timeout: timeouts.event,
            cleanup_timeout: timeouts.cleanup,
            cancellation,
        };
        let opened = match side.read_nonprogress(timeouts.open) {
            Ok(value) => value,
            Err(error) => {
                side.abort();
                return Err(error);
            }
        };
        if let Err(error) = side.validate_opened(&opened) {
            side.abort();
            return Err(error);
        }
        Ok(side)
    }

    pub fn ask(&mut self, question: &str) -> Result<String, FleetError> {
        if question.trim().is_empty() {
            return Err(FleetError::new(
                FleetErrorKind::InvalidRequest,
                "Question cannot be empty",
            ));
        }
        if question.len() > MAX_QUESTION_BYTES {
            return Err(FleetError::new(
                FleetErrorKind::InvalidRequest,
                "Consultation question exceeded the 64 KiB safety limit",
            ));
        }
        if self.cleanup_confirmed || self.transport_aborted {
            return Err(FleetError::new(
                FleetErrorKind::Error,
                "Remote side channel is closed",
            ));
        }
        self.turn += 1;
        let mut line = serde_json::to_vec(&json!({"question":question}))
            .map_err(|error| FleetError::new(FleetErrorKind::InvalidRequest, error.to_string()))?;
        line.push(b'\n');
        if line.len() > MAX_MESSAGE_BYTES {
            return Err(FleetError::new(
                FleetErrorKind::InvalidRequest,
                "Consultation question exceeded the safety limit",
            ));
        }
        let input = self.input.as_ref().ok_or_else(|| {
            FleetError::new(FleetErrorKind::Error, "Remote side channel is closed")
        })?;
        if let Err(error) = input.send(
            line,
            Instant::now() + self.event_timeout,
            &self.cancellation,
        ) {
            self.abort();
            return Err(FleetError::new(
                FleetErrorKind::OutcomeUnknown,
                format!("Remote turn delivery is unknown: {}", error.message),
            ));
        }
        let event = self.read_nonprogress(self.event_timeout)?;
        let object = match object(&event, "Remote side channel event is not an object") {
            Ok(object) => object,
            Err(error) => {
                self.abort();
                return Err(error);
            }
        };
        if object.get("type").and_then(Value::as_str) != Some("answer") {
            self.abort();
            return Err(FleetError::new(
                FleetErrorKind::Incompatible,
                "Remote side channel returned an invalid answer",
            ));
        }
        let text = match required_text(
            object.get("text"),
            "Remote side channel returned an invalid answer",
            MAX_MESSAGE_BYTES,
        ) {
            Ok(text) => text,
            Err(error) => {
                self.abort();
                return Err(error);
            }
        };
        self.answers_received += 1;
        Ok(text)
    }

    pub fn close(&mut self) -> Result<ConsultationReceipt, FleetError> {
        if self.cleanup_confirmed {
            return Ok(self.closed_receipt());
        }
        if self.transport_aborted {
            return Err(
                self.cleanup_unknown("Remote cleanup is unconfirmed after the connection closed")
            );
        }
        if let Some(mut input) = self.input.take()
            && input
                .send(
                    b"{\"close\":true}\n".to_vec(),
                    Instant::now() + self.cleanup_timeout,
                    &CancellationToken::default(),
                )
                .is_err()
        {
            input.close();
            self.abort();
            return Err(self.cleanup_unknown("Remote cleanup could not be requested"));
        }
        let deadline = Instant::now() + self.cleanup_timeout;
        loop {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                self.abort();
                return Err(self.cleanup_unknown("Remote cleanup timed out"));
            };
            let event = match self.read_nonprogress(remaining) {
                Ok(event) => event,
                Err(error)
                    if error
                        .receipt
                        .as_ref()
                        .and_then(|receipt| receipt.stage.as_deref())
                        == Some("cleanup") =>
                {
                    continue;
                }
                Err(_) => {
                    self.abort();
                    return Err(self.cleanup_unknown("Remote cleanup could not be confirmed"));
                }
            };
            let object = match object(&event, "Remote cleanup returned an unexpected event") {
                Ok(object) => object,
                Err(_) => {
                    self.abort();
                    return Err(self.cleanup_unknown("Remote cleanup returned an unexpected event"));
                }
            };
            if object.get("type").and_then(Value::as_str) != Some("closed") {
                self.abort();
                return Err(self.cleanup_unknown("Remote cleanup returned an unexpected event"));
            }
            let complete = object.get("receipt_version").and_then(Value::as_i64) == Some(2)
                && object.get("discarded").and_then(Value::as_bool) == Some(true)
                && object.get("cleanup").and_then(Value::as_str) == Some("complete");
            if !complete {
                let cleanup = if object.get("cleanup").and_then(Value::as_str) == Some("failed") {
                    "failed"
                } else {
                    "unknown"
                };
                self.abort();
                return Err(FleetError {
                    kind: FleetErrorKind::OutcomeUnknown,
                    message: "Remote side cleanup was not verified. Keep any received answer; do not resend it.".to_owned(),
                    receipt: Some(Box::new(ConsultationReceipt { cleanup: Some(cleanup.to_owned()), ..self.closed_receipt() })),
                });
            }
            if !wait_child(&mut self.child, remaining.min(Duration::from_secs(2))) {
                // The remote cleanup receipt is authoritative; only the local
                // owned SSH transport is still lingering.
                if let Err(error) = self.shutdown_transport() {
                    return Err(FleetError::new(
                        FleetErrorKind::Unreachable,
                        format!(
                            "Remote cleanup completed but the local SSH transport could not be reaped: {error}"
                        ),
                    ));
                }
            } else if let Err(error) = self.shutdown_transport() {
                return Err(FleetError::new(
                    FleetErrorKind::Unreachable,
                    format!(
                        "Remote cleanup completed but the local SSH transport could not be reaped: {error}"
                    ),
                ));
            }
            self.cleanup_confirmed = true;
            return Ok(self.closed_receipt());
        }
    }

    pub fn cancel(&mut self) -> Result<(), FleetError> {
        self.abort();
        Err(self.cleanup_unknown(
            "Remote side connection cancelled; cleanup is unconfirmed. Do not resend the question.",
        ))
    }

    fn validate_opened(&self, value: &Value) -> Result<(), FleetError> {
        let object = object(
            value,
            "Remote side channel returned an invalid opening receipt",
        )?;
        let expected_parent = self.session.session.provider_thread_id();
        let workstream = object
            .get("workstream_id")
            .and_then(Value::as_str)
            .or_else(|| object.get("parent_id").and_then(Value::as_str));
        let valid = object.get("type").and_then(Value::as_str) == Some("opened")
            && object.get("provider").and_then(Value::as_str)
                == Some(self.session.session.provider.as_str())
            && object.get("parent_id").and_then(Value::as_str) == Some(expected_parent)
            && workstream == Some(&self.session.session.session_id)
            && object.get("consultation_mode").and_then(Value::as_str)
                == Some(&self.policy.consultation_mode)
            && object.get("model").and_then(Value::as_str) == Some(&self.policy.model)
            && object.get("effort").and_then(Value::as_str) == Some(&self.policy.effort);
        if valid {
            Ok(())
        } else {
            Err(FleetError::new(
                FleetErrorKind::Quarantined,
                format!(
                    "Remote side channel returned a different conversation identity or policy; no question was sent. Refresh expert lookup for {} before retrying.",
                    self.session.qualified_name()
                ),
            ))
        }
    }

    fn read_nonprogress(&mut self, timeout: Duration) -> Result<Value, FleetError> {
        let deadline = Instant::now() + timeout;
        loop {
            if self.cancellation.is_cancelled() {
                self.abort();
                return Err(self.cleanup_unknown(
                    "Remote side connection cancelled; its owned child was terminated locally but remote cleanup could not be confirmed.",
                ));
            }
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                self.abort();
                return Err(FleetError::new(
                    FleetErrorKind::Unreachable,
                    "Remote side channel timed out",
                ));
            };
            let event = match self
                .events
                .as_ref()
                .ok_or_else(|| {
                    FleetError::new(FleetErrorKind::Unreachable, "Remote side channel closed")
                })?
                .recv_timeout(remaining.min(Duration::from_millis(25)))
            {
                Ok(Ok(event)) => event,
                Ok(Err(error)) => {
                    self.abort();
                    return Err(error);
                }
                Err(RecvTimeoutError::Timeout) if Instant::now() < deadline => continue,
                Err(RecvTimeoutError::Timeout) => {
                    self.abort();
                    return Err(FleetError::new(
                        FleetErrorKind::Unreachable,
                        "Remote side channel timed out",
                    ));
                }
                Err(RecvTimeoutError::Disconnected) => {
                    let detail = self
                        .stderr
                        .lock()
                        .ok()
                        .map(|value| String::from_utf8_lossy(&value).into_owned())
                        .unwrap_or_default();
                    self.abort();
                    return Err(FleetError::new(
                        FleetErrorKind::Unreachable,
                        if detail.trim().is_empty() {
                            "Remote side channel closed".to_owned()
                        } else {
                            prefix_tail(&sanitize_terminal_text(&detail), 500)
                        },
                    ));
                }
            };
            let object = match object(&event, "Remote side channel event is not an object") {
                Ok(object) => object,
                Err(error) => {
                    self.abort();
                    return Err(error);
                }
            };
            match object.get("type").and_then(Value::as_str) {
                Some("progress") => {
                    if let Err(error) = validate_progress(object) {
                        self.abort();
                        return Err(error);
                    }
                }
                Some("error") => {
                    let error = FleetError::new(
                        parse_error_kind(object.get("kind").and_then(Value::as_str)),
                        object
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("remote consultation failed"),
                    )
                    .with_receipt(object);
                    self.abort();
                    return Err(error);
                }
                _ => return Ok(event),
            }
        }
    }

    fn abort(&mut self) {
        self.transport_aborted = true;
        let _ = self.shutdown_transport();
    }

    fn shutdown_transport(&mut self) -> anyhow::Result<()> {
        let child_result = terminate_child(&mut self.child);
        // A helper can inherit SSH's descriptors after leaving the owned
        // process group. It must neither be signalled nor be allowed to keep
        // these local workers blocked forever.
        self.pipe_stop.cancel();
        if let Some(input) = self.input.as_mut() {
            input.close();
        }
        self.input.take();
        self.events.take();
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
        child_result
    }

    fn cleanup_unknown(&self, message: &str) -> FleetError {
        FleetError {
            kind: FleetErrorKind::OutcomeUnknown,
            message: format!(
                "{message} Inspect the consultation on {}.",
                self.node.ssh_target
            ),
            receipt: Some(Box::new(ConsultationReceipt {
                stage: Some("cleanup".to_owned()),
                delivery: None,
                cleanup: Some("unknown".to_owned()),
                answers_received: Some(self.answers_received),
                turn: Some(self.turn),
                retry_safe: Some(false),
            })),
        }
    }
    fn closed_receipt(&self) -> ConsultationReceipt {
        ConsultationReceipt {
            stage: Some("cleanup".to_owned()),
            delivery: None,
            cleanup: Some("complete".to_owned()),
            answers_received: Some(self.answers_received),
            turn: Some(self.turn),
            retry_safe: Some(false),
        }
    }
}

impl Drop for RemoteConsultation {
    fn drop(&mut self) {
        if !self.cleanup_confirmed {
            self.abort();
        }
    }
}

/// Local operations exposed to an already-authenticated SSH caller. Implementors
/// must revalidate exact process ownership before every action; the fleet layer
/// supplies immutable node/provider/conversation routing and bounded wire I/O.
pub trait FleetService {
    fn snapshot(&mut self, expert_directory: bool) -> Result<Value, FleetError>;
    fn candidates(&mut self, include_unconfirmed: bool) -> Result<Vec<Candidate>, FleetError>;
    fn adopt(&mut self, provider: Provider, session_id: &str) -> Result<Session, FleetError>;
    fn peek(
        &mut self,
        provider: Provider,
        session_id: &str,
        lines: usize,
    ) -> Result<String, FleetError>;
    fn acknowledge(
        &mut self,
        provider: Provider,
        session_id: &str,
        expected_last_event_at: f64,
    ) -> Result<bool, FleetError>;
    fn untrack(&mut self, provider: Provider, session_id: &str) -> Result<(i64, bool), FleetError>;
}

pub fn handle_fleet_stdio<S: FleetService, R: BufRead, W: Write>(
    store: &Store,
    machine: &str,
    package_version: &str,
    service: &mut S,
    mut input: R,
    mut output: W,
) -> Result<(), FleetError> {
    let node_id = store.ensure_local_node_id()?;
    let machine = machine_alias(machine)?;
    let mut line = Vec::new();
    loop {
        line.clear();
        let state = read_bounded_line(&mut input, &mut line, MAX_MESSAGE_BYTES)
            .map_err(|error| FleetError::new(FleetErrorKind::InvalidRequest, error.to_string()))?;
        if state == BoundedLine::Eof {
            break;
        }
        let response = if state.overflowed() {
            error_wire(FleetError::new(
                FleetErrorKind::InvalidRequest,
                "request too large",
            ))
        } else {
            match serde_json::from_slice::<Value>(&line) {
                Ok(request) => match handle_request(
                    store,
                    &node_id,
                    &machine,
                    package_version,
                    service,
                    &request,
                ) {
                    Ok(value) => value,
                    Err(error) => error_wire(error),
                },
                Err(_) => error_wire(FleetError::new(
                    FleetErrorKind::InvalidRequest,
                    "request must be valid JSON",
                )),
            }
        };
        let mut encoded = serde_json::to_vec(&response)
            .map_err(|error| FleetError::new(FleetErrorKind::Error, error.to_string()))?;
        encoded.push(b'\n');
        if encoded.len() > MAX_MESSAGE_BYTES {
            encoded =
                b"{\"type\":\"error\",\"kind\":\"error\",\"message\":\"response too large\"}\n"
                    .to_vec();
        }
        output
            .write_all(&encoded)
            .and_then(|_| output.flush())
            .map_err(|error| FleetError::new(FleetErrorKind::Error, error.to_string()))?;
    }
    Ok(())
}

fn handle_request<S: FleetService>(
    store: &Store,
    node_id: &str,
    machine: &str,
    package_version: &str,
    service: &mut S,
    request: &Value,
) -> Result<Value, FleetError> {
    let request = request.as_object().ok_or_else(|| {
        FleetError::new(FleetErrorKind::InvalidRequest, "request must be an object")
    })?;
    if request.get("protocol").and_then(Value::as_str) != Some(PROTOCOL_NAME)
        || request.get("version").and_then(Value::as_i64) != Some(PROTOCOL_VERSION)
    {
        return Err(FleetError::new(
            FleetErrorKind::Incompatible,
            format!("Pika fleet protocol {PROTOCOL_VERSION} required"),
        ));
    }
    if request
        .get("expected_node_id")
        .is_some_and(|value| value.as_str() != Some(node_id))
    {
        return Err(FleetError::new(
            FleetErrorKind::Quarantined,
            format!(
                "NODE IDENTITY CHANGED: expected {}, received {}",
                request
                    .get("expected_node_id")
                    .and_then(Value::as_str)
                    .map(|v| prefix(v, 8))
                    .unwrap_or("invalid"),
                prefix(node_id, 8)
            ),
        ));
    }
    let op = request.get("op").and_then(Value::as_str).ok_or_else(|| {
        FleetError::new(FleetErrorKind::InvalidRequest, "Fleet operation is missing")
    })?;
    let allowed: &[&str] = match op {
        "hello" => &["op", "protocol", "version", "expected_node_id"],
        "snapshot" => &[
            "op",
            "protocol",
            "version",
            "expected_node_id",
            "expert_directory",
            "directory_notices",
        ],
        "candidates" => &[
            "op",
            "protocol",
            "version",
            "expected_node_id",
            "include_unconfirmed",
        ],
        "adopt" | "untrack" => &[
            "op",
            "protocol",
            "version",
            "expected_node_id",
            "provider",
            "session_id",
            "request_id",
        ],
        "peek" => &[
            "op",
            "protocol",
            "version",
            "expected_node_id",
            "provider",
            "session_id",
            "lines",
        ],
        "acknowledge" => &[
            "op",
            "protocol",
            "version",
            "expected_node_id",
            "provider",
            "session_id",
            "expected_last_event_at",
        ],
        _ => {
            return Err(FleetError::new(
                FleetErrorKind::InvalidRequest,
                format!("Unsupported fleet operation {op:?}"),
            ));
        }
    };
    if request.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err(FleetError::new(
            FleetErrorKind::InvalidRequest,
            "Fleet request contains unsupported fields",
        ));
    }
    match op {
        "hello" => Ok(json!({
            "type":"hello", "protocol":PROTOCOL_NAME, "version":PROTOCOL_VERSION,
            "node_id":node_id, "machine":machine, "package_version":package_version,
            "capabilities":CAPABILITIES,
        })),
        "snapshot" => {
            let extended =
                optional_bool(request.get("expert_directory"), false, "expert_directory")?;
            let include_notices =
                optional_bool(request.get("directory_notices"), false, "directory_notices")?;
            let mut snapshot = service.snapshot(extended)?;
            if !include_notices {
                snapshot
                    .as_object_mut()
                    .ok_or_else(|| {
                        FleetError::new(FleetErrorKind::Error, "Local snapshot is malformed")
                    })?
                    .remove("directory_notices");
            }
            validate_snapshot(&snapshot, Some(node_id))
        }
        "candidates" => {
            let include = optional_bool(
                request.get("include_unconfirmed"),
                false,
                "include_unconfirmed",
            )?;
            Ok(
                json!({"type":"candidates", "node_id":node_id, "candidates":service.candidates(include)?.iter().map(candidate_to_wire).collect::<Vec<_>>() }),
            )
        }
        "adopt" => {
            let (provider, session_id) = exact_request_identity(request)?;
            let request_id = request_uuid(request)?;
            let key = format!("fleet:adopt:{request_id}");
            if let Some(saved) = store.get_meta(&key)? {
                return serde_json::from_str(&saved).map_err(|_| {
                    FleetError::new(FleetErrorKind::Error, "Stored adoption receipt is corrupt")
                });
            }
            let session = service.adopt(provider, &session_id)?;
            if session.provider != provider || session.session_id != session_id {
                return Err(FleetError::new(
                    FleetErrorKind::Quarantined,
                    "Adoption returned the wrong exact identity",
                ));
            }
            let receipt = json!({"type":"adopted", "node_id":node_id, "request_id":request_id, "session":session_to_wire(&session, false)});
            store.set_meta(
                &key,
                &serde_json::to_string(&receipt).expect("serializable"),
            )?;
            Ok(receipt)
        }
        "peek" => {
            let (provider, session_id) = exact_request_identity(request)?;
            let lines = request
                .get("lines")
                .and_then(Value::as_u64)
                .unwrap_or(200)
                .clamp(1, 2000) as usize;
            Ok(
                json!({"type":"peek", "node_id":node_id, "text":service.peek(provider, &session_id, lines)?}),
            )
        }
        "acknowledge" => {
            let (provider, session_id) = exact_request_identity(request)?;
            let expected_last_event_at = request
                .get("expected_last_event_at")
                .and_then(Value::as_f64)
                .filter(|number| number.is_finite() && *number >= 0.0)
                .ok_or_else(|| {
                    FleetError::new(
                        FleetErrorKind::Incompatible,
                        "Event-bound acknowledgement requires expected_last_event_at; legacy acknowledgement is incompatible",
                    )
                })?;
            Ok(
                json!({"type":"acknowledged", "node_id":node_id, "acknowledged":service.acknowledge(provider, &session_id, expected_last_event_at)?}),
            )
        }
        "untrack" => {
            let (provider, session_id) = exact_request_identity(request)?;
            let request_id = request_uuid(request)?;
            let key = format!("fleet:untrack:{request_id}");
            if let Some(saved) = store.get_meta(&key)? {
                return serde_json::from_str(&saved).map_err(|_| {
                    FleetError::new(FleetErrorKind::Error, "Stored untrack receipt is corrupt")
                });
            }
            let (panes, reconciled) = service.untrack(provider, &session_id)?;
            if panes < 0 {
                return Err(FleetError::new(
                    FleetErrorKind::Error,
                    "Untrack returned an invalid pane count",
                ));
            }
            let receipt = json!({"type":"untracked", "node_id":node_id, "request_id":request_id, "panes_cleared":panes, "reconciled":reconciled});
            store.set_meta(
                &key,
                &serde_json::to_string(&receipt).expect("serializable"),
            )?;
            Ok(receipt)
        }
        _ => unreachable!(),
    }
}

fn exact_request_identity(request: &Map<String, Value>) -> Result<(Provider, String), FleetError> {
    let provider = parse_provider(request.get("provider")).map_err(|_| {
        FleetError::new(
            FleetErrorKind::InvalidRequest,
            "Invalid exact provider identity",
        )
    })?;
    let session = valid_session_id(
        request.get("session_id"),
        "Invalid exact conversation identity",
    )
    .map_err(|_| {
        FleetError::new(
            FleetErrorKind::InvalidRequest,
            "Invalid exact conversation identity",
        )
    })?;
    if !Providers::valid_id(provider, &session) {
        return Err(FleetError::new(
            FleetErrorKind::InvalidRequest,
            "Invalid exact conversation identity",
        ));
    }
    Ok((provider, session))
}
fn request_uuid(request: &Map<String, Value>) -> Result<String, FleetError> {
    canonical_uuid(
        request.get("request_id"),
        "Mutation requires a UUID request_id",
    )
    .map_err(|_| {
        FleetError::new(
            FleetErrorKind::InvalidRequest,
            "Mutation requires a UUID request_id",
        )
    })
}
fn optional_bool(value: Option<&Value>, default: bool, field: &str) -> Result<bool, FleetError> {
    match value {
        None => Ok(default),
        Some(value) => value.as_bool().ok_or_else(|| {
            FleetError::new(
                FleetErrorKind::InvalidRequest,
                format!("{field} must be a boolean"),
            )
        }),
    }
}
fn error_wire(error: FleetError) -> Value {
    json!({"type":"error", "kind":error.kind.as_str(), "message":error.message})
}

#[derive(Debug)]
struct Hello {
    node_id: String,
    package_version: String,
    capabilities: Vec<String>,
}

fn validate_hello(
    value: &Value,
    local_node_id: Option<&str>,
    expected_node_id: Option<&str>,
) -> Result<Hello, FleetError> {
    let object = object(value, "Remote Pika handshake envelope is malformed")?;
    exact_fields(
        object,
        &set(&[
            "type",
            "protocol",
            "version",
            "node_id",
            "machine",
            "package_version",
            "capabilities",
        ]),
        "Remote Pika handshake envelope is malformed",
    )?;
    if object.get("type").and_then(Value::as_str) != Some("hello")
        || object.get("protocol").and_then(Value::as_str) != Some(PROTOCOL_NAME)
        || object.get("version").and_then(Value::as_i64) != Some(PROTOCOL_VERSION)
    {
        return Err(FleetError::new(
            FleetErrorKind::Incompatible,
            format!("Remote Pika protocol is incompatible with {PROTOCOL_VERSION}"),
        ));
    }
    let node_id = canonical_uuid(
        object.get("node_id"),
        "Remote Pika returned an invalid node identity",
    )?;
    if local_node_id == Some(&node_id) {
        return Err(FleetError::new(
            FleetErrorKind::Quarantined,
            "That SSH target resolves back to this Pika node",
        ));
    }
    if expected_node_id.is_some_and(|expected| expected != node_id) {
        return Err(FleetError::new(
            FleetErrorKind::Quarantined,
            "NODE IDENTITY CHANGED: refusing to operate on the machine now behind this SSH alias",
        ));
    }
    required_text(
        object.get("machine"),
        "Remote Pika handshake has an invalid machine name",
        63,
    )?;
    let package_version = required_text(
        object.get("package_version"),
        "Remote Pika did not report a valid package version",
        64,
    )?;
    let capabilities = string_array(
        object.get("capabilities"),
        "Remote Pika lacks required fleet capabilities",
        64,
    )?;
    let unique: BTreeSet<_> = capabilities.iter().collect();
    if unique.len() != capabilities.len()
        || REQUIRED_CAPABILITIES
            .iter()
            .any(|required| !capabilities.iter().any(|value| value == required))
    {
        return Err(FleetError::new(
            FleetErrorKind::Incompatible,
            "Remote Pika lacks required fleet capabilities",
        ));
    }
    Ok(Hello {
        node_id,
        package_version,
        capabilities,
    })
}

fn validate_mutation_receipt(
    value: &Value,
    operation: &str,
    expected_node: &str,
    expected_request: &str,
) -> Result<(), FleetError> {
    let object = object(value, &format!("Remote {operation} receipt is malformed"))?;
    let allowed = if operation == "adopted" {
        set(&["type", "node_id", "request_id", "session"])
    } else {
        set(&[
            "type",
            "node_id",
            "request_id",
            "panes_cleared",
            "reconciled",
        ])
    };
    if object.keys().any(|key| !allowed.contains(key.as_str()))
        || object.get("type").and_then(Value::as_str) != Some(operation)
    {
        return Err(FleetError::new(
            FleetErrorKind::Quarantined,
            format!("Remote {operation} receipt is malformed"),
        ));
    }
    if object.get("node_id").and_then(Value::as_str) != Some(expected_node) {
        return Err(FleetError::new(
            FleetErrorKind::Quarantined,
            format!("Remote {operation} receipt has the wrong node identity"),
        ));
    }
    if object.get("request_id").and_then(Value::as_str) != Some(expected_request) {
        return Err(FleetError::new(
            FleetErrorKind::Quarantined,
            format!("Remote {operation} receipt has the wrong request identity"),
        ));
    }
    if operation == "untracked"
        && (object
            .get("panes_cleared")
            .and_then(Value::as_i64)
            .is_none_or(|value| value < 0)
            || object
                .get("reconciled")
                .is_some_and(|value| !value.is_boolean()))
    {
        return Err(FleetError::new(
            FleetErrorKind::Quarantined,
            "Remote untracked receipt has an invalid pane count",
        ));
    }
    Ok(())
}

fn parse_provider(value: Option<&Value>) -> Result<Provider, FleetError> {
    value
        .and_then(Value::as_str)
        .ok_or_else(|| {
            FleetError::new(
                FleetErrorKind::Incompatible,
                "Remote record has an invalid provider",
            )
        })?
        .parse()
        .map_err(|_| {
            FleetError::new(
                FleetErrorKind::Incompatible,
                "Remote record has an invalid provider",
            )
        })
}
fn parse_status(value: Option<&Value>) -> Result<Status, FleetError> {
    value
        .and_then(Value::as_str)
        .ok_or_else(|| {
            FleetError::new(
                FleetErrorKind::Incompatible,
                "Remote session has an invalid state",
            )
        })?
        .parse()
        .map_err(|_| {
            FleetError::new(
                FleetErrorKind::Incompatible,
                "Remote session has an invalid state",
            )
        })
}
fn valid_session_id(value: Option<&Value>, message: &str) -> Result<String, FleetError> {
    let value = value
        .and_then(Value::as_str)
        .ok_or_else(|| FleetError::new(FleetErrorKind::Incompatible, message))?;
    if value.is_empty()
        || value.len() > 128
        || !value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, ':' | '.' | '_' | '%' | '+' | '-'))
    {
        Err(FleetError::new(FleetErrorKind::Incompatible, message))
    } else {
        Ok(value.to_owned())
    }
}
fn optional_session_id(value: Option<&Value>, message: &str) -> Result<Option<String>, FleetError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        value => valid_session_id(value, message).map(Some),
    }
}
fn finite_nonnegative(value: Option<&Value>, message: &str) -> Result<f64, FleetError> {
    value
        .and_then(Value::as_f64)
        .filter(|number| number.is_finite() && *number >= 0.0)
        .ok_or_else(|| FleetError::new(FleetErrorKind::Incompatible, message))
}
fn optional_nonnegative(value: Option<&Value>, field: &str) -> Result<Option<f64>, FleetError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        value => finite_nonnegative(
            value,
            &format!("Remote session field {field} has an invalid value"),
        )
        .map(Some),
    }
}
fn wire_string(
    value: Option<&Value>,
    field: &str,
    max: usize,
) -> Result<Option<String>, FleetError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) if value.chars().count() <= max => {
            Ok(Some(sanitize_terminal_text(value)))
        }
        _ => Err(FleetError::new(
            FleetErrorKind::Incompatible,
            format!("Remote {field} has the wrong type or size"),
        )),
    }
}
fn required_text(value: Option<&Value>, message: &str, max: usize) -> Result<String, FleetError> {
    match value {
        Some(Value::String(value)) if !value.is_empty() && value.chars().count() <= max => {
            Ok(sanitize_terminal_text(value))
        }
        _ => Err(FleetError::new(FleetErrorKind::Incompatible, message)),
    }
}
fn expert_text(value: Option<&Value>, message: &str, max: usize) -> Result<String, FleetError> {
    match value {
        Some(Value::String(value)) if value.chars().count() <= max => {
            Ok(sanitize_terminal_text(value))
        }
        _ => Err(FleetError::new(FleetErrorKind::Incompatible, message)),
    }
}
fn string_array(
    value: Option<&Value>,
    message: &str,
    max_items: usize,
) -> Result<Vec<String>, FleetError> {
    let items = value
        .and_then(Value::as_array)
        .filter(|items| items.len() <= max_items)
        .ok_or_else(|| FleetError::new(FleetErrorKind::Incompatible, message))?;
    items
        .iter()
        .map(|value| required_text(Some(value), message, MAX_TEXT_CHARS))
        .collect()
}
fn object<'a>(value: &'a Value, message: &str) -> Result<&'a Map<String, Value>, FleetError> {
    value
        .as_object()
        .ok_or_else(|| FleetError::new(FleetErrorKind::Incompatible, message))
}
fn exact_fields(
    object: &Map<String, Value>,
    expected: &BTreeSet<&str>,
    message: &str,
) -> Result<(), FleetError> {
    if object.len() != expected.len() || object.keys().any(|key| !expected.contains(key.as_str())) {
        Err(FleetError::new(FleetErrorKind::Incompatible, message))
    } else {
        Ok(())
    }
}
fn set<'a>(values: &'a [&'a str]) -> BTreeSet<&'a str> {
    values.iter().copied().collect()
}
fn canonical_uuid(value: Option<&Value>, message: &str) -> Result<String, FleetError> {
    let raw = value
        .and_then(Value::as_str)
        .ok_or_else(|| FleetError::new(FleetErrorKind::Incompatible, message))?;
    Uuid::parse_str(raw)
        .map(|value| value.to_string())
        .map_err(|_| FleetError::new(FleetErrorKind::Incompatible, message))
}
fn validate_exact_route(node: &FleetNode, session: &FleetSession) -> Result<(), FleetError> {
    Uuid::parse_str(&node.node_id).map_err(|_| {
        FleetError::new(
            FleetErrorKind::InvalidRequest,
            "Invalid trusted node identity",
        )
    })?;
    if node.node_id != session.node_id {
        return Err(FleetError::new(
            FleetErrorKind::Quarantined,
            "Remote route combines different node identities",
        ));
    }
    valid_session_id(
        Some(&Value::String(session.session.session_id.clone())),
        "Invalid exact conversation identity",
    )?;
    if !Providers::valid_id(session.session.provider, &session.session.session_id) {
        return Err(FleetError::new(
            FleetErrorKind::InvalidRequest,
            "Invalid exact conversation identity",
        ));
    }
    Ok(())
}

/// Translate a peer timestamp onto the local observation clock. Peer clock
/// offset cancels because only the peer-relative age at capture crosses the
/// trust boundary; elapsed freshness then advances on the receiver's clock.
fn receiver_clock_timestamp(received_at: f64, remote_captured_at: f64, event_at: f64) -> f64 {
    received_at - (remote_captured_at - event_at).max(0.0)
}

#[cfg(test)]
mod receiver_clock_tests {
    use super::*;

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    #[cfg(unix)]
    fn write_executable(path: &Path, body: &str) {
        fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(path, permissions).unwrap();
    }

    #[cfg(unix)]
    fn consultation_fixture(ssh: &Path, event_timeout: Duration) -> RemoteConsultation {
        let node_id = "11111111-1111-4111-8111-111111111111";
        let node = FleetNode {
            node_id: node_id.into(),
            alias: "fixture".into(),
            ssh_target: "fixture".into(),
            sources: vec!["test".into()],
            status: "ready".into(),
            protocol_version: Some(PROTOCOL_VERSION),
            package_version: Some("test".into()),
            capabilities: CAPABILITIES.iter().map(|value| (*value).into()).collect(),
            last_seen: 0.0,
            last_attempt_at: 0.0,
            last_error: None,
            created_at: 0.0,
            updated_at: 0.0,
        };
        let session_id = "22222222-2222-4222-8222-222222222222";
        let session = FleetSession {
            node_id: node_id.into(),
            node_name: "fixture".into(),
            session: Session {
                provider: Provider::Codex,
                session_id: session_id.into(),
                name: Some("fixture".into()),
                cwd: Some("/tmp".into()),
                branch: None,
                transcript_path: None,
                tmux_session: None,
                tmux_pane: None,
                root_pid: None,
                status: Status::Working,
                unread: false,
                model: None,
                source: "test".into(),
                managed: true,
                error: None,
                attention_reason: None,
                created_at: 0.0,
                updated_at: 0.0,
                last_event_at: 0.0,
                last_activity_at: 0.0,
                live: true,
                attached: false,
                home_state: "exact".into(),
                cpu_percent: None,
                rss_kb: None,
                input_tokens: None,
                output_tokens: None,
                cached_input_tokens: None,
                cache_write_tokens: None,
                total_tokens: None,
                estimated_cost_usd: None,
                active_thread_id: None,
            },
            stale: false,
            remote_error: None,
            seen_at: 0.0,
            card_status: None,
            card_detail: None,
            watched: true,
            availability: Some("source-available".into()),
            scope_updated_at: None,
            current_state_updated_at: None,
            current_state_status: None,
        };
        RemoteConsultation::open(
            &SshTransport::new(ssh, Duration::from_secs(1), Duration::from_secs(1)),
            node,
            session,
            ConsultationPolicy {
                consultation_mode: "default".into(),
                model: "gpt-test".into(),
                effort: "low".into(),
            },
            Duration::from_secs(1),
            event_timeout,
            Duration::from_millis(200),
        )
        .unwrap()
    }

    #[cfg(unix)]
    fn opened_fixture() -> String {
        json!({
            "type":"opened",
            "provider":"codex",
            "parent_id":"22222222-2222-4222-8222-222222222222",
            "workstream_id":"22222222-2222-4222-8222-222222222222",
            "consultation_mode":"default",
            "model":"gpt-test",
            "effort":"low"
        })
        .to_string()
    }

    #[cfg(unix)]
    #[test]
    fn remote_cleanup_cancels_inherited_pipes_without_signalling_foreign_holder() {
        let root = tempfile::tempdir().unwrap();
        let ssh = root.path().join("ssh");
        let pid_path = root.path().join("escaped.pid");
        let release_path = root.path().join("release");
        let done_path = root.path().join("escaped.done");
        let test_binary = std::env::current_exe().unwrap();
        write_executable(
            &ssh,
            &format!(
                "printf '%s\\n' {}\nPIKA_TEST_ESCAPED_PID={} PIKA_TEST_ESCAPED_RELEASE={} PIKA_TEST_ESCAPED_DONE={} exec {} --exact consult::tests::escaped_pipe_holder_fixture --nocapture",
                shell_words::quote(&opened_fixture()),
                shell_words::quote(&pid_path.to_string_lossy()),
                shell_words::quote(&release_path.to_string_lossy()),
                shell_words::quote(&done_path.to_string_lossy()),
                shell_words::quote(&test_binary.to_string_lossy()),
            ),
        );
        let mut side = consultation_fixture(&ssh, Duration::from_secs(30));
        let holder_pid = (0..500)
            .find_map(|_| {
                let value = fs::read_to_string(&pid_path).ok();
                if value.is_none() {
                    thread::sleep(Duration::from_millis(2));
                }
                value
            })
            .expect("escaped pipe holder did not start")
            .parse::<i32>()
            .unwrap();

        let started = Instant::now();
        assert_eq!(
            side.cancel().unwrap_err().kind,
            FleetErrorKind::OutcomeUnknown
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(side.workers.is_empty());
        assert!(side.input.is_none());
        assert!(side.events.is_none());
        assert!(side.pipe_stop.is_cancelled());
        let _ = side.child.exit_status();
        // Repeated cleanup must use the cached child status; it must never
        // signal a numeric PID/PGID after ownership has been reaped.
        assert_eq!(
            side.cancel().unwrap_err().kind,
            FleetErrorKind::OutcomeUnknown
        );
        assert!(side.close().is_err());
        // SAFETY: signal zero only observes this disposable foreign fixture.
        assert_eq!(unsafe { libc::kill(holder_pid, 0) }, 0);

        fs::write(&release_path, b"release").unwrap();
        for _ in 0..500 {
            if done_path.exists() {
                return;
            }
            thread::sleep(Duration::from_millis(2));
        }
        panic!("escaped pipe holder did not finish after release");
    }

    #[cfg(unix)]
    #[test]
    fn remote_cleanup_interrupts_a_blocked_input_writer_and_reaps_child() {
        let root = tempfile::tempdir().unwrap();
        let ssh = root.path().join("ssh");
        write_executable(
            &ssh,
            &format!(
                "printf '%s\\n' {}; exec sleep 30",
                shell_words::quote(&opened_fixture())
            ),
        );
        let mut side = consultation_fixture(&ssh, Duration::from_millis(50));
        let started = Instant::now();
        let error = side.ask(&"x".repeat(MAX_QUESTION_BYTES)).unwrap_err();
        assert_eq!(error.kind, FleetErrorKind::OutcomeUnknown);
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(side.workers.is_empty());
        assert!(side.pipe_stop.is_cancelled());
        let _ = side.child.exit_status();
    }

    #[cfg(unix)]
    #[test]
    fn successful_remote_close_joins_workers_once_and_remains_idempotent() {
        let root = tempfile::tempdir().unwrap();
        let ssh = root.path().join("ssh");
        write_executable(
            &ssh,
            &format!(
                "printf '%s\\n' {}\nwhile IFS= read -r line; do case \"$line\" in *close*) printf '%s\\n' '{{\"type\":\"closed\",\"receipt_version\":2,\"discarded\":true,\"cleanup\":\"complete\"}}'; exit 0;; esac; done",
                shell_words::quote(&opened_fixture())
            ),
        );
        let mut side = consultation_fixture(&ssh, Duration::from_secs(1));
        assert_eq!(side.close().unwrap().cleanup.as_deref(), Some("complete"));
        assert!(side.workers.is_empty());
        assert!(side.input.is_none());
        assert!(side.events.is_none());
        let _ = side.child.exit_status();
        assert_eq!(side.close().unwrap().cleanup.as_deref(), Some("complete"));
    }

    fn wire_session(provider: Provider, session_id: &str, active_thread_id: Option<&str>) -> Value {
        session_to_wire(
            &Session {
                provider,
                session_id: session_id.into(),
                name: None,
                cwd: None,
                branch: None,
                transcript_path: None,
                tmux_session: None,
                tmux_pane: None,
                root_pid: None,
                status: Status::Parked,
                unread: false,
                model: None,
                source: "test".into(),
                managed: true,
                error: None,
                attention_reason: None,
                created_at: 0.0,
                updated_at: 0.0,
                last_event_at: 0.0,
                last_activity_at: 0.0,
                live: false,
                attached: false,
                home_state: "saved-idle".into(),
                cpu_percent: None,
                rss_kb: None,
                input_tokens: None,
                output_tokens: None,
                cached_input_tokens: None,
                cache_write_tokens: None,
                total_tokens: None,
                estimated_cost_usd: None,
                active_thread_id: active_thread_id.map(str::to_owned),
            },
            true,
        )
    }

    fn snapshot_with(session: Value) -> Value {
        json!({
            "type":"snapshot", "protocol":PROTOCOL_NAME, "version":PROTOCOL_VERSION,
            "node_id":"11111111-1111-4111-8111-111111111111", "machine":"remote",
            "captured_at":1.0, "sessions":[session], "expert_sessions":[],
            "profiles":[], "cards":[]
        })
    }

    #[test]
    fn future_peer_clock_cannot_pin_remote_freshness() {
        let observed = receiver_clock_timestamp(1_000.0, 4_000_000_000.0, 4_000_000_100.0);
        assert_eq!(observed, 1_000.0);
        assert_eq!((1_075.0_f64 - observed).max(0.0), 75.0);
    }

    #[test]
    fn peer_relative_age_survives_clock_translation() {
        assert_eq!(
            receiver_clock_timestamp(1_000.0, 4_000_000_000.0, 3_999_999_970.0),
            970.0
        );
    }

    #[test]
    fn snapshots_reject_provider_mismatched_conversation_ids() {
        for (provider, invalid) in [
            (Provider::Codex, "ses_abcdef12"),
            (Provider::Claude, "not-a-uuid"),
            (Provider::Opencode, "22222222-2222-4222-8222-222222222222"),
        ] {
            let error =
                validate_snapshot(&snapshot_with(wire_session(provider, invalid, None)), None)
                    .unwrap_err();
            assert!(error.message.contains("noncanonical"));
        }
    }

    #[test]
    fn snapshots_reject_noncanonical_active_thread_ids() {
        let session = wire_session(
            Provider::Codex,
            "22222222-2222-4222-8222-222222222222",
            Some("ses_wrong-provider"),
        );
        let error = validate_snapshot(&snapshot_with(session), None).unwrap_err();
        assert!(error.message.contains("active conversation"));
    }
}
fn mutation_request_id(
    store: &Store,
    key: &str,
    supplied: Option<&str>,
) -> Result<String, FleetError> {
    let candidate = supplied
        .map(str::to_owned)
        .or(store.get_meta(key)?)
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    let canonical = Uuid::parse_str(&candidate)
        .map_err(|_| {
            FleetError::new(
                FleetErrorKind::InvalidRequest,
                "Mutation requires a UUID request_id",
            )
        })?
        .to_string();
    store.set_meta(key, &canonical)?;
    Ok(canonical)
}
fn mutation_unknown(
    error: FleetError,
    operation: &str,
    alias: &str,
    request_id: &str,
) -> FleetError {
    if error.kind == FleetErrorKind::OutcomeUnknown {
        FleetError::new(
            FleetErrorKind::OutcomeUnknown,
            format!("{operation} OUTCOME UNKNOWN on {alias} · request {request_id}"),
        )
    } else {
        error
    }
}
fn keyed_values(value: Option<&Value>) -> BTreeMap<(String, String), Value> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|value| {
            let object = value.as_object()?;
            Some((
                (
                    object.get("provider")?.as_str()?.to_owned(),
                    object.get("session_id")?.as_str()?.to_owned(),
                ),
                value.clone(),
            ))
        })
        .collect()
}

fn snapshot_cache_notices(snapshot: &Value, node: &FleetNode) -> Vec<FleetCacheNotice> {
    cache_notices_from_values(
        snapshot
            .get("directory_notices")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default(),
        node,
    )
}

fn cache_notices_from_values(notices: &[Value], node: &FleetNode) -> Vec<FleetCacheNotice> {
    notices
        .iter()
        .filter_map(|notice| {
            Some(FleetCacheNotice {
                node_id: node.node_id.clone(),
                node_name: node.alias.clone(),
                kind: notice.get("kind")?.as_str()?.to_owned(),
                omitted_rows: usize::try_from(notice.get("omitted_rows")?.as_u64()?).ok()?,
                message: notice.get("message")?.as_str()?.to_owned(),
            })
        })
        .collect()
}

fn cache_input_notice(node: &FleetNode, kind: &str, detail: &str) -> FleetCacheNotice {
    FleetCacheNotice {
        node_id: node.node_id.clone(),
        node_name: node.alias.clone(),
        kind: kind.to_owned(),
        omitted_rows: 0,
        message: format!("{} · {detail}", node.alias),
    }
}

fn cached_fleet_session(
    node: &FleetNode,
    session: Session,
    stale: bool,
    seen_at: f64,
    remote_captured_at: f64,
    card: Option<&Map<String, Value>>,
    profile: Option<&Map<String, Value>>,
) -> FleetSession {
    FleetSession {
        node_id: node.node_id.clone(),
        node_name: node.alias.clone(),
        session,
        stale,
        remote_error: node.last_error.clone(),
        seen_at,
        card_status: card
            .and_then(|value| value.get("status"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        card_detail: card
            .and_then(|value| value.get("detail"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        watched: card
            .and_then(|value| value.get("watched"))
            .and_then(Value::as_bool)
            .unwrap_or(true),
        availability: card
            .and_then(|value| value.get("availability"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        scope_updated_at: profile
            .and_then(|value| value.get("scope_updated_at"))
            .and_then(Value::as_f64)
            .map(|value| receiver_clock_timestamp(seen_at, remote_captured_at, value)),
        current_state_updated_at: profile
            .and_then(|value| value.get("current_state_updated_at"))
            .and_then(Value::as_f64)
            .map(|value| receiver_clock_timestamp(seen_at, remote_captured_at, value)),
        current_state_status: card
            .and_then(|value| value.get("current_state_status"))
            .and_then(Value::as_str)
            .map(str::to_owned),
    }
}

fn profile_from_wire(value: &Value) -> Result<StoredExpertProfile, FleetError> {
    let object = object(value, "Remote expert profile is malformed")?;
    Ok(StoredExpertProfile {
        profile: ExpertProfile {
            provider: parse_provider(object.get("provider"))?,
            session_id: valid_session_id(
                object.get("session_id"),
                "Remote expert profile has an invalid conversation identity",
            )?,
            summary: expert_text(
                object.get("scope"),
                "Remote expert scope has the wrong type or size",
                MAX_TEXT_CHARS,
            )?,
            current_state: expert_text(
                object.get("current_state"),
                "Remote expert current state has the wrong type or size",
                MAX_TEXT_CHARS,
            )?,
            topics: string_array(
                object.get("topics"),
                "Remote expert topics are malformed",
                MAX_EXPERT_ITEMS,
            )?,
            artifacts: string_array(
                object.get("artifacts"),
                "Remote expert artifacts are malformed",
                MAX_EXPERT_ITEMS,
            )?,
            source: expert_text(
                object.get("source"),
                "Remote expert source has the wrong type or size",
                MAX_TEXT_CHARS,
            )?,
            updated_at: finite_nonnegative(
                object.get("updated_at"),
                "Remote expert profile has an invalid timestamp",
            )?,
            scope_updated_at: object
                .get("scope_updated_at")
                .map(|value| finite_nonnegative(Some(value), "Remote expert clock is invalid"))
                .transpose()?
                .unwrap_or(0.0),
            current_state_updated_at: object
                .get("current_state_updated_at")
                .map(|value| finite_nonnegative(Some(value), "Remote expert clock is invalid"))
                .transpose()?
                .unwrap_or(0.0),
        },
        transcript_mtime_ns: None,
        transcript_size: None,
        current_state_mtime_ns: None,
        current_state_size: None,
    })
}

fn parse_card_status(value: &str) -> Option<CardStatus> {
    match value {
        "CURRENT" => Some(CardStatus::Current),
        "STALE" => Some(CardStatus::Stale),
        "MISSING" => Some(CardStatus::Missing),
        "UNKNOWN" => Some(CardStatus::Unknown),
        _ => None,
    }
}

fn parse_error_kind(value: Option<&str>) -> FleetErrorKind {
    match value {
        Some("invalid_request") => FleetErrorKind::InvalidRequest,
        Some("incompatible") => FleetErrorKind::Incompatible,
        Some("quarantined") => FleetErrorKind::Quarantined,
        Some("unreachable") => FleetErrorKind::Unreachable,
        Some("auth") => FleetErrorKind::Authentication,
        Some("missing") => FleetErrorKind::Missing,
        Some("not_found") => FleetErrorKind::NotFound,
        Some("outcome_unknown") => FleetErrorKind::OutcomeUnknown,
        _ => FleetErrorKind::Error,
    }
}
fn remote_pika_command(arguments: &[String]) -> Result<String, FleetError> {
    for argument in arguments {
        if argument.contains(['\r', '\n', '\0']) || argument.len() > 1024 {
            return Err(FleetError::new(
                FleetErrorKind::InvalidRequest,
                "Unsafe remote command argument",
            ));
        }
    }
    let quoted = arguments
        .iter()
        .map(|word| shell_quote(word))
        .collect::<Vec<_>>()
        .join(" ");
    Ok(format!(
        "if command -v pika >/dev/null 2>&1; then exec pika {quoted}; else exec \"$HOME/.local/bin/pika\" {quoted}; fi"
    ))
}
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

#[derive(Debug)]
struct BoundedOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

fn run_bounded_command(
    command: &mut Command,
    input: Option<&[u8]>,
    timeout: Duration,
    stdout_limit: usize,
    stderr_limit: usize,
) -> Result<BoundedOutput, FleetError> {
    run_bounded_command_with_identity(command, input, timeout, stdout_limit, stderr_limit, None)
}

fn run_bounded_command_cancellable(
    command: &mut Command,
    input: Option<&[u8]>,
    timeout: Duration,
    stdout_limit: usize,
    stderr_limit: usize,
    cancellation: &CancellationToken,
) -> Result<BoundedOutput, FleetError> {
    run_bounded_command_inner(
        command,
        input,
        timeout,
        stdout_limit,
        stderr_limit,
        None,
        cancellation,
    )
}

fn run_bounded_command_with_identity(
    command: &mut Command,
    input: Option<&[u8]>,
    timeout: Duration,
    stdout_limit: usize,
    stderr_limit: usize,
    identity_guard: Option<(&str, Duration)>,
) -> Result<BoundedOutput, FleetError> {
    run_bounded_command_inner(
        command,
        input,
        timeout,
        stdout_limit,
        stderr_limit,
        identity_guard,
        &CancellationToken::default(),
    )
}

fn run_bounded_command_inner(
    command: &mut Command,
    input: Option<&[u8]>,
    timeout: Duration,
    stdout_limit: usize,
    stderr_limit: usize,
    identity_guard: Option<(&str, Duration)>,
    cancellation: &CancellationToken,
) -> Result<BoundedOutput, FleetError> {
    if input.is_some_and(|bytes| bytes.len() > MAX_REMOTE_INSTALL_BYTES) {
        return Err(FleetError::new(
            FleetErrorKind::InvalidRequest,
            "Command input exceeded the safety limit",
        ));
    }
    if cancellation.is_cancelled() {
        return Err(FleetError::new(
            FleetErrorKind::Unreachable,
            "Fleet request cancelled before process start",
        ));
    }
    let deadline = Instant::now() + timeout;
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = OwnedChild::spawn(command)
        .map_err(|error| FleetError::new(FleetErrorKind::Unreachable, error.to_string()))?;
    let stop = CancellationToken::default();
    let pipes = (|| -> std::io::Result<_> {
        Ok((
            CancellablePipe::new(
                child.stdout.take().expect("stdout configured"),
                stop.clone(),
            )?,
            CancellablePipe::new(
                child.stderr.take().expect("stderr configured"),
                stop.clone(),
            )?,
            CancellablePipe::new(child.stdin.take().expect("stdin configured"), stop.clone())?,
        ))
    })();
    let (stdout, stderr, mut stdin) = match pipes {
        Ok(pipes) => pipes,
        Err(error) => {
            kill_reap(&mut child);
            return Err(FleetError::new(
                FleetErrorKind::Unreachable,
                error.to_string(),
            ));
        }
    };
    let overflow = Arc::new(AtomicBool::new(false));
    let (stdout_thread, identity_result) = if let Some((expected, guard_timeout)) = identity_guard {
        let expected = expected.to_owned();
        let (sender, receiver) = mpsc::sync_channel(1);
        let overflow = overflow.clone();
        let reader = thread::spawn(move || {
            let mut stdout = BufReader::new(stdout);
            let result = read_install_identity(&mut stdout, &expected);
            let accepted = result.is_ok();
            let _ = sender.send(result);
            if accepted {
                read_bounded_output(stdout, stdout_limit, overflow)
            } else {
                Vec::new()
            }
        });
        (reader, Some((receiver, Instant::now() + guard_timeout)))
    } else {
        (
            spawn_bounded_reader(stdout, stdout_limit, overflow.clone()),
            None,
        )
    };
    let stderr_thread = spawn_bounded_reader(stderr, stderr_limit, overflow.clone());
    let (write_sender, write_result) = mpsc::sync_channel(1);
    let input = input.map(<[u8]>::to_vec);
    let writer_stop = stop.clone();
    let writer_cancellation = cancellation.clone();
    let writer_thread = thread::spawn(move || {
        let result = (|| -> Result<(), FleetError> {
            if let Some((identity_result, deadline)) = identity_result {
                loop {
                    if writer_stop.is_cancelled()
                        || writer_cancellation.is_cancelled()
                        || Instant::now() >= deadline
                    {
                        return Err(FleetError::new(
                            FleetErrorKind::Quarantined,
                            "Remote installation identity was not verified before the deadline; nothing installed",
                        ));
                    }
                    match identity_result.recv_timeout(Duration::from_millis(10)) {
                        Ok(result) => {
                            result?;
                            break;
                        }
                        Err(RecvTimeoutError::Timeout) => continue,
                        Err(RecvTimeoutError::Disconnected) => {
                            return Err(FleetError::new(
                                FleetErrorKind::Quarantined,
                                "Remote installation identity reader stopped; nothing installed",
                            ));
                        }
                    }
                }
                stdin
                    .write_all(b"PIKA-INSTALL-VERIFIED\n")
                    .map_err(|error| {
                        FleetError::new(FleetErrorKind::Unreachable, error.to_string())
                    })?;
            }
            input
                .as_deref()
                .map_or(Ok(()), |bytes| stdin.write_all(bytes))
                .and_then(|_| stdin.flush())
                .map_err(|error| {
                    FleetError::new(
                        FleetErrorKind::Unreachable,
                        format!("SSH input write failed: {error}"),
                    )
                })
        })();
        // Dropping stdin is part of the protocol: remote readers waiting for
        // EOF must be able to proceed before Pika waits for process exit.
        drop(stdin);
        let _ = write_sender.send(result);
    });
    let mut write_complete = false;
    let outcome = loop {
        if cancellation.is_cancelled() {
            break Err(FleetError::new(
                FleetErrorKind::Unreachable,
                "Fleet request cancelled",
            ));
        }
        if !write_complete {
            match write_result.try_recv() {
                Ok(Ok(())) => write_complete = true,
                Ok(Err(error)) => {
                    break Err(error);
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    break Err(FleetError::new(
                        FleetErrorKind::Unreachable,
                        "SSH input writer stopped",
                    ));
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if overflow.load(Ordering::Relaxed) {
            break Err(FleetError::new(
                FleetErrorKind::Incompatible,
                "Remote Pika response exceeded the safety limit",
            ));
        }
        match owned_child_exited(&mut child) {
            Ok(true)
                if write_complete && stdout_thread.is_finished() && stderr_thread.is_finished() =>
            {
                break Ok(());
            }
            Ok(_) => {}
            Err(error) => {
                break Err(FleetError::new(
                    FleetErrorKind::Unreachable,
                    error.to_string(),
                ));
            }
        }
        if Instant::now() >= deadline {
            break Err(FleetError::new(
                FleetErrorKind::Unreachable,
                format!("SSH timed out after {}s", timeout.as_secs_f64()),
            ));
        }
        thread::sleep(Duration::from_millis(2));
    };
    let cleanup = terminate_child(&mut child);
    stop.cancel();
    let _ = writer_thread.join();
    let stdout = stdout_thread
        .join()
        .map_err(|_| FleetError::new(FleetErrorKind::Error, "stdout reader failed"))?;
    let stderr = stderr_thread
        .join()
        .map_err(|_| FleetError::new(FleetErrorKind::Error, "stderr reader failed"))?;
    outcome?;
    cleanup.map_err(|error| FleetError::new(FleetErrorKind::Unreachable, error.to_string()))?;
    let status = child.exit_status();
    if overflow.load(Ordering::Relaxed) {
        return Err(FleetError::new(
            FleetErrorKind::Incompatible,
            "Remote Pika response exceeded the safety limit",
        ));
    }
    Ok(BoundedOutput {
        status,
        stdout,
        stderr,
    })
}

fn kill_reap(child: &mut OwnedChild) {
    let _ = terminate_child(child);
}

fn spawn_bounded_reader<R: Read + Send + 'static>(
    reader: R,
    limit: usize,
    overflow: Arc<AtomicBool>,
) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || read_bounded_output(reader, limit, overflow))
}

fn read_install_identity<R: BufRead>(reader: &mut R, expected: &str) -> Result<(), FleetError> {
    let mut line = Vec::new();
    let state = read_bounded_line(reader, &mut line, MAX_STDERR_BYTES).map_err(|error| {
        FleetError::new(
            FleetErrorKind::Quarantined,
            format!("Remote installation identity could not be read; nothing installed: {error}"),
        )
    })?;
    if state != (BoundedLine::Complete { overflow: false }) {
        return Err(FleetError::new(
            FleetErrorKind::Incompatible,
            "Remote installation identity response was incomplete or oversized; nothing installed",
        ));
    }
    let value: Value = serde_json::from_slice(&line).map_err(|_| {
        FleetError::new(
            FleetErrorKind::Incompatible,
            "Remote installation identity response was not JSON; nothing installed",
        )
    })?;
    if value.get("type").and_then(Value::as_str) == Some("error") {
        return Err(FleetError::new(
            parse_error_kind(value.get("kind").and_then(Value::as_str)),
            "Remote rejected the installation identity check; nothing installed",
        ));
    }
    validate_hello(&value, None, Some(expected))?;
    Ok(())
}

fn read_bounded_output<R: Read>(mut reader: R, limit: usize, overflow: Arc<AtomicBool>) -> Vec<u8> {
    let mut result = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) | Err(_) => return result,
            Ok(count) if result.len().saturating_add(count) > limit => {
                overflow.store(true, Ordering::Relaxed);
                return result;
            }
            Ok(count) => result.extend_from_slice(&chunk[..count]),
        }
    }
}
fn spawn_jsonl_reader<R: Read + Send + 'static>(
    reader: R,
    sender: SyncSender<Result<Value, FleetError>>,
    max_frame: usize,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut reader = BufReader::new(reader);
        loop {
            let mut bytes = Vec::new();
            match read_bounded_line(&mut reader, &mut bytes, max_frame) {
                Ok(BoundedLine::Eof) => return,
                Ok(BoundedLine::Partial { .. }) => {
                    let _ = sender.send(Err(FleetError::new(
                        FleetErrorKind::Incompatible,
                        "Remote side channel ended with a partial JSONL frame",
                    )));
                    return;
                }
                Ok(BoundedLine::Complete { overflow: true }) => {
                    let _ = sender.send(Err(FleetError::new(
                        FleetErrorKind::Incompatible,
                        "Remote side channel frame exceeded the safety limit",
                    )));
                    return;
                }
                Ok(BoundedLine::Complete { overflow: false }) => {}
                Err(error) => {
                    let _ = sender.send(Err(FleetError::new(
                        FleetErrorKind::Unreachable,
                        error.to_string(),
                    )));
                    return;
                }
            }
            let value = serde_json::from_slice(&bytes).map_err(|_| {
                FleetError::new(
                    FleetErrorKind::Incompatible,
                    "Remote side channel emitted malformed JSONL",
                )
            });
            if sender.send(value).is_err() {
                return;
            }
        }
    })
}
fn spawn_bounded_stderr<R: Read + Send + 'static>(
    reader: R,
    destination: Arc<Mutex<Vec<u8>>>,
    max: usize,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut reader = reader;
        let mut collected = Vec::new();
        let mut chunk = [0_u8; 8192];
        while let Ok(count) = reader.read(&mut chunk) {
            if count == 0 {
                break;
            }
            let remaining = max.saturating_sub(collected.len());
            collected.extend_from_slice(&chunk[..count.min(remaining)]);
        }
        if let Ok(mut target) = destination.lock() {
            *target = collected;
        }
    })
}
fn wait_child(child: &mut OwnedChild, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if poll_owned_child(child).ok().flatten().is_some() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(2));
    }
}
fn validate_progress(object: &Map<String, Value>) -> Result<(), FleetError> {
    if !matches!(
        object.get("stage").and_then(Value::as_str),
        Some("prepare" | "turn" | "response" | "cleanup")
    ) || !matches!(
        object.get("delivery").and_then(Value::as_str),
        Some("not_sent" | "unknown" | "confirmed")
    ) {
        Err(FleetError::new(
            FleetErrorKind::Incompatible,
            "Remote side channel returned invalid progress",
        ))
    } else {
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BoundedLine {
    Eof,
    Complete { overflow: bool },
    Partial { overflow: bool },
}

impl BoundedLine {
    fn overflowed(self) -> bool {
        matches!(
            self,
            Self::Complete { overflow: true } | Self::Partial { overflow: true }
        )
    }
}

fn read_bounded_line<R: BufRead>(
    reader: &mut R,
    output: &mut Vec<u8>,
    max: usize,
) -> std::io::Result<BoundedLine> {
    let mut overflow = false;
    let mut observed = false;
    loop {
        let buffer = reader.fill_buf()?;
        if buffer.is_empty() {
            return Ok(if observed {
                BoundedLine::Partial { overflow }
            } else {
                BoundedLine::Eof
            });
        }
        observed = true;
        let newline = buffer.iter().position(|byte| *byte == b'\n');
        let consumed = newline.map_or(buffer.len(), |index| index + 1);
        let content = newline.map_or(&buffer[..consumed], |index| &buffer[..index]);
        let remaining = max.saturating_sub(output.len());
        output.extend_from_slice(&content[..content.len().min(remaining)]);
        overflow |= content.len() > remaining;
        reader.consume(consumed);
        if newline.is_some() {
            return Ok(BoundedLine::Complete { overflow });
        }
    }
}

fn ssh_config_files(root: &Path) -> Vec<PathBuf> {
    fn visit(path: PathBuf, root: &Path, seen: &mut BTreeSet<PathBuf>, result: &mut Vec<PathBuf>) {
        if result.len() >= 64 {
            return;
        }
        let Ok(canonical) = path.canonicalize() else {
            return;
        };
        if !canonical.is_file() || !seen.insert(canonical.clone()) {
            return;
        }
        result.push(canonical.clone());
        let Ok(contents) = read_bounded(&canonical, 1024 * 1024) else {
            return;
        };
        for raw in contents.lines() {
            let Ok(parts) = shell_words::split(raw) else {
                continue;
            };
            if parts.len() < 2 || !parts[0].eq_ignore_ascii_case("include") {
                continue;
            }
            for pattern in &parts[1..] {
                let expanded = expand_tilde(pattern);
                let candidate = if expanded.is_absolute() {
                    expanded
                } else {
                    root.join(expanded)
                };
                for path in expand_pattern(&candidate) {
                    visit(path, root, seen, result);
                }
            }
        }
    }
    let mut result = Vec::new();
    visit(root.join("config"), root, &mut BTreeSet::new(), &mut result);
    result
}
fn expand_tilde(value: &str) -> PathBuf {
    if value == "~" {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(value))
    } else if let Some(rest) = value.strip_prefix("~/") {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .map_or_else(|| PathBuf::from(value), |home| home.join(rest))
    } else {
        PathBuf::from(value)
    }
}
fn expand_pattern(pattern: &Path) -> Vec<PathBuf> {
    let Some(name) = pattern.file_name().and_then(|value| value.to_str()) else {
        return Vec::new();
    };
    if !name.contains(['*', '?']) {
        return vec![pattern.to_path_buf()];
    }
    let Some(parent) = pattern.parent() else {
        return Vec::new();
    };
    let expression = format!(
        "^{}$",
        regex::escape(name).replace(r"\*", ".*").replace(r"\?", ".")
    );
    let Ok(regex) = regex::Regex::new(&expression) else {
        return Vec::new();
    };
    let Ok(entries) = fs::read_dir(parent) else {
        return Vec::new();
    };
    let mut result: Vec<_> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|value| value.to_str())
                .is_some_and(|value| regex.is_match(value))
        })
        .collect();
    result.sort();
    result
}
fn read_bounded(path: &Path, max: usize) -> std::io::Result<String> {
    let mut file = fs::File::open(path)?;
    let mut bytes = Vec::new();
    std::io::Read::by_ref(&mut file)
        .take(max as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > max {
        return Err(std::io::Error::other("file exceeds safety limit"));
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

pub fn sanitize_terminal_text(value: &str) -> String {
    let mut result = String::with_capacity(value.len());
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            if chars.next_if_eq(&'[').is_some() {
                for next in chars.by_ref() {
                    if ('@'..='~').contains(&next) {
                        break;
                    }
                }
            }
            continue;
        }
        if matches!(ch, '\n' | '\r') {
            result.push(' ')
        } else if ch == '\t' || !ch.is_control() {
            result.push(ch)
        }
    }
    result
}
fn prefix(value: &str, length: usize) -> &str {
    value
        .char_indices()
        .nth(length)
        .map_or(value, |(index, _)| &value[..index])
}
fn prefix_tail(value: &str, max_chars: usize) -> String {
    let chars: Vec<_> = value.chars().collect();
    chars[chars.len().saturating_sub(max_chars)..]
        .iter()
        .collect()
}
fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

#[cfg(all(test, unix))]
mod bounded_command_tests {
    use super::*;

    fn shell(script: &str) -> Command {
        let mut command = Command::new("sh");
        command.args(["-c", script]);
        command
    }

    #[test]
    fn bounded_io_drains_output_before_child_reads_input() {
        let input = vec![b'x'; 512 * 1024];
        let mut command = shell(
            "head -c 262144 /dev/zero | tr '\\000' x; cat >/dev/null; printf '\\nfinished\\n'",
        );
        let output = run_bounded_command(
            &mut command,
            Some(&input),
            Duration::from_secs(2),
            512 * 1024,
            1024,
        )
        .unwrap();
        assert!(output.status.success());
        assert!(output.stdout.ends_with(b"finished\n"));
    }

    #[test]
    fn bounded_io_closes_stdin_to_release_eof_reader() {
        let mut command = shell("cat >/dev/null; printf closed");
        let output = run_bounded_command(
            &mut command,
            Some(b"payload"),
            Duration::from_secs(1),
            64,
            64,
        )
        .unwrap();
        assert_eq!(output.stdout, b"closed");
    }

    #[test]
    fn bounded_io_times_out_when_child_never_reads() {
        let mut command = shell("exec sleep 5");
        let started = Instant::now();
        let error = run_bounded_command(
            &mut command,
            Some(&vec![b'x'; 4 * 1024 * 1024]),
            Duration::from_millis(150),
            64,
            64,
        )
        .unwrap_err();
        assert_eq!(error.kind, FleetErrorKind::Unreachable);
        assert!(error.message.contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn bounded_io_times_out_when_reader_stalls_mid_payload() {
        let mut command = shell("dd bs=1 count=1 >/dev/null 2>/dev/null; exec sleep 5");
        let started = Instant::now();
        let error = run_bounded_command(
            &mut command,
            Some(&vec![b'x'; 4 * 1024 * 1024]),
            Duration::from_millis(150),
            64,
            64,
        )
        .unwrap_err();
        assert_eq!(error.kind, FleetErrorKind::Unreachable);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn bounded_io_deadline_includes_pipes_after_launcher_exit() {
        for redirect in ["1>&2", "2>/dev/null"] {
            let mut command = shell(&format!("sleep 5 {redirect} & exit 0"));
            let started = Instant::now();
            let error = run_bounded_command(&mut command, None, Duration::from_millis(40), 64, 64)
                .unwrap_err();
            assert!(error.message.contains("timed out"), "{error:?}");
            assert!(started.elapsed() < Duration::from_millis(150));
        }
    }

    #[test]
    fn bounded_io_deadline_includes_inherited_stdin_after_launcher_exit() {
        // fd 3 avoids the shell's implicit /dev/null stdin for background jobs.
        let mut command = shell("exec 3<&0; sleep 5 <&3 >/dev/null 2>&1 & exit 0");
        let started = Instant::now();
        let error = run_bounded_command(
            &mut command,
            Some(&vec![b'x'; 4 * 1024 * 1024]),
            Duration::from_millis(40),
            64,
            64,
        )
        .unwrap_err();
        assert!(error.message.contains("timed out"), "{error:?}");
        assert!(started.elapsed() < Duration::from_millis(150));
    }

    #[test]
    fn bounded_io_exited_launcher_stress() {
        for _ in 0..20 {
            let mut command = shell("sleep 5 & exit 0");
            let started = Instant::now();
            let error = run_bounded_command(&mut command, None, Duration::from_millis(10), 64, 64)
                .unwrap_err();
            assert!(error.message.contains("timed out"), "{error:?}");
            assert!(started.elapsed() < Duration::from_millis(150));
        }
    }

    #[test]
    fn bounded_io_retains_output_and_overflow_checks_after_launcher_exit() {
        let mut command = shell("(printf complete; printf diagnostic >&2) & exit 0");
        let output =
            run_bounded_command(&mut command, None, Duration::from_secs(1), 64, 64).unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"complete");
        assert_eq!(output.stderr, b"diagnostic");

        let mut command = shell("(printf oversized; sleep 5) & exit 0");
        let error =
            run_bounded_command(&mut command, None, Duration::from_secs(1), 4, 64).unwrap_err();
        assert_eq!(error.kind, FleetErrorKind::Incompatible);
    }
}
