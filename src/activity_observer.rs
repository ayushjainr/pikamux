//! Owns local/fleet observation independently of any view or terminal renderer.
use crate::{
    consult::CancellationToken,
    core::Pika,
    fleet::{self, FleetManager, SshTransport},
    monitor::{BoardItem, ExpertAnnotation},
    store::{Store, StoreChangeWatcher},
    usage,
};
use anyhow::Result;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
const LOCAL_RECONCILE_INTERVAL: Duration = Duration::from_secs(20);
fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}
pub(crate) fn start(pika: &Pika) -> Result<crate::activity_feed::Source> {
    start_with_mode(pika, false)
}

pub(crate) fn start_for_open(pika: &Pika) -> Result<crate::activity_feed::Source> {
    start_with_mode(pika, true)
}

fn start_with_mode(pika: &Pika, wait_for_handoff: bool) -> Result<crate::activity_feed::Source> {
    // Paint all bounded durable state immediately, including offline fleet
    // rows. No provider, process, tmux, or SSH observation belongs here.
    let (cached, initial_fleet_health) = board_items(pika)?;
    let summary_source = crate::activity_feed::Source::default();
    if wait_for_handoff {
        summary_source.pause_observation();
    }
    let observation_enabled = summary_source.observation_gate();
    let summary_worker = summary_source.publisher();
    summary_worker.publish(cached, initial_fleet_health);
    let (refresh_sender, refresh_receiver) = mpsc::sync_channel(1);
    let stop = Arc::new(AtomicBool::new(false));
    let local_refresh_delayed = summary_source.delayed();
    let worker_refresh_delayed = Arc::clone(&local_refresh_delayed);
    let worker_stop = Arc::clone(&stop);
    let worker = pika.clone();
    // Only this observer publishes aggregate activity; remote workers commit cache data.
    let (local_done_sender, local_done_receiver) = mpsc::sync_channel(1);
    let local_refresh = thread::spawn(move || {
        let mut store_changes = worker.store.change_watcher().ok();
        let mut next_reconcile = Instant::now();
        let mut consecutive_failures = 0;
        let mut observing = !wait_for_handoff;
        while !worker_stop.load(Ordering::Relaxed) {
            if !observation_enabled.load(Ordering::Acquire) {
                if refresh_receiver.recv_timeout(Duration::from_millis(500))
                    == Err(mpsc::RecvTimeoutError::Disconnected)
                {
                    break;
                }
                continue;
            }
            if !observing {
                // The exact open already reconciled. Reuse its committed state
                // instead of racing it with another provider/process sweep.
                if let Ok((items, health)) = board_items(&worker) {
                    summary_worker.publish(items, health);
                }
                next_reconcile = Instant::now() + LOCAL_RECONCILE_INTERVAL;
                observing = true;
            }
            if Instant::now() >= next_reconcile {
                let refresh = worker.reconcile_local().and_then(|_| board_items(&worker));
                worker_refresh_delayed.store(
                    record_local_refresh_result(&mut consecutive_failures, refresh.is_ok()),
                    Ordering::Relaxed,
                );
                if let Ok((items, fleet_health)) = refresh {
                    summary_worker.publish(items, fleet_health);
                    // Never consume a coalesced reconcile/hook commit without
                    // re-reading the cache. If this creates the first watcher,
                    // the read also closes the database-creation race window.
                    if let Some((items, fleet_health)) =
                        snapshot_after_store_change(&worker.store, &mut store_changes, || {
                            board_items(&worker)
                        })
                    {
                        summary_worker.publish(items, fleet_health);
                    }
                }
                // Hooks and store notifications carry normal lifecycle changes
                // immediately. This slower full provider/process sweep is the
                // bounded fallback for missed hooks and external changes.
                next_reconcile = Instant::now() + LOCAL_RECONCILE_INTERVAL;
            }
            let wait = next_reconcile
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(500));
            match refresh_receiver.recv_timeout(wait) {
                Ok(()) => next_reconcile = Instant::now(),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if let Some((items, fleet_health)) =
                        snapshot_after_store_change(&worker.store, &mut store_changes, || {
                            board_items(&worker)
                        })
                    {
                        summary_worker.publish(items, fleet_health);
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        let _ = local_done_sender.send(());
    });
    let remote_stop = Arc::clone(&stop);
    let remote_cancel = CancellationToken::default();
    let worker_remote_cancel = remote_cancel.clone();
    let remote_worker = pika.clone();
    let remote_enabled = summary_source.observation_gate();
    let remote_refresh = thread::spawn(move || {
        while !remote_stop.load(Ordering::Relaxed) && !worker_remote_cancel.is_cancelled() {
            if !remote_enabled.load(Ordering::Acquire) {
                thread::park_timeout(Duration::from_secs(1));
                continue;
            }
            let manager = FleetManager::new(&remote_worker.store, SshTransport::default());
            if let Ok(nodes) = manager.nodes()
                && let Some(node) = fleet::next_remote_node(&nodes, None, now(), false)
            {
                // refresh_node commits its snapshot to the store. The sole
                // store-driven local publisher observes that commit and builds
                // the next complete board, preventing an older remote read
                // from overwriting a newer local hook state.
                let _ = manager.refresh_node_cancellable(&node.node_id, &worker_remote_cancel);
            }
            // The board unparks this exact worker during shutdown. A single
            // parked interval avoids periodic cancellation polling while
            // preserving the one-second fleet cadence.
            thread::park_timeout(Duration::from_secs(1));
        }
    });
    let fence = pika.clone();
    summary_source.own_observer(refresh_sender.clone(), move || {
        fence.invalidate_local_reconciliation();
        stop.store(true, Ordering::Relaxed);
        remote_cancel.cancel();
        remote_refresh.thread().unpark();
        let _ = remote_refresh.join();
        finish_board_observer(&stop, &refresh_sender, &local_done_receiver, local_refresh);
    });
    Ok(summary_source)
}

fn record_local_refresh_result(consecutive_failures: &mut u8, succeeded: bool) -> bool {
    *consecutive_failures = if succeeded {
        0
    } else {
        consecutive_failures.saturating_add(1)
    };
    *consecutive_failures >= 2
}

fn ensure_store_change_watcher(store: &Store, watcher: &mut Option<StoreChangeWatcher>) -> bool {
    if watcher.is_none() {
        *watcher = store.change_watcher().ok();
        return watcher.is_some();
    }
    false
}

fn store_changed(store: &Store, watcher: &mut Option<StoreChangeWatcher>) -> bool {
    if ensure_store_change_watcher(store, watcher) {
        return true;
    }
    match watcher.as_mut().map(StoreChangeWatcher::changed) {
        Some(Ok(changed)) => changed,
        Some(Err(_)) => {
            // A replaced/corrupt connection is not permanent: retry from a
            // fresh data_version baseline on the next poll.
            *watcher = None;
            false
        }
        None => false,
    }
}

fn snapshot_after_store_change<T>(
    store: &Store,
    watcher: &mut Option<StoreChangeWatcher>,
    load: impl FnOnce() -> Result<T>,
) -> Option<T> {
    if !store_changed(store, watcher) {
        return None;
    }
    match load() {
        Ok(snapshot) => Some(snapshot),
        Err(_) => {
            // Re-establishing the watcher makes the next poll reload once even
            // if SQLite has no newer commit after this transient read failure.
            *watcher = None;
            None
        }
    }
}

pub(crate) fn finish_board_observer(
    stop: &AtomicBool,
    refresh: &mpsc::SyncSender<()>,
    done: &mpsc::Receiver<()>,
    worker: thread::JoinHandle<()>,
) {
    stop.store(true, Ordering::Relaxed);
    // Wake the local observer, but never hold Enter/quit behind an in-flight
    // provider or tmux read. `bare` fences its writes before reaching here, so
    // a completed observer can be joined and a slow read can finish detached
    // without ever becoming authoritative.
    let _ = refresh.try_send(());
    if done.try_recv().is_ok() && worker.is_finished() {
        let _ = worker.join();
    }
}

fn board_items(pika: &Pika) -> Result<(Vec<BoardItem>, Vec<String>)> {
    board_items_from_inventory(pika, pika.cached_inventory()?)
}

fn board_items_from_inventory(
    pika: &Pika,
    inventory: crate::core::Inventory,
) -> Result<(Vec<BoardItem>, Vec<String>)> {
    let mut inventory = inventory;
    let _ = usage::hydrate_cached_sessions(&pika.store, &mut inventory.sessions);
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
                    scope_freshness: Some(format!(
                        "updated {} ago",
                        short_age(if profile.scope_updated_at > 0.0 {
                            profile.scope_updated_at
                        } else {
                            profile.updated_at
                        })
                    )),
                    current_work_freshness: Some(format!(
                        "updated {} ago",
                        short_age(if profile.current_state_updated_at > 0.0 {
                            profile.current_state_updated_at
                        } else {
                            profile.updated_at
                        })
                    )),
                });
            BoardItem {
                expert,
                ..BoardItem::local(session)
            }
        })
        .collect::<Vec<_>>();
    for pending in inventory.pending {
        items.push(BoardItem {
            session: pika.pending_session(&pending)?,
            node_id: None,
            node_name: None,
            stale: false,
            pending_token: Some(pending.launch_token),
            expert: None,
        });
    }
    let mut fleet_health = Vec::new();
    append_cached_fleet(
        &mut items,
        &mut fleet_health,
        FleetManager::new(&pika.store, SshTransport::default())
            .cached_sessions_with_notices(None, false),
    );
    fleet_health.extend(fleet_node_health(&pika.store));
    Ok((items, fleet_health))
}

