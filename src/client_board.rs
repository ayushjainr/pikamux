//! Native client fleet: local rendering and caching, remote conversation truth.
//! Never constructs a local provider inventory or opens a reverse SSH tunnel.
use crate::{
    client_bridge::{
        ClientConfig, ClientNode, ProcessWindowLauncher, WindowLauncher, windows_terminal_command,
    },
    consult::CancellationToken,
    fleet::{
        self, FleetError, FleetErrorKind, FleetManager, FleetSession, FleetTransport, SshTransport,
    },
    model::{FleetNode, Provider},
    monitor::{
        self, BoardAction, BoardItem, ConsultationDriver, ConsultationEvent, ConsultationInput,
        ConsultationOutcome, ExpertAnnotation,
    },
    store::Store,
};
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::{
    path::Path,
    sync::{Mutex, mpsc},
    thread,
    time::Duration,
};

fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

/// Pairings remain the authority. This dedicated cache contains no local
/// provider sessions and never inherits another machine's fleet topology.
pub fn sync_pairings(store: &Store, config: &ClientConfig) -> Result<()> {
    ClientConfig::from_value(&config.to_value())?;
    store.initialize()?;
    for node in store.list_nodes()? {
        if !config.nodes.contains_key(&node.node_id) {
            store.delete_fleet_node(&node.node_id)?;
        }
    }
    for paired in config.nodes.values() {
        if let Some(existing) = store.get_fleet_node(&paired.node_id)? {
            if existing.ssh_target != paired.ssh_target {
                // Never carry fresh-looking evidence across a route change.
                store.delete_fleet_node(&paired.node_id)?;
            } else {
                if existing.alias != paired.alias {
                    store.upsert_fleet_node(&FleetNode {
                        alias: paired.alias.clone(),
                        ..existing
                    })?;
                }
                continue;
            }
        }
        store.upsert_fleet_node(&FleetNode {
            node_id: paired.node_id.clone(),
            alias: paired.alias.clone(),
            ssh_target: paired.ssh_target.clone(),
            sources: vec!["client-pairing".into()],
            status: "pending".into(),
            protocol_version: Some(fleet::PROTOCOL_VERSION),
            package_version: None,
            capabilities: Vec::new(),
            last_seen: 0.0,
            last_attempt_at: 0.0,
            last_error: None,
            created_at: now(),
            updated_at: now(),
        })?;
    }
    Ok(())
}

pub fn cached_board(store: &Store) -> Result<(Vec<BoardItem>, Vec<String>)> {
    let manager = FleetManager::new(store, SshTransport::default());
    let cached = manager.cached_sessions_with_notices(None, false)?;
    let items = cached
        .sessions
        .into_iter()
        .map(|remote| {
            let expert = ExpertAnnotation::from_remote(&remote);
            BoardItem {
                session: remote.session,
                node_id: Some(remote.node_id),
                node_name: Some(remote.node_name),
                stale: remote.stale,
                pending_token: None,
                expert,
            }
        })
        .collect();
    let mut health = cached
        .notices
        .into_iter()
        .map(|n| format!("{} · {}", n.node_name, n.message))
        .collect::<Vec<_>>();
    for node in manager.nodes()? {
        if let Some(error) = node.last_error {
            health.push(format!("{} · {error}", node.alias));
        } else if !store.has_remote_snapshot(&node.node_id)? {
            health.push(format!(
                "{} · connecting; no cached snapshot yet",
                node.alias
            ));
        }
    }
    Ok((items, health))
}

pub fn exact_remote(store: &Store, item: &BoardItem) -> Result<FleetSession> {
    let id = item
        .node_id
        .as_deref()
        .context("Missing machine identity")?;
    FleetManager::new(store, SshTransport::default())
        .cached_sessions(Some(id), false)?
        .into_iter()
        .find(|row| {
            row.session.provider == item.session.provider
                && row.session.session_id == item.session.session_id
        })
        .context(
            "The exact conversation is no longer in this machine's inventory. Press r to refresh.",
        )
}

/// Inventory/validation uses normal fleet transport. Only the final attach is
/// replaced by a locally constructed Windows Terminal command.
pub struct WindowTransport<T, L> {
    pub transport: T,
    pub launcher: Mutex<L>,
    pub cancellation: CancellationToken,
}

