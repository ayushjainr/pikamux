//! Board-home assistant surface. Durable memory is independent of provider sessions.
use crate::{
    assistant_host::{Client, Owner},
    assistant_memory::{DecisionState, NewRecord, Origin, RecordKind, Scope, Store},
};
use anyhow::{Context, Result, bail};
use crossterm::{
    cursor::{Hide, MoveTo, Show},
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
        MouseEventKind,
    },
    execute, queue,
    style::{Attribute, Color, Print, ResetColor, SetAttribute, SetForegroundColor},
    terminal::{
        self, Clear, ClearType, EndSynchronizedUpdate, EnterAlternateScreen, LeaveAlternateScreen,
    },
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    io::{self, IsTerminal},
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use unicode_width::UnicodeWidthChar;

#[path = "assistant_setup.rs"]
mod setup;

#[derive(clap::Args, Debug, Default)]
pub(crate) struct Args {
    #[arg(skip)]
    pub view_memory: Option<std::sync::Arc<std::sync::Mutex<ViewMemory>>>,
    #[arg(skip)]
    pub startup_notice: Option<String>,
    #[arg(skip)]
    pub restore_startup: bool,
    /// Keep the operational board beside the conversation.
    #[arg(skip)]
    pub board: bool,
    /// Local view focus is a reference, never permission to read a task.
    #[arg(skip)]
    pub focus: Option<Focus>,
    /// Emit durable assistant state without starting a provider.
    #[arg(long)]
    pub json: bool,
    /// Explicit personal/project memory scope (no implicit project content access).
    #[arg(long, default_value = "")]
    pub scope: String,
    /// Remember this existing profile/provider for normal startup and board P.
    #[arg(long, requires_all = ["profile_root", "enable_codex"])]
    pub set_default: bool,
    /// Open local memory without enabling the saved provider.
    #[arg(long, conflicts_with = "enable_codex")]
    pub offline: bool,
    /// Attach an existing local profile without relocating or copying its state.
    #[arg(long, requires = "expected_profile_id")]
    pub profile_root: Option<std::path::PathBuf>,
    /// Immutable identity required when choosing an existing profile directory.
    #[arg(long, requires = "profile_root")]
    pub expected_profile_id: Option<String>,
    /// Save an explicit user instruction without calling a model.
    #[arg(long, conflicts_with = "decision")]
    pub remember: Option<String>,
    /// Save a decision and rationale in your own words.
    #[arg(long)]
    pub decision: Option<String>,
    /// Explicitly enable this foreground assistant with an absolute Codex binary.
    /// Uses assistant/provider-home only; never copies existing credentials.
    #[arg(long, requires = "assistant_call_limit", conflicts_with_all = ["remember", "decision"])]
    pub enable_codex: Option<std::path::PathBuf>,
    /// Total lifetime call allowance, not a dollar ceiling (1–100).
    #[arg(long, requires = "enable_codex", group = "assistant_call_limit")]
    pub max_calls: Option<u64>,
    /// Remove the lifetime call ceiling; keep per-request limits and usage accounting.
    #[arg(long, requires = "enable_codex", group = "assistant_call_limit")]
    pub no_call_limit: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct Focus {
    pub identity: crate::assistant_observation::Identity,
    pub label: String,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
enum Request {
    Snapshot {
        scope: String,
    },
    Save {
        request_id: String,
        scope: String,
        kind: SaveKind,
        body: String,
        timestamp: i64,
    },
    Forget {
        record_id: String,
    },
    Enable {
        scope: String,
        executable: std::path::PathBuf,
        max_calls: u64,
        #[serde(default)]
        restore: bool,
    },
    Send {
        scope: String,
        request_id: String,
        body: String,
        #[serde(default)]
        timestamp: i64,
        #[serde(default)]
        focus: Option<crate::assistant_observation::Identity>,
    },
    Cancel,
    FreshContext {
        request_id: String,
    },
    Evolve {
        scope: String,
        request_id: String,
    },
    ApproveTool {
        scope: String,
        hash: String,
    },
    RevokeTool {
        scope: String,
        grant_id: String,
    },
    RollbackTool {
        scope: String,
        name: String,
        hash: String,
    },
    InvokeTool {
        scope: String,
        name: String,
        inputs: String,
    },
    Brief {
        scope: String,
    },
    AcknowledgeBrief {
        cursor: crate::assistant_presentation::BriefCursor,
    },
    MemoryPage {
        scope: String,
        cursor: Option<crate::assistant_presentation::MemoryCursor>,
        limit: usize,
    },
    MemoryRecord {
        scope: String,
        record_id: String,
        cursor: Option<crate::assistant_presentation::RecordChunkCursor>,
    },
    MemorySearch {
        scope: String,
        query: String,
    },
    ProfileView {
        scope: String,
        view: crate::assistant_profile_views::View,
    },
    Feedback {
        scope: String,
        note: Option<String>,
    },
    Recall {
        scope: String,
        record_id: String,
    },
    Correct {
        scope: String,
        request_id: String,
        record_id: String,
        body: String,
        #[serde(default)]
        timestamp: i64,
    },
    Investigate {
        scope: String,
        request_id: String,
        body: String,
        #[serde(default)]
        timestamp: i64,
        #[serde(default)]
        focus: Option<crate::assistant_observation::Identity>,
    },
    Help,
    Explain {
        scope: String,
        record_id: String,
        reply: crate::assistant_briefing::ExplainBack,
    },
    EvolveSpec {
        scope: String,
        request_id: String,
        spec: String,
    },
    ToolCatalog {
        scope: String,
        hash: Option<String>,
    },
    AssessTool {
        scope: String,
        hash: String,
        outcome: String,
        evidence: String,
        rollback: Option<String>,
    },
    Improvement {
        scope: String,
        correction_id: String,
    },
    DecisionState {
        scope: String,
        request_id: String,
        record_id: String,
        state: DecisionState,
        timestamp: i64,
    },
    DecisionRevision {
        scope: String,
        request_id: String,
        record_id: String,
        decision: crate::assistant_briefing::Decision,
        timestamp: i64,
    },
    ShareBoard {
        scope: String,
        confirmation: Option<String>,
    },
    RevokeBoard {
        scope: String,
    },
    Background {
        scope: String,
        max_calls: u64,
        #[serde(default)]
        hours: u64,
    },
    Maintenance {
        scope: String,
        max_calls: u64,
        #[serde(default)]
        hours: u64,
        interval_hours: u64,
    },
    MaintenanceStatus {
        scope: String,
    },
    MaintenanceOff {
        scope: String,
    },
    MaintenanceRevisit {
        scope: String,
        record_id: String,
        hours: u64,
    },
    Guidance {
        scope: String,
        record_id: String,
        enabled: bool,
    },
    Method {
        scope: String,
        action: String,
        input: String,
    },
    CommitmentDue {
        scope: String,
        request_id: String,
        record_id: String,
        due_at: i64,
    },
    CommitmentDone {
        scope: String,
        request_id: String,
        record_id: String,
        statement: String,
    },
    Pause,
    Resume,
    StopBackground,
    ConsultSources {
        scope: String,
    },
    AllowConsult {
        scope: String,
        provider: crate::model::Provider,
        conversation: String,
        #[serde(default)]
        hours: u64,
    },
    RevokeConsult {
        scope: String,
        id: String,
        forget: bool,
    },
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SaveKind {
    Draft,
    Instruction,
    Decision,
    Proposal,
    Grasp,
}

pub(crate) fn scope(value: &str) -> Result<Scope> {
    if value.is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
        bail!("Choose a scope of 1–256 printable bytes");
    }
    Ok(Scope {
        project: Some(value.into()),
        ..Scope::default()
    })
}

const HELP: &str = "PIKA · just talk to me
Ask a question, correct me, or describe how you want us to work.
Useful memory and reversible guidance are learned during the same conversation.
No JSON, special phrases, or approval ceremony is needed for routine learning.

/brief · what changed and what needs you
/profile · read-only IDENTITY.md, SOUL.md and MEMORY.md views
/feedback TEXT · save product feedback in user_feedback.md
/memory · inspect saved memory; /memory-search QUERY to find something
/remember TEXT · save an explicit standing instruction
/decision TEXT · record a decision you have made
/correct ID TEXT · correct saved wording
/guidance-off ID or /guidance-on ID · undo or restore learned guidance
/forget ID · remove dependent Pika memory; external copies are unaffected
/investigate QUESTION · 0–2 helpers only if needed, within existing allowance

/maintenance CALLS · daily memory review until disabled; no new data access
/maintenance · status; /maintenance off to stop
/background CALLS · board follow-up until disabled, after /board-share approval
/background off · stop background work
/board-share · preview exactly what would be shared; /board-share off to revoke
/consults · inspect approved private consultations
/pause or /resume · pause/resume Pika, never your project agents
/cancel · stop this request; uncertain delivery is not retried

/proposals or /tools · inspect tested improvements
/approve · enable the tested version displayed here, until revoked
/fresh-context · explain recovery without replaying requests
Advanced timed grants and exact-version/JSON workshop controls remain available
for developers; see docs/ASSISTANT.md. They are not required for ordinary learning.
Esc/F12 returns to the board. PgUp/PgDn or mouse wheel scrolls.";

pub(crate) fn serve(root: &Path, expected_profile: Option<&str>) -> Result<i32> {
    let owner = Owner::acquire(root)?;
    let mut memory = open_host_memory(root, expected_profile)?;
    let mut session = crate::assistant_session::Session::new();
    let mut recovery: Option<crate::assistant_recovery_service::RecoveryService> = None;
    let mut restored_recovery = None;
    let mut control = crate::assistant_control::Controller::open(root)?;
    let lifetime = control.gate();
    session.set_workshop_root(root)?;
    let mut presentation = PresentationDriver::new(root)?;
    let mut auxiliary = AuxiliaryWorkers::new(root, lifetime.clone())?;
    owner.serve_with_lifetime(
        move |request| {
            restore_completed_recovery(
                root,
                &mut session,
                &mut control,
                &recovery,
                &mut restored_recovery,
            )?;
            auxiliary.after_recovery(root, &recovery)?;
            let Some(request) = request else {
                if let Some(request) = auxiliary.ready_request() {
                    let result = handle_session(
                        root,
                        &mut memory,
                        &mut session,
                        &mut recovery,
                        &mut control,
                        request,
                    );
                    auxiliary.after_deferred(root, result)?;
                }
                tick_host_services(
                    root,
                    &mut memory,
                    &mut session,
                    &mut control,
                    &mut presentation,
                    &auxiliary,
                )?;
                return Ok(None);
            };
            let request: Request = serde_json::from_value(request)?;
            presentation.observe(&request);
            let result = handle_host_request(
                root,
                &mut memory,
                &mut session,
                &mut recovery,
                &mut control,
                &mut auxiliary,
                request,
            );
            auxiliary.maintenance.tick(session.busy());
            auxiliary.maintenance.foreground_pending(false);
            result.map(Some)
        },
        lifetime,
    )?;
    Ok(0)
}

fn tick_host_services(
    root: &Path,
    memory: &mut Store,
    session: &mut crate::assistant_session::Session,
    control: &mut crate::assistant_control::Controller,
    presentation: &mut PresentationDriver,
    auxiliary: &AuxiliaryWorkers,
) -> Result<()> {
    session.tick()?;
    control.tick(root, session)?;
    auxiliary.maintenance.tick(session.busy());
    presentation.tick(memory, session, control);
    Ok(())
}

/// Own only disposable noninteractive workers and their quiescence fence.
/// Provider session, memory authority and presentation remain separate owners.
struct AuxiliaryWorkers {
    maintenance: crate::assistant_maintenance_driver::Driver,
    methods: crate::assistant_method_service::MethodService,
    deferred: Option<Request>,
    recovering: bool,
    notice: Option<String>,
    gate: crate::assistant_lifecycle::LifetimeGate,
    cleanup_hold: Option<crate::assistant_lifecycle::CleanupHold>,
}
impl AuxiliaryWorkers {
    fn new(root: &Path, gate: crate::assistant_lifecycle::LifetimeGate) -> Result<Self> {
        Ok(Self {
            maintenance: crate::assistant_maintenance_driver::Driver::new(root.to_path_buf())?,
            methods: crate::assistant_method_service::MethodService::new(root.to_path_buf())?,
            deferred: None,
            recovering: false,
            notice: None,
            gate,
            cleanup_hold: None,
        })
    }
    fn after_recovery(
        &mut self,
        root: &Path,
        recovery: &Option<crate::assistant_recovery_service::RecoveryService>,
    ) -> Result<()> {
        if self.recovering
            && self.deferred.is_none()
            && recovery
                .as_ref()
                .is_some_and(|r| r.snapshot().state == "completed")
        {
            *self = Self::new(root, self.gate.clone())?;
        }
        Ok(())
    }
    fn ready_request(&mut self) -> Option<Request> {
        if self.deferred.is_some() && self.maintenance.is_quiescent() && self.methods.is_quiescent()
        {
            self.deferred.take()
        } else {
            None
        }
    }
    fn after_deferred(&mut self, root: &Path, result: Result<Value>) -> Result<()> {
        if result.is_err() {
            self.recovering = false;
        }
        self.notice = Some(match result {
            Ok(value) => value["notice"]
                .as_str()
                .unwrap_or("Owned-worker cleanup completed.")
                .to_owned(),
            Err(error) => format!("Queued cleanup failed: {error}. No request was replayed."),
        });
        if !self.recovering {
            let notice = self.notice.take();
            *self = Self::new(root, self.gate.clone())?;
            self.notice = notice;
        }
        // Fresh recovery has its own owned teardown; local forgetting is now
        // complete even if its requesting view already closed.
        self.cleanup_hold = None;
        Ok(())
    }
    fn fence_request(&mut self, request: Request) -> Result<Request, Value> {
        if self.deferred.is_some()
            && !matches!(
                request,
                Request::Snapshot { .. } | Request::Help | Request::MaintenanceStatus { .. }
            )
        {
            return Err(
                json!({"cleanup_pending":true,"notice":"Cleanup is already queued; waiting for Pika's owned workers to stop. No need to resend."}),
            );
        }
        if matches!(
            request,
            Request::FreshContext { .. } | Request::Forget { .. } | Request::RevokeBoard { .. }
        ) {
            self.recovering = matches!(request, Request::FreshContext { .. });
            // Both must be asked to stop even if the first is still busy.
            let maintenance = self.maintenance.quiesce();
            let methods = self.methods.quiesce();
            if !maintenance || !methods {
                self.cleanup_hold = Some(self.gate.retain_cleanup());
                self.deferred = Some(request);
                return Err(
                    json!({"cleanup_pending":true,"notice":"Stopping Pika's owned workers before cleanup. Your request is queued; do not resend. Project agents are untouched."}),
                );
            }
        }
        if matches!(request, Request::Cancel | Request::Pause) {
            self.methods.cancel();
        }
        Ok(request)
    }
    fn cached_control(&mut self, memory: &Store, request: &Request) -> Result<Option<Value>> {
        match request {
            Request::MaintenanceStatus { scope: name } => {
                let status = self.maintenance.status(name);
                Ok(Some(
                    json!({"maintenance":status,"local_output":serde_json::to_string_pretty(&status)?}),
                ))
            }
            Request::Method {
                scope: name,
                action,
                input,
            } => {
                let job = self.methods.begin(
                    scope(name)?,
                    action.clone(),
                    input.clone(),
                    timestamp(),
                    memory.forget_epoch()?,
                )?;
                Ok(Some(
                    json!({"notice":"Native workshop job started; results appear here when complete. No model call.","method_job":job}),
                ))
            }
            _ => Ok(None),
        }
    }
}

fn existing_recovery_reply(root: &Path, request: &Request) -> Result<Option<Value>> {
    if let Request::FreshContext { request_id } = request {
        if let Some(receipt) = crate::assistant_recovery::existing_receipt(root, request_id)? {
            return Ok(Some(
                json!({"recovery_id":receipt.receipt_id,"notice":"This recovery was already recorded. Current work was not touched."}),
            ));
        }
    }
    Ok(None)
}

fn needs_foreground_priority(request: &Request) -> bool {
    !matches!(
        request,
        Request::Snapshot { .. }
            | Request::Brief { .. }
            | Request::Help
            | Request::MemoryPage { .. }
            | Request::MemoryRecord { .. }
            | Request::MemorySearch { .. }
            | Request::ProfileView { .. }
            | Request::Recall { .. }
            | Request::ToolCatalog { .. }
            | Request::MaintenanceStatus { .. }
    )
}

fn handle_host_request(
    root: &Path,
    memory: &mut Store,
    session: &mut crate::assistant_session::Session,
    recovery: &mut Option<crate::assistant_recovery_service::RecoveryService>,
    control: &mut crate::assistant_control::Controller,
    auxiliary: &mut AuxiliaryWorkers,
    request: Request,
) -> Result<Value> {
    if let Request::Feedback { scope: name, note } = &request {
        scope(name)?;
        return crate::assistant_feedback::submit(root, name, note.as_deref(), timestamp());
    }
    if let Some(reply) = existing_recovery_reply(root, &request)? {
        return Ok(reply);
    }
    if recovery.as_ref().is_some_and(|r| r.busy())
        && !matches!(
            request,
            Request::Snapshot { .. } | Request::Help | Request::MaintenanceStatus { .. }
        )
    {
        bail!("Pika is finishing owned-job recovery; changes wait until it completes.");
    }
    let request = match auxiliary.fence_request(request) {
        Ok(request) => request,
        Err(reply) => return Ok(reply),
    };
    if needs_foreground_priority(&request) {
        auxiliary.maintenance.foreground_pending(true);
    }
    if let Some(reply) = auxiliary.cached_control(memory, &request)? {
        return Ok(reply);
    }
    if let Some(reply) = handle_maintenance(memory, session, control, &request)? {
        return Ok(reply);
    }
    dispatch_session_request(root, memory, session, recovery, control, auxiliary, request)
}

fn dispatch_session_request(
    root: &Path,
    memory: &mut Store,
    session: &mut crate::assistant_session::Session,
    recovery: &mut Option<crate::assistant_recovery_service::RecoveryService>,
    control: &mut crate::assistant_control::Controller,
    auxiliary: &mut AuxiliaryWorkers,
    request: Request,
) -> Result<Value> {
    let snapshot_scope = if let Request::Snapshot { scope: name } = &request {
        Some(scope(name)?)
    } else {
        None
    };
    let forgetting = matches!(
        request,
        Request::Forget { .. } | Request::RevokeBoard { .. }
    );
    let recovering = matches!(request, Request::FreshContext { .. });
    let result = handle_session(root, memory, session, recovery, control, request);
    if forgetting || (recovering && result.is_err()) {
        *auxiliary = AuxiliaryWorkers::new(root, auxiliary.gate.clone())?;
    }
    decorate_auxiliary_reply(memory, auxiliary, snapshot_scope, result?)
}

fn decorate_auxiliary_reply(
    memory: &Store,
    auxiliary: &AuxiliaryWorkers,
    snapshot_scope: Option<Scope>,
    mut reply: Value,
) -> Result<Value> {
    if let Some(selected) = snapshot_scope {
        reply["cleanup_pending"] = json!(auxiliary.deferred.is_some());
        reply["method_control"] = auxiliary
            .methods
            .snapshot(&selected, memory.forget_epoch()?);
        if let Some(notice) = &auxiliary.notice {
            reply["cleanup_notice"] = json!(notice);
        }
    }
    Ok(reply)
}

fn open_host_memory(root: &Path, expected_profile: Option<&str>) -> Result<Store> {
    let mut memory = match expected_profile {
        Some(profile) => Store::open_existing(root.join("memory.sqlite"), profile)?,
        None => Store::open(root.join("memory.sqlite"))?,
    };
    // Prepare shared maintenance tables before background workers and the first
    // foreground write can race to create them on separate connections.
    crate::assistant_maintenance::initialize(&memory)?;
    memory.set_busy_timeout(25)?;
    crate::assistant_retention::cleanup(root, memory.forget_epoch()?)?;
    Ok(memory)
}

fn handle_maintenance(
    memory: &mut Store,
    session: &crate::assistant_session::Session,
    control: &mut crate::assistant_control::Controller,
    request: &Request,
) -> Result<Option<Value>> {
    use crate::assistant_maintenance as maintenance;
    let notice = match request {
        Request::Maintenance {
            scope: name,
            max_calls,
            hours,
            interval_hours,
        } => {
            enable_maintenance(
                memory,
                session,
                control,
                name,
                *max_calls,
                *hours,
                *interval_hours,
            )?;
            "Memory consolidation and Reflection enabled within the existing scope, allowance and time window. No extra source access; one call only when meaningful work is pending. /pause pauses reasoning; /maintenance off disables maintenance.".to_owned()
        }
        Request::MaintenanceOff { scope: name } => {
            maintenance::configure(memory, &scope(name)?, 86400, false, timestamp())?;
            "Maintenance disabled for this scope. No project agent was stopped.".into()
        }
        Request::MaintenanceRevisit {
            scope: name,
            record_id,
            hours,
        } => schedule_reconsideration(memory, name, record_id, *hours)?,
        Request::Guidance {
            scope: name,
            record_id,
            enabled,
        } => set_guidance(memory, name, record_id, *enabled)?,
        _ => return Ok(None),
    };
    Ok(Some(json!({"notice":notice})))
}

fn enable_maintenance(
    memory: &mut Store,
    session: &crate::assistant_session::Session,
    control: &mut crate::assistant_control::Controller,
    name: &str,
    max_calls: u64,
    hours: u64,
    interval_hours: u64,
) -> Result<()> {
    let (selected, exe) = session
        .permission()
        .context("Enable the assistant provider and allowance first")?;
    if selected != scope(name)? {
        bail!("Maintenance must use the enabled provider scope");
    }
    let interval = interval_hours
        .checked_mul(3600)
        .context("Review interval too large")?;
    if !(1..=168).contains(&interval_hours) {
        bail!("Review interval must be 1–168 hours");
    }
    // The background cap is a subset of the existing total, not a new allowance.
    let policy = crate::assistant_policy::AssistantPolicy::open(
        memory.path().with_file_name("policy.sqlite"),
    )?;
    control.approve_maintenance(
        name,
        &exe,
        policy.config()?.max_total_calls,
        max_calls,
        hours,
    )?;
    crate::assistant_maintenance::configure(memory, &selected, interval, true, timestamp())
}

fn schedule_reconsideration(
    memory: &mut Store,
    name: &str,
    record_id: &str,
    hours: u64,
) -> Result<String> {
    let seconds = hours
        .checked_mul(3600)
        .and_then(|n| i64::try_from(n).ok())
        .context("Reconsideration interval too large")?;
    let now = timestamp();
    let id = crate::assistant_maintenance::schedule_revisit(
        memory,
        &scope(name)?,
        record_id,
        now.saturating_add(seconds),
        now,
    )?;
    Ok(format!(
        "Reconsideration saved ({id}). It becomes eligible when due, only within enabled maintenance permission and remaining allowance."
    ))
}

fn set_guidance(memory: &mut Store, name: &str, record_id: &str, enabled: bool) -> Result<String> {
    let record = memory
        .get_active(record_id)?
        .context("Guidance unavailable")?;
    if record.scope != scope(name)? {
        bail!("Guidance is outside this scope");
    }
    crate::assistant_guidance::set_enabled(memory, record_id, enabled, timestamp())?;
    Ok(if enabled {
        "Eligible guidance enabled; source validity still applies."
    } else {
        "Guidance disabled for subsequent turns."
    }
    .into())
}

struct PresentationDriver {
    scopes: std::collections::BTreeSet<String>,
    publisher: crate::assistant_presentation::Publisher,
    refreshed: std::time::Instant,
}

impl PresentationDriver {
    fn new(root: &Path) -> Result<Self> {
        Ok(Self {
            scopes: std::collections::BTreeSet::from(["personal".into()]),
            publisher: crate::assistant_presentation::Publisher::open(root)?,
            refreshed: std::time::Instant::now() - Duration::from_secs(1),
        })
    }
    fn observe(&mut self, request: &Request) {
        if let Request::Snapshot { scope: name } | Request::Brief { scope: name } = request {
            if self.scopes.len() < 32 && scope(name).is_ok() {
                self.scopes.insert(name.clone());
            }
        }
    }
    fn tick(
        &mut self,
        memory: &mut Store,
        session: &mut crate::assistant_session::Session,
        control: &crate::assistant_control::Controller,
    ) {
        if self.refreshed.elapsed() < Duration::from_secs(1) {
            return;
        }
        for name in &self.scopes {
            // Optional presentation must never terminate control on contention.
            let state = presentation_state(session, control, name);
            if let (Ok(state), Ok(scope)) = (state, scope(name)) {
                let _ = self.publisher.refresh(memory, &scope, state, timestamp());
            }
        }
        self.refreshed = std::time::Instant::now();
    }
}

fn restore_completed_recovery(
    root: &Path,
    session: &mut crate::assistant_session::Session,
    control: &mut crate::assistant_control::Controller,
    recovery: &Option<crate::assistant_recovery_service::RecoveryService>,
    restored: &mut Option<String>,
) -> Result<()> {
    let Some(state) = recovery.as_ref().map(|service| service.snapshot()) else {
        return Ok(());
    };
    if state.state == "completed" && restored.as_ref() != Some(&state.receipt_id) {
        session.set_workshop_root(root)?;
        control.recovered()?;
        *restored = Some(state.receipt_id);
    }
    Ok(())
}

fn handle_session(
    root: &Path,
    memory: &mut Store,
    session: &mut crate::assistant_session::Session,
    recovery: &mut Option<crate::assistant_recovery_service::RecoveryService>,
    control: &mut crate::assistant_control::Controller,
    mut request: Request,
) -> Result<Value> {
    if recovery.as_ref().is_some_and(|r| r.busy())
        && !matches!(request, Request::Snapshot { .. } | Request::Help)
    {
        bail!(
            "Pika is finishing recovery of its owned assistant jobs. New changes are blocked until cleanup finishes; project agents are untouched."
        );
    }
    if let Some(result) = handle_control(root, session, control, &request)? {
        return Ok(result);
    }
    if let Some(result) = handle_consultation_permissions(root, &request)? {
        return Ok(result);
    }
    if let Some(result) = handle_presentation(root, memory, session, control, &request)? {
        return Ok(result);
    }
    let raw_body = prepare_request(root, memory, session, control, &mut request)?;
    handle_prepared_session(root, memory, session, recovery, control, request, raw_body)
}

fn prepare_request(
    root: &Path,
    memory: &Store,
    session: &mut crate::assistant_session::Session,
    control: &mut crate::assistant_control::Controller,
    request: &mut Request,
) -> Result<Option<String>> {
    let raw_body = match &request {
        Request::Send { body, .. } | Request::Investigate { body, .. } => Some(body.clone()),
        _ => None,
    };
    preflight_request(control, request)?;
    let changes_context = match request {
        Request::Forget { record_id } => memory.get(record_id)?.is_some(),
        Request::FreshContext { request_id } => {
            crate::assistant_recovery::existing_receipt(root, request_id)?.is_none()
        }
        _ => false,
    };
    if changes_context {
        control.disable()?;
        control.tick(root, session)?;
    }
    Ok(raw_body)
}

fn refresh_presentation(
    root: &Path,
    memory: &mut Store,
    session: &mut crate::assistant_session::Session,
    control: &crate::assistant_control::Controller,
    name: &str,
) -> Result<crate::assistant_presentation::Presentation> {
    let state = presentation_state(session, control, name)?;
    crate::assistant_presentation::refresh(root, memory, &scope(name)?, state, timestamp())
}

fn presentation_state(
    session: &mut crate::assistant_session::Session,
    control: &crate::assistant_control::Controller,
    name: &str,
) -> Result<crate::assistant_presentation::ServiceState> {
    use crate::assistant_presentation::ServiceState;
    let status = session.snapshot(name);
    let controls = control.snapshot(name)?;
    let state = if controls["paused"] == true || controls["context_blocked"] == true {
        ServiceState::Paused
    } else {
        match status["state"].as_str() {
            Some("working" | "starting" | "cancelling" | "recovering") => {
                ServiceState::Investigating
            }
            Some("unavailable" | "stopped") => ServiceState::Unavailable,
            Some("ready" | "not_enabled") => ServiceState::Idle,
            _ => ServiceState::Unknown,
        }
    };
    Ok(state)
}

fn handle_presentation(
    root: &Path,
    memory: &mut Store,
    session: &mut crate::assistant_session::Session,
    control: &crate::assistant_control::Controller,
    request: &Request,
) -> Result<Option<Value>> {
    use crate::assistant_presentation as presentation;
    let value = match request {
        Request::Brief { scope: name } => {
            let view = refresh_presentation(root, memory, session, control, name)?;
            let mut lines = presentation_text(&view.brief);
            if view.has_more {
                lines.push("More saved updates remain after this page. /brief-seen acknowledges this page; /brief opens the next.".into());
            }
            lines.push("/brief-seen acknowledges only this displayed briefing; project unread state is unchanged.".into());
            json!({"presentation":view,"local_output":lines.join("\n"),"notice":"Saved briefing opened. Nothing acknowledged; no model call."})
        }
        Request::AcknowledgeBrief { cursor } => {
            presentation::acknowledge(root, memory, cursor)?;
            json!({"notice":"Briefing acknowledged. Newer changes and project unread state are untouched."})
        }
        Request::MemoryPage {
            scope: name,
            cursor,
            limit,
        } => {
            let page = presentation::memory_page(memory, &scope(name)?, cursor.as_ref(), *limit)?;
            let preview = record_previews(page.records.iter().map(|entry| &entry.record));
            json!({"memory_page":page,"local_output":format!("Memory page · bounded export\n{preview}\n/memory-next for continuation · /memory-record ID for full text")})
        }
        Request::MemoryRecord {
            scope: name,
            record_id,
            cursor,
        } => {
            let chunk = presentation::memory_record_chunk(
                memory,
                &scope(name)?,
                record_id,
                cursor.as_ref(),
            )?;
            json!({"memory_record":chunk,"local_output":serde_json::to_string_pretty(&chunk)?})
        }
        Request::MemorySearch { scope: name, query } => {
            let records = memory.search_bm25(&scope(name)?, query, 20)?;
            let preview = record_previews(records.iter());
            let coverage = "Up to 20 scoped active matches within 256 KiB of encoded records; this is a bounded lexical search, not a complete archive. /memory exports the archive in pages.";
            json!({"records":records,"coverage":coverage,"local_output":format!("{coverage}\n{preview}")})
        }
        _ => return Ok(None),
    };
    Ok(Some(value))
}

fn record_previews<'a>(
    records: impl IntoIterator<Item = &'a crate::assistant_memory::Record>,
) -> String {
    records
        .into_iter()
        .take(32)
        .map(|record| {
            format!(
                "{} · {:?} · {}{}",
                record.id,
                record.kind,
                crate::fleet::sanitize_terminal_text(&record.body)
                    .chars()
                    .take(160)
                    .collect::<String>(),
                if record.body.chars().count() > 160 {
                    "…"
                } else {
                    ""
                }
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn presentation_text(brief: &Value) -> Vec<String> {
    let mut lines = vec![
        brief["coverage"]
            .as_str()
            .unwrap_or("Bounded saved evidence")
            .to_owned(),
    ];
    append_brief_update_previews(brief, &mut lines);
    for (key, title) in [
        ("changes", "Changes · saved findings"),
        ("decisions", "Decisions"),
        ("commitments", "Commitments"),
        ("uncertainty", "Open questions / proposals"),
        ("instructions", "Current instructions"),
    ] {
        if let Some(entries) = brief[key].as_array().filter(|entries| !entries.is_empty()) {
            lines.push(format!("\n{title}"));
            for entry in entries {
                lines.push(format!(
                    "{} · {}",
                    entry["id"].as_str().unwrap_or(""),
                    entry["text"].as_str().unwrap_or("")
                ));
            }
        }
    }
    if brief["presentation_limited"] == true {
        lines.push("Some standing context is abbreviated or omitted in this bounded view.".into());
    }
    lines.push("Update previews are abbreviated. /memory-record ID shows the full saved words; /memory exports all scoped records in pages.".into());
    if let Some(notice) = brief["notice"].as_str() {
        lines.push(notice.into());
    }
    lines
}

fn append_brief_update_previews(brief: &Value, lines: &mut Vec<String>) {
    if let Some(updates) = brief["updates"].as_array().filter(|rows| !rows.is_empty()) {
        lines.push("\nUpdates covered by this page acknowledgement".into());
        for update in updates {
            let preview =
                crate::fleet::sanitize_terminal_lines(update["text"].as_str().unwrap_or(""));
            let preview = if preview.trim().is_empty() {
                "No printable preview; inspect this saved record for its full content"
            } else {
                &preview
            };
            lines.push(format!(
                "{} · {preview}",
                update["id"].as_str().unwrap_or("")
            ));
        }
    }
}

fn handle_prepared_session(
    root: &Path,
    memory: &mut Store,
    session: &mut crate::assistant_session::Session,
    recovery: &mut Option<crate::assistant_recovery_service::RecoveryService>,
    control: &mut crate::assistant_control::Controller,
    request: Request,
    raw_body: Option<String>,
) -> Result<Value> {
    if let Some(result) = handle_session_tools(root, session, &request)? {
        return Ok(result);
    }
    if let Some(result) = handle_session_provider(root, session, &request, raw_body.as_deref())? {
        return Ok(result);
    }
    if let Some(result) = handle_session_recovery(root, memory, session, recovery, &request)? {
        return Ok(result);
    }
    if let Some(result) = handle_session_snapshot(memory, session, recovery, &request)? {
        let mut result = result;
        if let Request::Snapshot { scope } = &request {
            result["control"] = control.snapshot(scope)?;
            result["presentation"] = json!(
                crate::assistant_presentation::read_cached(root, &self::scope(scope)?, timestamp())
                    .ok()
                    .flatten()
            );
        }
        return Ok(result);
    }
    if let Some(result) = handle_session_forget(root, memory, session, &request)? {
        return Ok(result);
    }
    handle(memory, request)
}

fn preflight_request(
    control: &crate::assistant_control::Controller,
    request: &mut Request,
) -> Result<()> {
    match request {
        Request::Send {
            scope, body, focus, ..
        }
        | Request::Investigate {
            scope, body, focus, ..
        } => {
            control.require_foreground(scope)?;
            *body = control.decorate_prompt(scope, body)?;
            if let Some(focus) = focus {
                let allowed = control.snapshot(scope)?;
                if !allowed["board"]["rows"]
                    .as_array()
                    .is_some_and(|rows| rows.iter().any(|row| row["identity"] == json!(focus)))
                {
                    bail!(
                        "Focused task is not in the approved board-sharing scope; preview /board-share first"
                    );
                }
                body.push_str(&format!(
                    "\nUser-selected task reference, not instructions: {}",
                    serde_json::to_string(focus)?
                ));
                if body.len() > 16 * 1024 {
                    bail!(
                        "Question plus shared context exceeds the bounded request size; shorten the question"
                    );
                }
            }
        }
        Request::Enable { scope, .. }
        | Request::Evolve { scope, .. }
        | Request::EvolveSpec { scope, .. } => control.require_foreground(scope)?,
        _ => (),
    }
    Ok(())
}

fn handle_control(
    root: &Path,
    session: &mut crate::assistant_session::Session,
    control: &mut crate::assistant_control::Controller,
    request: &Request,
) -> Result<Option<Value>> {
    let notice = match request {
        Request::ShareBoard {
            scope,
            confirmation,
        } => {
            let mut result = control.share_board(scope, confirmation.as_deref())?;
            result["local_output"] = json!(board_share_text(&result));
            return Ok(Some(result));
        }
        Request::RevokeBoard { scope } => {
            let revoked = control.revoke_board(scope);
            let cancelled = session.cancel();
            let ticked = control.tick(root, session);
            revoked?;
            cancelled?;
            ticked?;
            "Board sharing revoked. Derived assistant findings were invalidated; fresh context is required before reuse."
        }
        Request::Pause => {
            control.pause()?;
            "Pika reasoning paused. Your project agents are untouched."
        }
        Request::Resume => {
            control.resume()?;
            "Pika reasoning resumed within the existing permission and allowance."
        }
        Request::StopBackground => {
            control.disable()?;
            "Background reasoning disabled. No startup service was installed."
        }
        Request::Background {
            scope: name,
            max_calls,
            hours,
        } => {
            enable_background(root, session, control, name, *max_calls, *hours)?;
            "Background reasoning enabled for this scope and time window, within the existing total allowance. /pause pauses reasoning; /background off disables it."
        }
        _ => return Ok(None),
    };
    control.tick(root, session)?;
    Ok(Some(json!({"notice":notice})))
}

fn enable_background(
    root: &Path,
    session: &crate::assistant_session::Session,
    control: &mut crate::assistant_control::Controller,
    name: &str,
    max_calls: u64,
    hours: u64,
) -> Result<()> {
    let (selected, executable) = session
        .permission()
        .context("Enable Pika's foreground provider and allowance first")?;
    if selected != scope(name)? {
        bail!("Background scope must match the enabled provider exactly");
    }
    let allowance = crate::assistant_policy::AssistantPolicy::open(root.join("policy.sqlite"))?
        .config()?
        .max_total_calls;
    control.approve_background(name, &executable, allowance, max_calls, hours)
}

fn board_share_text(result: &Value) -> String {
    if result["approved"] == true {
        return "Board sharing enabled for the approved exact identities. New tasks remain excluded; /board-share off revokes sharing.".into();
    }
    let mut lines = vec!["Share with Pika's Codex model · names and status only".to_owned()];
    for row in result["rows"].as_array().into_iter().flatten() {
        let id = &row["identity"];
        lines.push(format!(
            "{} · {} / {} / {}",
            row["name"].as_str().unwrap_or("Unnamed"),
            id["node"].as_str().unwrap_or("?"),
            id["provider"].as_str().unwrap_or("?"),
            id["conversation"].as_str().unwrap_or("?")
        ));
    }
    if result["partial"] == true {
        lines.push("Partial coverage; only the identities above are included.".into());
    }
    lines.push("No transcripts, cards, paths, or permission to act on your tasks.".into());
    lines.push(format!(
        "To approve this exact preview: /board-share {}",
        result["confirmation"].as_str().unwrap_or("unavailable")
    ));
    lines.join("\n")
}

fn handle_session_tools(
    root: &Path,
    session: &mut crate::assistant_session::Session,
    request: &Request,
) -> Result<Option<Value>> {
    if let Some(value) = handle_tool_management(root, session, request)? {
        return Ok(Some(value));
    }
    match request {
        Request::EvolveSpec {
            scope,
            request_id,
            spec,
        } => {
            session.evolve_spec(root, scope, request_id, spec)?;
            Ok(Some(
                json!({"accepted":true,"notice":"Authoring a pure tool against your protected contrasting cases. Exact-version approval still required."}),
            ))
        }
        Request::Evolve { scope, request_id } => {
            session.evolve(root, scope, request_id)?;
            Ok(Some(
                json!({"accepted":true,"notice":"Creating a scoped pure-data tool. Evaluation follows; activation still needs your exact approval."}),
            ))
        }
        Request::ApproveTool { scope, hash } => {
            let grant = session.approve_evolution(scope, hash)?;
            Ok(Some(
                json!({"grant_id":grant,"notice":format!("Exact tool activated without time expiry. Revocation: /revoke {grant}")}),
            ))
        }
        Request::RevokeTool { scope, grant_id } => {
            session.revoke_evolution(scope, grant_id)?;
            Ok(Some(json!({"notice":"Tool grant revoked."})))
        }
        Request::RollbackTool { scope, name, hash } => {
            let grant = session.rollback_evolution(scope, name, hash)?;
            Ok(Some(
                json!({"notice":format!("Rolled back to the approved exact version without time expiry. /revoke {grant}"),"grant_id":grant}),
            ))
        }
        Request::InvokeTool {
            scope,
            name,
            inputs,
        } => {
            let output = session.invoke_evolution(scope, name, inputs)?;
            Ok(Some(
                json!({"notice":format!("Tool result · {}{}",output.value,output.provenance_warning.as_ref().map(|warning|format!("\n{warning}")).unwrap_or_default()),"tool_result":output.value,"provenance_warning":output.provenance_warning}),
            ))
        }
        _ => Ok(None),
    }
}

fn handle_tool_management(
    root: &Path,
    session: &mut crate::assistant_session::Session,
    request: &Request,
) -> Result<Option<Value>> {
    let result = match request {
        Request::ToolCatalog { scope, hash } => session.tool_catalog(scope, hash.as_deref())?,
        Request::AssessTool {
            scope,
            hash,
            outcome,
            evidence,
            rollback,
        } => session.assess_evolution(scope, hash, outcome, evidence, rollback.as_deref())?,
        Request::Improvement {
            scope,
            correction_id,
        } => session.prepare_improvement(root, scope, correction_id)?,
        _ => return Ok(None),
    };
    Ok(Some(
        json!({"local_output":serde_json::to_string_pretty(&result)?,"details":result}),
    ))
}

fn handle_session_provider(
    root: &Path,
    session: &mut crate::assistant_session::Session,
    request: &Request,
    raw_body: Option<&str>,
) -> Result<Option<Value>> {
    match request {
        Request::Investigate {
            scope,
            request_id,
            body,
            timestamp,
            ..
        } => {
            let status = session.snapshot(scope);
            if session.busy() || status["state"] != "ready" || status["provider"] != "codex" {
                bail!("Wait for the foreground provider to be ready; no investigation was sent");
            }
            session.begin_user(scope, request_id, raw_body.context("Missing original user question")?, &format!("Investigate the material evidence gap in this question; prefer existing evidence and use bounded helpers only if needed.\n{body}"), *timestamp)?;
            Ok(Some(
                json!({"accepted":true,"notice":"One main turn first; zero to two helpers only for missing evidence. At most four calls including synthesis, within your existing allowance."}),
            ))
        }
        Request::Enable {
            scope: name,
            executable,
            max_calls,
            restore,
        } => {
            if *restore && resume_existing(session, name, executable)? {
                return Ok(Some(session.snapshot(name)));
            }
            session.enable(root, name, executable.clone(), *max_calls)?;
            Ok(Some(
                json!({"state":"starting","notice":"Checking isolated provider. No background calls; monetary cost unknown."}),
            ))
        }
        Request::Send {
            scope: name,
            request_id,
            body,
            timestamp,
            ..
        } => {
            scope(name)?;
            session.begin_user(
                name,
                request_id,
                raw_body.context("Missing original user question")?,
                body,
                *timestamp,
            )?;
            Ok(Some(
                json!({"accepted":true,"request_id":request_id,"notice":"Accepted once; do not resend if delivery becomes unknown."}),
            ))
        }
        _ => Ok(None),
    }
}

fn resume_existing(
    session: &crate::assistant_session::Session,
    name: &str,
    executable: &Path,
) -> Result<bool> {
    let selected = scope(name)?;
    let Some((active_scope, active_executable)) = session.permission() else {
        return Ok(false);
    };
    if selected != active_scope || executable != active_executable {
        bail!("Another provider or scope already owns this assistant; it was not replaced");
    }
    Ok(true)
}

fn handle_session_recovery(
    root: &Path,
    memory: &mut Store,
    session: &mut crate::assistant_session::Session,
    recovery: &mut Option<crate::assistant_recovery_service::RecoveryService>,
    request: &Request,
) -> Result<Option<Value>> {
    match request {
        Request::FreshContext { request_id } => {
            if let Some(receipt) = crate::assistant_recovery::existing_receipt(root, request_id)? {
                return Ok(Some(
                    json!({"recovery_id":receipt.receipt_id,"notice":"This recovery was already recorded. Current work was not touched."}),
                ));
            }
            let old_session = std::mem::replace(session, crate::assistant_session::Session::new());
            *recovery = Some(crate::assistant_recovery_service::RecoveryService::start(
                root.to_path_buf(),
                request_id.clone(),
                memory.forget_epoch()?,
                move || {
                    drop(old_session);
                    Ok(())
                },
            )?);
            Ok(Some(
                json!({"notice":"Recovery started. Pika is stopping only its owned assistant jobs; your board and project agents remain available.","recovery_id":request_id,"local_output":"Recovery pending. No old request will be replayed or refunded. Saved memory stays; re-enable the provider explicitly after completion."}),
            ))
        }
        Request::Cancel => {
            session.cancel()?;
            Ok(Some(
                json!({"notice":"Cancellation requested; delivery remains uncertain until verified."}),
            ))
        }
        _ => Ok(None),
    }
}

fn handle_session_snapshot(
    memory: &mut Store,
    session: &mut crate::assistant_session::Session,
    recovery: &mut Option<crate::assistant_recovery_service::RecoveryService>,
    request: &Request,
) -> Result<Option<Value>> {
    let Request::Snapshot { scope: name } = request else {
        return Ok(None);
    };
    let mut snapshot = handle(
        memory,
        Request::Snapshot {
            scope: name.clone(),
        },
    )?;
    for (key, value) in session.snapshot(name).as_object().unwrap() {
        snapshot[key] = value.clone();
    }
    if let Some(recovery) = recovery.as_ref() {
        let state = recovery.snapshot();
        snapshot["recovery"] = serde_json::to_value(&state)?;
        if recovery.busy() {
            snapshot["state"] = json!("recovering");
        }
    }
    refresh_provider_notice(&mut snapshot);
    Ok(Some(snapshot))
}

fn handle_session_forget(
    root: &Path,
    memory: &mut Store,
    session: &mut crate::assistant_session::Session,
    request: &Request,
) -> Result<Option<Value>> {
    let Request::Forget { record_id } = request else {
        return Ok(None);
    };
    if memory.get(record_id)?.is_none() {
        bail!("Memory record not found; nothing was changed");
    }
    session.forget()?;
    let mut result = handle(
        memory,
        Request::Forget {
            record_id: record_id.clone(),
        },
    )?;
    crate::assistant_retention::cleanup(root, memory.forget_epoch()?)?;
    result["notice"] = json!(
        "Forgotten from active Pika memory. Derived tool experiments and cached replies were cleared conservatively. Provider logs and external backups are not deleted; provider continuation is blocked."
    );
    Ok(Some(result))
}
fn refresh_provider_notice(snapshot: &mut Value) {
    if snapshot["provider"] != "codex" {
        return;
    }
    snapshot["notice"] = json!(match snapshot["state"].as_str() {
        Some("starting") => "Checking the isolated provider. No new request has been sent.",
        Some("ready") => "Foreground provider ready. Requests use your explicit call allowance.",
        Some("working") => "Request in progress within the approved allowance. No automatic retry.",
        Some("recovering" | "cancelling") =>
            "Stopping owned work; uncertain requests are not replayed.",
        _ =>
            "Provider unavailable. Saved memory remains; failed requests are not retried automatically.",
    });
}

fn handle(memory: &mut Store, request: Request) -> Result<Value> {
    if let Some(result) = handle_commitment(memory, &request)? {
        return Ok(result);
    }
    if let Some(result) = handle_decision_revision(memory, &request)? {
        return Ok(result);
    }
    if is_display_request(&request) {
        return handle_display(memory, request);
    }
    if is_memory_write(&request) {
        return handle_memory_write(memory, request);
    }
    bail!("Provider operation requires the foreground host")
}

fn handle_commitment(memory: &mut Store, request: &Request) -> Result<Option<Value>> {
    use crate::assistant_decisions as decisions;
    match request {
        Request::CommitmentDue {
            scope: name,
            request_id,
            record_id,
            due_at,
        } => {
            let selected = scope(name)?;
            let now = timestamp();
            let record = decisions::set_commitment_due(
                memory,
                request_id,
                Origin::Human,
                selected.clone(),
                record_id,
                *due_at,
                now,
            )?;
            Ok(Some(
                json!({"saved":record.id,"notice":"Due condition saved for the brief and future context, independently of Reflection. No model call or action is scheduled. Use /revisit for an explicit reconsideration opportunity."}),
            ))
        }
        Request::CommitmentDone {
            scope: name,
            request_id,
            record_id,
            statement,
        } => {
            let record = decisions::report_commitment_completion(
                memory,
                request_id,
                Origin::Human,
                scope(name)?,
                record_id,
                statement,
                timestamp(),
            )?;
            Ok(Some(
                json!({"saved":record.id,"notice":"Your completion report is saved with attribution. It is not an independently verified execution receipt."}),
            ))
        }
        _ => Ok(None),
    }
}

fn handle_decision_revision(memory: &mut Store, request: &Request) -> Result<Option<Value>> {
    let record = match request {
        Request::DecisionState {
            scope: name,
            request_id,
            record_id,
            state,
            timestamp,
        } => crate::assistant_decisions::transition(
            memory,
            request_id,
            Origin::Human,
            scope(name)?,
            record_id,
            *state,
            *timestamp,
        )?,
        Request::DecisionRevision {
            scope: name,
            request_id,
            record_id,
            decision,
            timestamp,
        } => crate::assistant_decisions::revise(
            memory,
            request_id,
            Origin::Human,
            scope(name)?,
            record_id,
            decision.clone(),
            *timestamp,
        )?,
        _ => return Ok(None),
    };
    Ok(Some(
        json!({"saved":record.id,"notice":"Decision history updated in its exact scope. This records your choice; it does not execute or grant authority."}),
    ))
}

fn is_display_request(request: &Request) -> bool {
    matches!(
        request,
        Request::Help
            | Request::Brief { .. }
            | Request::Recall { .. }
            | Request::Snapshot { .. }
            | Request::ProfileView { .. }
    )
}

fn is_memory_write(request: &Request) -> bool {
    matches!(
        request,
        Request::Explain { .. }
            | Request::Correct { .. }
            | Request::Save { .. }
            | Request::Forget { .. }
    )
}

fn handle_display(memory: &mut Store, request: Request) -> Result<Value> {
    match request {
        Request::Help => Ok(json!({"local_output":HELP})),
        Request::Brief { scope: name } => handle_brief(memory, &name),
        Request::Recall {
            scope: name,
            record_id,
        } => handle_recall(memory, &name, &record_id),
        Request::Snapshot { scope: name } => handle_snapshot(memory, &name),
        Request::ProfileView { scope: name, view } => Ok(json!({
            "local_output":crate::assistant_profile_views::render(memory, &scope(&name)?, view)?,
            "notice":"Read-only profile view generated from current Pika state. No file, project action, or model call."
        })),
        _ => bail!("Provider operation requires the foreground host"),
    }
}

fn handle_brief(memory: &mut Store, name: &str) -> Result<Value> {
    let selected = scope(name)?;
    let brief = crate::assistant_briefing::load(memory, &selected, 0)?;
    let mut text = vec![
        "Saved evidence · no model call".into(),
        brief.coverage.clone(),
    ];
    append_brief_sections(&brief, &mut text);
    Ok(json!({"brief":brief,"local_output":text.join("\n")}))
}

fn append_brief_sections(brief: &crate::assistant_briefing::Brief, text: &mut Vec<String>) {
    for (title, entries) in [
        ("Changes (unverified findings)", &brief.changes),
        ("Decisions", &brief.decisions),
        ("Commitments", &brief.commitments),
        ("Open questions / proposals", &brief.uncertainty),
        ("Current instructions", &brief.instructions),
    ] {
        text.push(format!("\n{title}"));
        if entries.is_empty() {
            text.push("None recorded in this scope.".into());
        }
        for entry in entries.iter().take(12) {
            let state = entry
                .decision_state
                .map(|state| format!("{state:?} · "))
                .unwrap_or_default();
            text.push(format!(
                "{} · {state}{}",
                entry.id,
                crate::assistant_briefing::readable_decision(&entry.text)
            ));
        }
    }
}

fn handle_recall(memory: &mut Store, name: &str, record_id: &str) -> Result<Value> {
    let record = memory
        .get(record_id)?
        .ok_or_else(|| anyhow::anyhow!("Decision not found"))?;
    let recalled =
        crate::assistant_briefing::recall(&record, &scope(name)?).map_err(anyhow::Error::msg)?;
    Ok(
        json!({"recall":recalled,"local_output":format!("{}\n{}",crate::assistant_briefing::readable_decision(&recalled.original_words),recalled.caveat)}),
    )
}

fn handle_snapshot(memory: &mut Store, name: &str) -> Result<Value> {
    let selected = scope(name)?;
    let records = record_preview_values(memory.recent(&selected, 32)?)?;
    let brief = crate::assistant_briefing::load(memory, &selected, 0)?;
    let brief = bounded_brief(serde_json::to_value(brief)?)?;
    Ok(
        json!({"profile_id":memory.profile_id(),"memory_epoch":memory.forget_epoch()?,"scope":name,"state":"not_enabled","provider":"none","background_calls":0,"records":records,"brief":brief,"notice":"Local memory ready. A provider and spending allowance have not been enabled. Drafts remain unsent."}),
    )
}

fn record_preview_values(records: Vec<crate::assistant_memory::Record>) -> Result<Vec<Value>> {
    let mut values = Vec::new();
    let mut remaining = 128 * 1024usize;
    for mut record in records {
        record.body = record.body.chars().take(2048).collect();
        record.provenance = record.provenance.chars().take(1024).collect();
        let mut value = serde_json::to_value(record)?;
        value["preview"] = json!(true);
        let cost = serde_json::to_vec(&value)?.len();
        if cost > remaining {
            break;
        }
        remaining -= cost;
        values.push(value);
    }
    Ok(values)
}

fn bounded_brief(mut brief: Value) -> Result<Value> {
    let mut remaining = 128 * 1024usize;
    for key in [
        "instructions",
        "decisions",
        "changes",
        "uncertainty",
        "commitments",
    ] {
        if let Some(entries) = brief[key].as_array_mut() {
            for entry in entries.iter_mut() {
                if let Some(text) = entry["text"].as_str() {
                    entry["text"] = json!(text.chars().take(1536).collect::<String>());
                }
            }
            let mut kept = Vec::new();
            for entry in entries.drain(..) {
                let bytes = serde_json::to_vec(&entry)?.len();
                if bytes > remaining {
                    break;
                }
                remaining -= bytes;
                kept.push(entry);
            }
            *entries = kept;
        }
    }
    brief["preview_notice"] = json!(
        "Bounded preview; /memory-record ID returns full saved words. /brief shows changes since your last acknowledgement."
    );
    Ok(brief)
}

fn handle_memory_write(memory: &mut Store, request: Request) -> Result<Value> {
    match request {
        Request::Explain {
            scope: name,
            record_id,
            reply,
        } => handle_explain(memory, &name, record_id, reply),
        Request::Correct {
            scope: name,
            request_id,
            record_id,
            body,
            timestamp,
        } => handle_correct(memory, &name, &request_id, record_id, body, timestamp),
        Request::Save {
            request_id,
            scope: name,
            kind,
            body,
            timestamp,
        } => handle_save(memory, &request_id, &name, kind, body, timestamp),
        Request::Forget { record_id } => handle_forget(memory, &record_id),
        _ => bail!("Provider operation requires the foreground host"),
    }
}

fn handle_explain(
    memory: &mut Store,
    name: &str,
    record_id: String,
    reply: crate::assistant_briefing::ExplainBack,
) -> Result<Value> {
    let selected = scope(name)?;
    let record = memory
        .get(&record_id)?
        .ok_or_else(|| anyhow::anyhow!("Decision not found"))?;
    let recalled =
        crate::assistant_briefing::recall(&record, &selected).map_err(anyhow::Error::msg)?;
    let gap = recalled
        .structured
        .as_ref()
        .and_then(|decision| crate::assistant_briefing::explanation_gap(decision, &reply));
    memory.append(NewRecord {
        kind: RecordKind::GraspInteraction,
        origin: Origin::Human,
        scope: selected,
        body: serde_json::to_string(&reply)?,
        provenance: "user explain-back; completeness prompt only, not semantic grading".into(),
        timestamp: SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64,
        supersedes: None,
        dependencies: vec![record_id],
        decision_state: None,
        protected_policy: false,
    })?;
    Ok(
        json!({"local_output":format!("Your explanation is saved. {}\nCompare with your original reasoning:\n{}\nThis checks missing fields, not correctness or cognitive ability. /skip or /defer is always okay.",gap.unwrap_or("No required explanation field is missing; this is not a semantic correctness judgment."),crate::assistant_briefing::readable_decision(&record.body))}),
    )
}

fn handle_correct(
    memory: &mut Store,
    name: &str,
    request_id: &str,
    record_id: String,
    body: String,
    timestamp: i64,
) -> Result<Value> {
    let corrected = crate::assistant_decisions::correct(
        memory,
        request_id,
        Origin::Human,
        scope(name)?,
        &record_id,
        body,
        timestamp,
    )?;
    let next = if corrected.kind == RecordKind::Correction {
        format!(
            " /evolve {} prepares an improvement proposal without a model call. Nothing activates automatically.",
            corrected.id
        )
    } else {
        String::new()
    };
    Ok(
        json!({"saved":corrected.id,"notice":format!("Correction saved; earlier wording remains dated history.{next}")}),
    )
}

fn handle_save(
    memory: &mut Store,
    request_id: &str,
    name: &str,
    kind: SaveKind,
    body: String,
    timestamp: i64,
) -> Result<Value> {
    if body.trim().is_empty() || body.len() > 16 * 1024 {
        bail!("Message must contain 1–16384 bytes");
    }
    let (record_kind, decision_state) = save_record_kind(kind);
    let record = memory.append_idempotent(
        request_id,
        NewRecord {
            kind: record_kind,
            origin: Origin::Human,
            scope: scope(name)?,
            body,
            provenance: "explicit local user input".into(),
            timestamp,
            supersedes: None,
            dependencies: vec![],
            decision_state,
            protected_policy: false,
        },
    )?;
    Ok(json!({"saved":record.id,"profile_id":memory.profile_id(),"sent_to_provider":false}))
}

fn save_record_kind(kind: SaveKind) -> (RecordKind, Option<DecisionState>) {
    match kind {
        SaveKind::Draft => (RecordKind::Draft, None),
        SaveKind::Instruction => (RecordKind::UserInstruction, None),
        SaveKind::Decision => (RecordKind::Decision, Some(DecisionState::Accepted)),
        SaveKind::Proposal => (RecordKind::Decision, Some(DecisionState::Proposed)),
        SaveKind::Grasp => (RecordKind::GraspInteraction, None),
    }
}

fn handle_forget(memory: &mut Store, record_id: &str) -> Result<Value> {
    let count = memory.forget(record_id)?;
    Ok(
        json!({"forgotten":count,"notice":"Removed from active Pika memory. External backups and provider retention are not affected."}),
    )
}

pub(crate) fn run(mut args: Args) -> Result<i32> {
    apply_startup(&mut args)?;
    normalize_args(&mut args);
    scope(&args.scope)?;
    let mut client = attach_view(&args)?;
    let enabled = prepare_enable(&mut client, &mut args)?;
    if args.set_default {
        save_startup(&args)?;
    }
    if let Some(result) = enabled {
        return Ok(result);
    }
    let command = save_command(&args);
    if let Some((kind, body)) = command {
        return save_args(&mut client, &args.scope, args.json, kind, body);
    }
    finish_entry(&mut client, args)
}

fn finish_entry(client: &mut Client, args: Args) -> Result<i32> {
    if args.json || !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        println!("{}", send(client, Request::Snapshot { scope: args.scope })?);
        return Ok(0);
    }
    entry_view(
        client,
        &args.scope,
        args.focus,
        args.board,
        args.startup_notice,
        args.view_memory,
    )?;
    Ok(0)
}

fn prepare_enable(client: &mut Client, args: &mut Args) -> Result<Option<i32>> {
    match enable_from_args(client, args) {
        Err(error) if args.restore_startup => {
            args.startup_notice = Some(format!(
                "Provider not started: {error}. Local memory and /help remain available."
            ));
            Ok(None)
        }
        result => result,
    }
}

fn apply_startup(args: &mut Args) -> Result<()> {
    if args.profile_root.is_none() {
        let selection = crate::assistant_startup::load(&crate::assistant_startup::path()?)?;
        apply_selection(
            args,
            selection,
            io::stdin().is_terminal() && io::stdout().is_terminal(),
        );
    }
    Ok(())
}

fn apply_selection(
    args: &mut Args,
    selection: Option<crate::assistant_startup::Selection>,
    interactive: bool,
) {
    let Some(selection) = selection else {
        return;
    };
    args.profile_root = Some(selection.profile_root);
    args.expected_profile_id = Some(selection.profile_id);
    if args.scope.is_empty() {
        args.scope = selection.scope.clone();
    }
    if interactive
        && !args.json
        && !args.offline
        && save_command(args).is_none()
        && args.enable_codex.is_none()
        && args.scope == selection.scope
    {
        args.enable_codex = Some(selection.executable);
        args.max_calls = Some(selection.max_calls);
        args.restore_startup = true;
    }
}

fn save_startup(args: &Args) -> Result<()> {
    crate::assistant_startup::save(
        &crate::assistant_startup::path()?,
        &crate::assistant_startup::Selection {
            profile_root: args
                .profile_root
                .clone()
                .context("Choose an existing assistant profile")?,
            profile_id: args
                .expected_profile_id
                .clone()
                .context("Missing assistant identity")?,
            scope: args.scope.clone(),
            executable: args
                .enable_codex
                .clone()
                .context("Choose the assistant provider")?,
            max_calls: if args.no_call_limit {
                crate::assistant_policy::NO_CALL_LIMIT
            } else {
                args.max_calls.unwrap_or(0)
            },
        },
    )
}

fn attach_view(args: &Args) -> Result<Client> {
    if let (Some(root), Some(profile)) = (&args.profile_root, &args.expected_profile_id) {
        let mut client = Client::attach_existing_profile(root, profile)?;
        let snapshot = send(
            &mut client,
            Request::Snapshot {
                scope: args.scope.clone(),
            },
        )?;
        if snapshot["profile_id"].as_str() != Some(profile.as_str()) {
            bail!("Assistant profile changed during attachment; no action was submitted");
        }
        return Ok(client);
    }
    let root = crate::paths::Paths::discover()?.state_dir.join("assistant");
    Client::attach(&root)
}

pub(crate) fn interactive_view(
    client: &mut Client,
    scope: &str,
    focus: Option<Focus>,
) -> Result<()> {
    entry_view(client, scope, focus, false, None, None)
}

fn entry_view(
    client: &mut Client,
    scope: &str,
    focus: Option<Focus>,
    board: bool,
    notice: Option<String>,
    memory: Option<std::sync::Arc<std::sync::Mutex<ViewMemory>>>,
) -> Result<()> {
    let feed = match crate::activity_feed::current() {
        Some(context) => context,
        None => crate::activity_feed::Context::Source(crate::activity_observer::start(
            &crate::core::Pika::discover()?,
        )?),
    };
    crate::activity_feed::with(Some(feed), || {
        run_view(client, scope, focus, board, notice, memory)
    })
}

fn normalize_args(args: &mut Args) {
    if args.scope.is_empty() {
        args.scope = "personal".into();
    }
}

fn save_command(args: &Args) -> Option<(SaveKind, &str)> {
    args.remember
        .as_deref()
        .map(|body| (SaveKind::Instruction, body))
        .or_else(|| {
            args.decision
                .as_deref()
                .map(|body| (SaveKind::Decision, body))
        })
}

fn enable_from_args(client: &mut Client, args: &Args) -> Result<Option<i32>> {
    let Some(executable) = args.enable_codex.clone() else {
        return Ok(None);
    };
    let reply = send(
        client,
        Request::Enable {
            scope: args.scope.clone(),
            executable,
            restore: args.restore_startup,
            max_calls: if args.no_call_limit {
                crate::assistant_policy::NO_CALL_LIMIT
            } else {
                args.max_calls.unwrap_or(0)
            },
        },
    )?;
    if args.json || !io::stdin().is_terminal() {
        println!("{reply}");
        return Ok(Some(0));
    }
    Ok(None)
}

fn save_args(
    client: &mut Client,
    name: &str,
    json_output: bool,
    kind: SaveKind,
    body: &str,
) -> Result<i32> {
    let reply = send(client, save(name, kind, body))?;
    if json_output {
        println!("{}", serde_json::to_string(&reply)?);
    } else {
        println!(
            "Saved in Pika memory · scope {} · no provider call",
            crate::fleet::sanitize_terminal_text(name)
        );
    }
    Ok(0)
}

fn send(client: &mut Client, request: Request) -> Result<Value> {
    client.request(serde_json::to_value(request)?)
}
fn save(scope: &str, kind: SaveKind, body: &str) -> Request {
    Request::Save {
        request_id: uuid::Uuid::new_v4().to_string(),
        scope: scope.into(),
        kind,
        body: body.into(),
        timestamp: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64,
    }
}

pub(crate) fn timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

struct Screen;
impl Screen {
    fn enter() -> Result<Self> {
        terminal::enable_raw_mode()?;
        let screen = Self;
        execute!(io::stdout(), EnterAlternateScreen, EnableMouseCapture, Hide)?;
        Ok(screen)
    }
}
impl Drop for Screen {
    fn drop(&mut self) {
        let _ = execute!(
            io::stdout(),
            EndSynchronizedUpdate,
            ResetColor,
            Show,
            DisableMouseCapture,
            LeaveAlternateScreen
        );
        let _ = terminal::disable_raw_mode();
    }
}

fn run_view(
    client: &mut Client,
    scope: &str,
    focus: Option<Focus>,
    board: bool,
    notice: Option<String>,
    memory: Option<std::sync::Arc<std::sync::Mutex<ViewMemory>>>,
) -> Result<()> {
    let _screen = Screen::enter()?;
    let mut view = AssistantView::new(client, scope)?;
    view.focus = focus;
    view.board = board;
    if let Some(memory) = &memory {
        memory
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .restore(&mut view);
    }
    if let Some(notice) = notice {
        view.notice = notice;
    }
    let outcome = (|| loop {
        view.paint_if_dirty()?;
        if !event::poll(Duration::from_millis(100))? {
            view.refresh_if_due()?;
            continue;
        }
        if !view.handle_event(event::read()?)? {
            return Ok(());
        }
    })();
    if let Some(memory) = memory {
        memory
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .save(&view);
    }
    outcome
}

/// Ephemeral navigation state owned by the board, never a sent message or a
/// second memory store. Profile/scope/forget generation fence restored output.
#[derive(Debug, Default)]
pub(crate) struct ViewMemory {
    profile: String,
    scope: String,
    epoch: Option<u64>,
    draft: String,
    scroll: usize,
    output: String,
    brief: Option<crate::assistant_presentation::BriefCursor>,
    memory_next: Option<crate::assistant_presentation::MemoryCursor>,
    record_next: Option<crate::assistant_presentation::RecordChunkCursor>,
}

impl ViewMemory {
    fn restore(&self, view: &mut AssistantView<'_>) {
        if self.profile.is_empty()
            || view.snapshot["profile_id"].as_str() != Some(self.profile.as_str())
            || self.scope != view.scope
        {
            return;
        }
        view.draft.clone_from(&self.draft);
        if self.epoch.is_some()
            && self.epoch == view.snapshot["memory_epoch"].as_u64()
            && self.epoch == view.last_brief.as_ref().map(|cursor| cursor.epoch)
        {
            view.scroll = self.scroll;
            view.local_output.clone_from(&self.output);
            view.last_brief.clone_from(&self.brief);
            view.memory_next.clone_from(&self.memory_next);
            view.record_next.clone_from(&self.record_next);
        }
    }

    fn save(&mut self, view: &AssistantView<'_>) {
        self.profile = view.snapshot["profile_id"]
            .as_str()
            .unwrap_or_default()
            .into();
        self.scope = view.scope.into();
        self.epoch = view.last_memory_epoch;
        self.draft.clone_from(&view.draft);
        self.scroll = view.scroll;
        self.output.clone_from(&view.local_output);
        self.brief.clone_from(&view.last_brief);
        self.memory_next.clone_from(&view.memory_next);
        self.record_next.clone_from(&view.record_next);
    }
}

struct AssistantView<'a> {
    board: bool,
    last_brief: Option<crate::assistant_presentation::BriefCursor>,
    memory_next: Option<crate::assistant_presentation::MemoryCursor>,
    record_next: Option<crate::assistant_presentation::RecordChunkCursor>,
    focus: Option<Focus>,
    client: &'a mut Client,
    scope: &'a str,
    presenter: crate::monitor::FramePresenter,
    draft: String,
    snapshot: Value,
    notice: String,
    dirty: bool,
    scroll: usize,
    local_output: String,
    last_memory_epoch: Option<u64>,
    last_refresh: std::time::Instant,
}

impl<'a> AssistantView<'a> {
    fn new(client: &'a mut Client, scope: &'a str) -> Result<Self> {
        let snapshot = send(
            client,
            Request::Snapshot {
                scope: scope.into(),
            },
        )?;
        let notice = snapshot["notice"].as_str().unwrap_or_default().to_owned();
        let last_memory_epoch = snapshot["memory_epoch"].as_u64();
        let mut view = Self {
            board: false,
            last_brief: None,
            memory_next: None,
            record_next: None,
            focus: None,
            client,
            scope,
            presenter: crate::monitor::FramePresenter::default(),
            draft: String::new(),
            snapshot,
            notice,
            dirty: true,
            scroll: 0,
            local_output: String::new(),
            last_memory_epoch,
            last_refresh: std::time::Instant::now(),
        };
        match send(
            view.client,
            Request::Brief {
                scope: scope.into(),
            },
        ) {
            Ok(reply) => view.accept_reply(reply),
            Err(error) => {
                view.notice = format!(
                    "Briefing unavailable: {error}. /brief retries the local read; nothing was acknowledged."
                )
            }
        }
        Ok(view)
    }

    fn paint_if_dirty(&mut self) -> Result<()> {
        if !self.dirty {
            return Ok(());
        }
        self.snapshot["local_output"] = json!(self.local_output);
        self.snapshot["focus_label"] = json!(self.focus.as_ref().map(|focus| focus.label.as_str()));
        self.snapshot["board_view"] = json!(self.board);
        self.snapshot["board_focus"] = json!(self.focus.as_ref().map(|focus| &focus.identity));
        paint(
            &self.snapshot,
            &self.draft,
            &self.notice,
            self.scroll,
            &mut self.presenter,
        )?;
        self.dirty = false;
        Ok(())
    }

    fn refresh_if_due(&mut self) -> Result<()> {
        if self.last_refresh.elapsed() < Duration::from_secs(1) {
            return Ok(());
        }
        let next = send(
            self.client,
            Request::Snapshot {
                scope: self.scope.into(),
            },
        )?;
        if self.snapshot["notice"].as_str() == Some(self.notice.as_str()) {
            self.notice = next["notice"].as_str().unwrap_or_default().to_owned();
        }
        fence_cached_output(&next, &mut self.last_memory_epoch, &mut self.local_output);
        self.dirty = true;
        self.snapshot = next;
        self.last_refresh = std::time::Instant::now();
        Ok(())
    }

    fn handle_event(&mut self, event: Event) -> Result<bool> {
        match event {
            Event::Mouse(mouse) => {
                match mouse.kind {
                    MouseEventKind::ScrollDown => self.scroll = self.scroll.saturating_add(3),
                    MouseEventKind::ScrollUp => self.scroll = self.scroll.saturating_sub(3),
                    _ => {}
                }
                self.dirty = true;
            }
            Event::Resize(_, _) => self.dirty = true,
            Event::Key(key) if key.kind == KeyEventKind::Press => return self.handle_key(key),
            _ => {}
        }
        Ok(true)
    }

    fn handle_key(&mut self, key: crossterm::event::KeyEvent) -> Result<bool> {
        match key.code {
            KeyCode::Esc | KeyCode::F(12) => return Ok(false),
            KeyCode::F(1) => self.open_help()?,
            KeyCode::F(2) => self.open_settings()?,
            KeyCode::Enter if self.draft.is_empty() && needs_connection(&self.snapshot) => {
                self.open_settings()?;
            }
            KeyCode::PageDown => {
                self.scroll = self
                    .scroll
                    .saturating_add(usize::from(terminal::size()?.1.saturating_sub(6)).max(1))
            }
            KeyCode::PageUp => {
                self.scroll = self
                    .scroll
                    .saturating_sub(usize::from(terminal::size()?.1.saturating_sub(6)).max(1))
            }
            KeyCode::Home => self.scroll = 0,
            KeyCode::Char('c' | 'u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.draft.clear()
            }
            KeyCode::Backspace => {
                self.draft.pop();
            }
            KeyCode::Char(ch)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && self.draft.len() + ch.len_utf8() <= 16 * 1024 =>
            {
                self.draft.push(ch)
            }
            KeyCode::Enter if !self.draft.trim().is_empty() => self.submit()?,
            _ => {}
        }
        self.dirty = true;
        Ok(true)
    }

    fn submit(&mut self) -> Result<()> {
        if !self.draft.starts_with('/')
            && (self.snapshot["provider"] != "codex" || needs_connection(&self.snapshot))
        {
            self.notice = "Connect Pika to send this message. Your draft is still here.".into();
            self.open_settings()?;
            return Ok(());
        }
        let mut request = if let Some(request) = self.context_command() {
            request
        } else if !self.draft.starts_with('/') && self.snapshot["provider"] == "codex" {
            Ok(Request::Send {
                scope: self.scope.into(),
                request_id: uuid::Uuid::new_v4().to_string(),
                body: self.draft.clone(),
                timestamp: timestamp(),
                focus: None,
            })
        } else {
            input_request(&self.draft, self.scope)
        };
        if let (
            Ok(Request::Send { focus, .. } | Request::Investigate { focus, .. }),
            Some(selected),
        ) = (&mut request, &self.focus)
        {
            // A selected row is local focus only until exact sharing is approved.
            if self.snapshot["control"]["board"]["rows"]
                .as_array()
                .is_some_and(|rows| {
                    rows.iter()
                        .any(|row| row["identity"] == json!(selected.identity))
                })
            {
                *focus = Some(selected.identity.clone());
            }
        }
        match request {
            Ok(request) => self.send_draft(request),
            Err(error) => self.notice = error.to_string(),
        }
        self.snapshot = send(
            self.client,
            Request::Snapshot {
                scope: self.scope.into(),
            },
        )?;
        fence_cached_output(
            &self.snapshot,
            &mut self.last_memory_epoch,
            &mut self.local_output,
        );
        Ok(())
    }

    fn context_command(&self) -> Option<Result<Request>> {
        let request = match self.draft.as_str() {
            "/approve" => approve_displayed(&self.snapshot, self.scope),
            "/brief-seen" => self
                .last_brief
                .clone()
                .map(|cursor| Request::AcknowledgeBrief { cursor })
                .context("Open /brief first; nothing was acknowledged"),
            "/memory-next" => self
                .memory_next
                .clone()
                .map(|cursor| Request::MemoryPage {
                    scope: self.scope.into(),
                    cursor: Some(cursor),
                    limit: 32,
                })
                .context("No next memory page; start with /memory"),
            "/memory-record-next" => self
                .record_next
                .clone()
                .map(|cursor| Request::MemoryRecord {
                    scope: self.scope.into(),
                    record_id: cursor.id.clone(),
                    cursor: Some(cursor),
                })
                .context("No next record chunk; start with /memory-record ID"),
            _ => return None,
        };
        Some(request)
    }

    fn send_draft(&mut self, request: Request) {
        match send(self.client, request) {
            Ok(reply) => self.accept_reply(reply),
            Err(error) => self.notice = format!("{error}. Draft retained; no automatic retry."),
        }
    }

    fn accept_reply(&mut self, reply: Value) {
        if let Some(cursor) = reply.get("presentation").and_then(|v| v.get("cursor")) {
            self.last_brief = serde_json::from_value(cursor.clone()).ok();
        }
        if let Some(page) = reply.get("memory_page") {
            self.memory_next = serde_json::from_value(page["next"].clone()).ok().flatten();
        }
        if let Some(chunk) = reply.get("memory_record") {
            self.record_next = serde_json::from_value(chunk["next"].clone()).ok().flatten();
        }
        self.local_output = if reply.get("presentation").is_some() && self.draft != "/brief" {
            setup::brief_text(&reply["presentation"]["brief"])
        } else {
            reply["local_output"].as_str().unwrap_or("").to_owned()
        };
        if self.draft.starts_with("/forget ") {
            self.local_output.clear();
        }
        self.notice = reply["notice"]
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| default_notice(&reply, &self.draft));
        self.draft.clear();
        self.scroll = 0;
    }

    fn open_help(&mut self) -> Result<()> {
        let draft = self.draft.clone();
        let ui = crate::onboarding::Screen::embedded();
        let choice = ui.choice(
            "How can Pika help?",
            "Ask about a decision, remember something important, or pick up where you left off.",
            &[
                "Back to conversation",
                "See saved updates",
                "Leave feedback",
                "Advanced commands",
            ],
        )?;
        match choice {
            Some(1) => self.send_draft(Request::Brief {
                scope: self.scope.into(),
            }),
            Some(2) => {
                if let Some(note) =
                    ui.input("Leave feedback", "What should we improve about Pika?")?
                {
                    self.send_draft(Request::Feedback {
                        scope: self.scope.into(),
                        note: Some(note),
                    });
                }
            }
            Some(3) => {
                self.send_draft(Request::Help);
            }
            _ => {}
        }
        self.draft = draft;
        self.presenter = crate::monitor::FramePresenter::default();
        Ok(())
    }

    fn open_settings(&mut self) -> Result<()> {
        let result = self.configure_connection();
        self.presenter = crate::monitor::FramePresenter::default();
        if let Err(error) = result {
            self.notice = format!("Provider not started: {error}");
            let ui = crate::onboarding::Screen::embedded();
            if ui.choice(
                "Pika couldn't finish connecting",
                "Your draft and saved memories are still here. No message was sent.",
                &["Back to conversation", "Technical details"],
            )? == Some(1)
            {
                ui.details("Technical details", &self.notice)?;
            }
        }
        self.snapshot = send(
            self.client,
            Request::Snapshot {
                scope: self.scope.into(),
            },
        )?;
        Ok(())
    }

    fn configure_connection(&mut self) -> Result<()> {
        let mut settings = self.snapshot.clone();
        settings["view_notice"] = json!(self.notice);
        let Some(selection) = setup::choose(self.client.root(), &settings, self.scope)? else {
            return Ok(());
        };
        send(
            self.client,
            Request::Enable {
                scope: self.scope.into(),
                executable: selection.executable.clone(),
                max_calls: selection.max_calls,
                restore: false,
            },
        )?;
        crate::assistant_startup::save(&crate::assistant_startup::path()?, &selection)?;
        self.notice =
            "Connecting Pika… Your message will be sent only when you press Enter.".into();
        Ok(())
    }
}

fn needs_connection(snapshot: &Value) -> bool {
    matches!(snapshot["state"].as_str(), Some("not_enabled")) || snapshot["can_reconnect"] == true
}

/// Bind the shortcut to the exact report the user saw, never to "latest" at the
/// time the host handles it. Existing eligibility/source checks run at approval.
fn approve_displayed(snapshot: &Value, scope: &str) -> Result<Request> {
    let report = &snapshot["workshop_report"];
    if snapshot["scope"].as_str() != Some(scope)
        || snapshot["needs_approval"] != true
        || report["passed"] != true
    {
        bail!("No eligible tool is awaiting approval here. Open /tools to inspect saved versions.");
    }
    let hash = report["tool_hash"]
        .as_str()
        .context("Tool version is unavailable")?;
    Ok(Request::ApproveTool {
        scope: scope.into(),
        hash: hash.into(),
    })
}

fn default_notice(reply: &Value, draft: &str) -> String {
    if reply["accepted"] == true {
        return "Sent once. Waiting for Pika; no automatic retry.".into();
    }
    if draft.starts_with('/') {
        return "Saved locally. No provider call.".into();
    }
    "Draft saved, not sent. A provider and spending allowance must be enabled first.".into()
}

fn fence_cached_output(snapshot: &Value, last_epoch: &mut Option<u64>, local_output: &mut String) {
    let epoch = snapshot.get("memory_epoch").and_then(Value::as_u64);
    if epoch != *last_epoch {
        local_output.clear();
        *last_epoch = epoch;
    }
}

fn input_request(text: &str, scope: &str) -> Result<Request> {
    if text == "/help" {
        return Ok(Request::Help);
    }
    if let Some(request) = input_control(text, scope)? {
        return Ok(request);
    }
    if let Some(request) = input_explain(text, scope)? {
        return Ok(request);
    }
    if let Some(request) = input_scoped_action(text, scope) {
        return Ok(request);
    }
    if let Some(request) = input_tools(text, scope)? {
        return Ok(request);
    }
    if let Some(request) = input_memory(text, scope)? {
        return Ok(request);
    }
    if let Some(request) = input_recovery(text)? {
        return Ok(request);
    }
    if text.starts_with('/') {
        bail!(
            "Use /help for commands. /cancel interrupts owned work; /fresh-context explains recovery. Esc returns to the board."
        );
    }
    Ok(save(scope, SaveKind::Draft, text))
}

fn input_scoped_action(text: &str, scope: &str) -> Option<Request> {
    input_inspect(text, scope)
        .or_else(|| input_recall(text, scope))
        .or_else(|| input_investigation(text, scope))
        .or_else(|| input_feedback(text, scope))
}

fn input_feedback(text: &str, scope: &str) -> Option<Request> {
    if text == "/feedback" || text.starts_with("/feedback ") {
        Some(Request::Feedback {
            scope: scope.into(),
            note: text.strip_prefix("/feedback ").map(str::to_owned),
        })
    } else {
        None
    }
}

fn input_inspect(text: &str, scope: &str) -> Option<Request> {
    let profile_view = match text {
        "/profile" => Some(crate::assistant_profile_views::View::Index),
        "/profile identity" | "/profile IDENTITY.md" => {
            Some(crate::assistant_profile_views::View::Identity)
        }
        "/profile soul" | "/profile SOUL.md" => Some(crate::assistant_profile_views::View::Soul),
        "/profile memory" | "/profile MEMORY.md" => {
            Some(crate::assistant_profile_views::View::Memory)
        }
        _ => None,
    };
    if let Some(view) = profile_view {
        return Some(Request::ProfileView {
            scope: scope.into(),
            view,
        });
    }
    if text == "/memory" {
        return Some(Request::MemoryPage {
            scope: scope.into(),
            cursor: None,
            limit: 32,
        });
    }
    if let Some(record_id) = text.strip_prefix("/memory-record ") {
        return Some(Request::MemoryRecord {
            scope: scope.into(),
            record_id: record_id.trim().into(),
            cursor: None,
        });
    }
    text.strip_prefix("/memory-search ")
        .map(|query| Request::MemorySearch {
            scope: scope.into(),
            query: query.into(),
        })
}

fn input_control(text: &str, scope: &str) -> Result<Option<Request>> {
    let request = match text {
        "/pause" => Some(Request::Pause),
        "/resume" => Some(Request::Resume),
        "/background off" => Some(Request::StopBackground),
        "/maintenance" => Some(Request::MaintenanceStatus {
            scope: scope.into(),
        }),
        "/maintenance off" => Some(Request::MaintenanceOff {
            scope: scope.into(),
        }),
        "/proposals" => Some(Request::Method {
            scope: scope.into(),
            action: "pending".into(),
            input: String::new(),
        }),
        "/board-share" => Some(Request::ShareBoard {
            scope: scope.into(),
            confirmation: None,
        }),
        "/board-share off" => Some(Request::RevokeBoard {
            scope: scope.into(),
        }),
        "/consults" => Some(Request::ConsultSources {
            scope: scope.into(),
        }),
        _ => None,
    };
    if request.is_some() {
        return Ok(request);
    }
    if let Some(request) = input_maintenance(text, scope)? {
        return Ok(Some(request));
    }
    if let Some(hash) = text.strip_prefix("/board-share ") {
        return Ok(Some(Request::ShareBoard {
            scope: scope.into(),
            confirmation: Some(hash.trim().into()),
        }));
    }
    if let Some(settings) = text.strip_prefix("/background ") {
        let (calls, hours) = settings
            .trim()
            .split_once(' ')
            .unwrap_or((settings.trim(), "0"));
        return Ok(Some(Request::Background {
            scope: scope.into(),
            max_calls: calls.parse()?,
            hours: hours.parse()?,
        }));
    }
    input_consultation(text, scope)
}

fn input_maintenance(text: &str, scope: &str) -> Result<Option<Request>> {
    if let Some(request) = input_commitment(text, scope)? {
        return Ok(Some(request));
    }
    if let Some(settings) = text.strip_prefix("/maintenance ") {
        let parts: Vec<_> = settings.split_whitespace().collect();
        if !matches!(parts.len(), 1 | 3) {
            bail!(
                "Use /maintenance CALLS (daily, until disabled), optionally CALLS HOURS INTERVAL_HOURS, or /maintenance off"
            );
        }
        return Ok(Some(Request::Maintenance {
            scope: scope.into(),
            max_calls: parts[0].parse()?,
            hours: parts.get(1).unwrap_or(&"0").parse()?,
            interval_hours: parts.get(2).unwrap_or(&"24").parse()?,
        }));
    }
    if let Some(settings) = text.strip_prefix("/revisit ") {
        let (id, hours) = settings
            .split_once(' ')
            .context("Use /revisit MEMORY_ID HOURS")?;
        return Ok(Some(Request::MaintenanceRevisit {
            scope: scope.into(),
            record_id: id.into(),
            hours: hours.parse()?,
        }));
    }
    for (prefix, enabled) in [("/guidance-on ", true), ("/guidance-off ", false)] {
        if let Some(id) = text.strip_prefix(prefix) {
            return Ok(Some(Request::Guidance {
                scope: scope.into(),
                record_id: id.trim().into(),
                enabled,
            }));
        }
    }
    for (prefix, action) in [
        ("/method-test-json ", "test"),
        ("/method-approve ", "approve"),
        ("/method-assess-json ", "assess"),
    ] {
        if let Some(input) = text.strip_prefix(prefix) {
            return Ok(Some(Request::Method {
                scope: scope.into(),
                action: action.into(),
                input: input.into(),
            }));
        }
    }
    Ok(None)
}

fn input_commitment(text: &str, scope: &str) -> Result<Option<Request>> {
    if let Some(body) = text.strip_prefix("/commitment-due ") {
        let (record_id, due) = body
            .split_once(' ')
            .context("Use /commitment-due ID UNIX_SECONDS")?;
        return Ok(Some(Request::CommitmentDue {
            scope: scope.into(),
            request_id: uuid::Uuid::new_v4().to_string(),
            record_id: record_id.into(),
            due_at: due.trim().parse()?,
        }));
    }
    if let Some(body) = text.strip_prefix("/commitment-done ") {
        let (record_id, statement) = body
            .split_once(' ')
            .context("Use /commitment-done ID YOUR_COMPLETION_REPORT")?;
        return Ok(Some(Request::CommitmentDone {
            scope: scope.into(),
            request_id: uuid::Uuid::new_v4().to_string(),
            record_id: record_id.into(),
            statement: statement.into(),
        }));
    }
    Ok(None)
}

fn input_consultation(text: &str, scope: &str) -> Result<Option<Request>> {
    if let Some(identity) = text.strip_prefix("/consult-allow ") {
        let (provider, conversation) = identity.split_once(':').ok_or_else(|| anyhow::anyhow!("Use /consult-allow PROVIDER:EXACT_UUID. This permits private read-only consultation until revoked, within the existing call allowance."))?;
        return Ok(Some(Request::AllowConsult {
            scope: scope.into(),
            provider: provider.parse().map_err(anyhow::Error::msg)?,
            conversation: conversation.trim().into(),
            hours: 0,
        }));
    }
    for (prefix, forget) in [("/consult-revoke ", false), ("/consult-forget ", true)] {
        if let Some(id) = text.strip_prefix(prefix) {
            return Ok(Some(Request::RevokeConsult {
                scope: scope.into(),
                id: id.trim().into(),
                forget,
            }));
        }
    }
    Ok(None)
}

fn handle_consultation_permissions(root: &Path, request: &Request) -> Result<Option<Value>> {
    use crate::assistant_consultation_permissions as permissions;
    let result = match request {
        Request::ConsultSources { scope: name } => permissions::list(root, name)?,
        Request::AllowConsult {
            scope: name,
            provider,
            conversation,
            hours,
        } => {
            scope(name)?;
            permissions::approve(
                root,
                &crate::core::Pika::discover()?,
                name,
                *provider,
                conversation,
                crate::assistant_policy::permission_expiry(timestamp(), *hours)?,
            )?
        }
        Request::RevokeConsult {
            scope: name,
            id,
            forget,
        } => {
            let allowed = permissions::list(root, name)?;
            if !allowed["permissions"].as_array().is_some_and(|rows| {
                rows.iter()
                    .any(|row| row["id"].as_str() == Some(id.as_str()))
            }) {
                bail!("Consultation permission is not visible in this scope");
            }
            if *forget {
                permissions::remove(root, id)?
            } else {
                permissions::revoke(root, id)?
            }
        }
        _ => return Ok(None),
    };
    Ok(Some(
        json!({"details":result,"local_output":serde_json::to_string_pretty(&result)?}),
    ))
}

fn input_explain(text: &str, scope: &str) -> Result<Option<Request>> {
    if let Some(rest) = text.strip_prefix("/explain ") {
        let (id, reply) = rest.split_once(' ').ok_or_else(|| {
            anyhow::anyhow!(
                "Use /explain RECORD_ID JSON with choice, why, alternative, remaining_question"
            )
        })?;
        return Ok(Some(Request::Explain {
            scope: scope.into(),
            record_id: id.into(),
            reply: serde_json::from_str(reply)?,
        }));
    }
    if let Some(spec) = text.strip_prefix("/evolve-json ") {
        return Ok(Some(Request::EvolveSpec {
            scope: scope.into(),
            request_id: uuid::Uuid::new_v4().to_string(),
            spec: spec.into(),
        }));
    }
    Ok(None)
}

fn input_recall(text: &str, scope: &str) -> Option<Request> {
    if text == "/brief" {
        return Some(Request::Brief {
            scope: scope.into(),
        });
    }
    text.strip_prefix("/why ").map(|id| Request::Recall {
        scope: scope.into(),
        record_id: id.trim().into(),
    })
}

fn input_investigation(text: &str, scope: &str) -> Option<Request> {
    text.strip_prefix("/investigate ")
        .map(|body| Request::Investigate {
            scope: scope.into(),
            request_id: uuid::Uuid::new_v4().to_string(),
            body: body.into(),
            timestamp: timestamp(),
            focus: None,
        })
}

fn input_memory(text: &str, scope: &str) -> Result<Option<Request>> {
    if let Some(request) = input_decision_revision(text, scope)? {
        return Ok(Some(request));
    }
    if let Some(rest) = text.strip_prefix("/correct ") {
        let (id, body) = rest
            .split_once(' ')
            .ok_or_else(|| anyhow::anyhow!("Use /correct RECORD_ID NEW_WORDING"))?;
        return Ok(Some(Request::Correct {
            scope: scope.into(),
            request_id: uuid::Uuid::new_v4().to_string(),
            record_id: id.into(),
            body: body.into(),
            timestamp: timestamp(),
        }));
    }
    for (prefix, kind) in [
        ("/remember ", SaveKind::Instruction),
        ("/decision ", SaveKind::Decision),
        ("/propose-decision ", SaveKind::Proposal),
        ("/skip ", SaveKind::Grasp),
        ("/defer ", SaveKind::Grasp),
    ] {
        if let Some(body) = text.strip_prefix(prefix) {
            let body = if matches!(kind, SaveKind::Grasp) {
                format!("{}: {body}", prefix.trim().trim_start_matches('/'))
            } else {
                body.to_owned()
            };
            return Ok(Some(save(scope, kind, &body)));
        }
    }
    if let Some(body) = text.strip_prefix("/decision-json ") {
        let _: crate::assistant_briefing::Decision = serde_json::from_str(body)?;
        return Ok(Some(save(scope, SaveKind::Decision, body)));
    }
    Ok(None)
}

fn input_decision_revision(text: &str, scope: &str) -> Result<Option<Request>> {
    if let Some(rest) = text.strip_prefix("/decision-state ") {
        let (id, state) = rest.split_once(' ').ok_or_else(|| {
            anyhow::anyhow!(
                "Use /decision-state ID proposed|accepted|rejected|deferred|unresolved|superseded"
            )
        })?;
        return Ok(Some(Request::DecisionState {
            scope: scope.into(),
            request_id: uuid::Uuid::new_v4().to_string(),
            record_id: id.into(),
            state: serde_json::from_value(json!(state.trim()))?,
            timestamp: timestamp(),
        }));
    }
    if let Some(rest) = text.strip_prefix("/decision-revise-json ") {
        let (id, decision) = rest
            .split_once(' ')
            .ok_or_else(|| anyhow::anyhow!("Use /decision-revise-json ID JSON"))?;
        return Ok(Some(Request::DecisionRevision {
            scope: scope.into(),
            request_id: uuid::Uuid::new_v4().to_string(),
            record_id: id.into(),
            decision: serde_json::from_str(decision)?,
            timestamp: timestamp(),
        }));
    }
    Ok(None)
}

fn input_tools(text: &str, scope: &str) -> Result<Option<Request>> {
    if let Some(request) = input_tool_management(text, scope)? {
        return Ok(Some(request));
    }
    if let Some(rest) = text.strip_prefix("/tool ") {
        let (name, inputs) = rest
            .split_once(' ')
            .ok_or_else(|| anyhow::anyhow!("Use /tool NAME JSON_ARRAY"))?;
        return Ok(Some(Request::InvokeTool {
            scope: scope.into(),
            name: name.into(),
            inputs: inputs.into(),
        }));
    }
    if let Some(rest) = text.strip_prefix("/rollback ") {
        let (name, hash) = rest
            .split_once(' ')
            .ok_or_else(|| anyhow::anyhow!("Use /rollback NAME HASH"))?;
        return Ok(Some(Request::RollbackTool {
            scope: scope.into(),
            name: name.into(),
            hash: hash.trim().into(),
        }));
    }
    if let Some(hash) = text.strip_prefix("/approve ") {
        return Ok(Some(Request::ApproveTool {
            scope: scope.into(),
            hash: hash.trim().into(),
        }));
    }
    if let Some(grant_id) = text.strip_prefix("/revoke ") {
        return Ok(Some(Request::RevokeTool {
            scope: scope.into(),
            grant_id: grant_id.trim().into(),
        }));
    }
    if text == "/evolve" {
        return Ok(Some(Request::Evolve {
            scope: scope.into(),
            request_id: uuid::Uuid::new_v4().to_string(),
        }));
    }
    Ok(None)
}

fn input_tool_management(text: &str, scope: &str) -> Result<Option<Request>> {
    if text == "/tools" || text.starts_with("/tool-info ") {
        return Ok(Some(Request::ToolCatalog {
            scope: scope.into(),
            hash: text.strip_prefix("/tool-info ").map(|v| v.trim().into()),
        }));
    }
    if let Some(id) = text.strip_prefix("/evolve ") {
        return Ok(Some(Request::Improvement {
            scope: scope.into(),
            correction_id: id.trim().into(),
        }));
    }
    if let Some(value) = text.strip_prefix("/assess-tool ") {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Assessment {
            hash: String,
            outcome: String,
            evidence: String,
            rollback: Option<String>,
        }
        let value: Assessment = serde_json::from_str(value)?;
        return Ok(Some(Request::AssessTool {
            scope: scope.into(),
            hash: value.hash,
            outcome: value.outcome,
            evidence: value.evidence,
            rollback: value.rollback,
        }));
    }
    Ok(None)
}

fn input_recovery(text: &str) -> Result<Option<Request>> {
    if text == "/cancel" {
        return Ok(Some(Request::Cancel));
    }
    if text == "/fresh-context acknowledge" {
        return Ok(Some(Request::FreshContext {
            request_id: uuid::Uuid::new_v4().to_string(),
        }));
    }
    if text == "/fresh-context" {
        bail!(
            "This stops Pika's owned assistant jobs and retires their provider context, without replay or refund. Saved memory stays. Type /fresh-context acknowledge to proceed; you must explicitly re-enable the provider afterward."
        );
    }
    if let Some(record_id) = text.strip_prefix("/forget ") {
        return Ok(Some(Request::Forget {
            record_id: record_id.trim().into(),
        }));
    }
    Ok(None)
}

fn paint(
    snapshot: &Value,
    draft: &str,
    notice: &str,
    scroll: usize,
    presenter: &mut crate::monitor::FramePresenter,
) -> Result<()> {
    let dimensions = terminal::size()?;
    let buffer = compose(snapshot, draft, notice, dimensions, scroll)?;
    presenter.present(&mut io::stdout(), dimensions, |frame| {
        frame.extend_from_slice(&buffer);
        Ok(())
    })?;
    Ok(())
}

fn compose(
    snapshot: &Value,
    draft: &str,
    notice: &str,
    dimensions: (u16, u16),
    scroll: usize,
) -> Result<Vec<u8>> {
    let (columns, height) = dimensions;
    let notice = if snapshot["state"] == "unavailable" && notice.starts_with("Connecting Pika") {
        ""
    } else {
        notice
    };
    if columns < 8 || height < 8 {
        return resize_frame(columns, height);
    }
    let mut lines = header_lines(snapshot);
    if snapshot["board_view"] != true {
        append_board(&mut lines);
    }
    append_output(snapshot, &mut lines);
    append_provider_status(snapshot, &mut lines);
    append_recovery_status(snapshot, &mut lines);
    append_investigation(snapshot, &mut lines);
    append_tool_status(snapshot, &mut lines);
    append_records(snapshot, &mut lines);
    if snapshot["board_view"] == true
        && let Some(crate::activity_feed::Context::Source(source)) = crate::activity_feed::current()
        && let Some(state) = source.snapshot()
    {
        let focus = serde_json::from_value(snapshot["board_focus"].clone())
            .ok()
            .map(|identity| Focus {
                identity,
                label: snapshot["focus_label"].as_str().unwrap_or_default().into(),
            });
        let mut buffer = Vec::new();
        let origin =
            crate::monitor::assistant_backdrop(&mut buffer, state, focus.as_ref(), dimensions)?;
        let panel = (
            columns.saturating_sub(origin.0),
            height.saturating_sub(origin.1),
        );
        buffer.extend(render_lines_at(
            lines,
            draft,
            notice,
            panel,
            scroll,
            origin,
            needs_connection(snapshot),
        )?);
        return Ok(buffer);
    }
    render_lines(
        lines,
        draft,
        notice,
        dimensions,
        scroll,
        needs_connection(snapshot),
    )
}

fn resize_frame(columns: u16, height: u16) -> Result<Vec<u8>> {
    let mut frame = Vec::new();
    if columns > 0 && height > 0 {
        queue!(
            frame,
            MoveTo(0, 0),
            Clear(ClearType::All),
            Print("Resize".chars().take(columns as usize).collect::<String>())
        )?;
    }
    Ok(frame)
}

fn header_lines(snapshot: &Value) -> Vec<String> {
    let mut lines = vec![
        "✦ PIKA".to_owned(),
        if snapshot["control"]["context_blocked"] == true {
            "Pika needs to reconnect. Your memories are safe. F2 for details.".into()
        } else if snapshot["control"]["paused"] == true {
            "Paused · /resume when you're ready".into()
        } else {
            match snapshot["state"].as_str() {
                Some("working") => "Thinking…",
                Some("starting") => "Connecting…",
                Some("recovering") => "Reconnecting…",
                Some("cancelling") => "Stopping this reply…",
                Some("unavailable" | "stopped") => {
                    if snapshot["can_reconnect"] == true {
                        "! Couldn't connect · F2 to reconnect · your draft is safe"
                    } else {
                        "! Reply interrupted · F2 details · no message resent"
                    }
                }
                Some("ready") => "● Connected · Luna",
                _ => "Your persistent assistant",
            }
            .into()
        },
    ];
    if let Some(label) = snapshot["focus_label"].as_str() {
        lines.push(format!(
            "About {}",
            crate::fleet::sanitize_terminal_text(label)
        ));
    }
    lines.push(String::new());
    if snapshot["state"] == "not_enabled" {
        lines.extend([
            "Welcome to Pika".into(),
            "Keep track of decisions, remember what matters, and pick up your work.".into(),
            String::new(),
            "Connect once to start talking.".into(),
            "Enter  Connect Pika     F1  Look around".into(),
            String::new(),
        ]);
    }
    lines
}

fn background_label(control: &Value) -> String {
    let background = &control["background"];
    match background["state"].as_str().unwrap_or("off") {
        "off" => "background off".into(),
        "different_scope" => "background configured for another scope".into(),
        state => {
            let remaining = background["remaining_calls"].as_u64().unwrap_or(0);
            let expiry = background["config"]["expires_at"]
                .as_i64()
                .map(|value| {
                    if value == crate::assistant_policy::UNTIL_REVOKED {
                        " · until disabled".into()
                    } else {
                        format!(" · until {} local", crate::quota::reset_label(value))
                    }
                })
                .unwrap_or_default();
            format!("background {state} · {remaining} calls available{expiry}")
        }
    }
}

fn append_board(lines: &mut Vec<String>) {
    if let Some(crate::activity_feed::Context::Source(source)) = crate::activity_feed::current()
        && let Some(state) = source.snapshot()
    {
        let [needs, working, ready, parked] = state.summary.counts;
        lines.push(format!(
            "Board · {needs} need you · {working} working · {ready} ready · {parked} parked"
        ));
        for item in state
            .items
            .iter()
            .filter(|item| {
                matches!(
                    item.session.status,
                    crate::model::Status::NeedsYou | crate::model::Status::Error
                )
            })
            .take(5)
        {
            lines.push(format!(
                "Needs attention · {}{}{}",
                item.session.name.as_deref().unwrap_or("Unnamed thread"),
                item.node_name
                    .as_ref()
                    .map(|name| format!(" @{name}"))
                    .unwrap_or_default(),
                if item.stale {
                    " · cached; host freshness unknown"
                } else {
                    ""
                }
            ));
        }
        for health in state.health.iter().take(4) {
            lines.push(format!("Coverage · {health}"));
        }
    }
}

fn append_output(snapshot: &Value, lines: &mut Vec<String>) {
    lines.push(String::new());
    if let Some(text) = snapshot["local_output"]
        .as_str()
        .filter(|text| !text.is_empty())
    {
        lines.push(text.into());
        lines.push(String::new());
    }
}

fn append_provider_status(snapshot: &Value, lines: &mut Vec<String>) {
    append_native_method_status(snapshot, lines);
    if let Some(author) = snapshot["author"].as_object() {
        lines.push(format!(
            "Disposable tool author · {}",
            author
                .get("state")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
        ));
        if author.get("error").and_then(Value::as_str).is_some() {
            lines.push("! Couldn't finish preparing the tool · F2 for details".into());
        }
    }
}

fn append_native_method_status(snapshot: &Value, lines: &mut Vec<String>) {
    let method = &snapshot["method_control"];
    if let Some(state) = method["state"]
        .as_str()
        .filter(|state| !matches!(*state, "idle" | "different_scope"))
    {
        lines.push(format!("Native workshop · {state}"));
        if !method["result"].is_null() {
            lines.push("Tool check results are available · F2 for details".into());
        }
        if method["error"].as_str().is_some() {
            lines.push("! Couldn't finish checking the tool · F2 for details".into());
        }
    }
    if snapshot["cleanup_notice"].as_str().is_some() {
        lines.push("Assistant recovery has an update · F2 for details".into());
    }
}

fn append_recovery_status(snapshot: &Value, lines: &mut Vec<String>) {
    if let Some(recovery) = snapshot["recovery"].as_object() {
        lines.push(format!(
            "Recovery · {}",
            recovery
                .get("state")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
        ));
        if recovery.get("error").and_then(Value::as_str).is_some() {
            lines.push("! Recovery needs attention · F2 for details".into());
        }
        if recovery.get("state").and_then(Value::as_str) == Some("completed") {
            lines.push("Fresh context ready. Saved memory and call charges remain. Re-enable the provider explicitly; no request was replayed.".into());
        }
    }
}

fn append_investigation(snapshot: &Value, lines: &mut Vec<String>) {
    if let Some(investigation) = snapshot["investigation"].as_object() {
        lines.push(format!(
            "Investigation · {}",
            investigation
                .get("state")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
        ));
        if let Some(text) = investigation.get("text").and_then(Value::as_str) {
            lines.push(text.into());
        }
        if investigation.get("error").and_then(Value::as_str).is_some() {
            lines.push("! Couldn't finish the investigation · F2 for details".into());
        }
    }
    if let Some(partial) = snapshot["partial"].as_str().filter(|text| !text.is_empty()) {
        lines.push("✦ Pika".into());
        lines.push(partial.into());
        lines.push(String::new());
    }
}

fn append_tool_status(snapshot: &Value, lines: &mut Vec<String>) {
    if let Some(report) = snapshot["workshop_report"].as_object() {
        let eligible = report.get("passed") == Some(&json!(true));
        lines.push(format!(
            "Tool evaluation · {}",
            if eligible {
                if snapshot["needs_approval"] == true {
                    "passed — approval required"
                } else {
                    "approved"
                }
            } else {
                "not eligible"
            }
        ));
        for comparison in snapshot["workshop_comparison"]
            .as_array()
            .into_iter()
            .flatten()
        {
            lines.push(format!(
                "Against current tool · {} · passed cases {} → {}",
                comparison
                    .get("verdict")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown"),
                comparison
                    .get("baseline_passed")
                    .filter(|value| !value.is_null())
                    .map(ToString::to_string)
                    .unwrap_or_else(|| "no baseline".into()),
                comparison
                    .get("candidate_passed")
                    .map(ToString::to_string)
                    .unwrap_or_else(|| "unknown".into())
            ));
        }
        if let Some(hash) = report.get("tool_hash").and_then(Value::as_str) {
            lines.push(format!("Exact version: {hash}"));
            if eligible && snapshot["needs_approval"] == true {
                lines.push("/approve · enable this tested version until revoked".into());
            }
        }
    }
}

fn append_records(snapshot: &Value, lines: &mut Vec<String>) {
    if let Some(records) = snapshot["records"]
        .as_array()
        .filter(|_| snapshot["local_output"].as_str().is_none_or(str::is_empty))
    {
        if records.is_empty() && snapshot["state"] == "ready" {
            lines.push("What would you like to work on?".into());
            lines.push(String::new());
            lines.push("Ask about a decision, share an idea, or tell me what to remember.".into());
        }
        for record in records.iter().rev() {
            let label = match (record["kind"].as_str(), record["origin"].as_str()) {
                (Some("Finding"), Some("Human")) => "You",
                (Some("Finding"), Some("Worker")) => "Pika",
                (Some("InferredPreference"), _) => "Learned guidance · reversible",
                (Some(kind), _) => kind,
                _ => "Memory",
            };
            lines.push(match label {
                "You" => "› You".into(),
                "Pika" => "✦ Pika".into(),
                _ => label.into(),
            });
            lines.push(crate::assistant_briefing::readable_decision(
                record["body"].as_str().unwrap_or(""),
            ));
            lines.push(String::new());
        }
    }
}

fn render_lines(
    lines: Vec<String>,
    draft: &str,
    notice: &str,
    dimensions: (u16, u16),
    scroll: usize,
    connect: bool,
) -> Result<Vec<u8>> {
    let mut buffer = Vec::new();
    queue!(buffer, MoveTo(0, 0), Clear(ClearType::All))?;
    buffer.extend(render_lines_at(
        lines,
        draft,
        notice,
        dimensions,
        scroll,
        (0, 0),
        connect,
    )?);
    Ok(buffer)
}

fn render_lines_at(
    lines: Vec<String>,
    draft: &str,
    notice: &str,
    dimensions: (u16, u16),
    scroll: usize,
    origin: (u16, u16),
    connect: bool,
) -> Result<Vec<u8>> {
    let (columns, height) = dimensions;
    let width = usize::from(columns.saturating_sub(4)).min(104);
    let left = origin.0 + columns.saturating_sub(width as u16) / 2;
    let colors = std::env::var_os("NO_COLOR").is_none();
    let mut buffer = Vec::new();
    for row in 0..height {
        queue!(
            buffer,
            MoveTo(origin.0, origin.1 + row),
            Print(" ".repeat(columns as usize))
        )?;
    }
    let wrapped = lines
        .into_iter()
        .flat_map(|line| {
            let style = conversation_style(&line);
            wrap(&line, width).into_iter().map(move |row| (row, style))
        })
        .collect::<Vec<_>>();
    let visible = usize::from(height.saturating_sub(8));
    let scroll = scroll.min(wrapped.len().saturating_sub(visible));
    for (index, (line, (color, bold))) in wrapped.into_iter().skip(scroll).take(visible).enumerate()
    {
        queue!(buffer, MoveTo(left, origin.1 + 1 + index as u16))?;
        if colors {
            queue!(buffer, SetForegroundColor(color))?;
            if bold {
                queue!(buffer, SetAttribute(Attribute::Bold))?;
            }
        }
        queue!(
            buffer,
            Print(line),
            SetAttribute(Attribute::Reset),
            ResetColor
        )?;
    }
    for (offset, text) in composer_rows(width, draft, notice, connect) {
        let wrapped = wrap(&text, width);
        if colors {
            queue!(buffer, SetForegroundColor(composer_color(offset, draft)))?;
        }
        let row = if offset == 3 {
            wrapped.last()
        } else {
            wrapped.first()
        };
        queue!(
            buffer,
            MoveTo(left, origin.1 + height.saturating_sub(offset)),
            Print(row.cloned().unwrap_or_default()),
            ResetColor
        )?;
    }
    Ok(buffer)
}

fn composer_color(offset: u16, draft: &str) -> Color {
    match offset {
        6 => Color::Yellow,
        3 if !draft.is_empty() => Color::Reset,
        4 | 2 => Color::Cyan,
        _ => Color::DarkGrey,
    }
}

fn composer_rows(width: usize, draft: &str, notice: &str, connect: bool) -> [(u16, String); 5] {
    [
        (6, friendly_notice(notice)),
        (4, format!("╭{}", "─".repeat(width.saturating_sub(1)))),
        (
            3,
            if draft.is_empty() {
                if connect {
                    "│ › Enter to connect Pika".into()
                } else {
                    "│ › Message Pika…".into()
                }
            } else {
                format!("│ › {draft}▏")
            },
        ),
        (2, format!("╰{}", "─".repeat(width.saturating_sub(1)))),
        (
            1,
            if connect {
                "Enter connect · F1 help · F2 settings · Esc/F12 board".into()
            } else {
                "Enter send · F1 help · F2 settings · Esc/F12 board".into()
            },
        ),
    ]
}

fn conversation_style(line: &str) -> (Color, bool) {
    if line.starts_with('✦') {
        (Color::Cyan, true)
    } else if line.starts_with("› You") {
        (Color::Blue, true)
    } else if line.starts_with('!') || line.starts_with("Needs attention") {
        (Color::Yellow, false)
    } else if line.starts_with('●') {
        (Color::Green, false)
    } else if line.starts_with("Board ·") || line.starts_with("Your persistent") {
        (Color::DarkGrey, false)
    } else {
        (Color::Reset, false)
    }
}

fn friendly_notice(notice: &str) -> String {
    if notice.starts_with("Provider not started:") {
        return "Couldn't connect. Your saved memories are here. F2 for details.".into();
    }
    if notice.starts_with("Turn not accepted:") {
        return "Couldn't send. Your draft is still here. F2 for details.".into();
    }
    match notice {
        "Saved briefing opened. Nothing acknowledged; no model call."
        | "Local memory ready. A provider and spending allowance have not been enabled. Drafts remain unsent." => {
            String::new()
        }
        "Sent once. Waiting for Pika; no automatic retry."
        | "Accepted once; do not resend if delivery becomes unknown." => "Message sent.".into(),
        "Foreground provider ready. Requests use your explicit call allowance." => String::new(),
        "Checking the isolated provider. No new request has been sent." => {
            "Connecting Pika…".into()
        }
        "Request in progress within the approved allowance. No automatic retry." => {
            "Thinking…".into()
        }
        _ => notice.into(),
    }
}

fn wrap(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![String::new()];
    }
    let mut result = vec![String::new()];
    let mut cells = 0;
    for ch in crate::fleet::sanitize_terminal_lines(text).chars() {
        let size = ch.width().unwrap_or(0);
        if size > width {
            continue;
        }
        if ch == '\n' || cells + size > width.max(1) {
            result.push(String::new());
            cells = 0;
        }
        if ch != '\n' {
            result.last_mut().unwrap().push(ch);
            cells += size;
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_startup_enables_only_interactive_matching_scope_and_preserves_reads() {
        fn selected() -> crate::assistant_startup::Selection {
            crate::assistant_startup::Selection {
                profile_root: "/saved/profile".into(),
                profile_id: "saved-id".into(),
                scope: "pika".into(),
                executable: "/fake/codex".into(),
                max_calls: 19,
            }
        }
        let mut normal = Args::default();
        apply_selection(&mut normal, Some(selected()), true);
        assert_eq!(normal.scope, "pika");
        assert_eq!(normal.max_calls, Some(19));
        assert!(normal.restore_startup);
        assert_eq!(
            normal.profile_root.as_deref(),
            Some(Path::new("/saved/profile"))
        );
        for mut read in [
            Args {
                json: true,
                ..Default::default()
            },
            Args {
                offline: true,
                ..Default::default()
            },
            Args {
                remember: Some("a note".into()),
                ..Default::default()
            },
            Args {
                scope: "different-project".into(),
                ..Default::default()
            },
        ] {
            apply_selection(&mut read, Some(selected()), true);
            assert!(read.enable_codex.is_none());
            assert!(!read.restore_startup);
            assert_eq!(read.expected_profile_id.as_deref(), Some("saved-id"));
        }
        let mut redirected = Args::default();
        apply_selection(&mut redirected, Some(selected()), false);
        assert!(redirected.enable_codex.is_none());
    }

    #[test]
    fn board_chat_uses_feed_counts_and_renders_reply_and_composer_at_both_widths() {
        let source = crate::activity_feed::Source::default();
        source.publisher().publish(vec![], vec![]);
        let snapshot = json!({"board_view":true,"scope":"pika","state":"ready",
            "local_output":"Here is your answer", "records":[]});
        crate::activity_feed::with(
            Some(crate::activity_feed::Context::Source(source.clone())),
            || {
                for width in [80, 140] {
                    let output =
                        compose(&snapshot, "My next question", "Ready", (width, 35), 0).unwrap();
                    let rows = crate::terminal_frame::rows(&output, (width, 35))
                        .expect("valid terminal cells");
                    let text = crate::fleet::sanitize_terminal_lines(
                        &String::from_utf8(rows.concat()).unwrap(),
                    );
                    assert!(text.contains("0 need you"));
                    assert!(text.contains("Pika · chatting"));
                    assert!(text.contains("Here is your answer"));
                    assert!(text.contains("My next question"));
                    assert!(text.contains("Esc/F12 board"));
                }
            },
        );
        assert_eq!(source.snapshot().unwrap().revision, 1);
    }

    #[test]
    fn foreground_call_limit_requires_one_explicit_choice_and_a_provider() {
        use clap::Parser;
        #[derive(Parser)]
        struct TestCli {
            #[command(flatten)]
            args: Args,
        }
        for invalid in [
            vec!["pika", "--no-call-limit"],
            vec!["pika", "--max-calls", "12"],
            vec!["pika", "--enable-codex", "/fake/codex"],
            vec![
                "pika",
                "--enable-codex",
                "/fake/codex",
                "--max-calls",
                "12",
                "--no-call-limit",
            ],
        ] {
            assert!(TestCli::try_parse_from(invalid).is_err());
        }
        let unlimited =
            TestCli::try_parse_from(["pika", "--enable-codex", "/fake/codex", "--no-call-limit"])
                .unwrap();
        assert!(unlimited.args.no_call_limit);
        assert_eq!(unlimited.args.max_calls, None);
        let limited =
            TestCli::try_parse_from(["pika", "--enable-codex", "/fake/codex", "--max-calls", "12"])
                .unwrap();
        assert!(!limited.args.no_call_limit);
        assert_eq!(limited.args.max_calls, Some(12));
        let offline = TestCli::try_parse_from(["pika"]).unwrap();
        assert!(!offline.args.no_call_limit);
        assert_eq!(offline.args.enable_codex, None);
    }

    #[test]
    fn commitment_commands_preserve_scope_due_time_and_human_report_without_dispatch() {
        use crate::assistant_decisions as decisions;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("assistant");
        let mut memory = Store::open(root.join("memory.sqlite")).unwrap();
        let selected = scope("alpha").unwrap();
        let proposed = memory
            .append(
                decisions::proposed_commitment(
                    &selected,
                    "review release",
                    "after tests",
                    vec![],
                    timestamp(),
                )
                .unwrap(),
            )
            .unwrap();
        let accepted = decisions::transition(
            &mut memory,
            "accept",
            Origin::Human,
            selected.clone(),
            &proposed.id,
            DecisionState::Accepted,
            timestamp(),
        )
        .unwrap();
        let due = timestamp() + 3600;
        let command = format!("/commitment-due {} {due}", accepted.id);
        assert!(handle(&mut memory, input_request(&command, "beta").unwrap()).is_err());
        let saved = handle(&mut memory, input_request(&command, "alpha").unwrap()).unwrap();
        let due_id = saved["saved"].as_str().unwrap();
        let due_record = memory.get(due_id).unwrap().unwrap();
        assert_eq!(
            decisions::commitment(&due_record.body).unwrap().due_at,
            Some(due)
        );
        let completed = handle(
            &mut memory,
            input_request(
                &format!("/commitment-done {due_id} I reviewed the release"),
                "alpha",
            )
            .unwrap(),
        )
        .unwrap();
        let completed_id = completed["saved"].as_str().unwrap().to_owned();
        drop(memory);
        let memory = Store::open(root.join("memory.sqlite")).unwrap();
        let record = memory.get(&completed_id).unwrap().unwrap();
        assert_eq!(record.origin, Origin::Human);
        assert_eq!(record.scope, selected);
        assert_eq!(
            decisions::commitment(&record.body).unwrap().completion,
            decisions::Completion::HumanReported {
                statement: "I reviewed the release".into()
            }
        );
        assert!(!root.join("policy.sqlite").exists());
        assert!(!root.join("runtime.sqlite").exists());
    }

    #[test]
    fn recovery_rejection_preserves_workers_and_old_receipt_cannot_drop_queued_cleanup() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("assistant");
        let mut memory = Store::open(root.join("memory.sqlite")).unwrap();
        let mut control = crate::assistant_control::Controller::open(&root).unwrap();
        let mut session = crate::assistant_session::Session::new();
        let mut auxiliary = AuxiliaryWorkers::new(&root, control.gate()).unwrap();
        auxiliary.maintenance.quiesce();
        auxiliary.methods.quiesce();
        auxiliary.recovering = true;
        let (release, wait) = std::sync::mpsc::channel();
        let mut recovery = Some(
            crate::assistant_recovery_service::RecoveryService::start(
                root.clone(),
                uuid::Uuid::new_v4().to_string(),
                0,
                move || {
                    wait.recv_timeout(Duration::from_secs(3))
                        .map_err(|e| e.to_string())
                },
            )
            .unwrap(),
        );
        let rejected = handle_host_request(
            &root,
            &mut memory,
            &mut session,
            &mut recovery,
            &mut control,
            &mut auxiliary,
            Request::Forget {
                record_id: "not-yet-handled".into(),
            },
        );
        let feedback = handle_host_request(
            &root,
            &mut memory,
            &mut session,
            &mut recovery,
            &mut control,
            &mut auxiliary,
            input_request("/feedback Explain recovery in plain English.", "personal").unwrap(),
        )
        .unwrap();
        assert_eq!(feedback["feedback_saved"], true);
        assert!(
            std::fs::read_to_string(root.join("user_feedback.md"))
                .unwrap()
                .contains("> Explain recovery in plain English.")
        );
        assert!(
            memory
                .working_set(&scope("personal").unwrap(), 16)
                .unwrap()
                .is_empty()
        );
        assert_eq!(session.snapshot("personal")["provider"], "none");
        assert!(auxiliary.recovering);
        release.send(()).unwrap();
        assert!(
            rejected
                .unwrap_err()
                .to_string()
                .contains("finishing owned-job recovery")
        );
        assert!(auxiliary.recovering);
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while recovery.as_ref().unwrap().busy() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(recovery.as_ref().unwrap().snapshot().state, "completed");
        auxiliary.after_recovery(&root, &recovery).unwrap();
        assert!(!auxiliary.recovering);
        auxiliary
            .methods
            .begin(
                scope("personal").unwrap(),
                "pending".into(),
                String::new(),
                1,
                0,
            )
            .unwrap();

        // A later cleanup waits for the native worker while the last receipt
        // remains in the host. That old receipt must not consume the new intent.
        auxiliary.maintenance.quiesce();
        auxiliary.methods.quiesce();
        auxiliary.recovering = true;
        let new_id = uuid::Uuid::new_v4().to_string();
        auxiliary.deferred = Some(Request::FreshContext {
            request_id: new_id.clone(),
        });
        auxiliary.cleanup_hold = Some(auxiliary.gate.retain_cleanup());
        auxiliary.after_recovery(&root, &recovery).unwrap();
        assert!(auxiliary.gate.keeps_alive(1));
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        let delivered = loop {
            if let Some(request) = auxiliary.ready_request() {
                break request;
            }
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        };
        assert!(matches!(delivered, Request::FreshContext { request_id } if request_id == new_id));
        assert!(auxiliary.ready_request().is_none());
    }

    #[test]
    fn opening_reopening_and_ack_use_the_exact_brief_not_the_archive() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("assistant");
        let mut memory = Store::open(root.join("memory.sqlite")).unwrap();
        memory
            .append(NewRecord {
                kind: RecordKind::Finding,
                origin: Origin::Human,
                scope: scope("personal").unwrap(),
                body: "new-brief-marker".into(),
                provenance: "synthetic user fixture".into(),
                timestamp: 1,
                supersedes: None,
                dependencies: vec![],
                decision_state: None,
                protected_policy: false,
            })
            .unwrap();
        drop(memory);
        let owned = root.clone();
        let server = std::thread::spawn(move || serve(&owned, None).unwrap());
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !root.join("view.sock").exists() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        let mut one = Client::attach(&root).unwrap();
        let mut two = Client::attach(&root).unwrap();
        let profile = send(
            &mut two,
            Request::ProfileView {
                scope: "personal".into(),
                view: crate::assistant_profile_views::View::Memory,
            },
        )
        .unwrap();
        assert!(
            profile["local_output"]
                .as_str()
                .unwrap()
                .contains("new-brief-marker")
        );
        let view = AssistantView::new(&mut one, "personal").unwrap();
        assert!(view.local_output.contains("new-brief-marker"));
        let cursor = view.last_brief.clone().unwrap();
        drop(view);
        let still_unseen = send(
            &mut two,
            Request::Brief {
                scope: "personal".into(),
            },
        )
        .unwrap();
        assert!(
            still_unseen["local_output"]
                .as_str()
                .unwrap()
                .contains("new-brief-marker")
        );
        send(&mut two, Request::AcknowledgeBrief { cursor }).unwrap();
        let reopened = AssistantView::new(&mut one, "personal").unwrap();
        assert!(!reopened.local_output.contains("new-brief-marker"));
        assert!(reopened.last_brief.is_some());
        assert!(
            reopened.snapshot["records"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v["body"] == "new-brief-marker")
        );
        drop(reopened);
        drop(one);
        drop(two);
        assert_eq!(server.join().unwrap(), 0);
        assert!(!root.join("provider-home").exists());
    }
    #[test]
    fn background_display_tracks_real_control_state_and_remaining_allowance() {
        for state in ["enabled", "paused", "expired", "exhausted"] {
            let snapshot = json!({"scope":"alpha","state":"ready","control":{"background":{"state":state,"remaining_calls":3,"config":{"expires_at":2000000000_i64}}}});
            let details = setup::details(&snapshot);
            assert!(
                details.contains(&format!("background {state}")),
                "{details}"
            );
            assert!(details.contains("3 calls available"));
            assert!(details.contains("local"));
            assert!(
                !header_lines(&snapshot)
                    .join("\n")
                    .contains("calls available")
            );
        }
        assert_eq!(background_label(&json!({})), "background off");
        let blocked = header_lines(&json!({"control":{"context_blocked":true}})).join("\n");
        assert!(blocked.contains("needs to reconnect"));
    }

    #[test]
    fn board_return_restores_unsent_draft_but_never_forgotten_or_other_scope_output() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("assistant");
        let mut memory = Store::open(root.join("memory.sqlite")).unwrap();
        let record = memory
            .append(NewRecord {
                kind: RecordKind::Finding,
                origin: Origin::Human,
                scope: scope("personal").unwrap(),
                body: "forgettable marker".into(),
                provenance: "synthetic fixture".into(),
                timestamp: 1,
                supersedes: None,
                dependencies: vec![],
                decision_state: None,
                protected_policy: false,
            })
            .unwrap();
        drop(memory);
        let owned = root.clone();
        let server = std::thread::spawn(move || serve(&owned, None).unwrap());
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !root.join("view.sock").exists() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        let mut client = Client::attach(&root).unwrap();
        let mut navigation = ViewMemory::default();
        let mut view = AssistantView::new(&mut client, "personal").unwrap();
        view.draft = "unsent question".into();
        view.scroll = 3;
        assert!(view.local_output.contains("forgettable marker"));
        navigation.save(&view);
        drop(view);
        let mut reopened = AssistantView::new(&mut client, "personal").unwrap();
        navigation.restore(&mut reopened);
        assert_eq!(reopened.draft, "unsent question");
        assert_eq!(reopened.scroll, 3);
        assert!(reopened.local_output.contains("forgettable marker"));
        assert_eq!(
            reopened.snapshot["records"].as_array().unwrap().len(),
            1,
            "return did not send"
        );
        drop(reopened);
        let mut other = AssistantView::new(&mut client, "project:other").unwrap();
        navigation.restore(&mut other);
        assert!(other.draft.is_empty());
        assert!(!other.local_output.contains("forgettable marker"));
        drop(other);
        let forgotten_reply = send(
            &mut client,
            Request::Forget {
                record_id: record.id,
            },
        )
        .unwrap();
        assert_eq!(forgotten_reply["cleanup_pending"], true);
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            let snapshot = send(
                &mut client,
                Request::Snapshot {
                    scope: "personal".into(),
                },
            )
            .unwrap();
            if snapshot["memory_epoch"].as_u64() != navigation.epoch {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "queued forgetting never completed"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        let mut forgotten = AssistantView::new(&mut client, "personal").unwrap();
        navigation.restore(&mut forgotten);
        assert_eq!(forgotten.draft, "unsent question");
        assert!(!forgotten.local_output.contains("forgettable marker"));
        assert_eq!(forgotten.scroll, 0);
        drop(forgotten);
        drop(client);
        assert_eq!(server.join().unwrap(), 0);
        assert!(!root.join("provider-home").exists());
    }

    #[test]
    fn board_preview_exposes_exact_identities_and_approval_command_without_sending() {
        let preview = board_share_text(
            &json!({"rows":[{"name":"research","identity":{"node":"node-a","provider":"codex","conversation":"exact-id"}}],"confirmation":"exact-hash","partial":true}),
        );
        assert!(preview.contains("research · node-a / codex / exact-id"));
        assert!(preview.contains("/board-share exact-hash"));
        assert!(preview.contains("Partial coverage"));
        assert!(preview.contains("No transcripts"));
        let approved = board_share_text(&json!({"approved":true}));
        assert!(approved.contains("New tasks remain excluded"));
    }

    #[test]
    fn control_commands_keep_scope_and_do_not_promote_model_text() {
        for (command, view) in [
            ("/profile", "index"),
            ("/profile identity", "identity"),
            ("/profile SOUL.md", "soul"),
            ("/profile memory", "memory"),
        ] {
            let request = serde_json::to_value(input_request(command, "alpha").unwrap()).unwrap();
            assert_eq!(request["operation"], "profile_view");
            assert_eq!(request["view"], view);
            assert_eq!(request["scope"], "alpha");
        }
        for (command, operation) in [
            ("/background 4", "background"),
            ("/maintenance 4", "maintenance"),
        ] {
            let request = serde_json::to_value(input_request(command, "alpha").unwrap()).unwrap();
            assert_eq!(request["operation"], operation);
            assert_eq!(request["hours"], 0);
            assert_eq!(request["max_calls"], 4);
        }
        let command =
            serde_json::to_value(input_request("/board-share hash", "alpha").unwrap()).unwrap();
        assert_eq!(
            command,
            json!({"operation":"share_board","scope":"alpha","confirmation":"hash"})
        );
        let background =
            serde_json::to_value(input_request("/background 4 2", "alpha").unwrap()).unwrap();
        assert_eq!(
            background,
            json!({"operation":"background","scope":"alpha","max_calls":4,"hours":2})
        );
        assert!(input_request("/background unrestricted", "alpha").is_err());
        let proposal = serde_json::to_value(
            input_request("/propose-decision Test the idea", "alpha").unwrap(),
        )
        .unwrap();
        assert_eq!(proposal["kind"], "proposal");
        assert!(HELP.contains("0–2 helpers only if needed"));
        assert!(HELP.contains("/board-share off"));
        assert!(!HELP.contains("up to 3 calls"));
    }
    #[test]
    fn approval_shortcut_binds_displayed_version_and_rejects_missing_or_wrong_scope() {
        let mut view = json!({"scope":"alpha","needs_approval":true,"workshop_report":{"passed":true,"tool_hash":"shown-version"}});
        let request = serde_json::to_value(approve_displayed(&view, "alpha").unwrap()).unwrap();
        view["workshop_report"]["tool_hash"] = json!("newer-version");
        assert_eq!(request["hash"], "shown-version");
        assert_eq!(request["scope"], "alpha");
        assert!(approve_displayed(&view, "beta").is_err());
        view["workshop_report"]["passed"] = json!(false);
        assert!(approve_displayed(&view, "alpha").is_err());
        assert!(approve_displayed(&json!({}), "alpha").is_err());
    }
    #[test]
    fn multiline_briefs_keep_paragraphs_but_not_terminal_programs() {
        assert_eq!(
            wrap("Decisions\n\nOne choice\nNext choice", 80),
            vec!["Decisions", "", "One choice", "Next choice"]
        );
        assert_eq!(
            wrap("safe\u{1b}]52;c;secret\u{7}\nnext", 80),
            vec!["safe", "next"]
        );
    }
    #[test]
    fn briefing_identifies_nonprintable_updates_before_acknowledgement() {
        let lines = presentation_text(&json!({
            "updates": [{"id":"saved-control-text","text":"\u{1}\u{1b}]52;c;secret\u{7}"}],
            "presentation_limited":true
        }))
        .join("\n");
        assert!(lines.contains("saved-control-text · No printable preview"));
        assert!(lines.contains("abbreviated or omitted"));
        assert!(lines.contains("/memory-record ID"));
        assert!(!lines.contains("secret"));
        assert!(!lines.contains('\u{1b}'));
    }
    #[test]
    fn render_is_inert_bounded_scrollable_and_unchanged_frames_write_nothing() {
        let snapshot = json!({"scope":"personal","state":"ready","records":[],"local_output":format!("{}\nlast-line", "wide 界 and color \u{1b}]52;c;secret\u{7}\n".repeat(100))});
        for dimensions in [(0, 0), (1, 1), (7, 7), (8, 8), (40, 15), (120, 40)] {
            let frame = compose(&snapshot, &"draft".repeat(100), "notice", dimensions, 0).unwrap();
            assert!(!String::from_utf8_lossy(&frame).contains("\u{1b}]52"));
            assert!(frame.len() < 100_000);
        }
        let first = compose(&snapshot, "", "", (60, 18), 0).unwrap();
        let last = compose(&snapshot, "", "", (60, 18), usize::MAX).unwrap();
        assert_ne!(first, last);
        assert!(String::from_utf8_lossy(&last).contains("last-line"));
        let mut presenter = crate::monitor::FramePresenter::default();
        let mut output = Vec::new();
        assert!(
            presenter
                .present(&mut output, (60, 18), |frame| {
                    frame.extend_from_slice(&first);
                    Ok(())
                })
                .unwrap()
        );
        output.clear();
        assert!(
            !presenter
                .present(&mut output, (60, 18), |frame| {
                    frame.extend_from_slice(&first);
                    Ok(())
                })
                .unwrap()
        );
        assert!(output.is_empty());
    }
    #[test]
    #[ignore = "manual visual fixture; synthetic data only"]
    fn assistant_visual_fixture() {
        let dir = tempfile::Builder::new()
            .prefix("pika-chat-visual-")
            .tempdir_in("/tmp")
            .unwrap()
            .keep();
        for (name, dimensions, snapshot, draft) in [
            (
                "conversation",
                (120, 28),
                json!({"state":"ready","provider":"codex","records":[{"kind":"Finding","origin":"Worker","body":"I help you keep track of your work, remember decisions, and pick up where you left off.\n\nTell me what matters today, or ask me to recall a decision."},{"kind":"Finding","origin":"Human","body":"Tell me about yourself."}]}),
                "What should we focus on today?",
            ),
            (
                "reconnect",
                (80, 24),
                json!({"state":"unavailable","provider":"codex","can_reconnect":true,"records":[],"error":"Transport(internal)"}),
                "Keep this draft",
            ),
            (
                "first-use",
                (60, 24),
                json!({"state":"not_enabled","provider":"none","records":[]}),
                "",
            ),
        ] {
            let frame = compose(&snapshot, draft, "", dimensions, 0).unwrap();
            let rows = crate::terminal_frame::rows(&frame, dimensions).unwrap();
            let rows: Vec<_> = rows
                .into_iter()
                .map(|r| String::from_utf8(r).unwrap())
                .collect();
            std::fs::write(
                dir.join(format!("{name}.json")),
                serde_json::to_vec(&json!({"dimensions":dimensions,"rows":rows})).unwrap(),
            )
            .unwrap();
        }
        println!("VISUAL_FIXTURES={}", dir.display());
    }

    #[test]
    fn restored_startup_error_is_readable_in_details_not_in_conversation() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("assistant");
        let owner = Owner::acquire(&root).unwrap();
        let server = std::thread::spawn(move || {
            owner.serve(|request| {
                assert_eq!(request["operation"], "enable");
                bail!("Transport(RESTORED_STARTUP_DIAGNOSTIC)")
            })
        });
        let mut client = Client::attach(&root).unwrap();
        let mut args = Args {
            enable_codex: Some("/fake/pinned-codex".into()),
            restore_startup: true,
            no_call_limit: true,
            ..Default::default()
        };
        assert!(prepare_enable(&mut client, &mut args).unwrap().is_none());
        let notice = args.startup_notice.unwrap();
        assert!(notice.contains("RESTORED_STARTUP_DIAGNOSTIC"));
        let snapshot = json!({"state":"not_enabled","view_notice":notice});
        let frame = compose(&snapshot, "unsent draft", &notice, (100, 28), 0).unwrap();
        let text = String::from_utf8(frame).unwrap();
        assert!(text.contains("Couldn't connect"));
        assert!(text.contains("unsent draft"));
        assert!(!text.contains("RESTORED_STARTUP_DIAGNOSTIC"));
        assert!(setup::details(&snapshot).contains("RESTORED_STARTUP_DIAGNOSTIC"));
        drop(client);
        server.join().unwrap().unwrap();
    }

