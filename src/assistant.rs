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
    match request {
        Request::EvolveSpec {
            scope,
            request_id,
            spec,
        } => {
            if investigations.busy() {
                bail!("Wait for or cancel the running investigation");
            }
            session.evolve_spec(root, &scope, &request_id, &spec)?;
            Ok(
                json!({"accepted":true,"notice":"Authoring a pure tool against your protected contrasting cases. Exact-version approval still required."}),
            )
        }
        Request::Investigate {
            scope,
            request_id,
            body,
        } => {
            let status = session.snapshot(&scope);
            if session.busy() || status["state"] != "ready" || status["provider"] != "codex" {
                bail!("Wait for the foreground provider to be ready; no investigation was sent");
            }
            investigations.begin(&scope, &request_id, &body)?;
            Ok(
                json!({"accepted":true,"notice":"Investigating with disposable workers · up to 3 calls from your existing allowance · no project-thread access."}),
            )
        }
        Request::Evolve { scope, request_id } => {
            if investigations.busy() {
                bail!("Wait for or cancel the running investigation");
            }
            session.evolve(root, &scope, &request_id)?;
            Ok(
                json!({"accepted":true,"notice":"Creating a scoped pure-data tool. Evaluation follows; activation still needs your exact approval."}),
            )
        }
        Request::ApproveTool { scope, hash } => {
            let grant = session.approve_evolution(&scope, &hash)?;
            Ok(
                json!({"grant_id":grant,"notice":format!("Exact tool activated for one hour. Revocation: /revoke {grant}")}),
            )
        }
        Request::RevokeTool { scope, grant_id } => {
            session.revoke_evolution(&scope, &grant_id)?;
            Ok(json!({"notice":"Tool grant revoked."}))
        }
        Request::RollbackTool { scope, name, hash } => {
            let grant = session.rollback_evolution(&scope, &name, &hash)?;
            Ok(
                json!({"notice":format!("Rolled back to the approved exact version for 1 hour. /revoke {grant}"),"grant_id":grant}),
            )
        }
        Request::InvokeTool {
            scope,
            name,
            inputs,
        } => {
            let output = session.invoke_evolution(&scope, &name, &inputs)?;
            Ok(
                json!({"notice":format!("Tool result · {}{}",output.value,output.provenance_warning.as_ref().map(|warning|format!("\n{warning}")).unwrap_or_default()),"tool_result":output.value,"provenance_warning":output.provenance_warning}),
            )
        }
        Request::Enable {
            scope: name,
            executable,
            max_calls,
        } => {
            scope(&name)?;
            session.enable(root, &name, executable.clone(), max_calls)?;
            investigations.enable(root, executable, &name);
            Ok(
                json!({"state":"starting","notice":"Checking isolated provider. No background calls; monetary cost unknown."}),
            )
        }
        Request::Send {
            scope: name,
            request_id,
            body,
        } => {
            scope(&name)?;
            if investigations.busy() {
                bail!("Wait for or cancel the running investigation");
            }
            session.begin(&name, &request_id, &body)?;
            Ok(
                json!({"accepted":true,"request_id":request_id,"notice":"Accepted once; do not resend if delivery becomes unknown."}),
            )
        }
        Request::FreshContext { request_id } => {
            if let Some(receipt) = crate::assistant_recovery::existing_receipt(root, &request_id)? {
                return Ok(
                    json!({"recovery_id":receipt.receipt_id,"notice":"This recovery was already recorded. Current work was not touched."}),
                );
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
            Ok(
                json!({"notice":"Recovery started. Pika is stopping only its owned assistant jobs; your board and project agents remain available.","recovery_id":request_id,"local_output":"Recovery pending. No old request will be replayed or refunded. Saved memory stays; re-enable the provider explicitly after completion."}),
            )
        }
        Request::Cancel => {
            session.cancel()?;
            investigations.cancel()?;
            Ok(
                json!({"notice":"Cancellation requested; delivery remains uncertain until verified."}),
            )
        }
        Request::Snapshot { scope: name } => {
            let mut snapshot = handle(
                memory,
                Request::Snapshot {
                    scope: name.clone(),
                },
            )?;
            for (key, value) in session.snapshot(&name).as_object().unwrap() {
                snapshot[key] = value.clone();
            }
            snapshot["investigation"] = investigations.snapshot(&name);
            if let Some(recovery) = recovery.as_ref() {
                let state = recovery.snapshot();
                snapshot["recovery"] = serde_json::to_value(&state)?;
                if recovery.busy() {
                    snapshot["state"] = json!("recovering");
                }
            }
            refresh_provider_notice(&mut snapshot);
            Ok(snapshot)
        }
        Request::Forget { record_id } => {
            if memory.get(&record_id)?.is_none() {
                bail!("Memory record not found; nothing was changed");
            }
            investigations.forget()?;
            session.forget()?;
            let mut result = handle(memory, Request::Forget { record_id })?;
            crate::assistant_retention::cleanup(root, memory.forget_epoch()?)?;
            result["notice"] = json!(
                "Forgotten from active Pika memory. Derived tool experiments and cached replies were cleared conservatively. Provider logs and external backups are not deleted; provider continuation is blocked."
            );
            Ok(result)
        }
        request => handle(memory, request),
    }
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
    match request {
        Request::Help => Ok(
            json!({"local_output":"PIKA · commands\n/remember TEXT · scoped instruction\n/decision TEXT · save your decision and rationale\n/decision-json JSON · chosen, rationale, rejected[], owner, open_questions[], commitments[]\n/brief · saved evidence, no model call\n/why RECORD_ID · historical decision\n/correct RECORD_ID TEXT · scoped correction\n/explain RECORD_ID JSON · choice, why, alternative, remaining_question\n/skip REASON or /defer REASON · no penalty or project pause\n/investigate QUESTION · 2 disposable workers + synthesis (up to 3 calls)\n/evolve · scoped pure-tool experiment (1 call); approval still required\n/evolve-json JSON · specify need and 2–8 contrasting cases\n/approve HASH · approve exact tested version for 1 hour\n/tool NAME JSON_ARRAY · pure local invocation\n/revoke GRANT_ID or /rollback NAME HASH\n/forget RECORD_ID · delete dependent Pika memory; provider retention unchanged\n/cancel · request owned-work cancellation; uncertain delivery is not retried\n/fresh-context · explain explicit recovery without replay or refund\nEsc/F12 returns to the board. PgUp/PgDn or mouse wheel scrolls."}),
        ),
        Request::Explain {
            scope: name,
            record_id,
            reply,
        } => {
            let selected = scope(&name)?;
            let record = memory
                .get(&record_id)?
                .ok_or_else(|| anyhow::anyhow!("Decision not found"))?;
            let recalled = crate::assistant_briefing::recall(&record, &selected)
                .map_err(anyhow::Error::msg)?;
            let gap = recalled
                .structured
                .as_ref()
                .and_then(|decision| crate::assistant_briefing::explanation_gap(decision, &reply));
            memory.append(NewRecord {
                kind: RecordKind::GraspInteraction,
                origin: Origin::Human,
                scope: selected,
                body: serde_json::to_string(&reply)?,
                provenance: "user explain-back; completeness prompt only, not semantic grading"
                    .into(),
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
        Request::Brief { scope: name } => {
            let scope = scope(&name)?;
            let records = memory.recent(&scope, 256)?;
            let brief = crate::assistant_briefing::build(&records, &[scope], 0);
            let mut text = vec![
                "Saved evidence · no model call".into(),
                brief.coverage.clone(),
            ];
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
            Ok(json!({"brief":brief,"local_output":text.join("\n")}))
        }
        Request::Recall {
            scope: name,
            record_id,
        } => {
            let record = memory
                .get(&record_id)?
                .ok_or_else(|| anyhow::anyhow!("Decision not found"))?;
            let recalled = crate::assistant_briefing::recall(&record, &scope(&name)?)
                .map_err(anyhow::Error::msg)?;
            Ok(
                json!({"recall":recalled,"local_output":format!("{}\n{}",crate::assistant_briefing::readable_decision(&recalled.original_words),recalled.caveat)}),
            )
        }
        Request::Correct {
            scope: name,
            request_id,
            record_id,
            body,
        } => {
            let previous = memory
                .get(&record_id)?
                .ok_or_else(|| anyhow::anyhow!("Memory not found"))?;
            let selected = scope(&name)?;
            if previous.scope != selected || body.trim().is_empty() || body.len() > 16 * 1024 {
                bail!("Correction must stay in the exact original scope and contain 1–16384 bytes");
            }
            let corrected = memory.append_idempotent(
                &request_id,
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
        Request::Snapshot { scope: name } => {
            let records = memory.recent(&scope(&name)?, 32)?;
            let brief = crate::assistant_briefing::build(&records, &[scope(&name)?], 0);
            Ok(
                json!({"profile_id":memory.profile_id(),"memory_epoch":memory.forget_epoch()?,"scope":name,"state":"not_enabled","provider":"none","background_calls":0,"records":records,"brief":brief,"notice":"Local memory ready. A provider and spending allowance have not been enabled. Drafts remain unsent."}),
            )
        }
        Request::Save {
            request_id,
            scope: name,
            kind,
            body,
            timestamp,
        } => {
            if body.trim().is_empty() || body.len() > 16 * 1024 {
                bail!("Message must contain 1–16384 bytes");
            }
            let (record_kind, decision_state) = match kind {
                SaveKind::Draft => (RecordKind::Draft, None),
                SaveKind::Instruction => (RecordKind::UserInstruction, None),
                SaveKind::Decision => (RecordKind::Decision, Some(DecisionState::Accepted)),
                SaveKind::Grasp => (RecordKind::GraspInteraction, None),
            };
            let record = memory.append_idempotent(
                &request_id,
                NewRecord {
                    kind: record_kind,
                    origin: Origin::Human,
                    scope: scope(&name)?,
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
        Request::Forget { record_id } => {
            let count = memory.forget(&record_id)?;
            Ok(
                json!({"forgotten":count,"notice":"Removed from active Pika memory. External backups and provider retention are not affected."}),
            )
        }
        Request::Enable { .. }
        | Request::Send { .. }
        | Request::Cancel
        | Request::FreshContext { .. }
        | Request::Evolve { .. }
        | Request::ApproveTool { .. }
        | Request::RevokeTool { .. }
        | Request::RollbackTool { .. }
        | Request::InvokeTool { .. }
        | Request::Investigate { .. }
        | Request::EvolveSpec { .. } => {
            bail!("Provider operation requires the foreground host")
        }
    }
}

pub(crate) fn run(mut args: Args) -> Result<i32> {
    if args.scope.is_empty() {
        args.scope = "personal".into();
    }
    scope(&args.scope)?;
    let root = crate::paths::Paths::discover()?.state_dir.join("assistant");
    let mut client = Client::attach(&root)?;
    if let Some(executable) = args.enable_codex {
        let reply = send(
            &mut client,
            Request::Enable {
                scope: args.scope.clone(),
                executable,
                max_calls: args.max_calls.unwrap_or(0),
            },
        )?;
        if args.json || !io::stdin().is_terminal() {
            println!("{reply}");
            return Ok(0);
        }
    }
    let command = args
        .remember
        .as_ref()
        .map(|body| (SaveKind::Instruction, body))
        .or_else(|| {
            args.decision
                .as_ref()
                .map(|body| (SaveKind::Decision, body))
        });
    if let Some((kind, body)) = command {
        let reply = send(&mut client, save(&args.scope, kind, body))?;
        if args.json {
            println!("{}", serde_json::to_string(&reply)?);
        } else {
            println!(
                "Saved in Pika memory · scope {} · no provider call",
                crate::fleet::sanitize_terminal_text(&args.scope)
            );
        }
        return Ok(0);
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
    let mut presenter = crate::monitor::FramePresenter::default();
    let mut draft = String::new();
    let mut snapshot = send(
        client,
        Request::Snapshot {
            scope: scope.into(),
        },
    )?;
    let mut notice = snapshot["notice"].as_str().unwrap_or_default().to_owned();
    let mut dirty = true;
    let mut scroll = 0usize;
    let mut local_output = String::new();
    let mut last_memory_epoch = snapshot["memory_epoch"].as_u64();
    let mut last_refresh = std::time::Instant::now();
    loop {
        if dirty {
            snapshot["local_output"] = json!(local_output);
            paint(&snapshot, &draft, &notice, scroll, &mut presenter)?;
            dirty = false;
        }
        if !event::poll(Duration::from_millis(100))? {
            if last_refresh.elapsed() >= Duration::from_secs(1) {
                let next = send(
                    client,
                    Request::Snapshot {
                        scope: scope.into(),
                    },
                )?;
                if snapshot["notice"].as_str() == Some(notice.as_str()) {
                    notice = next["notice"].as_str().unwrap_or_default().to_owned();
                }
                fence_cached_output(&next, &mut last_memory_epoch, &mut local_output);
                dirty = true;
                snapshot = next;
                last_refresh = std::time::Instant::now();
            }
            continue;
        }
        match event::read()? {
            Event::Mouse(mouse) => {
                match mouse.kind {
                    MouseEventKind::ScrollDown => scroll = scroll.saturating_add(3),
                    MouseEventKind::ScrollUp => scroll = scroll.saturating_sub(3),
                    _ => {}
                }
                dirty = true;
            }
            Event::Resize(_, _) => dirty = true,
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                match key.code {
                    KeyCode::Esc | KeyCode::F(12) => return Ok(()),
                    KeyCode::PageDown => {
                        scroll = scroll.saturating_add(
                            usize::from(terminal::size()?.1.saturating_sub(6)).max(1),
                        )
                    }
                    KeyCode::PageUp => {
                        scroll = scroll.saturating_sub(
                            usize::from(terminal::size()?.1.saturating_sub(6)).max(1),
                        )
                    }
                    KeyCode::Home => scroll = 0,
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        draft.clear()
                    }
                    KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        draft.clear()
                    }
                    KeyCode::Backspace => {
                        draft.pop();
                    }
                    KeyCode::Char(ch)
                        if !key.modifiers.contains(KeyModifiers::CONTROL)
                            && draft.len() + ch.len_utf8() <= 16 * 1024 =>
                    {
                        draft.push(ch)
                    }
                    KeyCode::Enter if !draft.trim().is_empty() => {
                        let request = if !draft.starts_with('/') && snapshot["provider"] == "codex"
                        {
                            Ok(Request::Send {
                                scope: scope.into(),
                                request_id: uuid::Uuid::new_v4().to_string(),
                                body: draft.clone(),
                            })
                        } else {
                            input_request(&draft, scope)
                        };
                        match request {
                            Ok(request) => match send(client, request) {
                                Ok(reply) => {
                                    local_output =
                                        reply["local_output"].as_str().unwrap_or("").to_owned();
                                    if draft.starts_with("/forget ") {
                                        local_output.clear();
                                    }
                                    notice = reply["notice"].as_str().map(str::to_owned).unwrap_or_else(|| if reply["accepted"] == true { "Sent once. Waiting for Pika; no automatic retry." } else if draft.starts_with('/') { "Saved locally. No provider call." } else { "Draft saved, not sent. A provider and spending allowance must be enabled first." }.into());
                                    draft.clear();
                                    scroll = 0;
                                }
                                Err(error) => {
                                    notice = format!("{error}. Draft retained; no automatic retry.")
                                }
                            },
                            Err(error) => notice = error.to_string(),
                        }
                        snapshot = send(
                            client,
                            Request::Snapshot {
                                scope: scope.into(),
                            },
                        )?;
                        fence_cached_output(&snapshot, &mut last_memory_epoch, &mut local_output);
                    }
                    _ => {}
                }
                dirty = true;
            }
            _ => {}
        }
    }
}

fn fence_cached_output(snapshot: &Value, last_epoch: &mut Option<u64>, local_output: &mut String) {
    let epoch = snapshot.get("memory_epoch").and_then(Value::as_u64);
    if epoch != *last_epoch {
        local_output.clear();
        *last_epoch = epoch;
    }
}

fn input_request(text: &str, scope: &str) -> Result<Request> {
    if let Some(spec) = text.strip_prefix("/evolve-json ") {
        return Ok(Request::EvolveSpec {
            scope: scope.into(),
            request_id: uuid::Uuid::new_v4().to_string(),
            spec: spec.into(),
        });
    }
    if text == "/help" {
        return Ok(Request::Help);
    }
    if let Some(rest) = text.strip_prefix("/explain ") {
        let (id, reply) = rest.split_once(' ').ok_or_else(|| {
            anyhow::anyhow!(
                "Use /explain RECORD_ID JSON with choice, why, alternative, remaining_question"
            )
        })?;
        return Ok(Request::Explain {
            scope: scope.into(),
            record_id: id.into(),
            reply: serde_json::from_str(reply)?,
        });
    }
    if let Some(body) = text.strip_prefix("/investigate ") {
        return Ok(Request::Investigate {
            scope: scope.into(),
            request_id: uuid::Uuid::new_v4().to_string(),
            body: body.into(),
        });
    }
    if text == "/brief" {
        return Ok(Request::Brief {
            scope: scope.into(),
        });
    }
    if let Some(id) = text.strip_prefix("/why ") {
        return Ok(Request::Recall {
            scope: scope.into(),
            record_id: id.trim().into(),
        });
    }
    if let Some(rest) = text.strip_prefix("/correct ") {
        let (id, body) = rest
            .split_once(' ')
            .ok_or_else(|| anyhow::anyhow!("Use /correct RECORD_ID NEW_WORDING"))?;
        return Ok(Request::Correct {
            scope: scope.into(),
            request_id: uuid::Uuid::new_v4().to_string(),
            record_id: id.into(),
            body: body.into(),
        });
    }
    if let Some(body) = text.strip_prefix("/decision-json ") {
        let _: crate::assistant_briefing::Decision = serde_json::from_str(body)?;
        return Ok(save(scope, SaveKind::Decision, body));
    }
    if text == "/evolve" {
        return Ok(Request::Evolve {
            scope: scope.into(),
            request_id: uuid::Uuid::new_v4().to_string(),
        });
    }
    if let Some(hash) = text.strip_prefix("/approve ") {
        return Ok(Request::ApproveTool {
            scope: scope.into(),
            hash: hash.trim().into(),
        });
    }
    if let Some(grant) = text.strip_prefix("/revoke ") {
        return Ok(Request::RevokeTool {
            scope: scope.into(),
            grant_id: grant.trim().into(),
        });
    }
    if let Some(rest) = text.strip_prefix("/rollback ") {
        let (name, hash) = rest
            .split_once(' ')
            .ok_or_else(|| anyhow::anyhow!("Use /rollback NAME HASH"))?;
        return Ok(Request::RollbackTool {
            scope: scope.into(),
            name: name.into(),
            hash: hash.trim().into(),
        });
    }
    if let Some(rest) = text.strip_prefix("/tool ") {
        let (name, inputs) = rest
            .split_once(' ')
            .ok_or_else(|| anyhow::anyhow!("Use /tool NAME JSON_ARRAY"))?;
        return Ok(Request::InvokeTool {
            scope: scope.into(),
            name: name.into(),
            inputs: inputs.into(),
        });
    }
    if text == "/cancel" {
        return Ok(Request::Cancel);
    }
    if text == "/fresh-context acknowledge" {
        return Ok(Request::FreshContext {
            request_id: uuid::Uuid::new_v4().to_string(),
        });
    }
    if text == "/fresh-context" {
        bail!(
            "This stops Pika's owned assistant jobs and retires their provider context, without replay or refund. Saved memory stays. Type /fresh-context acknowledge to proceed; you must explicitly re-enable the provider afterward."
        );
    }
    if let Some(id) = text.strip_prefix("/forget ") {
        return Ok(Request::Forget {
            record_id: id.trim().into(),
        });
    }
    for (prefix, kind) in [
        ("/remember ", SaveKind::Instruction),
        ("/decision ", SaveKind::Decision),
        ("/skip ", SaveKind::Grasp),
        ("/defer ", SaveKind::Grasp),
        ("/explain ", SaveKind::Grasp),
    ] {
        if let Some(body) = text.strip_prefix(prefix) {
            let body = if matches!(kind, SaveKind::Grasp) {
                format!("{}: {body}", prefix.trim().trim_start_matches('/'))
            } else {
                body.to_owned()
            };
            return Ok(save(scope, kind, &body));
        }
    }
    if text.starts_with('/') {
        bail!(
            "Use /help for commands. /cancel interrupts owned work; /fresh-context explains recovery. Esc returns to the board."
        );
    }
    Ok(save(scope, SaveKind::Draft, text))
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
        let mut frame = Vec::new();
        if columns > 0 && height > 0 {
            queue!(
                frame,
                MoveTo(0, 0),
                Clear(ClearType::All),
                Print("Resize".chars().take(columns as usize).collect::<String>())
            )?;
        }
        return Ok(frame);
    }
    let width = usize::from(columns.saturating_sub(4));
    let colors = std::env::var_os("NO_COLOR").is_none();
    let mut lines = vec![
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
    ];
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
    lines.push(String::new());
    if let Some(text) = snapshot["local_output"]
        .as_str()
        .filter(|text| !text.is_empty())
    {
        lines.push(text.into());
        lines.push(String::new());
    }
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