impl<T: FleetTransport, L: WindowLauncher> FleetTransport for WindowTransport<T, L> {
    fn request(
        &self,
        target: &str,
        payload: &Value,
        mutating: bool,
    ) -> std::result::Result<Value, FleetError> {
        self.transport
            .request_cancellable(target, payload, mutating, &self.cancellation)
    }
    fn request_cancellable(
        &self,
        target: &str,
        payload: &Value,
        mutating: bool,
        cancellation: &CancellationToken,
    ) -> std::result::Result<Value, FleetError> {
        self.transport
            .request_cancellable(target, payload, mutating, cancellation)
    }
    fn run_exact(
        &self,
        node: &FleetNode,
        arguments: &[String],
        tty: bool,
    ) -> std::result::Result<i32, FleetError> {
        if self.cancellation.is_cancelled() {
            return Err(FleetError::new(
                FleetErrorKind::Unreachable,
                "Board closed before launch; no window opened",
            ));
        }
        // Accept only the fixed fleet attach grammar, never a generic command.
        if !tty
            || arguments.len() != 7
            || arguments[0] != "_fleet-open"
            || arguments[1] != "--expected-node-id"
            || arguments[2] != node.node_id
            || arguments[3] != "--provider"
            || arguments[5] != "--session-id"
        {
            return Err(FleetError::new(
                FleetErrorKind::InvalidRequest,
                "Window action is not an exact conversation attach",
            ));
        }
        let provider: Provider = arguments[4]
            .parse()
            .map_err(|_| FleetError::new(FleetErrorKind::InvalidRequest, "Invalid provider"))?;
        let paired = ClientNode {
            node_id: node.node_id.clone(),
            alias: node.alias.clone(),
            ssh_target: node.ssh_target.clone(),
            token: String::new(),
            remote_port: None,
            allow_fleet_relay: false,
        };
        let argv = windows_terminal_command(&paired, provider, &arguments[6], "wt.exe", "ssh.exe")
            .map_err(|error| FleetError::new(FleetErrorKind::InvalidRequest, error.to_string()))?;
        self.launcher.lock().map_err(|_| FleetError::new(FleetErrorKind::Error, "Window launcher unavailable"))?
            .launch(&argv).map_err(|error| FleetError::new(FleetErrorKind::Error,
                format!("{error}. Check Windows Terminal is installed and `wt` works in PowerShell. No automatic retry was made.")))?;
        Ok(0)
    }
}

pub fn run(config: ClientConfig, cache_path: &Path) -> Result<i32> {
    let store = Store::at(cache_path);
    sync_pairings(&store, &config)?;
    let (items, health) = cached_board(&store)?;
    let (updates, receive) = monitor::latest_channel();
    let (health_updates, health_receive) = monitor::latest_channel();
    let (refresh, requests) = mpsc::sync_channel(1);
    let cancellation = CancellationToken::default();
    let stop = cancellation.clone();
    let path = cache_path.to_owned();
    let (done_send, done) = mpsc::sync_channel(1);
    let worker = thread::spawn(move || {
        let store = Store::at(path);
        let manager = FleetManager::new(
            &store,
            SshTransport::new("ssh.exe", Duration::from_secs(5), Duration::from_secs(12)),
        );
        let mut first = true;
        loop {
            if stop.is_cancelled() {
                break;
            }
            let manual = first || requests.try_recv().is_ok();
            first = false;
            match manager.nodes() {
                Ok(nodes) => {
                    if let Some(node) = fleet::next_remote_node(&nodes, None, now(), manual) {
                        // Failures belong to one node; retain its last good cache
                        // and immediately admit another due node.
                        let _ = manager.refresh_node_cancellable(&node.node_id, &stop);
                    } else {
                        match requests.recv_timeout(Duration::from_secs(1)) {
                            Ok(()) => first = true,
                            Err(mpsc::RecvTimeoutError::Disconnected) => break,
                            Err(mpsc::RecvTimeoutError::Timeout) => {}
                        }
                    }
                }
                Err(error) => {
                    health_updates.publish(vec![error.to_string()]);
                    break;
                }
            }
            if stop.is_cancelled() {
                break;
            }
            match cached_board(&store) {
                Ok((items, health)) => {
                    updates.publish(items);
                    health_updates.publish(health);
                }
                Err(error) => {
                    health_updates.publish(vec![format!("Fleet cache unavailable · {error}")])
                }
            }
        }
        let _ = done_send.send(());
    });
    let action_path = cache_path.to_owned();
    let preview_path = cache_path.to_owned();
    let action_stop = cancellation.clone();
    let actions = monitor::ActionDriver::new(move |action| {
        let store = Store::at(&action_path);
        let transport = WindowTransport {
            transport: SshTransport::new(
                "ssh.exe",
                Duration::from_secs(5),
                Duration::from_secs(12),
            ),
            launcher: Mutex::new(ProcessWindowLauncher),
            cancellation: action_stop.clone(),
        };
        let manager = FleetManager::new(&store, transport);
        match action {
            BoardAction::Open(item) => {
                let remote = exact_remote(&store, &item)?;
                manager.attach(&remote)?;
                Ok(format!(
                    "Window launched · {} · {} · check that window for exact recovery",
                    remote.qualified_name(),
                    remote.session.session_id
                ))
            }
            BoardAction::Peek(item) => {
                let remote = exact_remote(&store, &item)?;
                Ok(format!(
                    "{} · peek · unread preserved\n{}",
                    remote.qualified_name(),
                    manager.capture(&remote, 80)?
                ))
            }
            BoardAction::Untrack(item) => {
                let remote = exact_remote(&store, &item)?;
                manager.untrack(&remote, None)?;
                Ok(format!(
                    "Stopped watching {}. The agent and conversation were left intact.",
                    remote.qualified_name()
                ))
            }
            _ => bail!("This action is not available in the client board"),
        }
    });
    let actions = actions.with_preview(move |item, cancellation| {
        let store = Store::at(&preview_path);
        let remote = exact_remote(&store, &item)?;
        FleetManager::new(
            &store,
            SshTransport::new("ssh.exe", Duration::from_secs(5), Duration::from_secs(10)),
        )
        .capture_cancellable(&remote, 100, &cancellation)
        .map_err(anyhow::Error::from)
    });
    let side_path = cache_path.to_owned();
    let driver = ConsultationDriver::new(move |io| {
        remote_consultation(
            &Store::at(&side_path),
            &SshTransport::new("ssh.exe", Duration::from_secs(5), Duration::from_secs(12)),
            io,
        )
    });
    let (update_checker, update_receiver) = crate::update_check::start(&store);
    let (quota_worker, quota_feed) = crate::quota::start(store.clone(), None);
    let result = monitor::run_client_board(
        items,
        receive,
        driver,
        refresh,
        monitor::FleetHealthFeed::new(health, health_receive).with_quota(quota_feed),
        actions,
        update_receiver,
    );
    cancellation.cancel();
    drop(update_checker);
    drop(quota_worker);
    // Cancellation owns SSH cleanup. A slow read-only observer may finish after
    // the UI exits, but cannot delay closing the console or launch another window.
    if done.recv_timeout(Duration::from_millis(50)).is_ok() {
        let _ = worker.join();
    }
    match result? {
        BoardAction::Update(version) => crate::windows_update::install(version.as_deref(), true),
        _ => Ok(0),
    }
}