    #[test]
    fn worker_diagnostics_and_results_are_available_in_details_without_filling_chat() {
        let snapshot = json!({"state":"ready","records":[],
            "author":{"state":"failed","error":"AUTHOR_DIAGNOSTIC"},
            "method_control":{"state":"failed","error":"METHOD_DIAGNOSTIC","result":{"raw":"METHOD_RESULT"}},
            "recovery":{"state":"failed","error":"RECOVERY_DIAGNOSTIC"},
            "investigation":{"state":"failed","error":"INVESTIGATION_DIAGNOSTIC","text":"Useful partial answer"},
            "cleanup_notice":"CLEANUP_DIAGNOSTIC"});
        let frame = compose(&snapshot, "", "", (100, 60), 0).unwrap();
        let chat = String::from_utf8(frame).unwrap();
        let details = setup::details(&snapshot);
        for marker in [
            "AUTHOR_DIAGNOSTIC",
            "METHOD_DIAGNOSTIC",
            "METHOD_RESULT",
            "RECOVERY_DIAGNOSTIC",
            "INVESTIGATION_DIAGNOSTIC",
            "CLEANUP_DIAGNOSTIC",
        ] {
            assert!(!chat.contains(marker), "{marker}");
            assert!(details.contains(marker), "{marker}");
        }
        assert!(chat.contains("Useful partial answer"));
        assert!(chat.contains("F2 for details"));
    }

