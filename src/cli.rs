use crate::{
    VERSION,
    attention::{self, AttentionTarget},
    client_bridge::{self, ClientWindowOutcome, TcpClientBridgeTransport},
    config::Config,
    core::{OpenError, Pika},
    doctor,
    fleet::{
        self, FleetError, FleetErrorKind, FleetInstallTransport, FleetManager, FleetService,
        NodeCandidate, SshTransport,
    },
    hooks::{self, HookContext},
    model::{Provider, Session},
    monitor::{
        self, BoardAction, BoardItem, ConsultationDriver, ConsultationEvent, ConsultationInput,
        ConsultationOutcome, ExpertAnnotation,
    },
    paths::Paths,
    process,
    scheduler::{self, ScheduleRequest},
    setup::{self, SetupOptions, SetupPaths},
    setup_preview, skill,
    store::Store,
    terminal::{self, Palette},
    update::{self, InstallRequest, ReleaseManifest, UpdateRequest},
    usage, wait as wait_contract,
};
use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use serde::Serialize;
use std::{
    collections::BTreeSet,
    ffi::OsString,
    fs,
    io::{self, BufRead, IsTerminal, Read, Write},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[derive(Parser, Debug)]
#[command(name="pika", version=VERSION, about="One home for your Codex, Claude, and OpenCode conversations.", after_help="Run `pika NAME` to find, protect, attach, resume, or create the exact conversation.")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// List tracked conversations without opening the board.
    List(ListArgs),
    /// Open the oldest conversation that needs attention.
    Next,
    /// Read recent output without changing unread state.
    Peek(PeekArgs),
    /// Wait for one conversation to need attention.
    Wait(WaitArgs),
    /// Stop watching without stopping or archiving it.
    Untrack(NameArg),
    /// Ask an expert in a private side conversation.
    Ask(AskArgs),
    /// Find exact conversation experts by topic, project, or artifact.
    Experts(QueryArgs),
    /// Publish, update, inspect, or clear expert cards.
    Expert(ExpertArgs),
    /// Explain why a visible state won.
    Explain(ExplainArgs),
    /// Show transcript-free attention history.
    Activity(ActivityArgs),
    /// Show or install the bundled agent-convo skill.
    Skill(SkillArgs),
    /// Preview and install provider lifecycle hooks.
    Setup(SetupArgs),
    /// Verify exact recovery and integrations.
    Doctor(DoctorArgs),
    /// Discover and manage trusted remote Pika machines.
    #[command(alias = "machine")]
    Machines(MachinesArgs),
    /// Refresh one trusted machine now.
    Sync(NameArg),
    /// Safely update an installer-managed Pika.
    Update(UpdateArgs),
    #[command(hide = true)]
    Open(NameArg),
    #[command(hide = true)]
    New(NewArgs),
    #[command(hide = true)]
    Adopt(NameArg),
    #[command(hide = true)]
    RecoverClosed(NameArg),
    #[command(hide = true)]
    Hook(HookArgs),
    #[command(name = "_process-exit", hide = true)]
    ProcessExit(ProcessExitArgs),
    #[command(name = "_terminal-bridge", hide = true)]
    TerminalBridge(TerminalBridgeArgs),
    #[command(name = "_install-native", hide = true)]
    InstallNative(InstallNativeArgs),
    #[command(name = "_fleet", hide = true)]
    Fleet(FleetInternalArgs),
    #[command(name = "_fleet-open", hide = true)]
    FleetOpen(FleetOpenArgs),
    #[command(name = "_fleet-ask", hide = true)]
    FleetAsk(FleetAskArgs),
    #[command(name = "_peek-popup", hide = true)]
    PeekPopup(PeekPopupArgs),
    #[command(name = "_client-pair", hide = true)]
    ClientPair(ClientPairArgs),
    #[command(name = "_enter", hide = true)]
    Enter(NameArg),
    #[command(external_subcommand)]
    Name(Vec<OsString>),
}

#[derive(Args, Debug)]
struct NameArg {
    /// Conversation name, UUID, or provider-qualified name such as codex:research.
    name: String,
}
#[derive(Args, Debug)]
struct ListArgs {
    /// Emit machine-readable JSON.
    #[arg(long)]
    json: bool,
    /// Skip best-effort token and cost enrichment.
    #[arg(long)]
    no_usage: bool,
    /// Include cached conversations from trusted machines.
    #[arg(long)]
    all_machines: bool,
}
#[derive(Args, Debug)]
struct QueryArgs {
    /// Search terms matched against expert scope, current work, topics, and artifacts.
    query: Vec<String>,
    /// Emit machine-readable JSON.
    #[arg(long)]
    json: bool,
}
#[derive(Args, Debug)]
struct PeekArgs {
    /// Exact conversation name or UUID.
    name: String,
    /// Number of recent terminal lines to show.
    #[arg(long)]
    lines: Option<usize>,
    /// Explicitly acknowledge the event that was inspected.
    #[arg(long)]
    ack: bool,
}
#[derive(Args, Debug)]
struct WaitArgs {
    /// Exact conversation name or UUID.
    name: String,
    /// Event class to wait for.
    #[arg(long="for", default_value="any", value_parser=["any","needs-you","ready","error"])]
    condition: String,
    /// Maximum wait in seconds.
    #[arg(long)]
    timeout: Option<f64>,
    /// Emit machine-readable JSON.
    #[arg(long)]
    json: bool,
}
#[derive(Args, Debug)]
struct AskArgs {
    /// Expert conversation name or UUID.
    name: String,
    /// Question for the private side conversation; reads stdin when omitted.
    question: Vec<String>,
    /// Stream protocol receipts and answers as JSON Lines.
    #[arg(long)]
    jsonl: bool,
    /// Emit one final machine-readable JSON result.
    #[arg(long)]
    json: bool,
    /// Use the provider's verified low-latency consultation profile.
    #[arg(long)]
    fast: bool,
}
#[derive(Args, Debug)]
struct ExplainArgs {
    /// Exact conversation name or UUID.
    name: String,
    /// Emit machine-readable JSON.
    #[arg(long)]
    json: bool,
}
#[derive(Args, Debug)]
struct ActivityArgs {
    /// Maximum number of transcript-free events.
    #[arg(long, default_value_t = 20)]
    limit: usize,
    /// Emit machine-readable JSON.
    #[arg(long)]
    json: bool,
}
#[derive(Args, Debug)]
struct NewArgs {
    name: String,
    #[arg(long)]
    agent: Option<Provider>,
    #[arg(long)]
    cwd: Option<PathBuf>,
}
#[derive(Args, Debug)]
struct SetupArgs {
    /// Default provider for genuinely new conversation names.
    #[arg(long)]
    default_provider: Option<Provider>,
    /// Codex executable or command override.
    #[arg(long)]
    codex_executable: Option<String>,
    /// Claude executable or command override.
    #[arg(long)]
    claude_executable: Option<String>,
    /// OpenCode executable or command override.
    #[arg(long)]
    opencode_executable: Option<String>,
    /// Friendly name for this Pika machine.
    #[arg(long)]
    machine_alias: Option<String>,
    /// Apply the preview without an interactive approval prompt.
    #[arg(long)]
    yes: bool,
    /// Print the complete preview and change nothing.
    #[arg(long)]
    dry_run: bool,
    /// Skip conversation discovery and adoption.
    #[arg(long)]
    no_import: bool,
    /// Adopt every explicitly named local conversation.
    #[arg(long)]
    import_all: bool,
    /// Also offer the bounded second screen of recent unnamed conversations.
    #[arg(long)]
    browse_all: bool,
    /// Skip passive machine discovery.
    #[arg(long)]
    no_machines: bool,
    /// Add one trusted SSH target; may be repeated.
    #[arg(long = "machine")]
    machines: Vec<String>,
    /// Verified native release bundle to offer selected remote machines.
    #[arg(long)]
    install_bundle: Option<PathBuf>,
    /// Adopt every explicitly named conversation on selected machines.
    #[arg(long)]
    remote_import_all: bool,
    /// Omit the short first-use command guide.
    #[arg(long)]
    skip_walkthrough: bool,
}
#[derive(Args, Debug)]
struct DoctorArgs {
    /// Emit machine-readable JSON.
    #[arg(long)]
    json: bool,
    /// Include every check, not only failures and the recovery certificate.
    #[arg(long)]
    verbose: bool,
    /// Remove only bookkeeping disproved by complete live evidence.
    #[arg(long)]
    repair_stale: bool,
}
#[derive(Args, Debug)]
struct UpdateArgs {
    /// Check for a compatible release without installing it.
    #[arg(long)]
    check: bool,
    /// Install from a verified local bundle without network access.
    #[arg(long)]
    bundle: Option<PathBuf>,
    /// Install one exact compatible version.
    #[arg(long)]
    release: Option<String>,
}
#[derive(Args, Debug)]
struct HookArgs {
    #[arg(long)]
    provider: Provider,
}
#[derive(Args, Debug)]
struct ProcessExitArgs {
    #[arg(long)]
    provider: Provider,
    #[arg(long)]
    session_id: Option<String>,
    #[arg(long)]
    launch_token: Option<String>,
    #[arg(long)]
    owner_token: Option<String>,
    #[arg(long)]
    code: i32,
}
#[derive(Args, Debug)]
struct PeekPopupArgs {
    #[arg(long)]
    target: String,
    #[arg(long)]
    lines: usize,
    #[arg(long)]
    name: String,
    #[arg(long)]
    provider: Provider,
    #[arg(long)]
    session_id: String,
}
#[derive(Args, Debug)]
struct ClientPairArgs {
    #[arg(long, required = true)]
    stdio: bool,
}
#[derive(Args, Debug)]
struct TerminalBridgeArgs {
    #[arg(long)]
    foreground: String,
    #[arg(long)]
    background: String,
    #[arg(last = true, required = true)]
    command: Vec<String>,
}
#[derive(Args, Debug)]
struct InstallNativeArgs {
    #[arg(long)]
    manifest: PathBuf,
    #[arg(long)]
    artifact: PathBuf,
    #[arg(long)]
    candidate: PathBuf,
    #[arg(long)]
    target: String,
    #[arg(long)]
    root: PathBuf,
    #[arg(long)]
    bin_dir: PathBuf,
    #[arg(long)]
    no_setup: bool,
}