fn fleet_node_health(store: &Store) -> Vec<String> {
    let nodes = match FleetManager::new(store, SshTransport::default()).nodes() {
        Ok(nodes) => nodes,
        Err(error) => return vec![format!("fleet registry unavailable · {}", error.message)],
    };
    let mut health = Vec::new();
    for node in nodes {
        if let Some(error) = node
            .last_error
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        {
            health.push(format!("{} · {error}", node.alias));
        } else if node.status != "ready" {
            health.push(format!("{} · machine status {}", node.alias, node.status));
        }
        match store.has_remote_snapshot(&node.node_id) {
            Ok(true) => {}
            Ok(false) => health.push(format!("{} · no cached snapshot yet", node.alias)),
            Err(error) => health.push(format!(
                "{} · cached snapshot unavailable · {error}",
                node.alias
            )),
        }
    }
    health.sort();
    health.dedup();
    health
}

pub(crate) fn append_cached_fleet(
    items: &mut Vec<BoardItem>,
    health: &mut Vec<String>,
    cached: std::result::Result<fleet::CachedFleetSessions, fleet::FleetError>,
) {
    match cached {
        Ok(cached) => {
            for remote in cached.sessions {
                let expert = ExpertAnnotation::from_remote(&remote);
                items.push(BoardItem {
                    session: remote.session,
                    node_id: Some(remote.node_id),
                    node_name: Some(remote.node_name),
                    stale: remote.stale,
                    pending_token: None,
                    expert,
                });
            }
            health.extend(
                cached
                    .notices
                    .into_iter()
                    .map(|notice| format!("{} · {}", notice.node_name, notice.message)),
            );
        }
        Err(error) => health.push(error.message),
    }
}

