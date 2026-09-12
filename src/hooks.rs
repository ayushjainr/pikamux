use crate::model::{ObservationKind, Provider, Session, Status, StatusObservation};
use crate::status::{ProjectionFallback, project_status};
use crate::store::{HookObservation, LiveOwner, PendingLaunch, Store};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::fs::File;
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

pub const MAX_HOOK_PAYLOAD_BYTES: usize = 1_048_576;
const MAX_ID_BYTES: usize = 512;
const MAX_TEXT_BYTES: usize = 8_192;

#[derive(Clone, Debug, PartialEq)]
pub struct HookPayload {
    pub session_id: String,
    pub hook_event_name: String,
    pub cwd: Option<String>,
    pub transcript_path: Option<String>,
    pub tool_name: Option<String>,
    pub notification_type: Option<String>,
    pub error: Option<String>,
    pub source: Option<String>,
    pub originator: Option<String>,
    pub entrypoint: Option<String>,
    pub thread_source: Option<String>,
    pub parent_session_id: Option<String>,
    pub session_title: Option<String>,
    pub desired_name: Option<String>,
    pub native_name_error: Option<String>,
    pub model: Option<String>,
    pub deleted: bool,
    pub background_tasks: usize,
}

#[derive(Clone, Debug)]
pub struct HookContext {
    pub now: f64,
    pub ephemeral: bool,
    pub expected_provider: Option<Provider>,
    pub expected_session_id: Option<String>,
    pub desired_name: Option<String>,
    pub launch_token: Option<String>,
    pub owner_token: String,
    pub owner_pid: Option<i64>,
    pub owner_start_time: Option<i64>,
    pub pane_id: Option<String>,
    pub pane_session: Option<String>,
    pub pane_attached: bool,
    /// True only after the caller has tagged and reread the exact pane identity.
    pub exact_home_verified: bool,
    pub hook_fingerprint: String,
    pub codex_worker_originators: Vec<String>,
    pub opencode_worker_title_prefixes: Vec<String>,
}

impl HookContext {
    pub fn at(now: f64) -> Self {
        Self {
            now,
            ephemeral: false,
            expected_provider: None,
            expected_session_id: None,
            desired_name: None,
            launch_token: None,
            owner_token: String::new(),
            owner_pid: None,
            owner_start_time: None,
            pane_id: None,
            pane_session: None,
            pane_attached: false,
            exact_home_verified: false,
            hook_fingerprint: "native-v1".into(),
            codex_worker_originators: Vec::new(),
            opencode_worker_title_prefixes: Vec::new(),
        }
    }