#[derive(Args, Debug)]
struct SkillArgs {
    #[command(subcommand)]
    command: SkillCommand,
}
#[derive(Subcommand, Debug)]
enum SkillCommand {
    /// Print the bundled skill without changing files.
    Show,
    /// Install the bundled skill, preserving unrelated content.
    Install {
        /// Destination skill directory; defaults to the current Codex home.
        path: Option<PathBuf>,
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
}
#[derive(Args, Debug)]
struct ExpertArgs {
    #[command(subcommand)]
    command: ExpertCommand,
}
#[derive(Subcommand, Debug)]
enum ExpertCommand {
    /// Publish this exact conversation's durable expertise and current work.
    Publish {
        /// Durable mandate: what this conversation knows.
        #[arg(long = "scope", alias = "summary")]
        summary: String,
        /// Current work, kept separate from durable expertise.
        #[arg(long = "now")]
        current_state: String,
        /// Searchable topic; repeat for multiple topics.
        #[arg(long = "topic", required = true)]
        topics: Vec<String>,
        /// Relevant artifact path or identifier; may be repeated.
        #[arg(long = "artifact")]
        artifacts: Vec<String>,
    },
    /// Update only this conversation's current-work field.
    Update {
        /// New current-work description.
        #[arg(long = "now", alias = "current-state")]
        current_state: String,
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Remove this conversation's published expert card.
    Clear,
    /// Inspect expert-card freshness without interviewing agents.
    Status {
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Refresh selected expert cards through bounded, quota-aware interviews.
    Refresh {
        /// Optional exact conversation name or UUID.
        name: Option<String>,
        /// Refresh every eligible card now, regardless of quota observations.
        #[arg(long)]
        all: bool,
        /// Refresh only the next quota-eligible stale or missing card.
        #[arg(long)]
        due: bool,
        /// Limit refresh to one provider.
        #[arg(long)]
        provider: Option<Provider>,
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
}
#[derive(Args, Debug)]
struct MachinesArgs {
    #[command(subcommand)]
    command: Option<MachinesCommand>,
}
#[derive(Subcommand, Debug)]
enum MachinesCommand {
    /// List trusted machines and their last verified state.
    List {
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Find SSH-config and Tailscale candidates without connecting.
    Discover {
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Trust one SSH target after an exact identity handshake.
    Add {
        /// SSH config alias or user@host target.
        ssh_target: String,
        /// Friendly Pika name; defaults to the discovered host name.
        #[arg(long)]
        alias: Option<String>,
    },
    /// Remove one machine from Pika without changing that machine.
    Remove {
        /// Trusted machine alias or immutable node ID.
        machine: String,
    },
    /// Hide a passive discovery candidate.
    Ignore {
        /// Passive SSH or Tailscale candidate to hide.
        ssh_target: String,
    },
    /// Install one verified native release on a trusted machine.
    Upgrade {
        /// Trusted machine alias or immutable node ID.
        machine: String,
        /// Verified local release bundle; required for explicit remote install.
        #[arg(long)]
        bundle: Option<PathBuf>,
        /// Apply the preview without an interactive approval prompt.
        #[arg(long)]
        yes: bool,
    },
}
#[derive(Args, Debug)]
struct FleetInternalArgs {
    #[arg(long, required = true)]
    stdio: bool,
}
#[derive(Args, Debug)]
struct FleetOpenArgs {
    #[arg(long)]
    expected_node_id: String,
    #[arg(long)]
    provider: Provider,
    #[arg(long)]
    session_id: String,
}
#[derive(Args, Debug)]
struct FleetAskArgs {
    #[arg(long)]
    expected_node_id: String,
    #[arg(long)]
    provider: Provider,
    #[arg(long)]
    session_id: String,
    #[arg(long)]
    fast: bool,
    #[arg(long)]
    consultation_mode: Option<String>,
    #[arg(long)]
    model: Option<String>,
    #[arg(long)]
    effort: Option<String>,
}

pub fn run<I, T>(args: I) -> Result<i32>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let cli = Cli::try_parse_from(args)?;
    match cli.command {
        Some(Command::InstallNative(args)) => install_native(args),
        Some(Command::TerminalBridge(args)) => terminal_bridge(args),
        Some(Command::Hook(args)) => hook(args),
        Some(Command::ProcessExit(args)) => process_exit(args),
        command => dispatch(&Pika::discover()?, command),
    }
}

fn dispatch(pika: &Pika, command: Option<Command>) -> Result<i32> {
    match command {
        None => bare(pika),
        Some(Command::List(a)) => list(pika, a),
        Some(Command::Next) => next(pika),
        Some(Command::Peek(a)) => peek(pika, a),
        Some(Command::Wait(a)) => wait(pika, a),
        Some(Command::Untrack(a)) => untrack(pika, &a.name),
        Some(Command::Experts(a)) => experts(pika, a),
        Some(Command::Expert(a)) => expert(pika, a),
        Some(Command::Explain(a)) => explain(pika, a),
        Some(Command::Activity(a)) => activity(pika, a),
        Some(Command::Skill(a)) => skill_command(pika, a),
        Some(Command::Setup(a)) => setup_command(pika, a),
        Some(Command::Doctor(a)) => doctor(pika, a),
        Some(Command::Open(a) | Command::RecoverClosed(a)) => open_name(pika, &a.name, false),
        Some(Command::New(a)) => new(pika, a),
        Some(Command::Adopt(a)) => adopt(pika, &a.name),
        Some(Command::Ask(a)) => ask(pika, a),
        Some(Command::Machines(a)) => machines(pika, a),
        Some(Command::Sync(a)) => sync(pika, &a.name),
        Some(Command::Update(a)) => update_command(a),
        Some(Command::Fleet(a)) => fleet_stdio(pika, a),
        Some(Command::FleetOpen(a)) => fleet_open(pika, a),
        Some(Command::FleetAsk(a)) => fleet_ask(pika, a),
        Some(Command::PeekPopup(a)) => peek_popup(pika, a),
        Some(Command::ClientPair(a)) => client_pair(pika, a),
        Some(Command::Enter(a)) => open_name(pika, &a.name, true),
        Some(Command::Name(parts)) => {
            let name = parts
                .into_iter()
                .map(|v| {
                    v.into_string()
                        .map_err(|_| anyhow::anyhow!("conversation name is not UTF-8"))
                })
                .collect::<Result<Vec<_>>>()?
                .join(" ");
            open_name(pika, &name, io::stdin().is_terminal())
        }
        Some(
            Command::InstallNative(_)
            | Command::TerminalBridge(_)
            | Command::Hook(_)
            | Command::ProcessExit(_),
        ) => unreachable!(),
    }
}

fn peek_popup(pika: &Pika, a: PeekPopupArgs) -> Result<i32> {
    let session = pika
        .store
        .get_session(a.provider, &a.session_id)?
        .context("The selected Pika conversation is no longer tracked")?;
    if session.tmux_pane.as_deref() != Some(&a.target) {
        bail!("The selected conversation's pane changed; reopen its peek")
    }
    println!("{}", pika.capture_exact(&session, a.lines)?);
    println!("\n[{}] Enter attaches; Esc or q returns.", a.name);
    if !io::stdin().is_terminal() {
        return Ok(0);
    }
    crossterm::terminal::enable_raw_mode()?;
    struct RawMode;
    impl Drop for RawMode {
        fn drop(&mut self) {
            let _ = crossterm::terminal::disable_raw_mode();
        }
    }
    let _raw = RawMode;
    let mut answer = [0_u8; 1];
    if io::stdin().read_exact(&mut answer).is_ok() && matches!(answer[0], b'\n' | b'\r') {
        drop(_raw);
        return open_local_session(pika, session);
    }
    Ok(0)
}

fn bare(pika: &Pika) -> Result<i32> {
    if !monitor::interactive_terminal() {
        let mut inventory = pika.reconcile_local()?;
        let _ = usage::hydrate_sessions(&pika.paths, &pika.store, &mut inventory.sessions);
        inventory.sessions.extend(
            inventory
                .pending
                .iter()
                .map(crate::core::session_from_pending),
        );
        return print_sessions(inventory.sessions, true);
    }
    let cached = board_items(pika)?;
    // At most one unpublished snapshot is useful: the board always wants the
    // newest complete observation, never a backlog of stale inventories.
    let (sender, receiver) = mpsc::sync_channel(1);
    let (refresh_sender, refresh_receiver) = mpsc::sync_channel(1);
    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = Arc::clone(&stop);
    let worker = pika.clone();
    let local_sender = sender.clone();
    let (local_done_sender, local_done_receiver) = mpsc::sync_channel(1);
    let local_refresh = thread::spawn(move || {
        while !worker_stop.load(Ordering::Relaxed) {
            if let Ok(mut inventory) = worker.reconcile_local() {
                let _ =
                    usage::hydrate_sessions(&worker.paths, &worker.store, &mut inventory.sessions);
                if let Ok(items) = board_items_from_inventory(&worker, inventory)
                    && local_sender.send(items).is_err()
                {
                    break;
                }
            }
            // Hooks persist important state immediately; this pass reconciles
            // provider/process evidence. Ten seconds keeps the fallback fresh
            // without continuously rescanning a large watched inventory;
            // lifecycle hooks still publish attention immediately.
            match refresh_receiver.recv_timeout(Duration::from_secs(10)) {
                Ok(()) | Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        let _ = local_done_sender.send(());
    });
    let remote_stop = Arc::clone(&stop);
    let remote_worker = pika.clone();
    let remote_sender = sender;
    let _remote_refresh = thread::spawn(move || {
        while !remote_stop.load(Ordering::Relaxed) {
            let manager = FleetManager::new(&remote_worker.store, SshTransport::default());
            if let Ok(nodes) = manager.nodes()
                && let Some(node) = fleet::next_remote_node(&nodes, None, now(), false)
            {
                let _ = manager.refresh_node(&node.node_id);
                if let Ok(items) = board_items(&remote_worker) {
                    match remote_sender.try_send(items) {
                        Ok(()) | Err(mpsc::TrySendError::Full(_)) => {}
                        Err(mpsc::TrySendError::Disconnected(_)) => break,
                    }
                }
            }
            for _ in 0..10 {
                if remote_stop.load(Ordering::Relaxed) {
                    return;
                }
                thread::sleep(Duration::from_millis(100));
            }
        }
    });
    let (update_sender, update_receiver) = mpsc::sync_channel(1);
    let update_executable = std::env::current_exe().ok();
    let _update_check = thread::spawn(move || {
        let notice = update_executable
            .as_deref()
            .and_then(update::cached_update_notice);
        let _ = update_sender.send(notice);
    });
    let action = monitor::run_items_dynamic_with_notice_and_refresh(
        cached,
        receiver,
        Some(board_consultation_driver(pika.clone())),
        update_receiver,
        refresh_sender.clone(),
    );
    finish_board_observer(&stop, &refresh_sender, &local_done_receiver, local_refresh);
    // Do not delay Enter/quit behind provider metadata or a bounded SSH
    // timeout. Exact actions revalidate independently before mutating state.
    match action? {
        BoardAction::Open(item) => open_board_item(pika, item),
        BoardAction::Peek(item) => peek_board_item(pika, item),
        BoardAction::Untrack(item) => untrack_board_item(pika, item),
        BoardAction::Ask(item) => ask_board_item(pika, item),
        BoardAction::Refresh => unreachable!("interactive refresh is handled in-place"),
        BoardAction::Update(version) => update_command(UpdateArgs {
            check: version.is_none(),
            bundle: None,
            release: version,
        }),
        BoardAction::Quit => Ok(0),
    }
}

fn finish_board_observer(
    stop: &AtomicBool,
    refresh: &mpsc::SyncSender<()>,
    done: &mpsc::Receiver<()>,
    worker: thread::JoinHandle<()>,
) {
    stop.store(true, Ordering::Relaxed);
    // Wake the local observer, but never hold Enter/quit behind an in-flight
    // provider or tmux read. Exact row actions revalidate independently. A
    // completed observer is joined; a slow one retains its own Pika clone and
    // exits after its now-bounded reconciliation call observes `stop`.
    let _ = refresh.try_send(());
    if done.recv_timeout(Duration::from_millis(50)).is_ok() {
        let _ = worker.join();
    }
}

fn board_items(pika: &Pika) -> Result<Vec<BoardItem>> {
    board_items_from_inventory(pika, pika.cached_inventory()?)
}

fn board_items_from_inventory(
    pika: &Pika,
    inventory: crate::core::Inventory,
) -> Result<Vec<BoardItem>> {
    let profiles = pika
        .store
        .list_stored_expert_profiles()?
        .into_iter()
        .map(|stored| {
            (
                (stored.profile.provider, stored.profile.session_id.clone()),
                stored.profile,
            )
        })
        .collect::<std::collections::HashMap<_, _>>();
    let mut items = inventory
        .sessions
        .into_iter()
        .map(|session| {
            let expert = profiles
                .get(&(session.provider, session.session_id.clone()))
                .map(|profile| ExpertAnnotation {
                    scope: Some(profile.summary.clone()),
                    current_work: (!profile.current_state.trim().is_empty())
                        .then(|| profile.current_state.clone()),
                    topics: profile.topics.clone(),
                    freshness: Some(format!("updated {} ago", short_age(profile.updated_at))),
                });
            BoardItem {
                expert,
                ..BoardItem::local(session)
            }
        })
        .collect::<Vec<_>>();
    items.extend(inventory.pending.into_iter().map(|pending| BoardItem {
        session: crate::core::session_from_pending(&pending),
        node_id: None,
        node_name: None,
        stale: false,
        pending_token: Some(pending.launch_token),
        expert: None,
    }));
    for remote in FleetManager::new(&pika.store, SshTransport::default())
        .cached_sessions(None, false)
        .map_err(anyhow::Error::from)?
    {
        items.push(BoardItem {
            session: remote.session,
            node_id: Some(remote.node_id),
            node_name: Some(remote.node_name),
            stale: remote.stale,
            pending_token: None,
            expert: remote.card_detail.map(|detail| ExpertAnnotation {
                scope: Some(detail),
                current_work: None,
                topics: Vec::new(),
                freshness: remote.card_status,
            }),
        });
    }
    Ok(items)
}

fn exact_remote(pika: &Pika, item: &BoardItem) -> Result<fleet::FleetSession> {
    let node_id = item
        .node_id
        .as_deref()
        .context("board item has no remote machine identity")?;
    FleetManager::new(&pika.store, SshTransport::default())
        .cached_sessions(Some(node_id), false)
        .map_err(anyhow::Error::from)?
        .into_iter()
        .find(|remote| {
            remote.session.provider == item.session.provider
                && remote.session.session_id == item.session.session_id
        })
        .context("the exact cached remote conversation disappeared")
}

fn open_board_item(pika: &Pika, item: BoardItem) -> Result<i32> {
    if let Some(token) = item.pending_token.as_deref() {
        let receipt = pika.open_pending(token, true)?;
        return finish_local_open(pika, &receipt);
    }
    if item.node_id.is_some() {
        let remote = exact_remote(pika, &item)?;
        if let Some(code) = maybe_open_client_window(
            pika,
            &remote.node_id,
            remote.session.provider,
            &remote.session.session_id,
        )? {
            return Ok(code);
        }
        return FleetManager::new(&pika.store, SshTransport::default())
            .attach(&remote)
            .map_err(anyhow::Error::from);
    }
    let local_node_id = pika.store.ensure_local_node_id()?;
    if let Some(code) = maybe_open_client_window(
        pika,
        &local_node_id,
        item.session.provider,
        &item.session.session_id,
    )? {
        return Ok(code);
    }
    open_local_session(pika, item.session)
}

fn open_local_session(pika: &Pika, session: Session) -> Result<i32> {
    match pika.open_session(session.clone(), true) {
        Ok(receipt) => finish_local_open(pika, &receipt),
        Err(error)
            if matches!(
                error.downcast_ref::<OpenError>(),
                Some(OpenError::OutsideLive(_))
            ) =>
        {
            let message = error.to_string();
            if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
                bail!("{message} Interactive choice is required.")
            }
            let exact = pika.outside_identity_generations(&session)?;
            if exact.len() != 1 || !process::can_terminate_generation() {
                bail!("{message}")
            }
            let (pid, generation) = exact[0];
            eprintln!(
                "pika: exact {} conversation {:?} is live outside Pika (PID {pid}).",
                session.provider,
                session.display_name()
            );
            eprintln!("  1. Keep it there (recommended) — no process or state changes");
            eprintln!(
                "  2. Clean and attach here — gracefully stop that exact PID generation, then resume the same UUID"
            );
            eprintln!("  3. Cancel — no process or state changes");
            print!("Choose 1-3 [1]: ");
            io::stdout().flush()?;
            let mut answer = String::new();
            io::stdin().read_line(&mut answer)?;
            match answer.trim().to_ascii_lowercase().as_str() {
                "2" => {
                    eprintln!(
                        "pika: requesting a graceful stop for exact {} PID {pid}; Pika will not force-kill it.",
                        session.provider
                    );
                    let receipt = pika.clean_and_attach(session, (pid, generation), true)?;
                    finish_local_open(pika, &receipt)
                }
                "3" | "q" | "cancel" => {
                    eprintln!("pika: Cancelled. No state changed.");
                    Ok(1)
                }
                _ => {
                    eprintln!(
                        "pika: Keep using the existing {} client. To move it later, run `/exit`, wait for the shell prompt, then run exactly: `pika {}`.",
                        session.provider,
                        shell_words::quote(&session.display_name())
                    );
                    Ok(1)
                }
            }
        }
        Err(error) => Err(error),
    }
}

fn maybe_open_client_window(
    pika: &Pika,
    target_node_id: &str,
    provider: Provider,
    session_id: &str,
) -> Result<Option<i32>> {
    let client_context = std::env::var_os("SSH_CONNECTION").is_some()
        || std::env::var("PIKA_CLIENT_BRIDGE").as_deref() == Ok("1");
    let source_node_id = pika.store.ensure_local_node_id()?;
    let mut transport = TcpClientBridgeTransport;
    match client_bridge::route_client_window(
        &pika.config.client_bridges,
        client_context,
        &source_node_id,
        target_node_id,
        provider,
        session_id,
        &mut transport,
    )
    .map_err(|error| anyhow::anyhow!("CLIENT WINDOW BLOCKED · {error}"))?
    {
        ClientWindowOutcome::ContinueWithExistingAttach => Ok(None),
        ClientWindowOutcome::WindowLaunched(receipt) => {
            if target_node_id == source_node_id {
                pika.store.record_attach(provider, session_id)?;
            }
            println!("{}", receipt.detail);
            Ok(Some(0))
        }
    }
}

fn peek_board_item(pika: &Pika, item: BoardItem) -> Result<i32> {
    if item.pending_token.is_some() {
        bail!("This conversation is still starting; open it to see provider output.")
    }
    if item.node_id.is_some() {
        let remote = exact_remote(pika, &item)?;
        println!(
            "{}",
            FleetManager::new(&pika.store, SshTransport::default())
                .capture(&remote, pika.config.peek_lines)
                .map_err(anyhow::Error::from)?
        );
        return Ok(0);
    }
    peek_session(pika, item.session, None, false)
}

fn untrack_board_item(pika: &Pika, item: BoardItem) -> Result<i32> {
    if item.pending_token.is_some() {
        bail!("A starting conversation cannot be unwatched until its exact identity is known.")
    }
    if item.node_id.is_some() {
        let remote = exact_remote(pika, &item)?;
        FleetManager::new(&pika.store, SshTransport::default())
            .untrack(&remote, None)
            .map_err(anyhow::Error::from)?;
        println!(
            "Stopped watching {}. The remote agent and its history were not stopped or archived.",
            remote.qualified_name()
        );
        return Ok(0);
    }
    untrack_exact(pika, item.session)
}

fn ask_board_item(pika: &Pika, item: BoardItem) -> Result<i32> {
    if item.node_id.is_some() {
        return ask_remote(
            pika,
            exact_remote(pika, &item)?,
            AskArgs {
                name: item.session.session_id,
                question: Vec::new(),
                jsonl: false,
                json: false,
                fast: false,
            },
        );
    }
    ask_interactive(pika, item.session)
}

fn board_consultation_driver(pika: Pika) -> ConsultationDriver {
    ConsultationDriver::new(move |io| {
        if io.item.pending_token.is_some() {
            bail!("the conversation is still starting")
        }
        if io.item.node_id.is_some() {
            return run_remote_board_consultation(&pika, io);
        }
        run_local_board_consultation(&pika, io)
    })
}

fn run_local_board_consultation(
    pika: &Pika,
    io: monitor::ConsultationIo,
) -> Result<ConsultationOutcome> {
    let mut options =
        crate::consult::ConsultationOptions::new(pika.config.executable(io.item.session.provider));
    options.cancellation = io.cancellation.clone();
    if io.item.session.provider == Provider::Opencode {
        options.opencode_database = Some(pika.paths.opencode_data_home.join("opencode.db"));
    }
    let mut side = open_local_consultation(pika, &io.item.session, options)?;
    let _ = io.events.send(ConsultationEvent::Opened {
        child_id: side.child_id().map(str::to_owned),
        policy: Some(side.policy().label()),
    });
    for command in io.commands {
        match command {
            ConsultationInput::Question(question) => match side.ask(&question) {
                Ok(answer) => {
                    let _ = io.events.send(ConsultationEvent::Answer(answer));
                }
                Err(error) => {
                    let _ = io.events.send(ConsultationEvent::Error {
                        message: error.to_string(),
                        retry_safe: error.receipt.retry_safe,
                    });
                }
            },
            ConsultationInput::Close => break,
        }
    }
    side.close().map_err(consultation_error)?;
    Ok(ConsultationOutcome::discarded())
}

fn run_remote_board_consultation(
    pika: &Pika,
    io: monitor::ConsultationIo,
) -> Result<ConsultationOutcome> {
    let remote = exact_remote(pika, &io.item)?;
    require_remote_source_available(&remote)?;
    let local_policy = crate::consult::consultation_policy(remote.session.provider, false)?;
    let policy = fleet::ConsultationPolicy {
        consultation_mode: local_policy.mode,
        model: local_policy.model.unwrap_or_default(),
        effort: local_policy.effort.unwrap_or_default(),
    };
    let node = pika
        .store
        .get_fleet_node(&remote.node_id)?
        .context("remote expert machine is no longer trusted")?;
    let mut side = fleet::RemoteConsultation::open_cancellable(
        &SshTransport::default(),
        node,
        remote,
        policy.clone(),
        fleet::ConsultationTimeouts {
            open: Duration::from_secs(30),
            event: Duration::from_secs(900),
            cleanup: Duration::from_secs(30),
        },
        io.cancellation.clone(),
    )
    .map_err(anyhow::Error::from)?;
    let _ = io.events.send(ConsultationEvent::Opened {
        child_id: None,
        policy: Some(if policy.model.is_empty() {
            policy.consultation_mode.clone()
        } else {
            format!("{} · {}", policy.consultation_mode, policy.model)
        }),
    });
    for command in io.commands {
        match command {
            ConsultationInput::Question(question) => match side.ask(&question) {
                Ok(answer) => {
                    let _ = io.events.send(ConsultationEvent::Answer(answer));
                }
                Err(error) => {
                    let retry_safe = error.kind == FleetErrorKind::InvalidRequest;
                    let _ = io.events.send(ConsultationEvent::Error {
                        message: error.to_string(),
                        retry_safe,
                    });
                }
            },
            ConsultationInput::Close => break,
        }
    }
    side.close().map_err(anyhow::Error::from)?;
    Ok(ConsultationOutcome::discarded())
}
fn list(pika: &Pika, a: ListArgs) -> Result<i32> {
    let inventory = if pika.store.exists() {
        pika.reconcile_local()?
    } else {
        pika.cached_inventory()?
    };
    let mut sessions = inventory.sessions;
    sessions.extend(
        inventory
            .pending
            .iter()
            .map(crate::core::session_from_pending),
    );
    if !a.no_usage {
        let _ = usage::hydrate_sessions(&pika.paths, &pika.store, &mut sessions);
    }
    if a.all_machines {
        let remote = FleetManager::new(&pika.store, SshTransport::default())
            .cached_sessions(None, false)
            .map_err(anyhow::Error::from)?;
        if a.json {
            let mut values = sessions
                .iter()
                .map(|session| serde_json::json!({"machine":"here", "stale":false, "session":session}))
                .collect::<Vec<_>>();
            values.extend(remote.iter().map(|item| {
                serde_json::json!({"machine":item.node_name, "node_id":item.node_id, "stale":item.stale, "session":item.session})
            }));
            println!("{}", serde_json::to_string(&values)?);
        } else {
            for session in sessions {
                println!(
                    "@here {:<11} {:<8} {}",
                    session.status,
                    session.provider,
                    session.display_name()
                );
            }
            for item in remote {
                println!(
                    "@{:<12} {:<11} {:<8} {}{}",
                    item.node_name,
                    item.session.status,
                    item.session.provider,
                    item.session.display_name(),
                    if item.stale { " · stale" } else { "" }
                );
            }
        }
        return Ok(0);
    }
    if a.json {
        println!("{}", serde_json::to_string(&sessions)?);
        Ok(0)
    } else {
        print_sessions(sessions, !a.no_usage)
    }
}
fn print_sessions(sessions: Vec<Session>, usage: bool) -> Result<i32> {
    if sessions.is_empty() {
        println!("No conversations are tracked yet. Run `pika setup` or `pika NAME`.");
        return Ok(0);
    }
    for s in sessions {
        let u = if usage {
            s.total_tokens
                .map(|v| format!(" · {} tokens", usage::format_tokens(Some(v))))
                .unwrap_or_default()
        } else {
            String::new()
        };
        println!(
            "{:<11} {:<8} {}{}",
            s.status,
            s.provider,
            s.display_name(),
            u
        )
    }
    Ok(0)
}

fn short_age(timestamp: f64) -> String {
    let seconds = (now() - timestamp).max(0.0) as u64;
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3_600 {
        format!("{}m", seconds / 60)
    } else if seconds < 172_800 {
        format!("{}h", seconds / 3_600)
    } else {
        format!("{}d", seconds / 86_400)
    }
}
fn open_name(pika: &Pika, name: &str, allow_create: bool) -> Result<i32> {
    if name == "-" {
        let (provider, session_id) = pika
            .store
            .previous_attached()?
            .context("No previous Pika conversation has been recorded yet")?;
        let session = pika
            .store
            .get_session(provider, &session_id)?
            .context("The previous Pika conversation is no longer tracked")?;
        let local_node_id = pika.store.ensure_local_node_id()?;
        if let Some(code) =
            maybe_open_client_window(pika, &local_node_id, session.provider, &session.session_id)?
        {
            return Ok(code);
        }
        return open_local_session(pika, session);
    }
    if name == "." {
        let current = std::env::current_dir()?.canonicalize()?;
        let root = repository_root(&current);
        let mut matches = pika
            .reconcile_local()?
            .sessions
            .into_iter()
            .filter(|session| {
                session
                    .cwd
                    .as_deref()
                    .and_then(|path| std::fs::canonicalize(path).ok())
                    .is_some_and(|path| path == current || repository_root(&path) == root)
            })
            .collect::<Vec<_>>();
        matches.sort_by(|left, right| {
            right
                .needs_attention()
                .cmp(&left.needs_attention())
                .then_with(|| right.last_activity_at.total_cmp(&left.last_activity_at))
        });
        if matches.is_empty() {
            bail!("No Pika conversation belongs to {}", root.display())
        }
        let selected = choose_session(matches, "Choose a conversation for this repository")?;
        let local_node_id = pika.store.ensure_local_node_id()?;
        if let Some(code) = maybe_open_client_window(
            pika,
            &local_node_id,
            selected.provider,
            &selected.session_id,
        )? {
            return Ok(code);
        }
        return open_local_session(pika, selected);
    }
    let manager = FleetManager::new(&pika.store, SshTransport::default());
    if let Some(remote) = manager
        .resolve(name, false, false)
        .map_err(anyhow::Error::from)?
    {
        if let Some(code) = maybe_open_client_window(
            pika,
            &remote.node_id,
            remote.session.provider,
            &remote.session.session_id,
        )? {
            return Ok(code);
        }
        return manager.attach(&remote).map_err(anyhow::Error::from);
    }
    let matches = pika.resolve_local(name)?;
    if !matches.is_empty() {
        let selected = choose_session(matches, &format!("Choose {name}"))?;
        let local_node_id = pika.store.ensure_local_node_id()?;
        if let Some(code) = maybe_open_client_window(
            pika,
            &local_node_id,
            selected.provider,
            &selected.session_id,
        )? {
            return Ok(code);
        }
        return open_local_session(pika, selected);
    }
    let receipt = pika.open_name(name, true, allow_create)?;
    finish_local_open(pika, &receipt)
}

fn finish_local_open(pika: &Pika, receipt: &crate::core::OpenReceipt) -> Result<i32> {
    let _ = pika;
    Ok(receipt.exit_code)
}

fn choose_session(mut sessions: Vec<Session>, prompt: &str) -> Result<Session> {
    if sessions.len() == 1 {
        return Ok(sessions.remove(0));
    }
    if !io::stdin().is_terminal() {
        bail!(
            "{} conversations match; use PROVIDER:NAME or the exact UUID.",
            sessions.len()
        )
    }
    println!("{prompt}:");
    for (index, session) in sessions.iter().enumerate() {
        println!(
            "  {}. {:<8} {} · {} · {}",
            index + 1,
            session.provider,
            session.display_name(),
            session.cwd.as_deref().unwrap_or("directory unavailable"),
            session.session_id.chars().take(8).collect::<String>()
        );
    }
    print!("Enter a number, or press Enter to cancel: ");
    io::stdout().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    if answer.trim().is_empty() {
        bail!("No conversation opened.")
    }
    let index = answer
        .trim()
        .parse::<usize>()
        .context("enter one listed conversation number")?;
    sessions
        .get(index.saturating_sub(1))
        .cloned()
        .context("conversation choice is out of range")
}

fn repository_root(path: &std::path::Path) -> PathBuf {
    path.ancestors()
        .find(|candidate| candidate.join(".git").exists())
        .unwrap_or(path)
        .to_path_buf()
}
fn next(pika: &Pika) -> Result<i32> {
    let manager = FleetManager::new(&pika.store, SshTransport::default());
    let selected = attention::choose(
        pika.reconcile_local()?.sessions,
        manager
            .cached_sessions(None, false)
            .map_err(anyhow::Error::from)?,
    )
    .context("No conversation needs you right now.")?;
    match selected {
        AttentionTarget::Local(session) => {
            let local_node_id = pika.store.ensure_local_node_id()?;
            if let Some(code) = maybe_open_client_window(
                pika,
                &local_node_id,
                session.provider,
                &session.session_id,
            )? {
                return Ok(code);
            }
            open_local_session(pika, *session)
        }
        AttentionTarget::Remote(remote) => {
            if let Some(code) = maybe_open_client_window(
                pika,
                &remote.node_id,
                remote.session.provider,
                &remote.session.session_id,
            )? {
                return Ok(code);
            }
            manager.attach(&remote).map_err(anyhow::Error::from)
        }
    }
}
fn select_one(pika: &Pika, name: &str) -> Result<Session> {
    let m = pika.resolve_local(name)?;
    match m.as_slice() {
        [] => bail!("No exact conversation named {name:?}."),
        [s] => Ok(s.clone()),
        many => bail!(
            "{name:?} matches {} conversations; run `pika PROVIDER:{name}`.",
            many.len()
        ),
    }
}
fn select_current_local(pika: &Pika, name: &str) -> Result<Session> {
    let selected = select_one(pika, name)?;
    Ok(pika
        .reconcile_local()?
        .sessions
        .into_iter()
        .find(|session| {
            session.provider == selected.provider && session.session_id == selected.session_id
        })
        .unwrap_or(selected))
}
fn peek(pika: &Pika, a: PeekArgs) -> Result<i32> {
    let manager = FleetManager::new(&pika.store, SshTransport::default());
    if let Some(remote) = manager
        .resolve(&a.name, true, false)
        .map_err(anyhow::Error::from)?
    {
        println!(
            "{}",
            manager
                .capture(&remote, a.lines.unwrap_or(pika.config.peek_lines))
                .map_err(anyhow::Error::from)?
        );
        if a.ack {
            manager.acknowledge(&remote).map_err(anyhow::Error::from)?;
        }
        return Ok(0);
    }
    let s = select_current_local(pika, &a.name)?;
    peek_session(pika, s, a.lines, a.ack)
}
fn peek_session(pika: &Pika, s: Session, lines: Option<usize>, ack: bool) -> Result<i32> {
    println!(
        "{}",
        pika.capture_exact(&s, lines.unwrap_or(pika.config.peek_lines))?
    );
    if ack {
        pika.store
            .acknowledge_attention(s.provider, &s.session_id, s.last_event_at, false)?;
    }
    Ok(0)
}
fn untrack(pika: &Pika, name: &str) -> Result<i32> {
    let manager = FleetManager::new(&pika.store, SshTransport::default());
    if let Some(remote) = manager
        .resolve(name, true, false)
        .map_err(anyhow::Error::from)?
    {
        manager
            .untrack(&remote, None)
            .map_err(anyhow::Error::from)?;
        println!(
            "Stopped watching {}. The remote agent and its history were not stopped or archived.",
            remote.qualified_name()
        );
        return Ok(0);
    }
    let s = select_current_local(pika, name)?;
    untrack_exact(pika, s)
}
fn untrack_exact(pika: &Pika, s: Session) -> Result<i32> {
    pika.store.untrack_session(s.provider, &s.session_id)?;
    let clear_error = s
        .tmux_pane
        .as_deref()
        .and_then(|_| pika.clear_exact_tags(&s).err());
    println!(
        "Stopped watching {}. The agent and its history were not stopped or archived.",
        s.display_name()
    );
    if let Some(error) = clear_error {
        eprintln!(
            "pika: Watch state was removed, but pane identity changed before its Pika tags could be cleared: {error}"
        );
        return Ok(2);
    }
    Ok(0)
}
fn wait(pika: &Pika, a: WaitArgs) -> Result<i32> {
    if a.name.contains('@') {
        bail!(
            "Remote waits are not supported because cached attention can become stale. Run exactly: `pika sync {}` and inspect again.",
            shell_words::quote(a.name.rsplit_once('@').map_or(&a.name, |(_, node)| node))
        )
    }
    let initial = select_current_local(pika, &a.name)?;
    let start = Instant::now();
    let mut next_reconcile = start + Duration::from_secs(5);
    loop {
        let now = Instant::now();
        let s = if now >= next_reconcile {
            next_reconcile = now + Duration::from_secs(5);
            pika.reconcile_local()?
                .sessions
                .into_iter()
                .find(|session| {
                    session.provider == initial.provider && session.session_id == initial.session_id
                })
        } else {
            pika.store
                .get_session(initial.provider, &initial.session_id)?
        }
        .context("The tracked conversation disappeared while waiting")?;
        let matched = wait_contract::matches(&s, &a.condition);
        if matched {
            if a.json {
                println!("{}", serde_json::to_string(&s)?)
            } else {
                let reason = s
                    .attention_reason
                    .as_deref()
                    .map(|value| format!(" — {value}"))
                    .unwrap_or_default();
                println!("{}: {}{}", s.display_name(), s.status, reason)
            }
            return Ok(0);
        }
        if a.timeout
            .is_some_and(|v| start.elapsed().as_secs_f64() >= v)
        {
            if a.json {
                println!(
                    "{}",
                    serde_json::to_string(&serde_json::json!({
                        "timed_out": true,
                        "session": s,
                    }))?
                );
            } else {
                eprintln!("Timed out waiting for {}", s.display_name());
            }
            return Ok(wait_contract::TIMEOUT_EXIT_CODE);
        }
        thread::sleep(Duration::from_millis(500));
    }
}
fn new(pika: &Pika, a: NewArgs) -> Result<i32> {
    if let Some(cwd) = a.cwd {
        std::env::set_current_dir(
            cwd.canonicalize()
                .context("new conversation directory does not exist")?,
        )?
    }
    let receipt = pika.new_session(
        &a.name,
        a.agent.unwrap_or(pika.config.default_provider),
        true,
    )?;
    finish_local_open(pika, &receipt)
}
fn adopt(pika: &Pika, name: &str) -> Result<i32> {
    let providers = crate::providers::Providers::new(&pika.paths, &pika.config);
    let c = Provider::ALL
        .into_iter()
        .flat_map(|p| providers.find(p, name))
        .find(|c| {
            c.session_id == name
                || c.name
                    .as_deref()
                    .is_some_and(|v| v.eq_ignore_ascii_case(name))
        })
        .context("No exact provider conversation matches that name or UUID.")?;
    pika.adopt_candidate(&c)?;
    println!(
        "Watching {}. Run exactly: `pika {}`.",
        c.name.as_deref().unwrap_or(&c.session_id),
        shell_words::quote(c.name.as_deref().unwrap_or(&c.session_id))
    );
    Ok(0)
}

fn experts(pika: &Pika, a: QueryArgs) -> Result<i32> {
    let mut tracked = pika.store.list_sessions()?;
    let unwatched = pika.store.list_untracked_sessions()?;
    let untracked = unwatched
        .iter()
        .map(|s| (s.provider, s.session_id.clone()))
        .collect();
    tracked.extend(unwatched);
    let profiles = pika.store.list_stored_expert_profiles()?;
    let query = a.query.join(" ");
    let source_index = crate::experts::LocalSourceIndex::read(&pika.paths, &pika.config, &tracked);
    let mut found = crate::experts::rank_experts(&profiles, &tracked, &query, &untracked);
    for item in &mut found {
        if let Some(session) = tracked.iter().find(|session| {
            session.provider == item.provider && session.session_id == item.session_id
        }) {
            item.availability = source_index.availability(session).as_str().to_owned();
        }
    }
    found.retain(|item| item.availability != "archived");
    found.extend(
        FleetManager::new(&pika.store, SshTransport::default())
            .expert_matches(&query)
            .map_err(anyhow::Error::from)?,
    );
    found.sort_by(|left, right| {
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
    if a.json {
        println!("{}", serde_json::to_string(&found)?)
    } else if found.is_empty() {
        println!("No expert cards match.")
    } else {
        for x in found {
            println!(
                "{} {:<45} · {} · {}",
                x.provider, x.qualified_name, x.availability, x.scope
            )
        }
    }
    Ok(0)
}
fn calling_session(pika: &Pika) -> Result<Session> {
    pika.current_exact_session()
        .context("Run this command inside the exact Pika conversation that owns the card")
}
fn expert(pika: &Pika, a: ExpertArgs) -> Result<i32> {
    match a.command {
        ExpertCommand::Publish {
            summary,
            current_state,
            topics,
            artifacts,
        } => {
            let s = calling_session(pika)?;
            let proof = crate::experts::PublisherProof::from_verified_identity(
                &s,
                s.provider,
                &s.session_id,
                s.provider_thread_id(),
            )?;
            let stored = crate::experts::publish(
                &pika.store,
                &s,
                &proof,
                crate::experts::PublishInput {
                    scope: summary,
                    current_state,
                    topics: split_values(topics),
                    artifacts: split_values(artifacts),
                    source: "self".into(),
                },
            )?;
            println!("Published expert card for {}.", stored.profile.summary)
        }
        ExpertCommand::Update {
            current_state,
            json,
        } => {
            let s = calling_session(pika)?;
            let proof = crate::experts::PublisherProof::from_verified_identity(
                &s,
                s.provider,
                &s.session_id,
                s.provider_thread_id(),
            )?;
            let stored =
                crate::experts::publish_current_work(&pika.store, &s, &proof, &current_state)?;
            if json {
                println!("{}", serde_json::to_string(&stored.profile)?)
            } else {
                println!("Updated current work for {}.", s.display_name())
            }
        }
        ExpertCommand::Clear => {
            let s = calling_session(pika)?;
            pika.store
                .delete_expert_profile(s.provider, &s.session_id)?;
            println!("Cleared the expert card for {}.", s.display_name())
        }
        ExpertCommand::Status { json } => {
            let mut values = Vec::new();
            for s in pika.store.list_sessions()? {
                let stored = pika
                    .store
                    .get_stored_expert_profile(s.provider, &s.session_id)?;
                values.push((
                    s.display_name(),
                    crate::experts::card_state(&s, stored.as_ref()),
                ))
            }
            if json {
                println!("{}", serde_json::to_string(&values)?)
            } else {
                for (name, state) in values {
                    println!("{name:<28} {:?} · {}", state.status, state.detail)
                }
            }
        }
        ExpertCommand::Refresh {
            name,
            all,
            due,
            provider,
            json,
        } => {
            if all && due {
                bail!("use --all or --due, not both")
            }
            if name.is_some() && (all || due) {
                bail!("a conversation name cannot be combined with --all or --due")
            }
            if provider.is_some() && !all && !due {
                bail!("--provider is only valid with --all or --due")
            }
            let sessions = pika.reconcile_local()?.sessions;
            let mut refresh = crate::expert_refresh::NativeExpertRefresh::native(
                &pika.store,
                &pika.config,
                &pika.paths,
            );
            let results = if due {
                refresh.refresh_due_now(&sessions, provider)?
            } else if all {
                refresh.refresh_all(&sessions, provider)?
            } else {
                let session = if let Some(name) = name {
                    select_expert_target(pika, &name)?
                } else {
                    calling_session(pika)?
                };
                refresh.refresh_one(&session)?
            };
            let failed = results
                .iter()
                .any(|result| matches!(result.status.as_str(), "FAILED" | "UNAVAILABLE"));
            if json {
                println!("{}", serde_json::to_string(&results)?)
            } else if results.is_empty() {
                println!("Thread profiles already current.")
            } else {
                for result in &results {
                    let subject = result
                        .name
                        .as_deref()
                        .unwrap_or_else(|| result.provider.as_str());
                    let quota = result
                        .remaining_percent
                        .map(|remaining| format!(" · {remaining:.0}% left"))
                        .unwrap_or_default();
                    let policy = match (&result.model, &result.effort, &result.consultation_mode) {
                        (Some(model), Some(effort), _) => format!(" · {model} · {effort}"),
                        (_, _, Some(mode)) if mode == "provider-native" => {
                            " · provider native".to_owned()
                        }
                        _ => String::new(),
                    };
                    println!(
                        "{} · {} · {}{}{} · {}",
                        result.status, subject, result.provider, quota, policy, result.detail
                    );
                }
            }
            return Ok(if failed { 1 } else { 0 });
        }
    }
    Ok(0)
}

fn select_expert_target(pika: &Pika, name: &str) -> Result<Session> {
    if name.contains('@') {
        let manager = FleetManager::new(&pika.store, SshTransport::default());
        if let Some(remote) = manager
            .resolve(name, false, true)
            .map_err(anyhow::Error::from)?
        {
            let node = pika
                .store
                .get_fleet_node(&remote.node_id)?
                .context("the authoritative machine is no longer trusted")?;
            bail!(
                "Thread interviews run on the authoritative machine. Run exactly: `ssh {} pika expert refresh {}`; then run exactly: `pika sync {}`.",
                shell_words::quote(&node.ssh_target),
                shell_words::quote(&remote.session.session_id),
                shell_words::quote(&remote.node_name)
            )
        }
    }
    let (provider, query) = name
        .split_once(':')
        .and_then(|(prefix, query)| {
            prefix
                .parse::<Provider>()
                .ok()
                .map(|provider| (Some(provider), query))
        })
        .unwrap_or((None, name));
    let matches = pika
        .store
        .list_sessions()?
        .into_iter()
        .chain(pika.store.list_untracked_sessions()?)
        .filter(|session| provider.is_none_or(|value| session.provider == value))
        .filter(|session| {
            session.session_id == query
                || session.provider_thread_id() == query
                || session
                    .name
                    .as_deref()
                    .is_some_and(|value| value.eq_ignore_ascii_case(query))
        })
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [session] => Ok(session.clone()),
        [] => bail!("No tracked or unwatched expert conversation matches {name:?}."),
        many => bail!(
            "{name:?} matches {} expert conversations; use PROVIDER:{name} or the exact UUID.",
            many.len()
        ),
    }
}
fn split_values(v: Vec<String>) -> Vec<String> {
    v.into_iter()
        .flat_map(|s| {
            s.split(',')
                .map(str::trim)
                .filter(|x| !x.is_empty())
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .collect()
}
fn explain(pika: &Pika, a: ExplainArgs) -> Result<i32> {
    let manager = FleetManager::new(&pika.store, SshTransport::default());
    let remote = manager
        .resolve(&a.name, false, false)
        .map_err(anyhow::Error::from)?;
    let s = if let Some(remote) = &remote {
        remote.session.clone()
    } else {
        select_current_local(pika, &a.name)?
    };
    let o = if remote.is_some() {
        Vec::new()
    } else {
        pika.store.status_observations(s.provider, &s.session_id)?
    };
    #[derive(Serialize)]
    struct E<'a> {
        session: &'a Session,
        observations: &'a [crate::model::StatusObservation],
    }
    if a.json {
        println!(
            "{}",
            serde_json::to_string(&E {
                session: &s,
                observations: &o
            })?
        )
    } else {
        println!(
            "{} · {}\nIdentity: {}:{}\nHome: {}",
            s.status,
            s.display_name(),
            s.provider,
            s.session_id,
            s.home_state
        );
        if let Some(remote) = remote {
            println!(
                "Machine: {} · cached {} ago{}",
                remote.node_name,
                short_age(remote.seen_at),
                if remote.stale { " · stale" } else { "" }
            );
        }
        if let Some(r) = &s.attention_reason {
            println!("Why: {r}")
        }
        for x in o {
            println!(
                "Evidence: {:?} {} at {:.3} · {}",
                x.kind, x.status, x.observed_at, x.source
            )
        }
    }
    Ok(0)
}
fn activity(pika: &Pika, a: ActivityArgs) -> Result<i32> {
    let _ = pika.reconcile_local()?;
    let e = pika.store.list_activity_events(a.limit.min(10_000))?;
    if a.json {
        println!("{}",serde_json::to_string(&e.iter().map(|x|serde_json::json!({"id":x.event_id,"provider":x.provider,"session_id":x.session_id,"name":x.name,"status":x.status,"reason":x.attention_reason,"error":x.error,"at":x.event_at})).collect::<Vec<_>>())?)
    } else if e.is_empty() {
        println!("No attention events recorded.")
    } else {
        for x in e {
            println!(
                "{:<11} {:<8} {}",
                x.status,
                x.provider,
                x.name.as_deref().unwrap_or(&x.session_id)
            )
        }
    }
    Ok(0)
}
fn skill_command(pika: &Pika, a: SkillArgs) -> Result<i32> {
    match a.command {
        SkillCommand::Show => print!("{}", skill::AGENT_CONVO_SKILL),
        SkillCommand::Install { path, json } => {
            let r = skill::install(&path.unwrap_or_else(|| skill::default_target(&pika.paths)))?;
            if json {
                println!("{}", serde_json::to_string(&r)?)
            } else {
                println!("Agent consultation skill ready at {}", r.path.display())
            }
        }
    }
    Ok(0)
}

fn setup_command(pika: &Pika, a: SetupArgs) -> Result<i32> {
    let first_setup = !pika.paths.config.is_file();
    let explicit_machines = a.machines.clone();
    let explicit_machine_setup = !explicit_machines.is_empty();
    let install_bundle = a.install_bundle.clone();
    let remote_import_all = a.remote_import_all;
    let explicit_import = a.import_all || a.browse_all || remote_import_all;
    let tracked_before = if a.dry_run {
        Vec::new()
    } else {
        pika.store.list_sessions()?
    };
    let executable = std::env::current_exe()?.canonicalize()?;
    let mut executables = pika.config.provider_executables.clone();
    for (provider, value) in [
        ("codex", a.codex_executable),
        ("claude", a.claude_executable),
        ("opencode", a.opencode_executable),
    ] {
        if let Some(v) = value {
            executables.insert(provider.into(), v);
        }
    }
    let options = SetupOptions {
        default_provider: a.default_provider.unwrap_or(pika.config.default_provider),
        machine_alias: a.machine_alias,
        provider_executables: executables,
        provider_runtime_path: pika.config.provider_runtime_path.clone(),
        pika_executable: executable,
    };
    let mut changes = setup::proposed_hook_changes(&SetupPaths::from(&pika.paths), &options)?;
    changes.push(skill::proposed_change(&pika.paths)?);
    let schedule_platform = scheduler::SchedulePlatform::current();
    let base_directories = directories::BaseDirs::new();
    let schedule_directory =
        schedule_platform
            .zip(base_directories.as_ref())
            .map(|(platform, base)| {
                scheduler::default_unit_directory(
                    platform,
                    base.home_dir(),
                    std::env::var_os("XDG_CONFIG_HOME")
                        .as_deref()
                        .map(std::path::Path::new),
                )
            });
    let runtime_path = options
        .provider_runtime_path
        .clone()
        .or_else(|| std::env::var("PATH").ok())
        .unwrap_or_else(|| "/usr/local/bin:/usr/bin:/bin".into());
    let schedule_request =
        schedule_platform
            .zip(schedule_directory.as_deref())
            .map(|(platform, unit_directory)| ScheduleRequest {
                platform,
                unit_directory,
                pika_executable: &options.pika_executable,
                runtime_path: &runtime_path,
            });
    if let Some(request) = &schedule_request {
        changes.extend(scheduler::schedule_changes(request)?);
    }
    println!(
        "Pika setup · exact recovery for Codex + Claude + OpenCode\nPreview first · existing settings retained · backups before writes\n"
    );
    let changed = changes.iter().filter(|change| change.changed()).count();
    for c in changes.iter().filter(|c| c.changed()) {
        print!("{}", setup_preview::sanitized_unified_diff(c));
    }
    if a.dry_run {
        println!("Dry run only · no files changed.");
        return Ok(0);
    }
    if changed > 0 && !a.yes {
        if !io::stdin().is_terminal() {
            eprintln!("pika: Re-run exactly: `pika setup --yes` to apply these changes.");
            return Ok(2);
        }
        if !confirm("Apply these changes? [y/N] ")? {
            println!("No changes applied.");
            return Ok(0);
        }
    }
    if changed == 0 {
        println!("Pika hooks, skill, configuration, and schedule are already current.");
    }
    let receipt = setup::apply_changes(&changes, &format!("{}", now() as u64))?;
    pika.store.initialize()?;
    for backup in &receipt.backups {
        println!("Backup: {}", backup.display());
    }
    println!(
        "Setup files applied · {} file(s) updated",
        receipt.written.len()
    );
    if let Some(request) = &schedule_request {
        match scheduler::activate_schedule(request) {
            Ok(activation) => println!("Expert refresh active · {}", activation.detail),
            Err(error) => println!(
                "Expert refresh inactive · {error}. Run `pika expert refresh --due` manually."
            ),
        }
    }
    if !first_setup {
        match pika.reconcile_local() {
            Ok(inventory) => println!(
                "Reconciled {} tracked conversation name(s).",
                inventory.sessions.len()
            ),
            Err(error) => println!(
                "Name reconciliation incomplete · {error}. No conversation identity was changed."
            ),
        }
    }

    let required = tracked_before
        .iter()
        .map(|session| session.provider)
        .chain(std::iter::once(options.default_provider))
        .collect::<BTreeSet<_>>();
    let commissioning = setup::commissioning_report(
        &SetupPaths::from(&pika.paths),
        &pika.store,
        &options,
        &required,
        now(),
    )?;
    println!("\nCommissioning status");
    for provider in &commissioning.providers {
        let proof = provider.observation.as_ref().map_or_else(
            || "not yet proven".to_owned(),
            |observation| {
                if provider.observed() {
                    format!(
                        "{} · id {} · {:.0}s ago",
                        setup_receipt_text(&observation.event_name),
                        setup_receipt_text(&observation.session_id)
                            .chars()
                            .take(8)
                            .collect::<String>(),
                        (now() - observation.observed_at).max(0.0)
                    )
                } else {
                    "fingerprint does not match this setup".to_owned()
                }
            },
        );
        println!(
            "  {:<8} binary {}  hooks {}  observed {} · {} · launches {} · {}",
            setup_provider_label(provider.provider),
            if provider.runtime.compatible() {
                "✓"
            } else {
                "✗"
            },
            if provider.hooks_active { "✓" } else { "✗" },
            if provider.observed() { "✓" } else { "○" },
            proof,
            if provider.overdue_launches.is_empty() {
                "healthy"
            } else {
                "DEGRADED"
            },
            if provider.required {
                "required"
            } else {
                "optional"
            },
        );
    }
    if commissioning.commissioned() {
        println!(
            "\nPika commissioned · required integrations are compatible, active, observed, and healthy."
        );
    } else {
        println!(
            "\nPika not yet commissioned · missing proof: {}.",
            commissioning.missing_proofs().join("; ")
        );
    }

    if !a.no_machines && (explicit_machine_setup || (first_setup && io::stdin().is_terminal())) {
        let home = directories::BaseDirs::new()
            .context("cannot determine home directory for machine discovery")?;
        let report = fleet::discover_node_candidates(
            &pika.store,
            home.home_dir(),
            std::path::Path::new("tailscale"),
            Duration::from_secs(3),
        )
        .map_err(anyhow::Error::from)?;
        let selected = if explicit_machines.is_empty() {
            choose_machine_candidates(report.candidates)?
        } else {
            let discovered = report
                .candidates
                .into_iter()
                .map(|candidate| (candidate.key(), candidate))
                .collect::<std::collections::HashMap<_, _>>();
            explicit_machines
                .into_iter()
                .map(|target| {
                    discovered.get(&target.to_lowercase()).cloned().map_or_else(
                        || {
                            Ok(NodeCandidate {
                                alias: fleet::suggest_alias(&target)
                                    .map_err(anyhow::Error::from)?,
                                ssh_target: target,
                                sources: vec!["explicit".to_owned()],
                                hostname: None,
                                online: None,
                                os_name: None,
                            })
                        },
                        Ok,
                    )
                })
                .collect::<Result<Vec<_>>>()?
        };
        let manager = FleetManager::new(&pika.store, SshTransport::default());
        let mut added = Vec::new();
        for candidate in selected {
            match manager.add(&candidate, Some(&candidate.alias)) {
                Ok(node) => {
                    println!(
                        "Trusted Pika machine {} · {}",
                        node.alias,
                        &node.node_id[..8]
                    );
                    added.push(node);
                }
                Err(error) if error.kind == FleetErrorKind::Missing => {
                    if let Some(bundle_path) = install_bundle.as_deref() {
                        let transport = SshTransport::default();
                        let target = transport
                            .native_target(&candidate.ssh_target)
                            .map_err(anyhow::Error::from)?;
                        let prepared = update::prepare_remote_install_bundle(
                            bundle_path,
                            &target,
                            Some(VERSION),
                        )?;
                        if a.yes
                            || confirm(&format!(
                                "Install Pika {} on {} ({})? [y/N] ",
                                prepared.version(),
                                candidate.ssh_target,
                                target
                            ))?
                        {
                            transport
                                .install_bundle(&candidate.ssh_target, &prepared, None)
                                .map_err(anyhow::Error::from)?;
                            let node = manager
                                .add(&candidate, Some(&candidate.alias))
                                .map_err(anyhow::Error::from)?;
                            println!(
                                "Installed and trusted {} · node {} verified",
                                node.alias,
                                &node.node_id[..8]
                            );
                            added.push(node);
                        } else {
                            println!("Nothing installed on {}.", candidate.ssh_target);
                        }
                    } else {
                        eprintln!(
                            "pika: Pika is missing on {}. Nothing was installed. Install Pika there, then rerun exactly: `pika setup --machine {}`; or provide a reviewed native bundle with `--install-bundle PATH`.",
                            candidate.ssh_target,
                            shell_words::quote(&candidate.ssh_target)
                        );
                    }
                }
                Err(error) => eprintln!(
                    "pika: {} was not added: {}. No other candidate was contacted.",
                    candidate.alias, error
                ),
            }
        }
        if remote_import_all {
            for node in added {
                let candidates = manager
                    .remote_candidates(&node, false)
                    .map_err(anyhow::Error::from)?;
                let mut count = 0;
                for candidate in candidates {
                    manager
                        .adopt(&node, &candidate, None)
                        .map_err(anyhow::Error::from)?;
                    count += 1;
                }
                println!("Watching {count} conversation(s) on {}.", node.alias);
            }
        }
    }
    if !a.no_import && (first_setup || explicit_import) {
        let named = pika.import_named()?;
        let mut selected = if a.import_all {
            named
        } else {
            choose_candidates(named)?
        };
        let browse_recent = a.browse_all
            || (!a.import_all
                && first_setup
                && io::stdin().is_terminal()
                && confirm("Browse recent provider-labeled conversations too? [y/N] ")?);
        if browse_recent {
            let recent = pika.import_recent_unnamed(20)?;
            if a.import_all {
                selected.extend(recent);
            } else {
                selected.extend(choose_recent_candidates(recent)?);
            }
        }
        for c in &selected {
            pika.adopt_candidate(c)?;
        }
        if !selected.is_empty() {
            println!("Watching {} conversation(s).", selected.len())
        }
    }
    if !a.skip_walkthrough && io::stdin().is_terminal() {
        println!(
            "\nReady. Run `pika` for the live board or `pika NAME` to return to, adopt, or create a conversation.\nExpert cards refresh gradually; `pika ask NAME \"QUESTION\"` consults one privately."
        );
    }
    Ok(0)
}
fn setup_provider_label(provider: Provider) -> &'static str {
    match provider {
        Provider::Codex => "Codex",
        Provider::Claude => "Claude",
        Provider::Opencode => "OpenCode",
    }
}
fn setup_receipt_text(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(120)
        .collect()
}
fn choose_machine_candidates(candidates: Vec<NodeCandidate>) -> Result<Vec<NodeCandidate>> {
    if candidates.is_empty() {
        return Ok(Vec::new());
    }
    println!("\nPika found these machine candidates without connecting:");
    for (index, candidate) in candidates.iter().enumerate() {
        println!(
            "  {:>2}. {:<20} {:<36} {}",
            index + 1,
            candidate.alias,
            candidate.ssh_target,
            candidate.sources.join(" + ")
        );
    }
    print!("Add machine numbers, `all`, or press Enter for none: ");
    io::stdout().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    select_numbered(candidates, &answer)
}
fn choose_candidates(c: Vec<crate::model::Candidate>) -> Result<Vec<crate::model::Candidate>> {
    if c.is_empty() {
        return Ok(Vec::new());
    }
    println!("\nNamed conversations available to watch:");
    for (i, x) in c.iter().enumerate() {
        println!(
            "  {:>2}. {:<8} {:<28} {}",
            i + 1,
            x.provider,
            x.name.as_deref().unwrap_or("<unnamed>"),
            x.cwd.as_deref().unwrap_or("—")
        )
    }
    if !io::stdin().is_terminal() {
        return Ok(Vec::new());
    }
    print!("Choose numbers, `all`, or press Enter for none: ");
    io::stdout().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    select_numbered(c, &answer)
}
fn choose_recent_candidates(
    c: Vec<crate::model::Candidate>,
) -> Result<Vec<crate::model::Candidate>> {
    if c.is_empty() {
        println!("No recent unnamed conversations found.");
        return Ok(Vec::new());
    }
    println!("\nRecent unnamed conversations · provider labels are shown only for orientation:");
    for (index, candidate) in c.iter().enumerate() {
        let label = candidate
            .name
            .as_deref()
            .filter(|name| !name.trim().is_empty())
            .unwrap_or("<unnamed>");
        println!(
            "  {:>2}. {:<8} {:<28} {}",
            index + 1,
            candidate.provider,
            label,
            candidate.cwd.as_deref().unwrap_or("—")
        );
    }
    print!("Choose numbers, `all`, or press Enter for none: ");
    io::stdout().flush()?;
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    select_numbered(c, &answer)
}
fn select_numbered<T: Clone>(values: Vec<T>, answer: &str) -> Result<Vec<T>> {
    let answer = answer.trim();
    if answer.is_empty() {
        return Ok(Vec::new());
    }
    if answer.eq_ignore_ascii_case("all") {
        return Ok(values);
    }
    answer
        .split([',', ' '])
        .filter(|v| !v.is_empty())
        .map(|v| {
            let i = v.parse::<usize>().context("choices must be numbers")?;
            values
                .get(i.saturating_sub(1))
                .cloned()
                .context("choice is out of range")
        })
        .collect()
}
fn confirm(prompt: &str) -> Result<bool> {
    if !io::stdin().is_terminal() {
        return Ok(false);
    }
    print!("{prompt}");
    io::stdout().flush()?;
    let mut v = String::new();
    io::stdin().read_line(&mut v)?;
    Ok(matches!(
        v.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}
fn doctor(pika: &Pika, a: DoctorArgs) -> Result<i32> {
    let exe = std::env::current_exe()?.canonicalize()?;
    let report = if a.repair_stale {
        doctor::inspect_native_and_repair_stale(&pika.paths, &pika.store, &pika.tmux, &exe, 300.0)
    } else {
        doctor::inspect_native(&pika.paths, &pika.store, &pika.tmux, &exe)
    };
    if a.json {
        println!("{}", serde_json::to_string(&report)?)
    } else {
        print!("{}", report.render_human(a.verbose));
        for repair in &report.repairs {
            println!(
                "REPAIRED {} {} · {}",
                repair.kind, repair.reference, repair.reason
            );
        }
    }
    Ok(if report.safe_to_disconnect { 0 } else { 1 })
}

fn hook(a: HookArgs) -> Result<i32> {
    let paths = Paths::discover()?;
    let config = Config::load(&paths).unwrap_or_default();
    let store = Store::from_paths(&paths);
    let payload = match hooks::parse_hook_payload(io::stdin().lock(), a.provider) {
        Ok(p) => p,
        Err(_) => {
            if a.provider == Provider::Codex {
                println!("{{}}")
            }
            return Ok(0);
        }
    };
    let mut context = HookContext::from_environment(a.provider)?;
    context.codex_worker_originators = config.codex_worker_originators.clone();
    context.opencode_worker_title_prefixes = config.opencode_worker_title_prefixes.clone();
    let process_observation = process::observe();
    let Some(processes) = complete_hook_processes(&process_observation) else {
        if a.provider == Provider::Codex {
            println!("{{}}")
        }
        return Ok(0);
    };
    let parent = i64::from(unsafe { libc::getppid() });
    context.owner_pid = process::provider_ancestor(parent, a.provider, processes);
    context.owner_start_time = context
        .owner_pid
        .and_then(|pid| processes.get(&pid))
        .and_then(|r| i64::try_from(r.start_time).ok());
    if let Some(pane_id) = context.pane_id.as_deref()
        && let Ok(Some(pane)) = crate::tmux::Tmux::default().get_pane(pane_id)
    {
        context.pane_session = Some(pane.session_name.clone());
        context.pane_attached = pane.attached;
        let tree = process::process_tree(pane.pane_pid, processes);
        context.exact_home_verified = context.owner_pid.is_some_and(|pid| tree.contains(&pid))
            && (pane.pika_launch_token == context.launch_token
                || (pane.pika_provider == Some(a.provider)
                    && pane.pika_session_id.as_deref() == Some(&payload.session_id)));
    }
    if let Ok(result) = hooks::handle_hook(&store, a.provider, &payload, &context) {
        if context.exact_home_verified
            && let Some(tag) = &result.tag_request
        {
            let tmux = crate::tmux::Tmux::default();
            let tagged = tmux.tag_pane(
                &tag.pane_id,
                Some(tag.provider),
                Some(&tag.session_id),
                Some(&tag.name),
                context.launch_token.as_deref(),
            );
            if tagged.is_ok()
                && let (Some(token), Some(pid), Some(start)) = (
                    context.launch_token.as_deref(),
                    context.owner_pid,
                    context.owner_start_time,
                )
                && let Ok(Some(verified)) = tmux.get_pane(&tag.pane_id)
                && verified.pika_provider == Some(tag.provider)
                && verified.pika_session_id.as_deref() == Some(&tag.session_id)
            {
                // The pane reservation initially records tmux's holding-shell
                // PID. Once the provider hook proves its exact descendant and
                // generation inside that pane, promote the provider PID before
                // certifying the durable recovery owner.
                let _ = store.finalize_pending_pane(
                    token,
                    &verified.session_name,
                    &verified.pane_id,
                    Some(pid),
                    Some(start),
                );
                let _ = hooks::certify_hook_home(&store, token, tag, pid, start);
            }
        }
        if config.alerts == "tmux"
            && let Some(alert) = &result.alert
        {
            let reason = alert
                .attention_reason
                .as_deref()
                .unwrap_or(alert.status.as_str());
            let _ = crate::tmux::Tmux::default().display_alert(&format!(
                "Pika: {} ({}) — {reason}",
                alert.name, alert.provider
            ));
        }
        if let Some(rename) = &result.native_name_request {
            let key = format!(
                "native_name_error:{}:{}",
                rename.provider, rename.session_id
            );
            let providers = crate::providers::Providers::new(&paths, &config);
            if providers.set_codex_native_name(&rename.session_id, &rename.name) {
                let _ = store.delete_meta(&key);
            } else {
                let _ = store.set_meta(&key, &rename.name);
            }
        }
        let out = hooks::hook_stdout(a.provider, &result);
        if !out.is_empty() {
            println!("{out}")
        }
    } else if a.provider == Provider::Codex {
        println!("{{}}")
    }
    Ok(0)
}

fn complete_hook_processes(
    observation: &process::ProcessObservation,
) -> Option<&std::collections::BTreeMap<i64, process::ProcessRecord>> {
    observation.require_complete("accept hook identity").ok()
}

fn process_exit(a: ProcessExitArgs) -> Result<i32> {
    let paths = Paths::discover()?;
    hooks::handle_process_exit(
        &Store::from_paths(&paths),
        a.provider,
        a.code,
        a.session_id.as_deref(),
        a.launch_token.as_deref(),
        a.owner_token.as_deref(),
        now(),
    )?;
    Ok(0)
}
fn terminal_bridge(a: TerminalBridgeArgs) -> Result<i32> {
    let foreground = terminal::decode_color(&a.foreground).context("invalid foreground color")?;
    let background = terminal::decode_color(&a.background).context("invalid background color")?;
    terminal::run_pty_bridge(
        &a.command,
        Some(Palette {
            foreground,
            background,
        }),
        false,
    )
}
fn install_native(a: InstallNativeArgs) -> Result<i32> {
    let manifest = ReleaseManifest::parse(&fs::read(&a.manifest)?)?;
    let outcome = update::install_staged(InstallRequest {
        manifest: &manifest,
        target: &a.target,
        artifact: &a.artifact,
        candidate: &a.candidate,
        root: &a.root,
        bin_dir: &a.bin_dir,
    })?;
    println!(
        "Pika {} · {}",
        if outcome.activated {
            "installed"
        } else {
            "already current"
        },
        outcome.launcher.display()
    );
    if !a.no_setup {
        println!("Run `pika setup` to review provider integration.")
    }
    Ok(0)
}
fn update_command(a: UpdateArgs) -> Result<i32> {
    let executable =
        std::env::current_exe().context("cannot locate the running Pika executable")?;
    let outcome = update::update_managed(UpdateRequest {
        executable: &executable,
        bundle: a.bundle.as_deref(),
        release: a.release.as_deref(),
        check: a.check,
    })?;
    println!("{}", outcome.message());
    Ok(0)
}
fn ask(pika: &Pika, a: AskArgs) -> Result<i32> {
    let manager = FleetManager::new(&pika.store, SshTransport::default());
    if let Some(remote) = manager
        .resolve(&a.name, false, true)
        .map_err(anyhow::Error::from)?
    {
        return ask_remote(pika, remote, a);
    }
    let session = select_one(pika, &a.name)?;
    let mut options =
        crate::consult::ConsultationOptions::new(pika.config.executable(session.provider));
    options.fast = a.fast;
    if session.provider == Provider::Opencode {
        options.opencode_database = Some(pika.paths.opencode_data_home.join("opencode.db"));
    }
    let mut side = open_local_consultation(pika, &session, options)?;
    if a.jsonl {
        return ask_jsonl(&mut side, &session, &a.question.join(" "));
    }
    let question = if a.question.is_empty() {
        if !io::stdin().is_terminal() {
            bail!("provide a question after `--` or use --jsonl");
        }
        print!("Ask {} › ", session.display_name());
        io::stdout().flush()?;
        let mut value = String::new();
        io::stdin().read_line(&mut value)?;
        value
    } else {
        a.question.join(" ")
    };
    let answer = match side.ask(&question) {
        Ok(answer) => answer,
        Err(error) => return Err(consultation_error(error)),
    };
    let close_error = side.close().err();
    let receipt = side.receipt();
    if a.json {
        println!(
            "{}",
            serde_json::to_string(&serde_json::json!({
                "answer": answer,
                "receipt": receipt,
                "cleanup_error": close_error.as_ref().and_then(|error| error.cleanup_error.clone()),
            }))?
        );
    } else {
        println!("{answer}");
        if let Some(error) = &close_error {
            eprintln!("pika: answer received, but private-side cleanup failed: {error}");
            return Ok(2);
        }
    }
    Ok(if close_error.is_none() { 0 } else { 2 })
}

fn ask_remote(pika: &Pika, remote: fleet::FleetSession, a: AskArgs) -> Result<i32> {
    require_remote_source_available(&remote)?;
    let local_policy = crate::consult::consultation_policy(remote.session.provider, a.fast)?;
    let policy = fleet::ConsultationPolicy {
        consultation_mode: local_policy.mode.clone(),
        model: local_policy.model.clone().unwrap_or_default(),
        effort: local_policy.effort.clone().unwrap_or_default(),
    };
    let node = pika
        .store
        .get_fleet_node(&remote.node_id)?
        .context("Remote expert machine is no longer trusted")?;
    let mut side = fleet::RemoteConsultation::open(
        &SshTransport::default(),
        node,
        remote.clone(),
        policy,
        Duration::from_secs(30),
        Duration::from_secs(900),
        Duration::from_secs(30),
    )
    .map_err(anyhow::Error::from)?;
    if a.jsonl {
        return ask_remote_jsonl(&mut side, &remote, &a.question.join(" "));
    }
    let question = if a.question.is_empty() {
        if !io::stdin().is_terminal() {
            bail!("provide a question after `--` or use --jsonl")
        }
        print!("Ask {} › ", remote.qualified_name());
        io::stdout().flush()?;
        let mut value = String::new();
        io::stdin().read_line(&mut value)?;
        value
    } else {
        a.question.join(" ")
    };
    let answer = side.ask(&question).map_err(anyhow::Error::from)?;
    let cleanup = side.close();
    if a.json {
        println!(
            "{}",
            serde_json::to_string(&serde_json::json!({
                "answer": answer,
                "cleanup": cleanup.as_ref().map(|receipt| &receipt.cleanup).ok(),
                "cleanup_error": cleanup.as_ref().err().map(ToString::to_string),
            }))?
        );
    } else {
        println!("{answer}");
        if let Err(error) = cleanup {
            eprintln!(
                "pika: answer received, but remote private-side cleanup is unconfirmed: {error}"
            );
            return Ok(2);
        }
    }
    Ok(0)
}

fn require_remote_source_available(remote: &fleet::FleetSession) -> Result<()> {
    let availability = remote.source_availability();
    if availability == "machine-unreachable" {
        bail!(
            "{} is stale. Run exactly: `pika sync {}` before asking it.",
            remote.qualified_name(),
            shell_words::quote(&remote.node_name)
        )
    }
    if !crate::experts::source_is_available(availability) {
        bail!(
            "Cannot consult {}: {availability}. No question was sent; watching is unchanged.",
            remote.qualified_name(),
        )
    }
    Ok(())
}

fn ask_remote_jsonl(
    side: &mut fleet::RemoteConsultation,
    remote: &fleet::FleetSession,
    initial: &str,
) -> Result<i32> {
    println!(
        "{}",
        serde_json::to_string(&serde_json::json!({
            "type": "opened",
            "ephemeral": true,
            "provider": remote.session.provider,
            "workstream_id": remote.session.session_id,
            "node_id": remote.node_id,
            "machine": remote.node_name,
        }))?
    );
    let mut result = 0;
    if !initial.trim().is_empty() {
        match side.ask(initial) {
            Ok(answer) => println!(
                "{}",
                serde_json::to_string(&serde_json::json!({"type":"answer", "text":answer}))?
            ),
            Err(error) => {
                println!(
                    "{}",
                    serde_json::to_string(
                        &serde_json::json!({"type":"error", "kind":error.kind.as_str(), "message":error.message, "receipt":error.receipt})
                    )?
                );
                result = 2;
            }
        }
    }
    if result == 0 {
        let mut input = io::stdin().lock();
        loop {
            let line = match read_bounded_jsonl_line(&mut input) {
                Ok(Some(line)) => line,
                Ok(None) => break,
                Err(error) => {
                    eprintln!("pika: remote consultation input failed: {error}");
                    result = 2;
                    break;
                }
            };
            let value: serde_json::Value = match serde_json::from_str(&line) {
                Ok(value) => value,
                Err(error) => {
                    println!(
                        "{}",
                        serde_json::to_string(
                            &serde_json::json!({"type":"error", "kind":"invalid_request", "message":format!("invalid JSONL question: {error}")})
                        )?
                    );
                    result = 2;
                    break;
                }
            };
            if value.get("close").and_then(serde_json::Value::as_bool) == Some(true) {
                break;
            }
            let Some(question) = value.get("question").and_then(serde_json::Value::as_str) else {
                println!(
                    "{}",
                    serde_json::to_string(
                        &serde_json::json!({"type":"error", "kind":"invalid_request", "message":"each JSONL object needs `question` or {\"close\":true}"})
                    )?
                );
                result = 2;
                break;
            };
            match side.ask(question) {
                Ok(answer) => println!(
                    "{}",
                    serde_json::to_string(&serde_json::json!({"type":"answer", "text":answer}))?
                ),
                Err(error) => {
                    println!(
                        "{}",
                        serde_json::to_string(
                            &serde_json::json!({"type":"error", "kind":error.kind.as_str(), "message":error.message, "receipt":error.receipt})
                        )?
                    );
                    result = 2;
                    break;
                }
            }
            io::stdout().flush()?;
        }
    }
    match side.close() {
        Ok(receipt) => println!(
            "{}",
            serde_json::to_string(&serde_json::json!({
                "type":"closed", "receipt_version":2, "discarded":true,
                "cleanup":receipt.cleanup, "answers_received":receipt.answers_received,
                "turn":receipt.turn, "retry_safe":receipt.retry_safe,
            }))?
        ),
        Err(error) => {
            println!(
                "{}",
                serde_json::to_string(&serde_json::json!({
                    "type":"closed", "receipt_version":2, "discarded":false,
                    "cleanup":"unknown", "kind":error.kind.as_str(), "message":error.message,
                }))?
            );
            result = 2;
        }
    }
    Ok(result)
}

fn ask_jsonl(
    side: &mut crate::consult::Consultation,
    session: &Session,
    initial: &str,
) -> Result<i32> {
    let mut input = io::stdin().lock();
    println!(
        "{}",
        serde_json::to_string(&serde_json::json!({
            "type": "opened",
            "ephemeral": true,
            "provider": session.provider,
            "workstream_id": session.session_id,
            "parent_id": side.parent_id(),
            "child_id": side.child_id(),
            "name": session.display_name(),
            "receipt": side.receipt(),
            "policy": side.policy(),
            "consultation_mode": side.policy().mode,
            "model": side.policy().model.as_deref().unwrap_or(""),
            "effort": side.policy().effort.as_deref().unwrap_or(""),
        }))?
    );
    let mut result = 0;
    if !initial.trim().is_empty() {
        match side.ask(initial) {
            Ok(answer) => println!(
                "{}",
                serde_json::to_string(&serde_json::json!({
                    "type": "answer",
                    "text": answer,
                    "receipt": side.receipt(),
                    "policy": side.policy(),
                }))?
            ),
            Err(error) => {
                let receipt = &error.receipt;
                println!(
                    "{}",
                    serde_json::to_string(&serde_json::json!({
                        "type":"error", "kind":"error", "message":error.to_string(),
                        "stage":receipt.stage, "delivery":receipt.delivery,
                        "cleanup":receipt.cleanup, "answers_received":receipt.answers_received,
                        "turn":receipt.turn, "retry_safe":receipt.retry_safe,
                        "receipt":receipt, "cleanup_error":error.cleanup_error,
                        "policy":side.policy(),
                    }))?
                );
                result = 2;
            }
        }
    }
    loop {
        if result != 0 {
            break;
        }
        let line = match read_bounded_jsonl_line(&mut input) {
            Ok(Some(line)) => line,
            Ok(None) => break,
            Err(error) => {
                emit_jsonl_input_error(side, &format!("consultation input failed: {error}"))?;
                result = 2;
                break;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        let value: serde_json::Value = match serde_json::from_str::<serde_json::Value>(&line) {
            Ok(value) if value.is_object() => value,
            Ok(_) => {
                emit_jsonl_input_error(side, "each --jsonl line must be a JSON object")?;
                result = 2;
                break;
            }
            Err(error) => {
                emit_jsonl_input_error(side, &format!("invalid JSONL question: {error}"))?;
                result = 2;
                break;
            }
        };
        if value.get("close").and_then(serde_json::Value::as_bool) == Some(true) {
            break;
        }
        let Some(question) = value.get("question").and_then(serde_json::Value::as_str) else {
            emit_jsonl_input_error(
                side,
                "each --jsonl object needs a string `question` or {\"close\":true}",
            )?;
            result = 2;
            break;
        };
        match side.ask(question) {
            Ok(answer) => println!(
                "{}",
                serde_json::to_string(&serde_json::json!({
                    "type": "answer",
                    "text": answer,
                    "receipt": side.receipt(),
                    "policy": side.policy(),
                }))?
            ),
            Err(error) => {
                let retry_safe = error.receipt.retry_safe;
                let receipt = &error.receipt;
                println!(
                    "{}",
                    serde_json::to_string(&serde_json::json!({
                        "type": "error",
                        "error": error.to_string(),
                        "message": error.to_string(),
                        "kind": "error",
                        "stage": receipt.stage,
                        "delivery": receipt.delivery,
                        "cleanup": receipt.cleanup,
                        "answers_received": receipt.answers_received,
                        "turn": receipt.turn,
                        "retry_safe": receipt.retry_safe,
                        "receipt": receipt,
                        "cleanup_error": error.cleanup_error,
                        "policy": side.policy(),
                    }))?
                );
                if !retry_safe {
                    result = 2;
                    break;
                }
            }
        }
        io::stdout().flush()?;
    }
    let cleanup = side.close();
    if let Err(error) = &cleanup {
        result = 2;
        let receipt = &error.receipt;
        println!(
            "{}",
            serde_json::to_string(&serde_json::json!({
                "type": "error",
                "error": error.to_string(),
                "message": error.to_string(),
                "kind": "outcome_unknown",
                "stage": receipt.stage,
                "delivery": receipt.delivery,
                "cleanup": receipt.cleanup,
                "answers_received": receipt.answers_received,
                "turn": receipt.turn,
                "retry_safe": receipt.retry_safe,
                "receipt": receipt,
                "cleanup_error": &error.cleanup_error,
                "policy": side.policy(),
            }))?
        );
    }
    let receipt = side.receipt();
    println!(
        "{}",
        serde_json::to_string(&serde_json::json!({
            "type": "closed",
            "discarded": cleanup.is_ok(),
            "receipt_version": receipt.receipt_version,
            "stage": receipt.stage,
            "delivery": receipt.delivery,
            "cleanup": receipt.cleanup,
            "answers_received": receipt.answers_received,
            "turn": receipt.turn,
            "retry_safe": receipt.retry_safe,
            "parent_transcript_unchanged": null,
            "parent_transcript_verification": "not_performed",
            "receipt": receipt,
            "policy": side.policy(),
        }))?
    );
    Ok(result)
}

fn read_bounded_jsonl_line(input: &mut impl io::BufRead) -> io::Result<Option<String>> {
    const LIMIT: usize = fleet::MAX_MESSAGE_BYTES;
    let mut bytes = Vec::with_capacity(4096);
    input
        .take((LIMIT + 2) as u64)
        .read_until(b'\n', &mut bytes)?;
    if bytes.is_empty() {
        return Ok(None);
    }
    let has_newline = bytes.last() == Some(&b'\n');
    let content_len = bytes.len().saturating_sub(usize::from(has_newline));
    if content_len > LIMIT || (!has_newline && bytes.len() > LIMIT) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "JSONL input exceeds the 4 MiB limit",
        ));
    }
    if has_newline {
        bytes.pop();
        if bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "JSONL input is not UTF-8"))
}

enum FleetConsultationInput {
    Question(String),
    Close,
    InputError(String),
}

fn spawn_fleet_consultation_input(
    cancellation: crate::consult::CancellationToken,
) -> mpsc::Receiver<FleetConsultationInput> {
    spawn_fleet_consultation_reader(io::BufReader::new(io::stdin()), cancellation)
}

fn spawn_fleet_consultation_reader<R>(
    mut input: R,
    cancellation: crate::consult::CancellationToken,
) -> mpsc::Receiver<FleetConsultationInput>
where
    R: io::BufRead + Send + 'static,
{
    let (sender, receiver) = mpsc::sync_channel(1);
    thread::spawn(move || {
        loop {
            let line = match read_bounded_jsonl_line(&mut input) {
                Ok(Some(line)) => line,
                Ok(None) => {
                    cancellation.cancel();
                    let _ = sender.try_send(FleetConsultationInput::Close);
                    return;
                }
                Err(error) => {
                    cancellation.cancel();
                    let _ = sender.try_send(FleetConsultationInput::InputError(format!(
                        "consultation input failed: {error}"
                    )));
                    return;
                }
            };
            if line.trim().is_empty() {
                continue;
            }
            let value = match serde_json::from_str::<serde_json::Value>(&line) {
                Ok(value) if value.is_object() => value,
                Ok(_) => {
                    cancellation.cancel();
                    let _ = sender.try_send(FleetConsultationInput::InputError(
                        "each --jsonl line must be a JSON object".into(),
                    ));
                    return;
                }
                Err(error) => {
                    cancellation.cancel();
                    let _ = sender.try_send(FleetConsultationInput::InputError(format!(
                        "invalid JSONL question: {error}"
                    )));
                    return;
                }
            };
            if value.get("close").and_then(serde_json::Value::as_bool) == Some(true) {
                cancellation.cancel();
                let _ = sender.try_send(FleetConsultationInput::Close);
                return;
            }
            let Some(question) = value.get("question").and_then(serde_json::Value::as_str) else {
                cancellation.cancel();
                let _ = sender.try_send(FleetConsultationInput::InputError(
                    "each --jsonl object needs a string `question` or {\"close\":true}".into(),
                ));
                return;
            };
            if sender
                .send(FleetConsultationInput::Question(question.to_owned()))
                .is_err()
            {
                return;
            }
        }
    });
    receiver
}

fn serve_fleet_consultation(
    side: &mut crate::consult::Consultation,
    session: &Session,
    inputs: mpsc::Receiver<FleetConsultationInput>,
    cancellation: &crate::consult::CancellationToken,
) -> Result<i32> {
    println!(
        "{}",
        serde_json::to_string(&serde_json::json!({
            "type": "opened",
            "ephemeral": true,
            "provider": session.provider,
            "workstream_id": session.session_id,
            "parent_id": side.parent_id(),
            "child_id": side.child_id(),
            "name": session.display_name(),
            "receipt": side.receipt(),
            "policy": side.policy(),
            "consultation_mode": side.policy().mode,
            "model": side.policy().model.as_deref().unwrap_or(""),
            "effort": side.policy().effort.as_deref().unwrap_or(""),
        }))?
    );
    io::stdout().flush()?;
    let mut result = 0;
    for input in inputs {
        match input {
            FleetConsultationInput::Question(question) => match side.ask(&question) {
                Ok(answer) => println!(
                    "{}",
                    serde_json::to_string(&serde_json::json!({
                        "type":"answer", "text":answer, "receipt":side.receipt(),
                        "policy":side.policy(),
                    }))?
                ),
                Err(_error) if cancellation.is_cancelled() => break,
                Err(error) => {
                    let retry_safe = error.receipt.retry_safe;
                    let receipt = &error.receipt;
                    println!(
                        "{}",
                        serde_json::to_string(&serde_json::json!({
                            "type":"error", "kind":"error", "message":error.to_string(),
                            "stage":receipt.stage, "delivery":receipt.delivery,
                            "cleanup":receipt.cleanup, "answers_received":receipt.answers_received,
                            "turn":receipt.turn, "retry_safe":retry_safe, "receipt":receipt,
                            "cleanup_error":error.cleanup_error, "policy":side.policy(),
                        }))?
                    );
                    if !retry_safe {
                        result = 2;
                        break;
                    }
                }
            },
            FleetConsultationInput::Close => break,
            FleetConsultationInput::InputError(message) => {
                emit_jsonl_input_error(side, &message)?;
                result = 2;
                break;
            }
        }
        io::stdout().flush()?;
    }
    let cleanup = side.close();
    if cleanup.is_err() {
        result = 2;
    }
    let receipt = side.receipt();
    println!(
        "{}",
        serde_json::to_string(&serde_json::json!({
            "type":"closed", "receipt_version":receipt.receipt_version,
            "discarded":cleanup.is_ok(), "cleanup":receipt.cleanup,
            "answers_received":receipt.answers_received, "turn":receipt.turn,
            "retry_safe":receipt.retry_safe,
            "kind":cleanup.as_ref().err().map(|_| "outcome_unknown"),
            "message":cleanup.as_ref().err().map(ToString::to_string),
        }))?
    );
    io::stdout().flush()?;
    Ok(result)
}

fn emit_jsonl_input_error(side: &crate::consult::Consultation, message: &str) -> Result<()> {
    let receipt = side.receipt();
    println!(
        "{}",
        serde_json::to_string(&serde_json::json!({
            "type":"error", "kind":"invalid_request", "message":message,
            "stage":receipt.stage, "delivery":receipt.delivery,
            "cleanup":receipt.cleanup, "answers_received":receipt.answers_received,
            "turn":receipt.turn, "retry_safe":receipt.retry_safe,
            "receipt":receipt, "policy":side.policy(),
        }))?
    );
    Ok(())
}

fn consultation_error(error: crate::consult::ConsultationError) -> anyhow::Error {
    anyhow::anyhow!(
        "{} · stage={:?} delivery={:?} cleanup={:?} retry_safe={}",
        error,
        error.receipt.stage,
        error.receipt.delivery,
        error.receipt.cleanup,
        error.receipt.retry_safe
    )
}

fn open_local_consultation(
    pika: &Pika,
    session: &Session,
    options: crate::consult::ConsultationOptions,
) -> Result<crate::consult::Consultation> {
    crate::experts::require_local_source_available(&pika.paths, &pika.config, session)?;
    crate::consult::Consultation::open(session, options).map_err(consultation_error)
}

fn ask_interactive(pika: &Pika, s: Session) -> Result<i32> {
    print!("Ask {} › ", s.display_name());
    io::stdout().flush()?;
    let mut q = String::new();
    io::stdin().read_line(&mut q)?;
    ask(
        pika,
        AskArgs {
            name: s.session_id,
            question: vec![q],
            jsonl: false,
            json: false,
            fast: false,
        },
    )
}
fn machines(pika: &Pika, a: MachinesArgs) -> Result<i32> {
    let manager = FleetManager::new(&pika.store, SshTransport::default());
    match a.command.unwrap_or(MachinesCommand::List { json: false }) {
        MachinesCommand::List { json } => {
            let nodes = manager.nodes().map_err(anyhow::Error::from)?;
            if json {
                println!("{}", serde_json::to_string(&nodes)?)
            } else if nodes.is_empty() {
                println!("No remote Pika machines are trusted yet. Run `pika machines discover`.")
            } else {
                for node in nodes {
                    println!("{:<20} {:<12} {}", node.alias, node.status, node.ssh_target)
                }
            }
        }
        MachinesCommand::Discover { json } => {
            let home = directories::BaseDirs::new()
                .context("cannot determine home directory for SSH discovery")?;
            let report = fleet::discover_node_candidates(
                &pika.store,
                home.home_dir(),
                std::path::Path::new("tailscale"),
                Duration::from_secs(3),
            )
            .map_err(anyhow::Error::from)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string(
                        &report
                            .candidates
                            .iter()
                            .map(|candidate| serde_json::json!({
                                "alias": candidate.alias,
                                "ssh_target": candidate.ssh_target,
                                "sources": candidate.sources,
                                "hostname": candidate.hostname,
                                "online": candidate.online,
                                "os": candidate.os_name,
                            }))
                            .collect::<Vec<_>>()
                    )?
                )
            } else if report.candidates.is_empty() {
                println!("No new SSH or Tailscale machine candidates found.")
            } else {
                println!("Pika found these machines without connecting:");
                for (index, candidate) in report.candidates.iter().enumerate() {
                    println!(
                        "  {:>2}. {:<20} {:<36} {}",
                        index + 1,
                        candidate.alias,
                        candidate.ssh_target,
                        candidate.sources.join(" + ")
                    )
                }
                println!("Add one with `pika machines add SSH_TARGET --alias NAME`.")
            }
        }
        MachinesCommand::Add { ssh_target, alias } => {
            let candidate = NodeCandidate {
                alias: alias
                    .clone()
                    .unwrap_or(fleet::suggest_alias(&ssh_target).map_err(anyhow::Error::from)?),
                ssh_target,
                sources: vec!["explicit".into()],
                hostname: None,
                online: None,
                os_name: None,
            };
            let node = manager
                .add(&candidate, alias.as_deref())
                .map_err(anyhow::Error::from)?;
            println!(
                "Trusted {} · {} · {} conversation(s) cached",
                node.alias,
                &node.node_id[..8],
                manager
                    .cached_sessions(Some(&node.node_id), false)
                    .map_err(anyhow::Error::from)?
                    .len()
            );
        }
        MachinesCommand::Remove { machine } => {
            let node = pika
                .store
                .get_fleet_node(&machine)?
                .with_context(|| format!("Unknown Pika machine {machine:?}"))?;
            pika.store.delete_fleet_node(&node.node_id)?;
            println!(
                "Stopped watching {}. Nothing was changed on that machine.",
                node.alias
            );
        }
        MachinesCommand::Ignore { ssh_target } => {
            fleet::validate_ssh_target(&ssh_target).map_err(anyhow::Error::from)?;
            pika.store.ignore_node_candidate(&ssh_target, now())?;
            println!("Ignored {ssh_target}. No SSH connection was made.");
        }
        MachinesCommand::Upgrade {
            machine,
            bundle,
            yes,
        } => {
            let bundle_path = bundle.context(
                "Remote upgrades require an explicit verified release bundle: `pika machines upgrade MACHINE --bundle PATH`.",
            )?;
            let (node, target) = manager
                .upgrade_target(&machine)
                .map_err(anyhow::Error::from)?;
            let prepared =
                update::prepare_remote_install_bundle(&bundle_path, &target, Some(VERSION))?;
            if !yes
                && !confirm(&format!(
                    "Install Pika {} on {} ({})? Running agents will not be restarted. [y/N] ",
                    prepared.version(),
                    node.alias,
                    target
                ))?
            {
                println!("No changes applied to {}.", node.alias);
                return Ok(0);
            }
            let updated = manager
                .upgrade_bundle(&machine, &prepared)
                .map_err(anyhow::Error::from)?;
            println!(
                "Updated {} to Pika {} · node {} verified",
                updated.alias,
                updated
                    .package_version
                    .as_deref()
                    .unwrap_or(prepared.version()),
                &updated.node_id[..8]
            );
        }
    }
    Ok(0)
}
fn sync(pika: &Pika, machine: &str) -> Result<i32> {
    let sessions = FleetManager::new(&pika.store, SshTransport::default())
        .refresh_node(machine)
        .map_err(anyhow::Error::from)?;
    println!("Refreshed {machine} · {} conversation(s)", sessions.len());
    Ok(0)
}
fn client_pair(pika: &Pika, a: ClientPairArgs) -> Result<i32> {
    if !a.stdio {
        bail!("client pairing requires --stdio")
    }
    let request =
        client_bridge::read_bridge_message(&mut io::stdin().lock()).map_err(anyhow::Error::from)?;
    let node_id = pika.store.ensure_local_node_id()?;
    let (routes, receipt) =
        client_bridge::accept_pairing(&pika.config.client_bridges, &node_id, &request)
            .map_err(anyhow::Error::from)?;
    let mut config = pika.config.clone();
    config.client_bridges = routes;
    if pika.paths.config.is_file() {
        let stamp = now() as u64;
        let mut backup = pika
            .paths
            .config
            .with_file_name(format!("config.json.pika-backup-{stamp}"));
        let mut suffix = 2_u32;
        while backup.exists() {
            backup = pika
                .paths
                .config
                .with_file_name(format!("config.json.pika-backup-{stamp}-{suffix}"));
            suffix += 1;
        }
        fs::copy(&pika.paths.config, &backup).with_context(|| {
            format!(
                "cannot back up client pairing configuration to {}",
                backup.display()
            )
        })?;
    }
    config.write(&pika.paths)?;
    println!("{}", serde_json::to_string(&receipt)?);
    Ok(0)
}
fn fleet_stdio(pika: &Pika, a: FleetInternalArgs) -> Result<i32> {
    if !a.stdio {
        bail!("fleet service requires --stdio")
    }
    let mut service = LocalFleetService { pika };
    let machine = local_machine_alias(pika)?;
    fleet::handle_fleet_stdio(
        &pika.store,
        &machine,
        VERSION,
        &mut service,
        io::BufReader::new(io::stdin().lock()),
        io::stdout().lock(),
    )
    .map_err(anyhow::Error::from)?;
    Ok(0)
}
fn fleet_open(pika: &Pika, a: FleetOpenArgs) -> Result<i32> {
    verify_local_node(pika, &a.expected_node_id)?;
    let session = exact_local_session(pika, a.provider, &a.session_id, true)?;
    let receipt = pika.open_session(session, true)?;
    finish_local_open(pika, &receipt)
}
fn fleet_ask(pika: &Pika, a: FleetAskArgs) -> Result<i32> {
    verify_local_node(pika, &a.expected_node_id)?;
    let session = exact_local_session(pika, a.provider, &a.session_id, true)?;
    let fast = a.fast || a.consultation_mode.as_deref() == Some("fast");
    let expected = crate::consult::consultation_policy(a.provider, fast)?;
    if a.consultation_mode
        .as_deref()
        .is_some_and(|mode| mode != expected.mode)
        || a.model
            .as_deref()
            .is_some_and(|model| expected.model.as_deref() != Some(model))
        || a.effort
            .as_deref()
            .is_some_and(|effort| expected.effort.as_deref() != Some(effort))
    {
        bail!("requested consultation policy does not match this provider's fixed Pika policy")
    }
    let mut options =
        crate::consult::ConsultationOptions::new(pika.config.executable(session.provider));
    options.fast = fast;
    let cancellation = crate::consult::CancellationToken::default();
    let inputs = spawn_fleet_consultation_input(cancellation.clone());
    options.cancellation = cancellation.clone();
    if session.provider == Provider::Opencode {
        options.opencode_database = Some(pika.paths.opencode_data_home.join("opencode.db"));
    }
    let mut side = open_local_consultation(pika, &session, options)?;
    serve_fleet_consultation(&mut side, &session, inputs, &cancellation)
}

fn verify_local_node(pika: &Pika, expected: &str) -> Result<()> {
    let actual = pika.store.ensure_local_node_id()?;
    if actual != expected {
        bail!(
            "NODE IDENTITY CHANGED: expected {}, received {}",
            expected.chars().take(8).collect::<String>(),
            actual.chars().take(8).collect::<String>()
        )
    }
    Ok(())
}

fn exact_local_session(
    pika: &Pika,
    provider: Provider,
    session_id: &str,
    fresh: bool,
) -> Result<Session> {
    if fresh {
        pika.reconcile_local()?;
    }
    let session = pika
        .store
        .get_session_by_thread(provider, session_id)?
        .with_context(|| format!("No exact {provider} conversation {session_id:?}"))?;
    if session.session_id != session_id && session.provider_thread_id() != session_id {
        bail!("exact provider conversation identity changed before the action")
    }
    Ok(session)
}

fn local_machine_alias(pika: &Pika) -> Result<String> {
    if let Some(value) = pika
        .config
        .additional
        .get("machine_alias")
        .and_then(serde_json::Value::as_str)
    {
        return fleet::machine_alias(value).map_err(anyhow::Error::from);
    }
    let hostname = std::env::var("HOSTNAME")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "pika-machine".into());
    fleet::machine_alias(hostname.split('.').next().unwrap_or(&hostname))
        .map_err(anyhow::Error::from)
}

struct LocalFleetService<'a> {
    pika: &'a Pika,
}

impl FleetService for LocalFleetService<'_> {
    fn snapshot(
        &mut self,
        expert_directory: bool,
    ) -> std::result::Result<serde_json::Value, FleetError> {
        let inventory = self.pika.reconcile_local()?;
        let node_id = self.pika.store.ensure_local_node_id()?;
        let machine = local_machine_alias(self.pika).map_err(FleetError::from)?;
        let mut sessions = inventory.sessions;
        let mut watched = std::collections::BTreeSet::new();
        for session in &sessions {
            watched.insert((session.provider, session.session_id.clone()));
        }
        let mut expert_sessions = Vec::new();
        if expert_directory {
            for session in self.pika.store.list_untracked_sessions()? {
                if self
                    .pika
                    .store
                    .get_stored_expert_profile(session.provider, &session.session_id)?
                    .is_some()
                {
                    expert_sessions.push(session);
                }
            }
        }
        let source_sessions = sessions
            .iter()
            .chain(expert_sessions.iter())
            .cloned()
            .collect::<Vec<_>>();
        let source_index = crate::experts::LocalSourceIndex::read(
            &self.pika.paths,
            &self.pika.config,
            &source_sessions,
        );
        expert_sessions.retain(|session| {
            source_index.availability(session) != crate::experts::SourceAvailability::Archived
        });
        let identities: std::collections::BTreeSet<_> = sessions
            .iter()
            .chain(expert_sessions.iter())
            .map(|session| (session.provider, session.session_id.clone()))
            .collect();
        let profiles: Vec<_> = self
            .pika
            .store
            .list_stored_expert_profiles()?
            .into_iter()
            .filter(|stored| {
                identities.contains(&(stored.profile.provider, stored.profile.session_id.clone()))
            })
            .collect();
        let profile_map: std::collections::BTreeMap<_, _> = profiles
            .iter()
            .map(|stored| {
                (
                    (stored.profile.provider, stored.profile.session_id.clone()),
                    stored,
                )
            })
            .collect();
        let mut cards = Vec::new();
        for session in sessions.iter().chain(expert_sessions.iter()) {
            let stored = profile_map
                .get(&(session.provider, session.session_id.clone()))
                .copied();
            let state = crate::experts::card_state(session, stored);
            let freshness = crate::experts::profile_freshness(session, stored, now());
            cards.push(serde_json::json!({
                "provider": session.provider,
                "session_id": session.session_id,
                "status": state.status,
                "detail": state.detail,
                "watched": watched.contains(&(session.provider, session.session_id.clone())),
                "availability": source_index.availability(session).as_str(),
                "current_state_status": freshness.current_state_status,
            }));
        }
        let mut result = serde_json::json!({
            "type": "snapshot",
            "protocol": fleet::PROTOCOL_NAME,
            "version": fleet::PROTOCOL_VERSION,
            "node_id": node_id,
            "machine": machine,
            "captured_at": now(),
            "sessions": sessions.iter().map(|session| fleet::session_to_wire(session, false)).collect::<Vec<_>>(),
            "profiles": profiles.iter().map(|stored| fleet::profile_to_wire(&stored.profile, expert_directory)).collect::<Vec<_>>(),
            "cards": cards,
        });
        if expert_directory {
            result.as_object_mut().expect("snapshot object").insert(
                "expert_sessions".into(),
                serde_json::Value::Array(
                    expert_sessions
                        .iter()
                        .map(|session| fleet::session_to_wire(session, true))
                        .collect(),
                ),
            );
        }
        // Retain vector ownership only until serialization has completed.
        sessions.clear();
        Ok(result)
    }

    fn candidates(
        &mut self,
        include_unconfirmed: bool,
    ) -> std::result::Result<Vec<crate::model::Candidate>, FleetError> {
        if include_unconfirmed {
            let providers = crate::providers::Providers::new(&self.pika.paths, &self.pika.config);
            Ok(Provider::ALL
                .into_iter()
                .flat_map(|provider| providers.browse(provider))
                .collect())
        } else {
            self.pika.import_named().map_err(FleetError::from)
        }
    }

    fn adopt(
        &mut self,
        provider: Provider,
        session_id: &str,
    ) -> std::result::Result<Session, FleetError> {
        let providers = crate::providers::Providers::new(&self.pika.paths, &self.pika.config);
        let candidate = providers
            .browse(provider)
            .into_iter()
            .find(|candidate| candidate.session_id == session_id)
            .ok_or_else(|| {
                FleetError::new(
                    FleetErrorKind::NotFound,
                    "Exact provider conversation is unavailable",
                )
            })?;
        self.pika.adopt_candidate(&candidate)?;
        self.pika
            .store
            .get_session(provider, session_id)?
            .ok_or_else(|| {
                FleetError::new(
                    FleetErrorKind::Error,
                    "Adopted conversation was not persisted",
                )
            })
    }

    fn peek(
        &mut self,
        provider: Provider,
        session_id: &str,
        lines: usize,
    ) -> std::result::Result<String, FleetError> {
        let session =
            exact_local_session(self.pika, provider, session_id, true).map_err(FleetError::from)?;
        self.pika
            .capture_exact(&session, lines)
            .map_err(FleetError::from)
    }

    fn acknowledge(
        &mut self,
        provider: Provider,
        session_id: &str,
    ) -> std::result::Result<bool, FleetError> {
        let session =
            exact_local_session(self.pika, provider, session_id, true).map_err(FleetError::from)?;
        self.pika
            .store
            .acknowledge_attention(provider, &session.session_id, session.last_event_at, false)
            .map_err(FleetError::from)
    }

    fn untrack(
        &mut self,
        provider: Provider,
        session_id: &str,
    ) -> std::result::Result<(i64, bool), FleetError> {
        let session =
            exact_local_session(self.pika, provider, session_id, true).map_err(FleetError::from)?;
        self.pika
            .store
            .untrack_session(provider, &session.session_id)?;
        let mut cleared = 0;
        if session.tmux_pane.is_some() && self.pika.clear_exact_tags(&session).is_ok() {
            cleared = 1;
        }
        Ok((cleared, true))
    }
}
fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

#[cfg(all(test, unix))]
mod fleet_consultation_tests {
    use super::*;
    use crate::consult::{Consultation, ConsultationOptions};
    use crate::model::Status;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixStream;