    #[test]
    fn disconnected_surface_hides_debug_errors_and_keeps_recovery_visible() {
        let snapshot = json!({"state":"unavailable","provider":"codex","can_reconnect":true,"error":"Transport(SECRET_INTERNAL_ERROR)","records":[]});
        let frame = compose(
            &snapshot,
            "keep my draft",
            "Turn not accepted: Closed. Draft retained; no automatic retry.",
            (80, 24),
            0,
        )
        .unwrap();
        let rows = crate::terminal_frame::rows(&frame, (80, 24)).unwrap();
        let text =
            crate::fleet::sanitize_terminal_lines(&String::from_utf8(rows.concat()).unwrap());
        assert!(text.contains("F2 to reconnect"));
        assert!(text.contains("keep my draft"));
        assert!(text.contains("Couldn't send"));
        assert!(!text.contains("Closed"));
        assert!(!text.contains("SECRET_INTERNAL_ERROR"));
        assert!(text.contains('╭') && text.contains('╰'));
        assert!(needs_connection(&snapshot));
        assert!(!needs_connection(
            &json!({"state":"unavailable","request_id":"accepted-turn","can_reconnect":false})
        ));
    }

    #[test]
    fn first_use_layout_keeps_connection_and_composer_visible_without_diagnostics() {
        let snapshot =
            json!({"scope":"personal","state":"not_enabled","provider":"none","records":[]});
        for width in [60, 100, 200] {
            let frame = compose(
                &snapshot,
                "",
                "Saved briefing opened. Nothing acknowledged; no model call.",
                (width, 24),
                0,
            )
            .unwrap();
            let rows = crate::terminal_frame::rows(&frame, (width, 24)).unwrap();
            let text =
                crate::fleet::sanitize_terminal_lines(&String::from_utf8(rows.concat()).unwrap());
            assert!(text.contains("Connect once to start talking"));
            assert!(text.contains("Enter to connect Pika"));
            assert!(text.contains("F2 settings"));
            assert!(!text.contains("Nothing acknowledged"));
            assert!(!text.contains("provider off"));
            let footer = crate::fleet::sanitize_terminal_lines(
                &String::from_utf8(rows[23].clone()).unwrap(),
            );
            assert!(footer.contains("Esc/F12 board"));
        }
    }
    #[test]
    fn provider_snapshot_notice_does_not_claim_enabled_provider_is_off() {
        for state in ["starting", "ready", "working", "unavailable", "recovering"] {
            let mut snapshot = json!({"provider":"codex", "state":state,
                "notice":"Provider and spending have not been enabled"});
            refresh_provider_notice(&mut snapshot);
            let notice = snapshot["notice"].as_str().unwrap();
            assert!(!notice.contains("not been enabled"), "{state}: {notice}");
            assert!(!notice.is_empty());
        }
        let mut offline = json!({"provider":"none","notice":"Recovery required"});
        refresh_provider_notice(&mut offline);
        assert_eq!(offline["notice"], "Recovery required");
    }