    pub fn from_environment(provider: Provider) -> Result<Self> {
        let expected_provider = match nonempty_env("PIKA_PROVIDER") {
            Some(value) => Some(
                value
                    .parse()
                    .map_err(|message: String| anyhow::anyhow!(message))?,
            ),
            None => None,
        };
        let mut context = Self::at(now());
        context.ephemeral = std::env::var("PIKA_EPHEMERAL").as_deref() == Ok("1");
        context.expected_provider = expected_provider;
        context.expected_session_id = nonempty_env("PIKA_SESSION_ID");
        context.desired_name = nonempty_env("PIKA_NAME");
        context.launch_token = nonempty_env("PIKA_LAUNCH_TOKEN");
        context.owner_token = std::env::var("PIKA_OWNER_TOKEN").unwrap_or_default();
        context.pane_id = nonempty_env("TMUX_PANE");
        if let Ok(executable) = std::env::current_exe() {
            context.hook_fingerprint = crate::setup::hook_spec_fingerprint(provider, &executable)?;
        }
        Ok(context)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookDisposition {
    Ignored,
    OwnerOnly,
    Updated,
    Deleted,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HookTagRequest {
    pub pane_id: String,
    pub provider: Provider,
    pub session_id: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HookNativeNameRequest {
    pub provider: Provider,
    pub session_id: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HookAlert {
    pub provider: Provider,
    pub session_id: String,
    pub name: String,
    pub status: Status,
    pub attention_reason: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HookResult {
    pub disposition: HookDisposition,
    pub provider: Provider,
    pub session_id: Option<String>,
    pub status: Option<Status>,
    pub unread: bool,
    pub reason: Option<String>,
    pub provider_output: Option<Value>,
    pub tag_request: Option<HookTagRequest>,
    pub native_name_request: Option<HookNativeNameRequest>,
    pub alert: Option<HookAlert>,
    pub launch_certified: bool,
}

impl HookResult {
    fn ignored(provider: Provider, session_id: Option<String>, reason: &str) -> Self {
        Self {
            disposition: HookDisposition::Ignored,
            provider,
            session_id,
            status: None,
            unread: false,
            reason: Some(reason.into()),
            provider_output: None,
            tag_request: None,
            native_name_request: None,
            alert: None,
            launch_certified: false,
        }
    }
}

pub fn parse_hook_payload<R: Read>(reader: R, provider: Provider) -> Result<HookPayload> {
    let mut bytes = Vec::new();
    reader
        .take((MAX_HOOK_PAYLOAD_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .context("cannot read provider hook payload")?;
    if bytes.len() > MAX_HOOK_PAYLOAD_BYTES {
        bail!("provider hook payload exceeds {MAX_HOOK_PAYLOAD_BYTES} bytes");
    }
    let value: Value = serde_json::from_slice(&bytes).context("invalid provider hook JSON")?;
    let object = value
        .as_object()
        .context("provider hook payload must be a JSON object")?;
    let session_id = required_text(object, "session_id", MAX_ID_BYTES)?;
    let hook_event_name = required_text(object, "hook_event_name", 64)?;
    if !valid_event(provider, &hook_event_name) {
        bail!(
            "unsupported {} hook event {hook_event_name:?}",
            provider.as_str()
        );
    }
    Ok(HookPayload {
        session_id,
        hook_event_name,
        cwd: optional_text(object, "cwd", MAX_TEXT_BYTES)?,
        transcript_path: optional_text(object, "transcript_path", MAX_TEXT_BYTES)?,
        tool_name: optional_text(object, "tool_name", 512)?,
        notification_type: optional_text(object, "notification_type", 512)?,
        error: optional_text(object, "error", MAX_TEXT_BYTES)?,
        source: optional_text(object, "source", 512)?,
        originator: optional_text(object, "originator", 512)?,
        entrypoint: optional_text(object, "entrypoint", 512)?,
        thread_source: optional_text(object, "thread_source", 512)?,
        parent_session_id: optional_text(object, "parent_session_id", MAX_ID_BYTES)?,
        session_title: optional_text(object, "session_title", 2_048)?,
        desired_name: optional_text(object, "desired_name", 2_048)?,
        native_name_error: optional_text(object, "native_name_error", MAX_TEXT_BYTES)?,
        model: optional_text(object, "model", 512)?,
        deleted: optional_bool(object, "deleted")?.unwrap_or(false),
        background_tasks: optional_array_len(object, "background_tasks")?.unwrap_or(0),
    })
}

pub fn event_state(provider: Provider, payload: &HookPayload) -> StatusObservation {
    let (status, unread, reason, error) = match payload.hook_event_name.as_str() {
        "PreToolUse"
            if (provider == Provider::Codex
                && payload.tool_name.as_deref() == Some("request_user_input"))
                || (provider == Provider::Claude
                    && payload.tool_name.as_deref() == Some("AskUserQuestion")) =>
        {
            (Status::NeedsYou, true, Some("question"), None)
        }
        "UserPromptSubmit" | "PermissionReply" | "QuestionReply" => {
            (Status::Working, false, None, None)
        }
        "PermissionRequest" => (Status::NeedsYou, true, Some("permission"), None),
        "QuestionRequest" => (Status::NeedsYou, true, Some("question"), None),
        "Notification" => match payload.notification_type.as_deref() {
            Some("permission_prompt") => (Status::NeedsYou, true, Some("permission"), None),
            Some("agent_needs_input" | "elicitation_dialog") => {
                (Status::NeedsYou, true, Some("question"), None)
            }
            Some("agent_completed" | "idle_prompt") => {
                (Status::Ready, true, Some("completed"), None)
            }
            _ => (Status::Working, false, None, None),
        },
        "Stop" if provider == Provider::Claude && payload.background_tasks > 0 => {
            (Status::Working, false, None, None)
        }
        "Stop" => (Status::Ready, true, Some("completed"), None),
        "StopFailure" => (
            Status::Error,
            true,
            Some("failed"),
            Some(payload.error.as_deref().unwrap_or("Claude turn failed")),
        ),
        "SessionEnd" => (Status::Parked, false, None, None),
        "SessionStart" => (Status::Ready, false, None, None),
        _ => (Status::Working, false, None, None),
    };
    StatusObservation {
        kind: ObservationKind::Lifecycle,
        status,
        unread,
        attention_reason: reason.map(str::to_owned),
        error: error.map(str::to_owned),
        observed_at: 0.0,
        source: format!("hook:{}", payload.hook_event_name),
    }
}

pub fn handle_hook(
    store: &Store,
    provider: Provider,
    payload: &HookPayload,
    context: &HookContext,
) -> Result<HookResult> {
    let enriched = enrich_hook_payload(provider, payload);
    let payload = &enriched;
    validate_payload(provider, payload)?;
    validate_context(context)?;
    if context.ephemeral {
        return Ok(HookResult::ignored(
            provider,
            Some(payload.session_id.clone()),
            "ephemeral consultation",
        ));
    }
    if context
        .expected_provider
        .is_some_and(|expected| expected != provider)
    {
        return Ok(HookResult::ignored(
            provider,
            Some(payload.session_id.clone()),
            "inherited provider identity does not match",
        ));
    }
    if provider == Provider::Claude
        && context
            .expected_session_id
            .as_deref()
            .is_some_and(|expected| expected != payload.session_id)
    {
        return Ok(HookResult::ignored(
            provider,
            Some(payload.session_id.clone()),
            "inherited Claude identity does not match",
        ));
    }

    store.initialize()?;
    if store.is_untracked(provider, &payload.session_id)? {
        store.delete_live_owner(provider, &payload.session_id, None, None)?;
        return Ok(HookResult::ignored(
            provider,
            Some(payload.session_id.clone()),
            "conversation is explicitly untracked",
        ));
    }
    if worker_originator(provider, payload, context).is_some() {
        let _ = store.delete_session(provider, &payload.session_id, false)?;
        return Ok(HookResult::ignored(
            provider,
            Some(payload.session_id.clone()),
            "subordinate automation worker",
        ));
    }

    let mut existing = store.get_session_by_thread(provider, &payload.session_id)?;
    let mut canonical_id = existing
        .as_ref()
        .map(|session| session.session_id.clone())
        .unwrap_or_else(|| payload.session_id.clone());

    if let Some(parent_id) = payload.parent_session_id.as_deref()
        && provider == Provider::Codex
        && existing.is_none()
        && let Some(parent) = store.get_session_by_thread(provider, parent_id)?
        && same_optional_path(parent.cwd.as_deref(), payload.cwd.as_deref())
        && parent.tmux_pane.as_deref() == context.pane_id.as_deref()
        && context.pane_id.is_some()
    {
        if parent.status == Status::Working {
            record_launch_conflict(
                store,
                None,
                &parent,
                provider,
                &payload.session_id,
                "multiple active Codex continuation threads",
                context.now,
            )?;
            return Ok(HookResult::ignored(
                provider,
                Some(parent.session_id),
                "concurrent continuation",
            ));
        }
        canonical_id = parent.session_id.clone();
        existing = Some(parent);
    }

    if store.is_untracked(provider, &canonical_id)?
        || store.is_untracked(provider, &payload.session_id)?
    {
        store.delete_live_owner(provider, &canonical_id, None, None)?;
        return Ok(HookResult::ignored(
            provider,
            Some(canonical_id),
            "conversation is explicitly untracked",
        ));
    }

    store.record_hook_observation(&HookObservation {
        provider,
        fingerprint: context.hook_fingerprint.clone(),
        event_name: payload.hook_event_name.clone(),
        session_id: payload.session_id.clone(),
        observed_at: context.now,
        source: payload.source.clone(),
        managed: context.launch_token.is_some(),
    })?;

    if provider == Provider::Opencode && payload.hook_event_name == "SessionEnd" && payload.deleted
    {
        store.delete_live_owner(provider, &canonical_id, None, None)?;
        store.delete_recovery_owner(provider, &canonical_id)?;
        store.delete_session(provider, &canonical_id, false)?;
        return Ok(HookResult {
            disposition: HookDisposition::Deleted,
            provider,
            session_id: Some(canonical_id),
            status: None,
            unread: false,
            reason: None,
            provider_output: None,
            tag_request: None,
            native_name_request: None,
            alert: None,
            launch_certified: false,
        });
    }

    let launch = validate_launch(
        store,
        provider,
        &canonical_id,
        payload,
        context,
        existing.as_ref(),
    )?;
    if let LaunchDecision::Ignore(reason) = launch {
        return Ok(HookResult::ignored(provider, Some(canonical_id), &reason));
    }
    let launch_token = match launch {
        LaunchDecision::Use(value) => value,
        LaunchDecision::Ignore(_) => unreachable!(),
    };

    update_owner(store, provider, &canonical_id, payload, context)?;
    if provider == Provider::Opencode && payload.hook_event_name == "SessionHeartbeat" {
        return Ok(HookResult {
            disposition: HookDisposition::OwnerOnly,
            provider,
            session_id: Some(canonical_id),
            status: existing.as_ref().map(|session| session.status),
            unread: existing.as_ref().is_some_and(|session| session.unread),
            reason: None,
            provider_output: None,
            tag_request: None,
            native_name_request: None,
            alert: None,
            launch_certified: false,
        });
    }

    let placeholder = context
        .pane_id
        .as_deref()
        .map(|pane| store.get_session(provider, &format!("unbound:{pane}")))
        .transpose()?
        .flatten();
    let requested_name = payload
        .desired_name
        .as_ref()
        .or(context.desired_name.as_ref());
    let preserve_requested_name = provider == Provider::Opencode
        && payload.hook_event_name == "SessionStart"
        && requested_name.is_some()
        && (payload.native_name_error.is_some()
            || payload.session_title.as_ref() != requested_name);
    let name = if preserve_requested_name {
        requested_name.cloned()
    } else {
        payload
            .session_title
            .clone()
            .or_else(|| {
                launch_token
                    .as_ref()
                    .and_then(|_| pending_name(store, context).ok().flatten())
            })
            .or_else(|| existing.as_ref().and_then(|session| session.name.clone()))
            .or_else(|| {
                placeholder
                    .as_ref()
                    .and_then(|session| session.name.clone())
            })
            .or_else(|| context.desired_name.clone())
    };

    if provider == Provider::Opencode
        && existing.is_none()
        && launch_token.is_none()
        && placeholder.is_none()
        && context.pane_id.is_none()
        && requested_name.is_none()
        && name.as_deref().is_some_and(opencode_placeholder_title)
    {
        return Ok(HookResult {
            disposition: HookDisposition::OwnerOnly,
            provider,
            session_id: Some(canonical_id),
            status: None,
            unread: false,
            reason: Some("provider-generated placeholder title".into()),
            provider_output: None,
            tag_request: None,
            native_name_request: None,
            alert: None,
            launch_certified: false,
        });
    }
    if existing.is_none()
        && launch_token.is_none()
        && placeholder.is_none()
        && context.pane_id.is_none()
        && name.is_none()
    {
        return Ok(HookResult {
            disposition: HookDisposition::OwnerOnly,
            provider,
            session_id: Some(canonical_id),
            status: None,
            unread: false,
            reason: Some("unnamed external conversation".into()),
            provider_output: None,
            tag_request: None,
            native_name_request: None,
            alert: None,
            launch_certified: false,
        });
    }

    let mut observation = event_state(provider, payload);
    if context.pane_attached && observation.status == Status::Ready {
        observation.unread = false;
    }
    if payload.hook_event_name == "SessionEnd"
        && let Some(current) = &existing
        && current.unread
    {
        observation.status = current.status;
        observation.unread = true;
        observation.attention_reason = current.attention_reason.clone();
        observation.error = current.error.clone();
        observation.observed_at = current.last_event_at;
    } else {
        observation.observed_at = context.now;
    }
    store.record_status_observation(provider, &canonical_id, &observation)?;
    if payload.hook_event_name != "SessionEnd" {
        store.clear_status_observation(provider, &canonical_id, ObservationKind::Runtime)?;
    }
    let projection = project_status(
        &store.status_observations(provider, &canonical_id)?,
        payload.hook_event_name != "SessionEnd",
        "unknown",
        ProjectionFallback {
            status: observation.status,
            unread: observation.unread,
            attention_reason: observation.attention_reason.as_deref(),
            error: observation.error.as_deref(),
            observed_at: observation.observed_at,
        },
    );
    let timestamp = context.now;
    let managed = launch_token.is_some()
        || placeholder.is_some()
        || existing.as_ref().is_some_and(|session| session.managed);
    let session = Session {
        provider,
        session_id: canonical_id.clone(),
        name,
        cwd: payload
            .cwd
            .clone()
            .or_else(|| existing.as_ref().and_then(|session| session.cwd.clone()))
            .or_else(|| placeholder.as_ref().and_then(|session| session.cwd.clone())),
        branch: existing.as_ref().and_then(|session| session.branch.clone()),
        transcript_path: payload.transcript_path.clone().or_else(|| {
            existing
                .as_ref()
                .and_then(|session| session.transcript_path.clone())
        }),
        tmux_session: context.pane_session.clone().or_else(|| {
            existing
                .as_ref()
                .and_then(|session| session.tmux_session.clone())
        }),
        tmux_pane: context.pane_id.clone().or_else(|| {
            existing
                .as_ref()
                .and_then(|session| session.tmux_pane.clone())
        }),
        root_pid: context
            .owner_pid
            .or_else(|| existing.as_ref().and_then(|session| session.root_pid)),
        status: projection.status,
        unread: projection.unread,
        model: payload
            .model
            .clone()
            .or_else(|| existing.as_ref().and_then(|session| session.model.clone())),
        source: if managed { "managed" } else { "external" }.into(),
        managed,
        error: projection.error.clone(),
        attention_reason: projection.attention_reason.clone(),
        created_at: existing
            .as_ref()
            .map_or(timestamp, |session| session.created_at),
        updated_at: timestamp,
        last_event_at: projection.observed_at,
        last_activity_at: timestamp,
        live: payload.hook_event_name != "SessionEnd",
        attached: context.pane_attached,
        home_state: "unknown".into(),
        cpu_percent: None,
        rss_kb: None,
        input_tokens: None,
        output_tokens: None,
        cached_input_tokens: None,
        cache_write_tokens: None,
        total_tokens: None,
        estimated_cost_usd: None,
        active_thread_id: (payload.session_id != canonical_id).then(|| payload.session_id.clone()),
    };
    let newly_actionable = !existing.as_ref().is_some_and(|before| {
        before.unread
            && before.status == session.status
            && before.attention_reason == session.attention_reason
            && before.error == session.error
    });
    store.upsert_session(&session, true)?;
    if let Some(placeholder) = placeholder
        && placeholder.session_id != canonical_id
    {
        store.delete_session(provider, &placeholder.session_id, false)?;
    }

    let tag_request = context.pane_id.as_ref().map(|pane_id| HookTagRequest {
        pane_id: pane_id.clone(),
        provider,
        session_id: canonical_id.clone(),
        name: session.display_name(),
    });
    let mut launch_certified = false;
    if let Some(token) = launch_token.as_deref() {
        if let Some(pending) = store.get_pending(token)? {
            if context.exact_home_verified
                && pending.root_pid == context.owner_pid
                && pending.root_pid_start == context.owner_start_time
                && let (Some(pid), Some(start)) = (context.owner_pid, context.owner_start_time)
            {
                launch_certified =
                    store.certify_launch(token, provider, &canonical_id, pid, start)?;
            }
        } else {
            store.delete_pending(token)?;
        }
        if store
            .get_meta(&format!("attached_launch:{token}"))?
            .is_some()
        {
            store.record_attach(provider, &canonical_id)?;
            store.delete_meta(&format!("attached_launch:{token}"))?;
        }
    }

    let provider_output = if provider == Provider::Claude
        && payload.hook_event_name == "SessionStart"
        && context.expected_session_id.as_deref() == Some(payload.session_id.as_str())
        && payload.session_title.is_none()
    {
        context.desired_name.as_ref().map(|name| {
            json!({"hookSpecificOutput": {
                "hookEventName": "SessionStart",
                "sessionTitle": name,
            }})
        })
    } else {
        None
    };
    let native_name_request = if provider == Provider::Codex
        && context.desired_name.is_some()
        && payload.session_title.is_none()
    {
        Some(HookNativeNameRequest {
            provider,
            session_id: payload.session_id.clone(),
            name: context.desired_name.clone().expect("checked desired name"),
        })
    } else {
        None
    };
    let alert = (session.unread
        && newly_actionable
        && !context.pane_attached
        && matches!(
            session.status,
            Status::NeedsYou | Status::Ready | Status::Error
        ))
    .then(|| HookAlert {
        provider,
        session_id: canonical_id.clone(),
        name: session.display_name(),
        status: session.status,
        attention_reason: session.attention_reason.clone(),
    });
    Ok(HookResult {
        disposition: HookDisposition::Updated,
        provider,
        session_id: Some(canonical_id),
        status: Some(session.status),
        unread: session.unread,
        reason: session.attention_reason,
        provider_output,
        tag_request,
        native_name_request,
        alert,
        launch_certified,
    })
}

/// Add only immutable provider metadata needed for identity and worker provenance.
/// Reads are bounded and failures leave the validated hook payload unchanged.
pub fn enrich_hook_payload(provider: Provider, payload: &HookPayload) -> HookPayload {
    let Some(path) = payload.transcript_path.as_deref() else {
        return payload.clone();
    };
    match provider {
        Provider::Codex => enrich_codex_payload(path, payload),
        Provider::Claude => enrich_claude_payload(path, payload),
        Provider::Opencode => payload.clone(),
    }
}

fn enrich_codex_payload(path: &str, payload: &HookPayload) -> HookPayload {
    let Ok(file) = File::open(path) else {
        return payload.clone();
    };
    let mut line = String::new();
    if BufReader::new(file)
        .take(256 * 1024)
        .read_line(&mut line)
        .is_err()
    {
        return payload.clone();
    }
    let Ok(value) = serde_json::from_str::<Value>(&line) else {
        return payload.clone();
    };
    if value.get("type").and_then(Value::as_str) != Some("session_meta") {
        return payload.clone();
    }
    let Some(metadata) = value.get("payload").and_then(Value::as_object) else {
        return payload.clone();
    };
    let mut enriched = payload.clone();
    if let Some(identity) = metadata
        .get("id")
        .or_else(|| metadata.get("session_id"))
        .and_then(Value::as_str)
        .filter(|value| validate_text("session_id", value, MAX_ID_BYTES).is_ok())
    {
        enriched.session_id = identity.into();
    }
    if let Some(originator) = metadata.get("originator").and_then(Value::as_str) {
        enriched.originator = Some(originator.into());
    }
    match metadata.get("source") {
        Some(Value::String(source)) => enriched.source = Some(source.clone()),
        Some(Value::Object(source)) if source.contains_key("subagent") => {
            enriched.thread_source = Some("subagent".into());
        }
        _ => {}
    }
    if metadata.get("thread_source").and_then(Value::as_str) == Some("subagent") {
        enriched.thread_source = Some("subagent".into());
    }
    if let Some(parent) = metadata
        .get("parent_session_id")
        .or_else(|| metadata.get("parent_thread_id"))
        .and_then(Value::as_str)
    {
        enriched.parent_session_id = Some(parent.into());
    }
    enriched
}

fn enrich_claude_payload(path: &str, payload: &HookPayload) -> HookPayload {
    let Ok(file) = File::open(path) else {
        return payload.clone();
    };
    let mut reader = BufReader::new(file).take(1024 * 1024);
    let mut line = String::new();
    for _ in 0..256 {
        line.clear();
        let Ok(read) = reader.read_line(&mut line) else {
            break;
        };
        if read == 0 {
            break;
        }
        if !line.contains("entrypoint") || !line.contains(&payload.session_id) {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if value.get("sessionId").and_then(Value::as_str) != Some(&payload.session_id) {
            continue;
        }
        let mut enriched = payload.clone();
        if let Some(entrypoint) = value.get("entrypoint").and_then(Value::as_str) {
            enriched.entrypoint = Some(entrypoint.into());
        }
        return enriched;
    }
    payload.clone()
}

pub fn hook_stdout(provider: Provider, result: &HookResult) -> String {
    if let Some(value) = &result.provider_output {
        return serde_json::to_string(value).unwrap_or_else(|_| "{}".into());
    }
    if provider == Provider::Codex {
        "{}".into()
    } else {
        String::new()
    }
}

/// Complete a pending launch only after the caller applied `tag_request` and
/// reread both the pane tag and the provider PID generation successfully.
pub fn certify_hook_home(
    store: &Store,
    launch_token: &str,
    tag: &HookTagRequest,
    provider_pid: i64,
    provider_start_time: i64,
) -> Result<bool> {
    let Some(pending) = store.get_pending(launch_token)? else {
        return Ok(false);
    };
    if pending.provider != tag.provider
        || pending.tmux_pane.as_deref() != Some(tag.pane_id.as_str())
        || pending.root_pid != Some(provider_pid)
        || pending.root_pid_start != Some(provider_start_time)
        || store.get_launch_binding(launch_token)? != Some((tag.provider, tag.session_id.clone()))
    {
        return Ok(false);
    }
    store.certify_launch(
        launch_token,
        tag.provider,
        &tag.session_id,
        provider_pid,
        provider_start_time,
    )
}

pub fn handle_process_exit(
    store: &Store,
    provider: Provider,
    code: i32,
    session_id: Option<&str>,
    launch_token: Option<&str>,
    owner_token_value: Option<&str>,
    observed_at: f64,
) -> Result<bool> {
    let target = if let Some(token) = launch_token {
        match store.get_launch_binding(token)? {
            Some((bound_provider, id)) if bound_provider == provider => {
                store.get_session(provider, &id)?
            }
            _ => session_id
                .map(|id| store.get_session(provider, id))
                .transpose()?
                .flatten(),
        }
    } else {
        session_id
            .map(|id| store.get_session(provider, id))
            .transpose()?
            .flatten()
    };
    let Some(mut target) = target else {
        return Ok(false);
    };
    if store.is_untracked(provider, &target.session_id)? {
        if let Some(token) = launch_token {
            store.delete_launch_binding(token)?;
        }
        return Ok(false);
    }
    store.delete_live_owner(
        provider,
        &target.session_id,
        None,
        Some(owner_token_value.unwrap_or("")),
    )?;
    store.delete_recovery_owner(provider, &target.session_id)?;
    if !matches!(code, 0 | 130) {
        store.record_status_observation(
            provider,
            &target.session_id,
            &StatusObservation {
                kind: ObservationKind::Runtime,
                status: Status::Error,
                unread: true,
                attention_reason: Some("exited".into()),
                error: Some(format!("{} exited with status {code}", provider.as_str())),
                observed_at,
                source: "process-exit".into(),
            },
        )?;
    } else if !(target.unread
        && matches!(
            target.status,
            Status::Ready | Status::NeedsYou | Status::Error | Status::OpenTwice
        ))
    {
        store.clear_status_observation(provider, &target.session_id, ObservationKind::Runtime)?;
        store.record_status_observation(
            provider,
            &target.session_id,
            &StatusObservation {
                kind: ObservationKind::Lifecycle,
                status: Status::Parked,
                unread: false,
                attention_reason: None,
                error: None,
                observed_at,
                source: "process-exit".into(),
            },
        )?;
    }
    let projection = project_status(
        &store.status_observations(provider, &target.session_id)?,
        false,
        "unknown",
        ProjectionFallback {
            status: target.status,
            unread: target.unread,
            attention_reason: target.attention_reason.as_deref(),
            error: target.error.as_deref(),
            observed_at: target.last_event_at,
        },
    );
    target.status = projection.status;
    target.unread = projection.unread;
    target.attention_reason = projection.attention_reason;
    target.error = projection.error;
    target.last_event_at = projection.observed_at;
    target.root_pid = None;
    target.updated_at = observed_at;
    target.last_activity_at = observed_at;
    store.upsert_session(&target, true)?;
    store.clear_session_runtime(provider, &target.session_id, observed_at)?;
    if let Some(token) = launch_token {
        store.delete_launch_binding(token)?;
    }
    Ok(true)
}

enum LaunchDecision {
    Ignore(String),
    Use(Option<String>),
}

fn validate_launch(
    store: &Store,
    provider: Provider,
    session_id: &str,
    payload: &HookPayload,
    context: &HookContext,
    existing: Option<&Session>,
) -> Result<LaunchDecision> {
    let pane_pending = context
        .pane_id
        .as_deref()
        .map(|pane| store.find_pending_for_pane(pane))
        .transpose()?
        .flatten();
    if let Some(pending) = &pane_pending
        && context.launch_token.as_deref() != Some(pending.launch_token.as_str())
    {
        if payload.hook_event_name != "SessionEnd" {
            store.set_meta(
                &format!("launch_binding_error:{}", pending.launch_token),
                "refused launch hook: missing or wrong PIKA_LAUNCH_TOKEN",
            )?;
        }
        return Ok(LaunchDecision::Ignore(
            "missing or wrong launch token for pane".into(),
        ));
    }
    let mut token = context.launch_token.clone();
    let pending = token
        .as_deref()
        .map(|value| store.get_pending(value))
        .transpose()?
        .flatten();
    if token.is_some()
        && pending.is_none()
        && store
            .get_launch_binding(token.as_deref().expect("token exists"))?
            .is_none()
    {
        token = None;
    }
    if let Some(pending) = &pending
        && let Some(mismatch) = launch_mismatch(pending, provider, payload, context)
    {
        if payload.hook_event_name != "SessionEnd" {
            store.set_meta(
                &format!("launch_binding_error:{}", pending.launch_token),
                &format!("refused launch hook: {mismatch}"),
            )?;
        }
        return Ok(LaunchDecision::Ignore(mismatch));
    }
    if let Some(value) = token.as_deref() {
        if let Some((bound_provider, bound_id)) = store.get_launch_binding(value)? {
            if (bound_provider, bound_id.as_str()) != (provider, session_id) {
                let switched = provider == Provider::Opencode
                    && payload.hook_event_name != "SessionEnd"
                    && context.owner_pid.is_some()
                    && context.owner_start_time.is_some()
                    && bound_provider == provider
                    && store.switch_launch_binding(
                        value,
                        provider,
                        &bound_id,
                        session_id,
                        context.owner_pid.expect("checked owner pid"),
                        context.owner_start_time.expect("checked owner start"),
                    )?;
                if !switched {
                    if payload.hook_event_name != "SessionEnd" {
                        if let Some(winner) = store.get_session(bound_provider, &bound_id)? {
                            record_launch_conflict(
                                store,
                                Some(value),
                                &winner,
                                provider,
                                session_id,
                                "launch token observed competing provider identities",
                                context.now,
                            )?;
                        } else {
                            store.set_meta(
                                &format!("launch_binding_error:{value}"),
                                &format!("refused competing {}:{session_id}", provider.as_str()),
                            )?;
                        }
                    }
                    return Ok(LaunchDecision::Ignore("competing launch identity".into()));
                }
            }
        } else if !store.bind_launch(value, provider, session_id)? {
            return Ok(LaunchDecision::Ignore("competing launch identity".into()));
        }
    }
    let _ = existing;
    Ok(LaunchDecision::Use(token))
}

fn launch_mismatch(
    pending: &PendingLaunch,
    provider: Provider,
    payload: &HookPayload,
    context: &HookContext,
) -> Option<String> {
    if pending.provider != provider {
        return Some(format!(
            "expected provider {}, observed {}",
            pending.provider.as_str(),
            provider.as_str()
        ));
    }
    if pending
        .expected_session_id
        .as_deref()
        .is_some_and(|expected| expected != payload.session_id)
    {
        return Some(format!(
            "expected session {}, observed {}",
            pending.expected_session_id.as_deref().unwrap_or_default(),
            payload.session_id
        ));
    }
    if provider == Provider::Codex && payload.parent_session_id.is_some() {
        return Some("expected a new root thread, observed child thread".into());
    }
    if pending
        .tmux_pane
        .as_deref()
        .is_some_and(|expected| context.pane_id.as_deref() != Some(expected))
    {
        return Some(format!(
            "expected pane {}, observed {}",
            pending.tmux_pane.as_deref().unwrap_or_default(),
            context.pane_id.as_deref().unwrap_or("none")
        ));
    }
    if !same_optional_path(Some(&pending.cwd), payload.cwd.as_deref()) {
        return Some(format!(
            "expected cwd {}, observed {}",
            pending.cwd,
            payload.cwd.as_deref().unwrap_or("none")
        ));
    }
    None
}

#[allow(clippy::too_many_arguments)]
fn record_launch_conflict(
    store: &Store,
    launch_token: Option<&str>,
    winner: &Session,
    competitor_provider: Provider,
    competitor_id: &str,
    prefix: &str,
    observed_at: f64,
) -> Result<()> {
    if let Some(token) = launch_token {
        store.set_meta(
            &format!("launch_binding_error:{token}"),
            &format!(
                "{prefix}: {}:{}, {}:{}",
                winner.provider.as_str(),
                short(&winner.session_id),
                competitor_provider.as_str(),
                short(competitor_id)
            ),
        )?;
    }
    store.capture_identity_interruption(winner.provider, &winner.session_id)?;
    let status = if winner.provider == competitor_provider {
        Status::OpenTwice
    } else {
        Status::Error
    };
    let error = format!(
        "{prefix}: {}:{}, {}:{}",
        winner.provider.as_str(),
        short(&winner.session_id),
        competitor_provider.as_str(),
        short(competitor_id)
    );
    store.record_status_observation(
        winner.provider,
        &winner.session_id,
        &StatusObservation {
            kind: ObservationKind::Safety,
            status,
            unread: true,
            attention_reason: Some("identity".into()),
            error: Some(error.clone()),
            observed_at,
            source: "launch-conflict".into(),
        },
    )?;
    let mut updated = winner.clone();
    updated.status = status;
    updated.unread = true;
    updated.attention_reason = Some("identity".into());
    updated.error = Some(error);
    updated.last_event_at = observed_at;
    updated.last_activity_at = observed_at;
    store.upsert_session(&updated, true)?;
    Ok(())
}

fn update_owner(
    store: &Store,
    provider: Provider,
    session_id: &str,
    payload: &HookPayload,
    context: &HookContext,
) -> Result<()> {
    let Some(pid) = context.owner_pid else {
        return Ok(());
    };
    if payload.hook_event_name == "SessionEnd" {
        store.delete_live_owner(provider, session_id, Some(pid), owner_token(context))?;
    } else {
        if provider == Provider::Opencode {
            store.delete_other_live_owner_sessions(provider, pid, session_id)?;
        }
        store.set_live_owner(&LiveOwner {
            provider,
            session_id: session_id.into(),
            pid,
            start_time: context.owner_start_time,
            owner_token: context.owner_token.clone(),
            last_seen: context.now,
        })?;
    }
    Ok(())
}

fn owner_token(context: &HookContext) -> Option<&str> {
    Some(context.owner_token.as_str())
}

fn pending_name(store: &Store, context: &HookContext) -> Result<Option<String>> {
    context
        .launch_token
        .as_deref()
        .map(|token| store.get_pending(token))
        .transpose()
        .map(|pending| pending.flatten().map(|pending| pending.name))
}

fn worker_originator<'a>(
    provider: Provider,
    payload: &'a HookPayload,
    context: &'a HookContext,
) -> Option<&'a str> {
    match provider {
        Provider::Codex => {
            if payload.source.as_deref() == Some("exec") {
                return Some("codex-exec");
            }
            if payload.thread_source.as_deref() == Some("subagent") {
                return Some("codex-subagent");
            }
            let origin = payload.originator.as_deref()?;
            if origin.eq_ignore_ascii_case("codex_exec")
                || context
                    .codex_worker_originators
                    .iter()
                    .any(|value| value.eq_ignore_ascii_case(origin))
            {
                Some(origin)
            } else {
                None
            }
        }
        Provider::Claude => {
            (payload.entrypoint.as_deref() == Some("sdk-cli")).then_some("claude-sdk-cli")
        }
        Provider::Opencode => {
            if payload.parent_session_id.is_some() {
                return Some("opencode-subagent");
            }
            let title = payload.session_title.as_deref()?;
            context
                .opencode_worker_title_prefixes
                .iter()
                .find(|prefix| title.to_lowercase().starts_with(&prefix.to_lowercase()))
                .map(String::as_str)
        }
    }
}

fn opencode_placeholder_title(value: &str) -> bool {
    value.starts_with("New session - ") || (value.contains(" (fork #") && value.ends_with(')'))
}

fn valid_event(provider: Provider, event: &str) -> bool {
    let common = [
        "SessionStart",
        "UserPromptSubmit",
        "PermissionRequest",
        "PreToolUse",
        "PostToolUse",
        "Stop",
        "SessionEnd",
    ];
    common.contains(&event)
        || match provider {
            Provider::Codex => false,
            Provider::Claude => matches!(event, "Notification" | "StopFailure"),
            Provider::Opencode => matches!(
                event,
                "SessionHeartbeat"
                    | "PermissionReply"
                    | "QuestionRequest"
                    | "QuestionReply"
                    | "StopFailure"
            ),
        }
}

fn validate_payload(provider: Provider, payload: &HookPayload) -> Result<()> {
    validate_text("session_id", &payload.session_id, MAX_ID_BYTES)?;
    validate_text("hook_event_name", &payload.hook_event_name, 64)?;
    if !valid_event(provider, &payload.hook_event_name) {
        bail!(
            "unsupported {} hook event {:?}",
            provider.as_str(),
            payload.hook_event_name
        );
    }
    for (key, value, max) in [
        ("cwd", payload.cwd.as_deref(), MAX_TEXT_BYTES),
        (
            "transcript_path",
            payload.transcript_path.as_deref(),
            MAX_TEXT_BYTES,
        ),
        ("session_title", payload.session_title.as_deref(), 2_048),
        ("desired_name", payload.desired_name.as_deref(), 2_048),
        ("error", payload.error.as_deref(), MAX_TEXT_BYTES),
    ] {
        if let Some(value) = value {
            validate_text(key, value, max)?;
        }
    }
    Ok(())
}

fn validate_context(context: &HookContext) -> Result<()> {
    if !context.now.is_finite() || context.now < 0.0 {
        bail!("hook observation time must be finite and non-negative");
    }
    for (key, value, max) in [
        (
            "PIKA_SESSION_ID",
            context.expected_session_id.as_deref(),
            MAX_ID_BYTES,
        ),
        ("PIKA_NAME", context.desired_name.as_deref(), 2_048),
        (
            "PIKA_LAUNCH_TOKEN",
            context.launch_token.as_deref(),
            MAX_ID_BYTES,
        ),
        (
            "PIKA_OWNER_TOKEN",
            Some(context.owner_token.as_str()),
            MAX_ID_BYTES,
        ),
        ("TMUX_PANE", context.pane_id.as_deref(), 512),
        (
            "hook_fingerprint",
            Some(context.hook_fingerprint.as_str()),
            512,
        ),
    ] {
        if let Some(value) = value {
            if value.is_empty() && matches!(key, "PIKA_OWNER_TOKEN") {
                continue;
            }
            validate_text(key, value, max)?;
        }
    }
    if context.owner_pid.is_some_and(|pid| pid <= 0)
        || context.owner_start_time.is_some_and(|start| start < 0)
    {
        bail!("hook owner PID generation is invalid");
    }
    if context.exact_home_verified
        && (context.pane_id.is_none()
            || context.owner_pid.is_none()
            || context.owner_start_time.is_none())
    {
        bail!("exact home verification requires pane and PID-generation evidence");
    }
    Ok(())
}

fn validate_text(key: &str, value: &str, max: usize) -> Result<()> {
    if value.is_empty() || value.len() > max || value.contains('\0') {
        bail!("hook field {key:?} is empty, too long, or contains NUL");
    }
    Ok(())
}

fn required_text(object: &Map<String, Value>, key: &str, max: usize) -> Result<String> {
    optional_text(object, key, max)?.with_context(|| format!("missing hook field {key:?}"))
}

fn optional_text(object: &Map<String, Value>, key: &str, max: usize) -> Result<Option<String>> {
    let Some(value) = object.get(key) else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let text = value
        .as_str()
        .with_context(|| format!("hook field {key:?} must be a string"))?;
    if text.is_empty() || text.len() > max || text.chars().any(|character| character == '\0') {
        bail!("hook field {key:?} is empty, too long, or contains NUL");
    }
    Ok(Some(text.into()))
}

fn optional_bool(object: &Map<String, Value>, key: &str) -> Result<Option<bool>> {
    object
        .get(key)
        .map(|value| {
            value
                .as_bool()
                .with_context(|| format!("hook field {key:?} must be a boolean"))
        })
        .transpose()
}

fn optional_array_len(object: &Map<String, Value>, key: &str) -> Result<Option<usize>> {
    object
        .get(key)
        .map(|value| {
            value
                .as_array()
                .map(Vec::len)
                .with_context(|| format!("hook field {key:?} must be an array"))
        })
        .transpose()
}

fn same_optional_path(left: Option<&str>, right: Option<&str>) -> bool {
    let (Some(left), Some(right)) = (left, right) else {
        return false;
    };
    match (
        Path::new(left).canonicalize(),
        Path::new(right).canonicalize(),
    ) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

fn short(value: &str) -> &str {
    value.get(..8).unwrap_or(value)
}

fn nonempty_env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}