/// Shared with the host board: one ephemeral remote side for all follow-ups.
pub fn remote_consultation(
    store: &Store,
    transport: &SshTransport,
    io: monitor::ConsultationIo,
) -> Result<ConsultationOutcome> {
    let remote = exact_remote(store, &io.item)?;
    if !crate::experts::source_is_available(remote.source_availability()) {
        bail!(
            "Cannot consult {}: {}. No question was sent; watching is unchanged.",
            remote.qualified_name(),
            remote.source_availability()
        );
    }
    let local = crate::consult::consultation_policy(remote.session.provider, false)?;
    let policy = fleet::ConsultationPolicy {
        consultation_mode: local.mode,
        model: local.model.unwrap_or_default(),
        effort: local.effort.unwrap_or_default(),
    };
    let node = store
        .get_fleet_node(&remote.node_id)?
        .context("Expert machine is no longer trusted")?;
    let mut side = fleet::RemoteConsultation::open_cancellable(
        transport,
        node,
        remote,
        policy.clone(),
        fleet::ConsultationTimeouts {
            open: Duration::from_secs(30),
            event: Duration::from_secs(900),
            cleanup: Duration::from_secs(30),
        },
        io.cancellation.clone(),
    )?;
    let opening = side.opening_receipt();
    let _ = io.events.send(ConsultationEvent::Opened {
        child_id: opening.child_id.clone(),
        policy: Some(if policy.model.is_empty() {
            policy.consultation_mode
        } else {
            format!("{} · {}", policy.consultation_mode, policy.model)
        }),
        proof: Some(opening.proof_label()),
    });
    for command in io.commands {
        match command {
            ConsultationInput::Question(question) => match side.ask(&question) {
                Ok(answer) => {
                    let _ = io.events.send(ConsultationEvent::Answer(answer));
                }
                Err(error) => {
                    let _ = io.events.send(ConsultationEvent::Error {
                        retry_safe: error.kind == FleetErrorKind::InvalidRequest,
                        message: error.to_string(),
                    });
                }
            },
            ConsultationInput::Close => break,
        }
    }
    side.close()?;
    Ok(ConsultationOutcome::discarded())
}