pub(crate) fn short_age(timestamp: f64) -> String {
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

#[cfg(test)]
mod board_refresh_tests {
    use super::{
        ensure_store_change_watcher, fleet_node_health, record_local_refresh_result,
        snapshot_after_store_change, store_changed,
    };
    use crate::{model::FleetNode, store::Store};

    #[test]
    fn local_refresh_notice_requires_consecutive_failures_and_clears_on_success() {
        let mut failures = 0;
        assert!(!record_local_refresh_result(&mut failures, false));
        assert!(record_local_refresh_result(&mut failures, false));
        for _ in 0..1_000 {
            assert!(record_local_refresh_result(&mut failures, false));
        }
        assert!(!record_local_refresh_result(&mut failures, true));
        assert!(!record_local_refresh_result(&mut failures, false));
        assert!(!record_local_refresh_result(&mut failures, true));
        assert!(!record_local_refresh_result(&mut failures, false));
    }

    #[test]
    fn watcher_created_after_first_database_initialization_observes_later_commits() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::at(root.path().join("state/pika.db"));
        let mut watcher = store.change_watcher().ok();
        assert!(watcher.is_none());

        store.initialize().unwrap();
        ensure_store_change_watcher(&store, &mut watcher);
        assert!(watcher.is_some());
        store.set_meta("board-test", "updated").unwrap();
        assert!(store_changed(&store, &mut watcher));
    }

    #[test]
    fn hook_commit_between_reconcile_snapshot_and_watcher_consume_is_reloaded() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::at(root.path().join("state/pika.db"));
        store.initialize().unwrap();
        let mut watcher = None;
        assert!(ensure_store_change_watcher(&store, &mut watcher));

        let reconcile_snapshot = store.get_meta("interleaved-hook").unwrap();
        assert_eq!(reconcile_snapshot, None);
        store.set_meta("interleaved-hook", "newest").unwrap();

        let delivered = snapshot_after_store_change(&store, &mut watcher, || {
            store.get_meta("interleaved-hook")
        });
        assert_eq!(delivered, Some(Some("newest".into())));
    }

    #[test]
    fn fleet_health_reports_node_errors_and_missing_cache_then_clears_on_recovery() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::at(root.path().join("state/pika.db"));
        store.initialize().unwrap();
        let node_id = "77777777-7777-4777-8777-777777777777";
        let mut node = FleetNode {
            node_id: node_id.into(),
            alias: "atlas".into(),
            ssh_target: "atlas".into(),
            sources: vec!["ssh-config".into()],
            status: "error".into(),
            protocol_version: Some(crate::fleet::PROTOCOL_VERSION),
            package_version: Some(crate::VERSION.into()),
            capabilities: vec!["snapshot".into()],
            last_seen: 0.0,
            last_attempt_at: 1.0,
            last_error: Some("connection refused".into()),
            created_at: 1.0,
            updated_at: 1.0,
        };
        store.upsert_fleet_node(&node).unwrap();
        let health = fleet_node_health(&store);
        assert_eq!(health.len(), 2);
        assert!(
            health
                .iter()
                .any(|item| item.contains("connection refused"))
        );
        assert!(
            health
                .iter()
                .any(|item| item.contains("no cached snapshot"))
        );

        node.status = "ready".into();
        node.last_error = None;
        store.upsert_fleet_node(&node).unwrap();
        store
            .put_remote_snapshot(node_id, &serde_json::json!({"sessions": []}), 2.0)
            .unwrap();
        assert!(fleet_node_health(&store).is_empty());
    }
}

