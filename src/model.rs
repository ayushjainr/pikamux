use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Codex,
    Claude,
    Opencode,
}

impl Provider {
    pub const ALL: [Self; 3] = [Self::Codex, Self::Claude, Self::Opencode];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Opencode => "opencode",
        }
    }
}

impl fmt::Display for Provider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Provider {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.to_ascii_lowercase().as_str() {
            "codex" => Ok(Self::Codex),
            "claude" => Ok(Self::Claude),
            "opencode" | "oc" => Ok(Self::Opencode),
            _ => Err(format!("unknown provider: {value}")),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Hash, Serialize, Deserialize)]
pub enum Status {
    #[serde(rename = "STARTING")]
    Starting,
    #[serde(rename = "NEEDS YOU")]
    NeedsYou,
    #[serde(rename = "OPEN TWICE")]
    OpenTwice,
    #[serde(rename = "WORKING")]
    Working,
    #[serde(rename = "READY")]
    Ready,
    #[serde(rename = "PARKED")]
    Parked,
    #[serde(rename = "UNBOUND")]
    Unbound,
    #[serde(rename = "ERROR")]
    Error,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "STARTING",
            Self::NeedsYou => "NEEDS YOU",
            Self::OpenTwice => "OPEN TWICE",
            Self::Working => "WORKING",
            Self::Ready => "READY",
            Self::Parked => "PARKED",
            Self::Unbound => "UNBOUND",
            Self::Error => "ERROR",
        }
    }

    pub fn attention_order(self) -> u8 {
        match self {
            Self::NeedsYou => 0,
            Self::OpenTwice => 1,
            Self::Error => 2,
            Self::Ready => 3,
            Self::Working => 4,
            Self::Starting => 5,
            Self::Parked => 6,
            Self::Unbound => 7,
        }
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Status {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "STARTING" => Ok(Self::Starting),
            "NEEDS YOU" => Ok(Self::NeedsYou),
            "OPEN TWICE" => Ok(Self::OpenTwice),
            "WORKING" => Ok(Self::Working),
            "READY" => Ok(Self::Ready),
            "PARKED" => Ok(Self::Parked),
            "UNBOUND" => Ok(Self::Unbound),
            "ERROR" => Ok(Self::Error),
            _ => Err(format!("unknown status: {value}")),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Session {
    pub provider: Provider,
    pub session_id: String,
    pub name: Option<String>,
    pub cwd: Option<String>,
    pub branch: Option<String>,
    pub transcript_path: Option<String>,
    pub tmux_session: Option<String>,
    pub tmux_pane: Option<String>,
    pub root_pid: Option<i64>,
    pub status: Status,
    pub unread: bool,
    pub model: Option<String>,
    pub source: String,
    pub managed: bool,
    pub error: Option<String>,
    pub attention_reason: Option<String>,
    pub created_at: f64,
    pub updated_at: f64,
    pub last_event_at: f64,
    pub last_activity_at: f64,
    pub live: bool,
    pub attached: bool,
    #[serde(skip_serializing)]
    pub home_state: String,
    pub cpu_percent: Option<f64>,
    pub rss_kb: Option<i64>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub cached_input_tokens: Option<i64>,
    pub cache_write_tokens: Option<i64>,
    pub total_tokens: Option<i64>,
    pub estimated_cost_usd: Option<f64>,
    pub active_thread_id: Option<String>,
}

impl Session {
    pub fn display_name(&self) -> String {
        self.name
            .clone()
            .unwrap_or_else(|| format!("{}-{}", self.provider, prefix(&self.session_id, 8)))
    }

    pub fn provider_thread_id(&self) -> &str {
        self.active_thread_id.as_deref().unwrap_or(&self.session_id)
    }

    pub fn needs_attention(&self) -> bool {
        self.status == Status::NeedsYou
            || (self.unread && matches!(self.status, Status::Error | Status::OpenTwice))
    }

    /// Native reconciliation uses `exact`; Python fleet peers expose the same
    /// fact as `exact-live`. Keep the compatibility spelling at the wire edge
    /// instead of scattering string comparisons through identity code.
    pub fn has_exact_home(&self) -> bool {
        matches!(self.home_state.as_str(), "exact" | "exact-live")
    }
}

fn prefix(value: &str, chars: usize) -> &str {
    value
        .char_indices()
        .nth(chars)
        .map_or(value, |(index, _)| &value[..index])
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct StatusObservation {
    pub kind: ObservationKind,
    pub status: Status,
    pub unread: bool,
    pub attention_reason: Option<String>,
    pub error: Option<String>,
    pub observed_at: f64,
    pub source: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ObservationKind {
    Lifecycle,
    Runtime,
    Safety,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ExpertProfile {
    pub provider: Provider,
    pub session_id: String,
    pub summary: String,
    pub current_state: String,
    pub topics: Vec<String>,
    pub artifacts: Vec<String>,
    pub source: String,
    pub updated_at: f64,
    pub scope_updated_at: f64,
    pub current_state_updated_at: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct FleetNode {
    pub node_id: String,
    pub alias: String,
    pub ssh_target: String,
    pub sources: Vec<String>,
    pub status: String,
    pub protocol_version: Option<i64>,
    pub package_version: Option<String>,
    pub capabilities: Vec<String>,
    pub last_seen: f64,
    pub last_attempt_at: f64,
    pub last_error: Option<String>,
    pub created_at: f64,
    pub updated_at: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Candidate {
    pub provider: Provider,
    pub session_id: String,
    pub name: Option<String>,
    pub cwd: Option<String>,
    pub branch: Option<String>,
    pub transcript_path: Option<String>,
    pub model: Option<String>,
    pub updated_at: f64,
    pub live: bool,
    pub pid: Option<i64>,
    pub source: String,
    pub parent_session_id: Option<String>,
    pub created_at: f64,
    pub lifecycle_status: Option<Status>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Pane {
    pub session_name: String,
    pub pane_id: String,
    pub pane_pid: i64,
    pub cwd: String,
    pub current_command: String,
    pub attached: bool,
    pub dead: bool,
    pub dead_status: Option<i32>,
    pub activity: f64,
    pub created: f64,
    pub pika_provider: Option<Provider>,
    pub pika_session_id: Option<String>,
    pub pika_name: Option<String>,
    pub pika_launch_token: Option<String>,
}
