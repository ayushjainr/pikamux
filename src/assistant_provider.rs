//! A narrow, transport-injected Codex app-server adapter for the main assistant.
//!
//! This module owns no socket, process, filesystem, MCP, plugin, or shell
//! access.  A caller supplies an RPC transport (the production process/socket
//! adapter is a separate concern); tests use a deterministic fake.  That
//! boundary is intentional: this state machine cannot accidentally discover
//! credentials or ambient tools.

use serde_json::{Value, json};
use std::fmt;

pub const DEFAULT_MODEL: &str = "gpt-6-luna";
pub const DEFAULT_EFFORT: &str = "medium";
/// The provider-owned profile installed by the isolated transport.  Keeping
/// this identifier in one place prevents a caller from silently selecting a
/// broader ambient profile.
pub const ASSISTANT_PERMISSION_PROFILE: &str = "pika-assistant";
pub const MAX_PROMPT_BYTES: usize = 64 * 1024;
pub const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderConfig {
    pub model: String,
    pub effort: String,
    pub max_prompt_bytes: usize,
    pub max_response_bytes: usize,
    pub approval_policy: &'static str,
    pub sandbox_policy: SandboxPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxPolicy {
    pub read_only: bool,
    pub network_access: bool,
    pub readable_roots: Vec<String>,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            model: DEFAULT_MODEL.into(),
            effort: DEFAULT_EFFORT.into(),
            max_prompt_bytes: MAX_PROMPT_BYTES,
            max_response_bytes: MAX_RESPONSE_BYTES,
            approval_policy: "never",
            sandbox_policy: SandboxPolicy {
                read_only: true,
                network_access: false,
                readable_roots: Vec::new(),
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MainProfile {
    pub profile_id: String,
    /// Persist this exact provider id in the profile journal.  A missing id
    /// means a new session; it is never guessed from a title or cwd.
    pub thread_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderError {
    Transport(String),
    Protocol(String),
    NotReady,
    AlreadyInFlight,
    PromptTooLarge,
    ResponseTooLarge,
    UnknownDelivery,
    Cancelled,
    ToolRequestBlocked,
    InProgress,
}

impl fmt::Display for ProviderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for ProviderError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerEvent {
    ContextCompaction {
        thread_id: Option<String>,
        turn_id: String,
        item_id: String,
        completed: bool,
    },
    AgentDelta {
        thread_id: Option<String>,
        turn_id: String,
        text: String,
    },
    Completed {
        thread_id: Option<String>,
        turn_id: String,
        usage: Option<Usage>,
    },
    Failed {
        thread_id: Option<String>,
        turn_id: String,
        message: String,
    },
    ToolRequest {
        thread_id: Option<String>,
        turn_id: String,
    },
    Other {
        thread_id: Option<String>,
        turn_id: Option<String>,
    },
}

impl ServerEvent {
    /// Parse only the bounded event surface needed by the main assistant.
    /// Unknown notifications remain inert and never become instructions.
    pub fn from_json(value: &Value) -> Result<Self, ProviderError> {
        let method = value
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let params = value.get("params").unwrap_or(&Value::Null);
        let turn_id = params
            .get("turnId")
            .and_then(Value::as_str)
            .or_else(|| {
                params
                    .get("turn")
                    .and_then(|v| v.get("id"))
                    .and_then(Value::as_str)
            })
            .map(str::to_owned);
        let thread_id = params
            .get("threadId")
            .and_then(Value::as_str)
            .map(str::to_owned);
        match method {
            "item/started" | "item/completed"
                if params
                    .get("item")
                    .and_then(|i| i.get("type"))
                    .and_then(Value::as_str)
                    == Some("contextCompaction") =>
            {
                parse_compaction(params, thread_id, turn_id, method == "item/completed")
            }
            "item/agentMessage/delta" => Ok(Self::AgentDelta {
                thread_id,
                turn_id: turn_id
                    .ok_or_else(|| ProviderError::Protocol("delta missing turnId".into()))?,
                text: params
                    .get("delta")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            }),
            "turn/completed" => parse_completed_event(params, thread_id, turn_id),
            "error" | "turn/error" => Ok(Self::Failed {
                thread_id,
                turn_id: turn_id
                    .ok_or_else(|| ProviderError::Protocol("error missing turnId".into()))?,
                message: params
                    .get("message")
                    .or_else(|| params.get("error").and_then(|v| v.get("message")))
                    .and_then(Value::as_str)
                    .unwrap_or("provider error")
                    .to_owned(),
            }),
            method
                if method.contains("requestApproval")
                    || method.contains("/tool/")
                    || method.starts_with("mcpServer/")
                    || method.starts_with("serverRequest/") =>
            {
                Ok(Self::ToolRequest {
                    thread_id,
                    turn_id: turn_id.unwrap_or_default(),
                })
            }
            _ => Ok(Self::Other { thread_id, turn_id }),
        }
    }
}

fn parse_compaction(
    params: &Value,
    thread_id: Option<String>,
    turn_id: Option<String>,
    completed: bool,
) -> Result<ServerEvent, ProviderError> {
    let item_id = params
        .get("item")
        .and_then(|i| i.get("id"))
        .and_then(Value::as_str)
        .filter(|id| valid_compaction_id(id))
        .ok_or_else(|| ProviderError::Protocol("compaction missing valid item id".into()))?;
    let turn_id = turn_id
        .filter(|id| valid_compaction_id(id))
        .ok_or_else(|| ProviderError::Protocol("compaction missing valid turn id".into()))?;
    if thread_id
        .as_deref()
        .is_none_or(|id| !valid_compaction_id(id))
    {
        return Err(ProviderError::Protocol(
            "compaction missing valid thread id".into(),
        ));
    }
    Ok(ServerEvent::ContextCompaction {
        thread_id,
        turn_id,
        item_id: item_id.into(),
        completed,
    })
}

fn valid_compaction_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 256 && !id.chars().any(char::is_control)
}

/// Only exact completed item lifecycles become durable checkpoint signals.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactionEvent {
    pub thread_id: String,
    pub turn_id: String,
    pub item_id: String,
}

fn parse_completed_event(
    params: &Value,
    thread_id: Option<String>,
    turn_id: Option<String>,
) -> Result<ServerEvent, ProviderError> {
    let id = turn_id.ok_or_else(|| ProviderError::Protocol("completion missing turnId".into()))?;
    let status = params
        .get("turn")
        .and_then(|v| v.get("status"))
        .and_then(Value::as_str)
        .ok_or_else(|| ProviderError::Protocol("completion missing status".into()))?;
    if status != "completed" {
        return Ok(ServerEvent::Failed {
            thread_id,
            turn_id: id,
            message: status.to_owned(),
        });
    }
    let usage = params
        .get("turn")
        .and_then(|v| v.get("usage"))
        .and_then(|v| {
            Some(Usage {
                input_tokens: v.get("inputTokens")?.as_u64()?,
                output_tokens: v.get("outputTokens")?.as_u64()?,
            })
        });
    Ok(ServerEvent::Completed {
        thread_id,
        turn_id: id,
        usage,
    })
}

/// Implement this over an already-approved app-server process/socket.  The
/// provider module itself performs no I/O and cannot create ambient access.
pub trait RpcTransport {
    fn request(&mut self, method: &str, params: Value) -> Result<Value, ProviderError>;
    fn notify(&mut self, method: &str, params: Value) -> Result<(), ProviderError>;
    fn notifications(&mut self) -> Result<Vec<ServerEvent>, ProviderError>;
    fn interrupt(&mut self, thread_id: &str, turn_id: &str) -> Result<(), ProviderError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssistantState {
    New,
    Ready,
    InFlight { turn_id: String },
    UnknownDelivery { turn_id: String },
    Closed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnResult {
    Complete {
        turn_id: String,
        text: String,
        usage: Option<Usage>,
    },
    Failed {
        turn_id: String,
        text: String,
    },
}

pub struct MainAssistant<T: RpcTransport> {
    transport: T,
    profile: MainProfile,
    config: ProviderConfig,
    state: AssistantState,
    response: String,
    compaction_started: std::collections::BTreeSet<String>,
    compaction_events: Vec<CompactionEvent>,
}

impl<T: RpcTransport> MainAssistant<T> {
    pub fn new(transport: T, profile: MainProfile) -> Self {
        Self {
            transport,
            profile,
            config: ProviderConfig::default(),
            state: AssistantState::New,
            response: String::new(),
            compaction_started: Default::default(),
            compaction_events: Vec::new(),
        }
    }
    pub fn with_config(
        transport: T,
        profile: MainProfile,
        config: ProviderConfig,
    ) -> Result<Self, ProviderError> {
        if config.max_prompt_bytes == 0
            || config.max_response_bytes == 0
            || config.max_prompt_bytes > MAX_PROMPT_BYTES
            || config.max_response_bytes > MAX_RESPONSE_BYTES
            || config.approval_policy != "never"
            || !config.sandbox_policy.read_only
            || config.sandbox_policy.network_access
            || !config.sandbox_policy.readable_roots.is_empty()
        {
            return Err(ProviderError::Protocol(
                "unsafe assistant provider configuration".into(),
            ));
        }
        Ok(Self {
            transport,
            profile,
            config,
            state: AssistantState::New,
            response: String::new(),
            compaction_started: Default::default(),
            compaction_events: Vec::new(),
        })
    }
    pub fn state(&self) -> &AssistantState {
        &self.state
    }
    pub fn profile(&self) -> &MainProfile {
        &self.profile
    }
    pub fn partial_output(&self) -> &str {
        &self.response
    }
    pub fn transport(&self) -> &T {
        &self.transport
    }

    pub fn take_compaction_events(&mut self) -> Vec<CompactionEvent> {
        std::mem::take(&mut self.compaction_events)
    }

    pub fn start_or_resume(&mut self) -> Result<(), ProviderError> {
        if !matches!(self.state, AssistantState::New) {
            return Err(ProviderError::NotReady);
        }
        self.transport.request("initialize", json!({ "clientInfo": { "name": "pika-main-assistant", "title": "Pika main assistant", "version": env!("CARGO_PKG_VERSION") }, "capabilities": { "experimentalApi": true } }))?;
        self.transport.notify("initialized", json!({}))?;
        let (method, params) = match self.profile.thread_id.as_deref() {
            Some(id) if !id.is_empty() => (
                "thread/resume",
                json!({ "threadId": id, "model": self.config.model, "approvalPolicy": self.config.approval_policy, "permissions": ASSISTANT_PERMISSION_PROFILE }),
            ),
            _ => (
                "thread/start",
                json!({ "model": self.config.model, "approvalPolicy": self.config.approval_policy, "permissions": ASSISTANT_PERMISSION_PROFILE }),
            ),
        };
        let response = self.transport.request(method, params)?;
        let id = verified_thread_id(&response, &self.config)?;
        if let Some(expected) = self.profile.thread_id.as_deref() {
            if expected != id {
                return Err(ProviderError::Protocol(
                    "provider returned a different thread id".into(),
                ));
            }
        }
        self.profile.thread_id = Some(id);
        self.state = AssistantState::Ready;
        Ok(())
    }

    /// Dispatch exactly one turn. Completion is collected later by `poll_turn`.
    pub fn begin_turn(&mut self, prompt: &str) -> Result<String, ProviderError> {
        if !matches!(self.state, AssistantState::Ready) {
            return Err(if matches!(self.state, AssistantState::InFlight { .. }) {
                ProviderError::AlreadyInFlight
            } else {
                ProviderError::NotReady
            });
        }
        if prompt.len() > self.config.max_prompt_bytes {
            return Err(ProviderError::PromptTooLarge);
        }
        let thread_id = self
            .profile
            .thread_id
            .clone()
            .ok_or(ProviderError::NotReady)?;
        let response = match self.transport.request("turn/start", json!({ "threadId": thread_id, "input": [{ "type": "text", "text": prompt }], "model": self.config.model, "effort": self.config.effort, "approvalPolicy": self.config.approval_policy, "permissions": ASSISTANT_PERMISSION_PROFILE })) {
            Ok(response) => response,
            Err(error) => {
                // The request may have reached the provider even when its
                // response was a protocol rejection.  Preserve the bounded
                // diagnostic for operators, but keep the durable state
                // unknown so callers cannot auto-retry the prompt.
                self.state = AssistantState::UnknownDelivery { turn_id: "unknown".into() };
                return Err(error);
            }
        };
        let turn_id = response
            .get("turn")
            .or_else(|| response.get("result").and_then(|v| v.get("turn")))
            .and_then(|v| v.get("id"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        let turn_id = match turn_id {
            Some(id) if !id.is_empty() => id,
            _ => {
                self.state = AssistantState::UnknownDelivery {
                    turn_id: "unknown".into(),
                };
                return Err(ProviderError::UnknownDelivery);
            }
        };
        self.state = AssistantState::InFlight {
            turn_id: turn_id.clone(),
        };
        self.response.clear();
        self.compaction_started.clear();
        Ok(turn_id)
    }

    /// Consume one currently available notification batch. No completion in
    /// this batch leaves the turn in-flight, allowing cancellation or polling.
    pub fn poll_turn(&mut self) -> Result<Option<TurnResult>, ProviderError> {
        let turn_id = match &self.state {
            AssistantState::InFlight { turn_id } => turn_id.clone(),
            _ => return Err(ProviderError::NotReady),
        };
        let thread_id = self
            .profile
            .thread_id
            .clone()
            .ok_or(ProviderError::NotReady)?;
        let events = match self.transport.notifications() {
            Ok(events) => events,
            Err(error) => {
                self.state = AssistantState::UnknownDelivery {
                    turn_id: turn_id.clone(),
                };
                return Err(error);
            }
        };
        for event in events {
            let (event_thread, event_turn) = match &event {
                ServerEvent::AgentDelta {
                    thread_id, turn_id, ..
                }
                | ServerEvent::Completed {
                    thread_id, turn_id, ..
                }
                | ServerEvent::Failed {
                    thread_id, turn_id, ..
                }
                | ServerEvent::ToolRequest { thread_id, turn_id } => {
                    (thread_id.as_deref(), turn_id.as_str())
                }
                ServerEvent::ContextCompaction {
                    thread_id, turn_id, ..
                } => (thread_id.as_deref(), turn_id.as_str()),
                ServerEvent::Other { thread_id, turn_id } => {
                    (thread_id.as_deref(), turn_id.as_deref().unwrap_or(""))
                }
            };
            if event_turn != turn_id || event_thread != Some(thread_id.as_str()) {
                continue;
            }
            match event {
                ServerEvent::ContextCompaction {
                    item_id, completed, ..
                } => {
                    if !valid_compaction_id(&item_id) {
                        continue;
                    }
                    if completed {
                        if self.compaction_started.remove(&item_id)
                            && self.compaction_events.len() < 64
                        {
                            self.compaction_events.push(CompactionEvent {
                                thread_id: thread_id.clone(),
                                turn_id: turn_id.clone(),
                                item_id,
                            });
                        }
                    } else if self.compaction_started.len() < 64 {
                        self.compaction_started.insert(item_id);
                    }
                }
                ServerEvent::AgentDelta { text: delta, .. } => {
                    if self.response.len().saturating_add(delta.len())
                        > self.config.max_response_bytes
                    {
                        self.state = AssistantState::UnknownDelivery {
                            turn_id: turn_id.clone(),
                        };
                        return Err(ProviderError::ResponseTooLarge);
                    }
                    self.response.push_str(&delta);
                }
                ServerEvent::Completed { usage, .. } => {
                    self.state = AssistantState::Ready;
                    return Ok(Some(TurnResult::Complete {
                        turn_id: turn_id.clone(),
                        text: std::mem::take(&mut self.response),
                        usage,
                    }));
                }
                ServerEvent::Failed { message, .. } => {
                    self.state = AssistantState::Ready;
                    return Ok(Some(TurnResult::Failed {
                        turn_id: turn_id.clone(),
                        text: message,
                    }));
                }
                ServerEvent::ToolRequest { .. } => {
                    self.state = AssistantState::UnknownDelivery {
                        turn_id: turn_id.clone(),
                    };
                    return Err(ProviderError::ToolRequestBlocked);
                }
                ServerEvent::Other { .. } => {}
            }
        }
        Ok(None)
    }

    /// Compatibility helper for callers that explicitly accept a single poll.
    pub fn send(&mut self, prompt: &str) -> Result<TurnResult, ProviderError> {
        self.begin_turn(prompt)?;
        self.poll_turn()?.ok_or(ProviderError::InProgress)
    }

    pub fn cancel(&mut self) -> Result<(), ProviderError> {
        let (thread, turn) = match &self.state {
            AssistantState::InFlight { turn_id } => (
                self.profile
                    .thread_id
                    .clone()
                    .ok_or(ProviderError::NotReady)?,
                turn_id.clone(),
            ),
            _ => return Err(ProviderError::NotReady),
        };
        if let Err(error) = self.transport.interrupt(&thread, &turn) {
            self.state = AssistantState::UnknownDelivery { turn_id: turn };
            return Err(error);
        }
        // An interrupt RPC receipt means accepted, not turn-completed. Without
        // a final exact-turn event, a new turn could overlap the old one.
        self.state = AssistantState::UnknownDelivery { turn_id: turn };
        Err(ProviderError::UnknownDelivery)
    }
}

fn verified_thread_id(response: &Value, config: &ProviderConfig) -> Result<String, ProviderError> {
    let thread = response
        .get("thread")
        .or_else(|| response.get("result").and_then(|v| v.get("thread")))
        .ok_or_else(|| ProviderError::Protocol("thread response missing thread".into()))?;
    verify_active_permission_profile(response)?;
    verify_thread_sandbox(response)?;
    verify_thread_settings(response, config)?;
    Ok(thread
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| ProviderError::Protocol("thread response missing exact id".into()))?
        .to_owned())
}

fn verify_active_permission_profile(response: &Value) -> Result<(), ProviderError> {
    let profile = response
        .get("activePermissionProfile")
        .or_else(|| {
            response
                .get("result")
                .and_then(|v| v.get("activePermissionProfile"))
        })
        .ok_or_else(|| {
            ProviderError::Protocol("thread response omitted active permission profile".into())
        })?;
    if profile.get("id").and_then(Value::as_str) != Some(ASSISTANT_PERMISSION_PROFILE)
        || profile.get("extends") != Some(&Value::Null)
    {
        return Err(ProviderError::Protocol(
            "provider returned an unexpected assistant permission profile".into(),
        ));
    }
    Ok(())
}

fn verify_thread_sandbox(response: &Value) -> Result<(), ProviderError> {
    let sandbox = response
        .get("sandbox")
        .or_else(|| response.get("result").and_then(|v| v.get("sandbox")))
        .ok_or_else(|| {
            ProviderError::Protocol("thread response omitted effective sandbox".into())
        })?;
    if sandbox.as_object().is_none_or(|object| object.len() != 2)
        || sandbox.get("type").and_then(Value::as_str) != Some("readOnly")
        || sandbox.get("networkAccess") != Some(&Value::Bool(false))
    {
        return Err(ProviderError::Protocol(
            "provider returned an unsafe effective sandbox".into(),
        ));
    }
    Ok(())
}

fn verify_thread_settings(response: &Value, config: &ProviderConfig) -> Result<(), ProviderError> {
    let effective = response.get("result").unwrap_or(response);
    if effective.get("approvalPolicy").and_then(Value::as_str) != Some(config.approval_policy)
        || effective.get("model").and_then(Value::as_str) != Some(config.model.as_str())
    {
        return Err(ProviderError::Protocol(
            "provider returned unexpected assistant thread settings".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn compaction_requires_exact_started_item_and_bounds_collection() {
        let event = |method: &str, thread: &str, item: &str| {
            ServerEvent::from_json(&json!({"method":method,"params": {
                "threadId":thread,"turnId":"turn-1", "item":{"type":"contextCompaction","id":item}
            }}))
            .unwrap()
        };
        let mut events = vec![
            event("item/completed", "thr-new", "unstarted"),
            event("item/started", "wrong-thread", "wrong"),
            event("item/completed", "thr-new", "wrong"),
            event("item/started", "thr-new", "exact"),
            event("item/completed", "thr-new", "different"),
            event("item/completed", "thr-new", "exact"),
            event("item/completed", "thr-new", "exact"),
        ];
        for i in 0..100 {
            events.push(event("item/started", "thr-new", &format!("item-{i}")));
            events.push(event("item/completed", "thr-new", &format!("item-{i}")));
        }
        let mut assistant = MainAssistant::new(
            Fake {
                events,
                ..Default::default()
            },
            MainProfile {
                profile_id: "fixture".into(),
                thread_id: None,
            },
        );
        assistant.start_or_resume().unwrap();
        assistant.begin_turn("fixture").unwrap();
        assert!(assistant.poll_turn().unwrap().is_none());
        let checkpoints = assistant.take_compaction_events();
        assert_eq!(checkpoints.len(), 64);
        assert_eq!(checkpoints[0].item_id, "exact");
        assert!(assistant.take_compaction_events().is_empty());
        assert!(
            ServerEvent::from_json(&json!({"method":"item/completed","params":{
                "threadId":"thr-new", "turnId":"turn-1", "item":{"type":"contextCompaction","id":""}
            }}))
            .is_err()
        );
        assert!(matches!(
            ServerEvent::from_json(&json!({"method":"thread/compacted"})).unwrap(),
            ServerEvent::Other { .. }
        ));
    }
    #[derive(Default)]
    struct Fake {
        requests: Vec<(String, Value)>,
        events: Vec<ServerEvent>,
        batches: Vec<Vec<ServerEvent>>,
        fail_turn: bool,
    }
    impl RpcTransport for Fake {
        fn request(&mut self, method: &str, params: Value) -> Result<Value, ProviderError> {
            self.requests.push((method.into(), params));
            match method {
                "initialize" | "initialized" => Ok(json!({})),
                "thread/start" => Ok(
                    json!({ "thread": { "id": "thr-new" }, "activePermissionProfile": { "id": ASSISTANT_PERMISSION_PROFILE, "extends": null }, "sandbox": { "type": "readOnly", "networkAccess": false }, "approvalPolicy": "never", "model": DEFAULT_MODEL }),
                ),
                "thread/resume" => Ok(
                    json!({ "thread": { "id": "thr-existing" }, "activePermissionProfile": { "id": ASSISTANT_PERMISSION_PROFILE, "extends": null }, "sandbox": { "type": "readOnly", "networkAccess": false }, "approvalPolicy": "never", "model": DEFAULT_MODEL }),
                ),
                "turn/start" if self.fail_turn => {
                    Err(ProviderError::Transport("disconnected".into()))
                }
                "turn/start" => Ok(json!({ "turn": { "id": "turn-1" } })),
                _ => Ok(json!({})),
            }
        }
        fn notifications(&mut self) -> Result<Vec<ServerEvent>, ProviderError> {
            if !self.batches.is_empty() {
                Ok(self.batches.remove(0))
            } else {
                Ok(std::mem::take(&mut self.events))
            }
        }
        fn notify(&mut self, method: &str, params: Value) -> Result<(), ProviderError> {
            self.requests.push((method.into(), params));
            Ok(())
        }
        fn interrupt(&mut self, _: &str, _: &str) -> Result<(), ProviderError> {
            Ok(())
        }
    }
    #[test]
    fn starts_exact_profile_and_filters_stale_events() {
        let fake = Fake {
            events: vec![
                ServerEvent::AgentDelta {
                    thread_id: Some("thr-new".into()),
                    turn_id: "old".into(),
                    text: "bad".into(),
                },
                ServerEvent::AgentDelta {
                    thread_id: Some("thr-new".into()),
                    turn_id: "turn-1".into(),
                    text: "ok".into(),
                },
                ServerEvent::Completed {
                    thread_id: Some("thr-new".into()),
                    turn_id: "turn-1".into(),
                    usage: None,
                },
            ],
            ..Default::default()
        };
        let mut a = MainAssistant::new(
            fake,
            MainProfile {
                profile_id: "p".into(),
                thread_id: None,
            },
        );
        a.start_or_resume().unwrap();
        let result = a.send("hello").unwrap();
        assert_eq!(
            result,
            TurnResult::Complete {
                turn_id: "turn-1".into(),
                text: "ok".into(),
                usage: None
            }
        );
        assert_eq!(a.profile().thread_id.as_deref(), Some("thr-new"));
    }
    #[test]
    fn unknown_delivery_is_not_resendable_and_tools_are_blocked() {
        let fake = Fake {
            fail_turn: true,
            ..Default::default()
        };
        let mut a = MainAssistant::new(
            fake,
            MainProfile {
                profile_id: "p".into(),
                thread_id: None,
            },
        );
        a.start_or_resume().unwrap();
        assert_eq!(
            a.send("hello"),
            Err(ProviderError::Transport("disconnected".into()))
        );
        assert!(matches!(a.state(), AssistantState::UnknownDelivery { .. }));
        assert_eq!(a.send("retry"), Err(ProviderError::NotReady));
    }
    #[test]
    fn resume_must_return_exact_uuid_and_config_is_restricted() {
        let fake = Fake::default();
        let mut a = MainAssistant::new(
            fake,
            MainProfile {
                profile_id: "p".into(),
                thread_id: Some("other".into()),
            },
        );
        assert!(matches!(
            a.start_or_resume(),
            Err(ProviderError::Protocol(_))
        ));
        assert!(
            MainAssistant::with_config(
                Fake::default(),
                MainProfile {
                    profile_id: "p".into(),
                    thread_id: None
                },
                ProviderConfig {
                    approval_policy: "on-request",
                    ..ProviderConfig::default()
                }
            )
            .is_err()
        );
        assert!(
            MainAssistant::with_config(
                Fake::default(),
                MainProfile {
                    profile_id: "p".into(),
                    thread_id: None
                },
                ProviderConfig {
                    sandbox_policy: SandboxPolicy {
                        read_only: true,
                        network_access: false,
                        readable_roots: vec!["/tmp".into()],
                    },
                    ..ProviderConfig::default()
                }
            )
            .is_err()
        );
    }
    #[test]
    fn parses_documented_nested_turn_events_and_blocks_approval_requests() {
        let completion = ServerEvent::from_json(&json!({
            "method": "turn/completed",
            "params": { "turn": { "id": "t", "status": "completed" } }
        }))
        .unwrap();
        assert_eq!(
            completion,
            ServerEvent::Completed {
                thread_id: None,
                turn_id: "t".into(),
                usage: None
            }
        );
        let approval = ServerEvent::from_json(&json!({
            "method": "item/commandExecution/requestApproval",
            "params": { "turnId": "t" }
        }))
        .unwrap();
        assert_eq!(
            approval,
            ServerEvent::ToolRequest {
                thread_id: None,
                turn_id: "t".into()
            }
        );
    }

    #[test]
    fn effective_thread_settings_fail_closed_on_profile_or_sandbox_drift() {
        let mut response = json!({
            "activePermissionProfile": { "id": ASSISTANT_PERMISSION_PROFILE, "extends": null },
            "sandbox": { "type": "readOnly", "networkAccess": false },
            "approvalPolicy": "never",
            "model": DEFAULT_MODEL,
        });
        assert!(verify_active_permission_profile(&response).is_ok());
        assert!(verify_thread_sandbox(&response).is_ok());
        assert!(verify_thread_settings(&response, &ProviderConfig::default()).is_ok());

        response["activePermissionProfile"]["extends"] = json!(":workspace");
        assert!(matches!(
            verify_active_permission_profile(&response),
            Err(ProviderError::Protocol(_))
        ));
        response["activePermissionProfile"]["extends"] = Value::Null;
        response["sandbox"]["access"] = json!({ "type": "restricted" });
        assert!(matches!(
            verify_thread_sandbox(&response),
            Err(ProviderError::Protocol(_))
        ));
        response["sandbox"] = json!({ "type": "readOnly", "networkAccess": false });
        response["approvalPolicy"] = json!("on-request");
        assert!(matches!(
            verify_thread_settings(&response, &ProviderConfig::default()),
            Err(ProviderError::Protocol(_))
        ));
    }
    #[test]
    fn polling_preserves_inflight_turn_and_cancel_is_reachable() {
        let fake = Fake {
            batches: vec![
                vec![ServerEvent::AgentDelta {
                    thread_id: Some("thr-new".into()),
                    turn_id: "turn-1".into(),
                    text: "part".into(),
                }],
                vec![ServerEvent::Completed {
                    thread_id: Some("thr-new".into()),
                    turn_id: "turn-1".into(),
                    usage: None,
                }],
            ],
            ..Default::default()
        };
        let mut assistant = MainAssistant::new(
            fake,
            MainProfile {
                profile_id: "p".into(),
                thread_id: None,
            },
        );
        assistant.start_or_resume().unwrap();
        assistant.begin_turn("hello").unwrap();
        assert_eq!(assistant.poll_turn().unwrap(), None);
        assert_eq!(
            assistant.poll_turn().unwrap(),
            Some(TurnResult::Complete {
                turn_id: "turn-1".into(),
                text: "part".into(),
                usage: None
            })
        );
        assistant.begin_turn("again").unwrap();
        assert_eq!(assistant.cancel(), Err(ProviderError::UnknownDelivery));
        assert!(matches!(
            assistant.state(),
            AssistantState::UnknownDelivery { .. }
        ));
    }
}