#[cfg(test)]
mod open_observer_tests {
    use super::start_for_open;
    use crate::{
        config::Config,
        core::{Pika, session_from_candidate},
        model::{Candidate, Provider, Status},
        paths::Paths,
        store::Store,
        tmux::Tmux,
    };
    use std::{
        fs,
        path::Path,
        sync::atomic::Ordering,
        thread,
        time::{Duration, Instant},
    };

    fn test_pika(root: &Path, tmux_log: &Path) -> Pika {
        let paths = Paths {
            config_dir: root.join("config"),
            state_dir: root.join("state"),
            config: root.join("config/config.json"),
            database: root.join("state/pika.db"),
            codex_home: root.join("codex"),
            claude_home: root.join("claude"),
            opencode_data_home: root.join("opencode-data"),
            opencode_config_home: root.join("opencode-config"),
            muse_data_home: root.join("muse-data"),
            muse_config_home: root.join("muse-config"),
        };
        for path in [
            &paths.config_dir,
            &paths.state_dir,
            &paths.codex_home,
            &paths.claude_home,
            &paths.opencode_data_home,
            &paths.opencode_config_home,
        ] {
            fs::create_dir_all(path).unwrap();
        }
        let store = Store::at(paths.database.clone());
        store.initialize().unwrap();
        let tmux = fake_tmux(root, tmux_log);
        Pika::with_components(paths, Config::default(), store, tmux)
    }