    fn fixture_session(root: &std::path::Path) -> Session {
        Session {
            provider: Provider::Codex,
            session_id: "workstream".into(),
            name: Some("expert".into()),
            cwd: Some(root.to_string_lossy().into_owned()),
            branch: None,
            transcript_path: None,
            tmux_session: None,
            tmux_pane: None,
            root_pid: None,
            status: Status::Working,
            unread: false,
            model: None,
            source: "fixture".into(),
            managed: true,
            error: None,
            attention_reason: None,
            created_at: 1.0,
            updated_at: 1.0,
            last_event_at: 1.0,
            last_activity_at: 1.0,
            live: true,
            attached: false,
            home_state: String::new(),
            cpu_percent: None,
            rss_kb: None,
            input_tokens: None,
            output_tokens: None,
            cached_input_tokens: None,
            cache_write_tokens: None,
            total_tokens: None,
            estimated_cost_usd: None,
            active_thread_id: Some("parent-thread".into()),
        }
    }

    #[test]
    fn board_exit_does_not_join_a_stalled_local_observer() {
        let stop = Arc::new(AtomicBool::new(false));
        let (refresh_sender, _refresh_receiver) = mpsc::sync_channel(1);
        let (done_sender, done_receiver) = mpsc::sync_channel(1);
        let (release_sender, release_receiver) = mpsc::sync_channel(1);
        let worker = thread::spawn(move || {
            let _ = release_receiver.recv();
            let _ = done_sender.send(());
        });

        let started = Instant::now();
        finish_board_observer(&stop, &refresh_sender, &done_receiver, worker);
        assert!(started.elapsed() < Duration::from_millis(150));
        assert!(stop.load(Ordering::Relaxed));

        release_sender.send(()).unwrap();
        done_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("detached observer exits safely after its bounded work returns");
    }

