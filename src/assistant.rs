//! Board-home assistant surface. Durable memory is independent of provider sessions.
use crate::{
    assistant_host::{Client, Owner},
    assistant_memory::{DecisionState, NewRecord, Origin, RecordKind, Scope, Store},
};
use anyhow::{Result, bail};
use crossterm::{
    cursor::{Hide, MoveTo, Show},
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
        MouseEventKind,
    },
    execute, queue,
    style::{Color, Print, ResetColor, SetForegroundColor},
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

#[derive(clap::Args, Debug, Default)]
pub(crate) struct Args {
    /// Emit durable assistant state without starting a provider.
    #[arg(long)]
    pub json: bool,
    /// Explicit personal/project memory scope (no implicit project content access).
    #[arg(long, default_value = "personal")]
    pub scope: String,
    /// Save an explicit user instruction without calling a model.
    #[arg(long, conflicts_with = "decision")]
    pub remember: Option<String>,
    /// Save a decision and rationale in your own words.
    #[arg(long)]
    pub decision: Option<String>,
    /// Explicitly enable this foreground assistant with an absolute Codex binary.
    /// Uses assistant/provider-home only; never copies existing credentials.
    #[arg(long, requires = "max_calls", conflicts_with_all = ["remember", "decision"])]
    pub enable_codex: Option<std::path::PathBuf>,
    /// Total lifetime call allowance, not a dollar ceiling (1–100).
    #[arg(long, requires = "enable_codex")]
    pub max_calls: Option<u64>,
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
    },
    Send {
        scope: String,
        request_id: String,
        body: String,
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
    Recall {
        scope: String,
        record_id: String,
    },
    Correct {
        scope: String,
        request_id: String,
        record_id: String,
        body: String,
    },
    Investigate {
        scope: String,
        request_id: String,
        body: String,
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
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SaveKind {
    Draft,
    Instruction,
    Decision,
    Grasp,
}

fn scope(value: &str) -> Result<Scope> {
    if value.is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
        bail!("Choose a scope of 1–256 printable bytes");
    }
    Ok(Scope {
        project: Some(value.into()),
        ..Scope::default()
    })
}

pub(crate) fn serve(root: &Path) -> Result<i32> {
    let owner = Owner::acquire(root)?;
    let mut memory = Store::open(root.join("memory.sqlite"))?;
    crate::assistant_retention::cleanup(root, memory.forget_epoch()?)?;
    let mut session = crate::assistant_session::Session::new();
    let mut investigations = crate::assistant_investigation_ui::Investigations::default();
    let mut recovery: Option<crate::assistant_recovery_service::RecoveryService> = None;
    let mut restored_recovery = None;
    session.set_workshop_root(root)?;
    owner.serve_with_idle(move |request| {
        if let Some(state) = recovery.as_ref().map(|service| service.snapshot()) {
            if state.state == "completed" && restored_recovery.as_ref() != Some(&state.receipt_id) {
                session.set_workshop_root(root)?;
                restored_recovery = Some(state.receipt_id);
            }
        }
        let Some(request) = request else {
            session.tick()?;
            return Ok(None);
        };
        handle_session(
            root,
            &mut memory,
            &mut session,
            &mut investigations,
            &mut recovery,
            serde_json::from_value(request)?,
        )
        .map(Some)
    })?;
    Ok(0)
}

fn handle_session(
    root: &Path,
    memory: &mut Store,
    session: &mut crate::assistant_session::Session,
    investigations: &mut crate::assistant_investigation_ui::Investigations,
    recovery: &mut Option<crate::assistant_recovery_service::RecoveryService>,
    request: Request,
) -> Result<Value> {
    if recovery.as_ref().is_some_and(|r| r.busy())
        && !matches!(request, Request::Snapshot { .. } | Request::Help)
    {
        bail!(
            "Pika is finishing recovery of its owned assistant jobs. New changes are blocked until cleanup finishes; project agents are untouched."
        );
    }
    if let Some(result) = handle_session_tools(root, session, investigations, &request)? {
        return Ok(result);
    }
    if let Some(result) = handle_session_provider(root, session, investigations, &request)? {
        return Ok(result);
    }
    if let Some(result) =
        handle_session_recovery(root, memory, session, investigations, recovery, &request)?
    {
        return Ok(result);
    }
    if let Some(result) =
        handle_session_snapshot(memory, session, investigations, recovery, &request)?
    {
        return Ok(result);
    }
    if let Some(result) = handle_session_forget(root, memory, session, investigations, &request)? {
        return Ok(result);
    }
    handle(memory, request)
}

fn handle_session_tools(
    root: &Path,
    session: &mut crate::assistant_session::Session,
    investigations: &mut crate::assistant_investigation_ui::Investigations,
    request: &Request,
) -> Result<Option<Value>> {
    match request {
        Request::EvolveSpec {
            scope,
            request_id,
            spec,
        } => {
            if investigations.busy() {
                bail!("Wait for or cancel the running investigation");
            }
            session.evolve_spec(root, scope, request_id, spec)?;
            Ok(Some(
                json!({"accepted":true,"notice":"Authoring a pure tool against your protected contrasting cases. Exact-version approval still required."}),
            ))
        }
        Request::Evolve { scope, request_id } => {
            if investigations.busy() {
                bail!("Wait for or cancel the running investigation");
            }
            session.evolve(root, scope, request_id)?;
            Ok(Some(
                json!({"accepted":true,"notice":"Creating a scoped pure-data tool. Evaluation follows; activation still needs your exact approval."}),
            ))
        }
        Request::ApproveTool { scope, hash } => {
            let grant = session.approve_evolution(scope, hash)?;
            Ok(Some(
                json!({"grant_id":grant,"notice":format!("Exact tool activated for one hour. Revocation: /revoke {grant}")}),
            ))
        }
        Request::RevokeTool { scope, grant_id } => {
            session.revoke_evolution(scope, grant_id)?;
            Ok(Some(json!({"notice":"Tool grant revoked."})))
        }
        Request::RollbackTool { scope, name, hash } => {
            let grant = session.rollback_evolution(scope, name, hash)?;
            Ok(Some(
                json!({"notice":format!("Rolled back to the approved exact version for 1 hour. /revoke {grant}"),"grant_id":grant}),
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

fn handle_session_provider(
    root: &Path,
    session: &mut crate::assistant_session::Session,
    investigations: &mut crate::assistant_investigation_ui::Investigations,
    request: &Request,
) -> Result<Option<Value>> {
    match request {
        Request::Investigate {
            scope,
            request_id,
            body,
        } => {
            let status = session.snapshot(scope);
            if session.busy() || status["state"] != "ready" || status["provider"] != "codex" {
                bail!("Wait for the foreground provider to be ready; no investigation was sent");
            }
            investigations.begin(scope, request_id, body)?;
            Ok(Some(
                json!({"accepted":true,"notice":"Investigating with disposable workers · up to 3 calls from your existing allowance · no project-thread access."}),
            ))
        }
        Request::Enable {
            scope: name,
            executable,
            max_calls,
        } => {
            scope(name)?;
            session.enable(root, name, executable.clone(), *max_calls)?;
            investigations.enable(root, executable.clone(), name);
            Ok(Some(
                json!({"state":"starting","notice":"Checking isolated provider. No background calls; monetary cost unknown."}),
            ))
        }
        Request::Send {
            scope: name,
            request_id,
            body,
        } => {
            scope(name)?;
            if investigations.busy() {
                bail!("Wait for or cancel the running investigation");
            }
            session.begin(name, request_id, body)?;
            Ok(Some(
                json!({"accepted":true,"request_id":request_id,"notice":"Accepted once; do not resend if delivery becomes unknown."}),
            ))
        }
        _ => Ok(None),
    }
}

fn handle_session_recovery(
    root: &Path,
    memory: &mut Store,
    session: &mut crate::assistant_session::Session,
    investigations: &mut crate::assistant_investigation_ui::Investigations,
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
            let old_investigations = std::mem::take(investigations);
            let old_session = std::mem::replace(session, crate::assistant_session::Session::new());
            *recovery = Some(crate::assistant_recovery_service::RecoveryService::start(
                root.to_path_buf(),
                request_id.clone(),
                memory.forget_epoch()?,
                move || {
                    drop(old_session);
                    drop(old_investigations);
                    Ok(())
                },
            )?);
            Ok(Some(
                json!({"notice":"Recovery started. Pika is stopping only its owned assistant jobs; your board and project agents remain available.","recovery_id":request_id,"local_output":"Recovery pending. No old request will be replayed or refunded. Saved memory stays; re-enable the provider explicitly after completion."}),
            ))
        }
        Request::Cancel => {
            session.cancel()?;
            investigations.cancel()?;
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
    investigations: &mut crate::assistant_investigation_ui::Investigations,
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
    snapshot["investigation"] = investigations.snapshot(name);
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
    investigations: &mut crate::assistant_investigation_ui::Investigations,
    request: &Request,
) -> Result<Option<Value>> {
    let Request::Forget { record_id } = request else {
        return Ok(None);
    };
    if memory.get(record_id)?.is_none() {
        bail!("Memory record not found; nothing was changed");
    }
    investigations.forget()?;
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
        Some("working") => "Request in progress. No automatic retry or background calls.",
        Some("recovering" | "cancelling") =>
            "Stopping owned work; uncertain requests are not replayed.",
        _ =>
            "Provider unavailable. Saved memory remains; failed requests are not retried automatically.",
    });
}

fn handle(memory: &mut Store, request: Request) -> Result<Value> {
    if is_display_request(&request) {
        return handle_display(memory, request);
    }
    if is_memory_write(&request) {
        return handle_memory_write(memory, request);
    }
    bail!("Provider operation requires the foreground host")
}

fn is_display_request(request: &Request) -> bool {
    matches!(
        request,
        Request::Help | Request::Brief { .. } | Request::Recall { .. } | Request::Snapshot { .. }
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
        Request::Help => Ok(
            json!({"local_output":"PIKA · commands\n/remember TEXT · scoped instruction\n/decision TEXT · save your decision and rationale\n/decision-json JSON · chosen, rationale, rejected[], owner, open_questions[], commitments[]\n/brief · saved evidence, no model call\n/why RECORD_ID · historical decision\n/correct RECORD_ID TEXT · scoped correction\n/explain RECORD_ID JSON · choice, why, alternative, remaining_question\n/skip REASON or /defer REASON · no penalty or project pause\n/investigate QUESTION · 2 disposable workers + synthesis (up to 3 calls)\n/evolve · scoped pure-tool experiment (1 call); approval still required\n/evolve-json JSON · specify need and 2–8 contrasting cases\n/approve HASH · approve exact tested version for 1 hour\n/tool NAME JSON_ARRAY · pure local invocation\n/revoke GRANT_ID or /rollback NAME HASH\n/forget RECORD_ID · delete dependent Pika memory; provider retention unchanged\n/cancel · request owned-work cancellation; uncertain delivery is not retried\n/fresh-context · explain explicit recovery without replay or refund\nEsc/F12 returns to the board. PgUp/PgDn or mouse wheel scrolls."}),
        ),
        Request::Brief { scope: name } => handle_brief(memory, &name),
        Request::Recall {
            scope: name,
            record_id,
        } => handle_recall(memory, &name, &record_id),
        Request::Snapshot { scope: name } => handle_snapshot(memory, &name),
        _ => bail!("Provider operation requires the foreground host"),
    }
}

fn handle_brief(memory: &mut Store, name: &str) -> Result<Value> {
    let selected = scope(name)?;
    let records = memory.recent(&selected, 256)?;
    let brief = crate::assistant_briefing::build(&records, &[selected], 0);
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
    let records = memory.recent(&selected, 32)?;
    let brief = crate::assistant_briefing::build(&records, &[selected], 0);
    Ok(
        json!({"profile_id":memory.profile_id(),"memory_epoch":memory.forget_epoch()?,"scope":name,"state":"not_enabled","provider":"none","background_calls":0,"records":records,"brief":brief,"notice":"Local memory ready. A provider and spending allowance have not been enabled. Drafts remain unsent."}),
    )
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
        } => handle_correct(memory, &name, &request_id, record_id, body),
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
) -> Result<Value> {
    let previous = memory
        .get(&record_id)?
        .ok_or_else(|| anyhow::anyhow!("Memory not found"))?;
    let selected = scope(name)?;
    if previous.scope != selected || body.trim().is_empty() || body.len() > 16 * 1024 {
        bail!("Correction must stay in the exact original scope and contain 1–16384 bytes");
    }
    let corrected = memory.append_idempotent(
        request_id,
        NewRecord {
            kind: RecordKind::Correction,
            origin: Origin::Human,
            scope: selected,
            body,
            provenance: "explicit user correction".into(),
            timestamp: SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64,
            supersedes: Some(record_id.clone()),
            dependencies: vec![record_id],
            decision_state: None,
            protected_policy: false,
        },
    )?;
    Ok(
        json!({"saved":corrected.id,"notice":"Correction saved for this scope; the earlier wording remains dated history. To test a tool improvement, use /evolve-json with this correction_id, a need, and contrasting cases. Nothing runs automatically."}),
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
    normalize_args(&mut args);
    scope(&args.scope)?;
    let root = crate::paths::Paths::discover()?.state_dir.join("assistant");
    let mut client = Client::attach(&root)?;
    if let Some(result) = enable_from_args(&mut client, &args)? {
        return Ok(result);
    }
    let command = save_command(&args);
    if let Some((kind, body)) = command {
        return save_args(&mut client, &args.scope, args.json, kind, body);
    }
    if args.json || !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        println!(
            "{}",
            send(&mut client, Request::Snapshot { scope: args.scope })?
        );
        return Ok(0);
    }
    view(&mut client, &args.scope)?;
    Ok(0)
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
            max_calls: args.max_calls.unwrap_or(0),
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

fn view(client: &mut Client, scope: &str) -> Result<()> {
    let _screen = Screen::enter()?;
    let mut view = AssistantView::new(client, scope)?;
    loop {
        view.paint_if_dirty()?;
        if !event::poll(Duration::from_millis(100))? {
            view.refresh_if_due()?;
            continue;
        }
        if !view.handle_event(event::read()?)? {
            return Ok(());
        }
    }
}

struct AssistantView<'a> {
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
        Ok(Self {
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
        })
    }

    fn paint_if_dirty(&mut self) -> Result<()> {
        if !self.dirty {
            return Ok(());
        }
        self.snapshot["local_output"] = json!(self.local_output);
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
        let request = if !self.draft.starts_with('/') && self.snapshot["provider"] == "codex" {
            Ok(Request::Send {
                scope: self.scope.into(),
                request_id: uuid::Uuid::new_v4().to_string(),
                body: self.draft.clone(),
            })
        } else {
            input_request(&self.draft, self.scope)
        };
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

    fn send_draft(&mut self, request: Request) {
        match send(self.client, request) {
            Ok(reply) => self.accept_reply(reply),
            Err(error) => self.notice = format!("{error}. Draft retained; no automatic retry."),
        }
    }

    fn accept_reply(&mut self, reply: Value) {
        self.local_output = reply["local_output"].as_str().unwrap_or("").to_owned();
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
    if let Some(request) = input_explain(text, scope)? {
        return Ok(request);
    }
    if let Some(request) = input_recall(text, scope) {
        return Ok(request);
    }
    if let Some(request) = input_investigation(text, scope) {
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
        })
}

fn input_memory(text: &str, scope: &str) -> Result<Option<Request>> {
    if let Some(rest) = text.strip_prefix("/correct ") {
        let (id, body) = rest
            .split_once(' ')
            .ok_or_else(|| anyhow::anyhow!("Use /correct RECORD_ID NEW_WORDING"))?;
        return Ok(Some(Request::Correct {
            scope: scope.into(),
            request_id: uuid::Uuid::new_v4().to_string(),
            record_id: id.into(),
            body: body.into(),
        }));
    }
    for (prefix, kind) in [
        ("/remember ", SaveKind::Instruction),
        ("/decision ", SaveKind::Decision),
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

fn input_tools(text: &str, scope: &str) -> Result<Option<Request>> {
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
    if columns < 8 || height < 8 {
        return resize_frame(columns, height);
    }
    let mut lines = header_lines(snapshot);
    append_board(&mut lines);
    append_output(snapshot, &mut lines);
    append_provider_status(snapshot, &mut lines);
    append_recovery_status(snapshot, &mut lines);
    append_investigation(snapshot, &mut lines);
    append_tool_status(snapshot, &mut lines);
    append_records(snapshot, &mut lines);
    render_lines(lines, draft, notice, dimensions, scroll)
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
    vec![
        "PIKA · your persistent assistant".to_owned(),
        format!(
            "{} · {} · background spending off",
            match snapshot["state"].as_str() {
                Some("ready") => "Ready",
                Some("working") => "Thinking",
                Some("starting") => "Connecting",
                Some("recovering") => "Recovering",
                Some("cancelling") => "Stopping this request",
                Some("unavailable" | "stopped") => "Provider unavailable",
                _ => "Local memory · provider off",
            },
            snapshot["scope"].as_str().unwrap_or("personal")
        ),
        String::new(),
    ]
}

fn append_board(lines: &mut Vec<String>) {
    if let Some(crate::activity_feed::Context::Source(source)) = crate::activity_feed::current()
        && let Some(state) = source.snapshot()
    {
        let [needs, working, ready, parked] = state.summary.counts;
        lines.push(format!(
            "Board · {needs} need you · {working} working · {ready} ready · {parked} parked"
        ));
        lines.push("Shared board feed · no extra scan · source unread unchanged".into());
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
    if let Some(error) = snapshot["error"].as_str() {
        lines.push(format!("Provider unavailable · {error}"));
    }
    if let Some(author) = snapshot["author"].as_object() {
        lines.push(format!(
            "Disposable tool author · {}",
            author
                .get("state")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
        ));
        if let Some(error) = author.get("error").and_then(Value::as_str) {
            lines.push(format!("Author incomplete · {error}"));
        }
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
        if let Some(error) = recovery.get("error").and_then(Value::as_str) {
            lines.push(format!("Recovery incomplete · {error}"));
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
        if let Some(error) = investigation.get("error").and_then(Value::as_str) {
            lines.push(format!("Incomplete · {error}"));
        }
    }
    if let Some(partial) = snapshot["partial"].as_str().filter(|text| !text.is_empty()) {
        lines.push(format!("Pika · {partial}"));
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
                lines.push(format!("/approve {hash}"));
            }
        }
    }
}

fn append_records(snapshot: &Value, lines: &mut Vec<String>) {
    if let Some(records) = snapshot["records"]
        .as_array()
        .filter(|_| snapshot["local_output"].as_str().is_none_or(str::is_empty))
    {
        if records.is_empty() {
            lines.push("Start with what matters to you. /remember saves an instruction.".into());
        }
        for record in records.iter().rev() {
            lines.push(format!(
                "{} · {} · {}",
                record["kind"].as_str().unwrap_or("Memory"),
                record["id"].as_str().unwrap_or(""),
                crate::assistant_briefing::readable_decision(record["body"].as_str().unwrap_or(""))
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
) -> Result<Vec<u8>> {
    let (columns, height) = dimensions;
    let width = usize::from(columns.saturating_sub(4));
    let colors = std::env::var_os("NO_COLOR").is_none();
    let mut buffer = Vec::new();
    queue!(buffer, MoveTo(0, 0), Clear(ClearType::All))?;
    let wrapped = lines
        .into_iter()
        .flat_map(|line| wrap(&line, width))
        .collect::<Vec<_>>();
    let visible = usize::from(height.saturating_sub(6));
    let scroll = scroll.min(wrapped.len().saturating_sub(visible));
    for (index, line) in wrapped
        .into_iter()
        .skip(scroll)
        .take(usize::from(height.saturating_sub(6)))
        .enumerate()
    {
        queue!(buffer, MoveTo(2, index as u16))?;
        if colors && index == 0 {
            queue!(buffer, SetForegroundColor(Color::Cyan))?;
        }
        queue!(buffer, Print(line), ResetColor)?;
    }
    for (offset, text) in [
        (5, notice.to_owned()),
        (3, format!("› {draft}")),
        (
            1,
            "Esc/F12 board · PgUp/PgDn scroll · /help · Ctrl+C clear draft".into(),
        ),
    ] {
        let wrapped = wrap(&text, width);
        queue!(
            buffer,
            MoveTo(2, height.saturating_sub(offset)),
            Print(
                if offset == 3 {
                    wrapped.last()
                } else {
                    wrapped.first()
                }
                .cloned()
                .unwrap_or_default()
            )
        )?;
    }
    Ok(buffer)
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