    fn fake_tmux(root: &Path, log: &Path) -> Tmux {
        let executable = root.join("fake-tmux");
        let log_path = log.to_string_lossy();
        let quoted = shell_words::quote(&log_path);
        fs::write(
            &executable,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {quoted}\nif [ \"$1\" = \"-V\" ]; then printf 'tmux 3.7\\n'; fi\n"
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        }
        Tmux::with_executable(executable.to_string_lossy(), None)
    }

    fn committed_session() -> crate::model::Session {
        session_from_candidate(&Candidate {
            provider: Provider::Codex,
            session_id: "open-gate-committed".into(),
            name: Some("open-gate-committed".into()),
            cwd: Some("/tmp".into()),
            branch: None,
            transcript_path: None,
            model: None,
            updated_at: 1.0,
            live: false,
            pid: None,
            source: "test".into(),
            parent_session_id: None,
            created_at: 1.0,
            lifecycle_status: Some(Status::Ready),
        })
    }

    fn wait_until(deadline: Instant, condition: impl Fn() -> bool) {
        while !condition() {
            assert!(
                Instant::now() < deadline,
                "condition was not observed in time"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn paused_open_observer_does_not_observe_before_handoff() {
        let root = tempfile::tempdir().unwrap();
        let tmux_log = root.path().join("tmux.log");
        let pika = test_pika(root.path(), &tmux_log);
        let source = start_for_open(&pika).unwrap();

        assert!(!source.observation_gate().load(Ordering::Acquire));
        thread::sleep(Duration::from_millis(100));
        assert!(!tmux_log.exists() || fs::read_to_string(&tmux_log).unwrap().is_empty());

        let before_activation_revision = source.snapshot().unwrap().revision;
        source.activate_observation();
        wait_until(Instant::now() + Duration::from_secs(2), || {
            source
                .snapshot()
                .is_some_and(|snapshot| snapshot.revision > before_activation_revision)
        });
        source.refresh().try_send(()).unwrap();
        wait_until(Instant::now() + Duration::from_secs(2), || {
            fs::read_to_string(&tmux_log)
                .unwrap_or_default()
                .contains("list-panes")
        });
        drop(source);
    }

    #[test]
    fn paused_open_reuses_action_commit_without_an_immediate_reconcile() {
        let root = tempfile::tempdir().unwrap();
        let tmux_log = root.path().join("tmux.log");
        let pika = test_pika(root.path(), &tmux_log);
        let source = start_for_open(&pika).unwrap();

        pika.store
            .upsert_session(&committed_session(), false)
            .unwrap();
        source.activate_observation();
        wait_until(Instant::now() + Duration::from_secs(2), || {
            source.snapshot().is_some_and(|snapshot| {
                snapshot
                    .items
                    .iter()
                    .any(|item| item.session.session_id == "open-gate-committed")
            })
        });

        // Activation republishes the action's committed cache, but does not
        // immediately launch the normal provider/process/tmux sweep.
        thread::sleep(Duration::from_millis(100));
        assert!(!tmux_log.exists() || fs::read_to_string(&tmux_log).unwrap().is_empty());
        drop(source);
    }

    #[test]
    fn dropping_a_paused_open_observer_is_bounded_and_safe() {
        let root = tempfile::tempdir().unwrap();
        let tmux_log = root.path().join("tmux.log");
        let pika = test_pika(root.path(), &tmux_log);
        let source = start_for_open(&pika).unwrap();
        let started = Instant::now();

        drop(source);

        assert!(started.elapsed() < Duration::from_secs(1));
        thread::sleep(Duration::from_millis(100));
        assert!(!tmux_log.exists() || fs::read_to_string(&tmux_log).unwrap().is_empty());
    }
}