    fn assert_disconnect_cancels_exact_child(explicit_close: bool) {
        let root = tempfile::tempdir().unwrap();
        let child_pid = root.path().join("child.pid");
        let executable = root.path().join("codex");
        fs::write(
            &executable,
            format!(
                r#"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*) printf '{{"id":%s,"result":{{}}}}\n' "$id" ;;
    *'"method":"thread/fork"'*) printf '{{"id":%s,"result":{{"thread":{{"id":"side-id","ephemeral":true}},"model":"gpt-5.6-sol","reasoningEffort":"medium"}}}}\n' "$id" ;;
    *'"method":"turn/start"'*)
      printf '{{"id":%s,"result":{{"turn":{{"id":"turn-1"}}}}}}\n' "$id"
      sleep 30 &
      printf '%s' "$!" > '{}'
      wait ;;
  esac
done
"#,
                child_pid.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();

        let cancellation = crate::consult::CancellationToken::default();
        let (read_end, mut write_end) = UnixStream::pair().unwrap();
        let inputs =
            spawn_fleet_consultation_reader(io::BufReader::new(read_end), cancellation.clone());
        writeln!(write_end, "{{\"question\":\"why\"}}").unwrap();
        write_end.flush().unwrap();

        let mut options = ConsultationOptions::new(executable);
        options.timeout = Duration::from_secs(30);
        options.cancellation = cancellation.clone();
        let mut side = Consultation::open(&fixture_session(root.path()), options).unwrap();
        let FleetConsultationInput::Question(question) = inputs.recv().unwrap() else {
            panic!("question was not delivered")
        };
        let worker = thread::spawn(move || {
            let answer = side.ask(&question);
            let cleanup = side.close();
            (answer, cleanup)
        });
        for _ in 0..500 {
            if child_pid.is_file() {
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        assert!(child_pid.is_file(), "side turn never started");
        let cancelled_at = Instant::now();
        if explicit_close {
            writeln!(write_end, "{{\"close\":true}}").unwrap();
            write_end.flush().unwrap();
        } else {
            drop(write_end);
        }
        for _ in 0..500 {
            if cancellation.is_cancelled() {
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        let (answer, cleanup) = worker.join().unwrap();
        assert!(answer.unwrap_err().to_string().contains("cancelled"));
        assert!(cleanup.is_ok());
        assert!(cancelled_at.elapsed() < Duration::from_secs(1));
        let pid: i32 = fs::read_to_string(child_pid).unwrap().parse().unwrap();
        for _ in 0..50 {
            // SAFETY: signal zero only probes the exact fixture descendant.
            if unsafe { libc::kill(pid, 0) } != 0 {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("owned side descendant {pid} survived disconnect");
    }

    #[test]
    fn explicit_close_cancels_blocked_exact_side_turn() {
        assert_disconnect_cancels_exact_child(true);
    }

    #[test]
    fn stdin_eof_cancels_blocked_exact_side_turn() {
        assert_disconnect_cancels_exact_child(false);
    }

    #[test]
    fn hook_identity_rejects_partial_process_observation() {
        let observation = process::ProcessObservation::partial(
            std::collections::BTreeMap::new(),
            vec!["process changed during observation".into()],
        );
        assert!(complete_hook_processes(&observation).is_none());
    }

    #[test]
    fn board_local_ask_refuses_unavailable_source_before_provider_spawn() {
        let root = tempfile::tempdir().unwrap();
        let marker = root.path().join("provider-called");
        let executable = root.path().join("codex");
        fs::write(
            &executable,
            format!(
                "#!/bin/sh\nprintf called > '{}'\nexit 91\n",
                marker.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let transcript = root.path().join("saved.jsonl");
        fs::write(&transcript, "saved\n").unwrap();
        let mut session = fixture_session(root.path());
        session.session_id = "11111111-1111-4111-8111-111111111111".into();
        session.active_thread_id = None;
        session.transcript_path = Some(transcript.to_string_lossy().into_owned());
        let config_dir = root.path().join("config");
        let state_dir = root.path().join("state");
        let paths = crate::paths::Paths {
            config: config_dir.join("config.json"),
            database: state_dir.join("pika.db"),
            config_dir,
            state_dir,
            codex_home: root.path().join("codex-home"),
            claude_home: root.path().join("claude-home"),
            opencode_data_home: root.path().join("opencode-data"),
            opencode_config_home: root.path().join("opencode-config"),
        };
        fs::create_dir_all(&paths.codex_home).unwrap();
        fs::write(paths.codex_home.join("state_corrupt.sqlite"), "not sqlite").unwrap();
        let mut config = crate::config::Config::default();
        config.provider_executables.insert(
            Provider::Codex.as_str().to_owned(),
            executable.to_string_lossy().into_owned(),
        );
        let store = Store::at(&paths.database);
        let pika = Pika::with_components(
            paths,
            config,
            store,
            crate::tmux::Tmux::with_executable("fixture-tmux", Some("isolated".into())),
        );
        let (_commands, command_receiver) = mpsc::channel();
        let (event_sender, _events) = mpsc::sync_channel(1);
        let error = run_local_board_consultation(
            &pika,
            monitor::ConsultationIo {
                item: BoardItem::local(session),
                commands: command_receiver,
                events: event_sender,
                cancellation: crate::consult::CancellationToken::default(),
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("source-unavailable"));
        assert!(error.to_string().contains("No question was sent"));
        assert!(!marker.exists());
    }

    #[test]
    fn remote_cli_and_board_refuse_unavailable_source_before_transport() {
        let root = tempfile::tempdir().unwrap();
        let config_dir = root.path().join("config");
        let state_dir = root.path().join("state");
        let paths = crate::paths::Paths {
            config: config_dir.join("config.json"),
            database: state_dir.join("pika.db"),
            config_dir,
            state_dir,
            codex_home: root.path().join("codex-home"),
            claude_home: root.path().join("claude-home"),
            opencode_data_home: root.path().join("opencode-data"),
            opencode_config_home: root.path().join("opencode-config"),
        };
        let store = Store::at(&paths.database);
        let node_id = uuid::Uuid::new_v4().to_string();
        store
            .upsert_fleet_node(&crate::model::FleetNode {
                node_id: node_id.clone(),
                alias: "atlas".into(),
                ssh_target: "transport-must-not-run".into(),
                sources: vec!["explicit".into()],
                status: "ready".into(),
                protocol_version: Some(fleet::PROTOCOL_VERSION),
                package_version: Some("test".into()),
                capabilities: fleet::CAPABILITIES
                    .iter()
                    .map(|value| (*value).to_owned())
                    .collect(),
                last_seen: now(),
                last_attempt_at: now(),
                last_error: None,
                created_at: now(),
                updated_at: now(),
            })
            .unwrap();
        let mut session = fixture_session(root.path());
        session.session_id = "22222222-2222-4222-8222-222222222222".into();
        session.active_thread_id = None;
        session.home_state = "exact".into();
        let captured_at = now();
        let snapshot = serde_json::json!({
            "type":"snapshot",
            "protocol":fleet::PROTOCOL_NAME,
            "version":fleet::PROTOCOL_VERSION,
            "node_id":node_id,
            "machine":"atlas",
            "captured_at":captured_at,
            "sessions":[fleet::session_to_wire(&session, false)],
            "profiles":[],
            "cards":[{
                "provider":"codex",
                "session_id":session.session_id,
                "status":"UNKNOWN",
                "detail":"provider source is unavailable",
                "watched":true,
                "availability":"source-unavailable",
                "current_state_status":"UNKNOWN"
            }]
        });
        store
            .put_remote_snapshot(&node_id, &snapshot, captured_at)
            .unwrap();
        let pika = Pika::with_components(
            paths,
            crate::config::Config::default(),
            store,
            crate::tmux::Tmux::with_executable("fixture-tmux", Some("isolated".into())),
        );
        let remote = FleetManager::new(&pika.store, SshTransport::default())
            .cached_sessions(Some(&node_id), false)
            .unwrap()
            .remove(0);
        let cli_error = ask_remote(
            &pika,
            remote.clone(),
            AskArgs {
                name: remote.qualified_name(),
                question: vec!["question".into()],
                jsonl: false,
                json: false,
                fast: false,
            },
        )
        .unwrap_err();
        assert!(cli_error.to_string().contains("source-unavailable"));
        assert!(cli_error.to_string().contains("No question was sent"));

        let (_commands, command_receiver) = mpsc::channel();
        let (event_sender, event_receiver) = mpsc::sync_channel(1);
        let board_error = run_remote_board_consultation(
            &pika,
            monitor::ConsultationIo {
                item: BoardItem {
                    session,
                    node_id: Some(node_id),
                    node_name: Some("atlas".into()),
                    stale: false,
                    pending_token: None,
                    expert: None,
                },
                commands: command_receiver,
                events: event_sender,
                cancellation: crate::consult::CancellationToken::default(),
            },
        )
        .unwrap_err();
        assert!(board_error.to_string().contains("source-unavailable"));
        assert!(board_error.to_string().contains("No question was sent"));
        assert!(event_receiver.try_recv().is_err());
    }
}