    #[test]
    fn identity_memory_decisions_and_drafts_survive_restart_without_provider() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("private/memory.sqlite");
        let mut memory = Store::open(&path).unwrap();
        let id = memory.profile_id().to_owned();
        handle(
            &mut memory,
            input_request(
                "/decision Prefer comparable results over a faster deadline",
                "research",
            )
            .unwrap(),
        )
        .unwrap();
        handle(
            &mut memory,
            input_request("Please brief me", "research").unwrap(),
        )
        .unwrap();
        drop(memory);
        let mut memory = Store::open(path).unwrap();
        let state = handle(
            &mut memory,
            Request::Snapshot {
                scope: "research".into(),
            },
        )
        .unwrap();
        assert_eq!(state["profile_id"], id);
        assert_eq!(state["records"].as_array().unwrap().len(), 2);
        assert_eq!(state["background_calls"], 0);
        assert_eq!(state["provider"], "none");
        let identity = handle(
            &mut memory,
            input_request("/profile identity", "research").unwrap(),
        )
        .unwrap();
        assert!(identity["local_output"].as_str().unwrap().contains(&id));
        let selected = handle(
            &mut memory,
            input_request("/profile memory", "research").unwrap(),
        )
        .unwrap();
        assert!(
            selected["local_output"]
                .as_str()
                .unwrap()
                .contains("Prefer comparable results")
        );
        assert!(
            !selected["local_output"]
                .as_str()
                .unwrap()
                .contains("Please brief me")
        );
        assert!(
            handle(
                &mut memory,
                Request::Snapshot {
                    scope: "other".into()
                }
            )
            .unwrap()["records"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }
    #[test]
    fn skip_and_defer_do_not_create_decisions_or_pause_projects() {
        assert!(matches!(
            input_request("/skip not now", "p").unwrap(),
            Request::Save {
                kind: SaveKind::Grasp,
                ..
            }
        ));
        assert!(matches!(
            input_request("/defer tomorrow", "p").unwrap(),
            Request::Save {
                kind: SaveKind::Grasp,
                ..
            }
        ));
        assert!(input_request("/grant unlimited", "p").is_err());
    }
    #[test]
    fn parsed_decision_brief_recall_and_explain_render_with_scope() {
        let temporary = tempfile::tempdir().unwrap();
        let mut memory = Store::open(temporary.path().join("private/memory.sqlite")).unwrap();
        let text = r#"/decision-json {"chosen":"cache snapshots","rationale":"avoid polling","rejected":["per-view scanners"],"owner":"user","open_questions":["staleness"],"commitments":["measure freshness"]}"#;
        let saved = handle(&mut memory, input_request(text, "research").unwrap()).unwrap();
        let id = saved["saved"].as_str().unwrap();
        let brief = handle(&mut memory, input_request("/brief", "research").unwrap()).unwrap();
        let frame = compose(&brief, "", "", (120, 80), 0).unwrap();
        let rendered = String::from_utf8_lossy(&frame);
        assert!(rendered.contains("Decisions"));
        assert!(rendered.contains("Commitments"));
        assert!(rendered.contains("measure freshness"));
        assert!(rendered.contains("Why · avoid polling"));
        assert!(!rendered.contains("\"chosen\""));
        let recall = handle(
            &mut memory,
            input_request(&format!("/why {id}"), "research").unwrap(),
        )
        .unwrap();
        assert!(
            String::from_utf8_lossy(&compose(&recall, "", "", (120, 80), 0).unwrap())
                .contains("per-view scanners")
        );
        let explain = handle(
            &mut memory,
            input_request(
                &format!("/explain {id} {{\"choice\":\"cache\",\"why\":\"reduce polling\"}}"),
                "research",
            )
            .unwrap(),
        )
        .unwrap();
        let frame = compose(&explain, "", "", (120, 80), 0).unwrap();
        assert!(String::from_utf8_lossy(&frame).contains("Which alternative"));
        assert!(
            handle(
                &mut memory,
                input_request(&format!("/why {id}"), "other").unwrap()
            )
            .is_err()
        );
    }
    #[test]
    fn cached_output_is_fenced_when_another_view_forgets() {
        let mut epoch = Some(0);
        let mut cached = "forgotten recall".to_owned();
        fence_cached_output(&json!({"memory_epoch": 0}), &mut epoch, &mut cached);
        assert_eq!(cached, "forgotten recall");
        fence_cached_output(&json!({"memory_epoch": 1}), &mut epoch, &mut cached);
        assert!(cached.is_empty());
    }
}
