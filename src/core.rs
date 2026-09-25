use crate::{
    config::Config,
    model::{Candidate, ObservationKind, Pane, Provider, Session, Status},
    named_discovery::NamedDiscovery,
    open_history,
    paths::Paths,
    process::{self, ProcessObservation, ProcessRecord},
    providers::Providers,
    resolve::{EvidenceState, NameCandidate, NameResolutionError, SelectionEvidence, resolve_name},
    status::{ProjectionFallback, project_status},
    store::{PendingLaunch, ReconcileLedger, ReconcileSession, Store},
    tmux::{ReceiptDelivery, Tmux},
};
use anyhow::{Context, Result, bail};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

pub const LIVE_OWNER_LEASE_SECONDS: f64 = 300.0;
const CONTINUATION_FRESH_SECONDS: f64 = 30.0 * 60.0;
const RECONCILE_WRITE_BATCH: usize = 64;

/// Retry a discarded *observation*, never its old writes or an attach/launch.
/// A closed-board generation fence and identity failures are not retryable.
fn fresh_observation<T>(mut observe: impl FnMut() -> Result<T>) -> Result<T> {
    for attempt in 0..3 {
        match observe() {
            Err(error) if error.is::<crate::store::ReconcileSuperseded>() => {
                if attempt == 2 {
                    bail!(
                        "Pika state kept changing during verification. No requested attach or launch was performed; retry the same command."
                    );
                }
            }
            result => return result,
        }
    }
    unreachable!("bounded observation attempts always return")
}

#[derive(Clone, Debug)]
pub struct Inventory {
    pub sessions: Vec<Session>,
    pub pending: Vec<PendingLaunch>,
}

struct ReconciledInventory {
    inventory: Inventory,
    candidates: BTreeMap<(Provider, String), Candidate>,
}

#[derive(Clone, Debug)]
pub enum OpenTarget {
    Session(Box<Session>),
    Pending(Box<PendingLaunch>),
}

#[derive(Clone, Debug)]
pub struct OpenReceipt {
    pub target: OpenTarget,
    pub kind: &'static str,
    pub exit_code: i32,
    /// Where the post-proof continuity receipt was observed. `None` means no
    /// interactive attach was requested; `Failed` preserves a cosmetic
    /// delivery failure without undoing an already-proven exact handoff.
    pub receipt_delivery: Option<ReceiptDelivery>,
}

#[derive(Clone, Debug)]
pub struct ExactPaneBinding {
    pub pane: Pane,
    pub pane_start_time: u64,
    pub provider_pid: i64,
    pub provider_start_time: u64,
    /// Known forwarding launcher, proved in the same revalidated pane ancestry.
    provider_launcher: Option<process::ProcessGeneration>,
}

/// A tagged, live Pika pane whose provider process is present but whose
/// provider identity is not currently provable. This is deliberately weaker
/// than [`ExactPaneBinding`]: it is only safe for an explicit terminal
/// handoff, never for resume, state updates, or identity certification.
#[derive(Clone, Debug)]
pub struct UnverifiedPaneBinding {
    pub pane: Pane,
    pub pane_start_time: u64,
    pub provider_pid: i64,
    pub provider_start_time: u64,
}

#[derive(Default)]
struct IdentityOwners {
    direct: BTreeSet<i64>,
    leases: BTreeSet<i64>,
    recovery: Option<crate::store::RecoveryOwner>,
}

impl IdentityOwners {
    fn pids(&self) -> BTreeSet<i64> {
        self.direct
            .iter()
            .chain(self.leases.iter())
            .copied()
            .chain(self.recovery.as_ref().map(|owner| owner.pid))
            .collect()
    }

    fn proves_pane(&self, pid: i64, pane: &Pane) -> bool {
        self.direct.contains(&pid)
            || self.recovery.as_ref().is_some_and(|owner| {
                owner.pid == pid
                    && pane.pika_launch_token.as_deref() == Some(owner.launch_token.as_str())
            })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    #[error("No exact conversation named {0:?}.")]
    NotFound(String),
    #[error("{0}")]
    Ambiguous(String),
    #[error("{0}")]
    OutsideLive(String),
    #[error(
        "Pika could not safely identify this conversation's existing terminal. Nothing was restarted and unread state is unchanged."
    )]
    IdentityUnproven,
    #[error("{0}")]
    Identity(String),
}

#[derive(Clone)]
pub struct Pika {
    pub paths: Paths,
    pub config: Config,
    pub store: Store,
    pub tmux: Tmux,
    process_observer: Arc<dyn Fn() -> ProcessObservation + Send + Sync>,
    local_reconcile_fence: Arc<LocalReconcileFence>,
    named_discovery: Arc<NamedDiscovery>,
}

#[derive(Default)]
struct LocalReconcileFence {
    generation: AtomicU64,
    write_gate: Mutex<()>,
}

fn exclusive_outside_owner(
    provider: Provider,
    pid: i64,
    processes: &BTreeMap<i64, ProcessRecord>,
) -> bool {
    processes.get(&pid).is_none_or(|record| {
        provider != Provider::Codex || !process::shared_provider_process(record, provider)
    })
}

fn distinct_fork_name(candidate: &Candidate, parent: &Session) -> bool {
    candidate
        .name
        .as_deref()
        .zip(parent.name.as_deref())
        .is_some_and(|(child, parent)| !child.eq_ignore_ascii_case(parent))
}

fn skip_independent_codex_fork(
    candidate: &Candidate,
    parent: &Session,
    known: &BTreeSet<(Provider, String)>,
) -> bool {
    !distinct_fork_name(candidate, parent)
        || known.contains(&(Provider::Codex, candidate.session_id.clone()))
        || parent.active_thread_id.as_deref() == Some(&candidate.session_id)
}

impl Pika {
    pub fn discover() -> Result<Self> {
        let paths = Paths::discover()?;
        let config = Config::load(&paths)?;
        let store = Store::from_paths(&paths);
        Ok(Self {
            paths,
            config,
            store,
            tmux: Tmux::default(),
            process_observer: Arc::new(process::observe),
            local_reconcile_fence: Arc::default(),
            named_discovery: Arc::default(),
        })
    }

    pub fn with_components(paths: Paths, config: Config, store: Store, tmux: Tmux) -> Self {
        Self {
            paths,
            config,
            store,
            tmux,
            process_observer: Arc::new(process::observe),
            local_reconcile_fence: Arc::default(),
            named_discovery: Arc::default(),
        }
    }

    fn observe_processes(&self) -> ProcessObservation {
        (self.process_observer)()
    }

    /// Return the durable cache without scanning providers, processes, tmux, or SSH.
    /// This is the first-frame path for the board.
    pub fn cached_inventory(&self) -> Result<Inventory> {
        Ok(Inventory {
            sessions: self.store.list_sessions()?,
            pending: self.store.list_visible_pending()?,
        })
    }

    /// Reconcile local identity and lifecycle evidence once. Remote machines are
    /// deliberately outside this operation so an offline node cannot stall it.
    pub fn reconcile_local(&self) -> Result<Inventory> {
        self.reconcile_local_with_candidates()
            .map(|observed| observed.inventory)
    }

    pub(crate) fn reconcile_for_action(&self) -> Result<Inventory> {
        fresh_observation(|| self.reconcile_local())
    }

    /// Fence an observation that was started for a board which has now closed.
    ///
    /// Reconciliation deliberately performs slow provider/process reads before
    /// taking its write gate. Bumping this generation under that same gate lets
    /// an exact board action proceed immediately without allowing an older
    /// observation to commit after it.
    #[cfg(not(windows))]
    pub(crate) fn invalidate_local_reconciliation(&self) {
        let _guard = self
            .local_reconcile_fence
            .write_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.local_reconcile_fence
            .generation
            .fetch_add(1, Ordering::AcqRel);
    }

    fn reconciled_write<T>(
        &self,
        generation: u64,
        store_session: &mut ReconcileSession,
        write: impl FnOnce(&ReconcileLedger<'_>) -> Result<T>,
    ) -> Result<T> {
        let _guard = self
            .local_reconcile_fence
            .write_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self
            .local_reconcile_fence
            .generation
            .load(Ordering::Acquire)
            != generation
        {
            bail!("local reconciliation was superseded before it could commit")
        }
        store_session.transaction(write)
    }

    fn reconcile_local_with_candidates(&self) -> Result<ReconciledInventory> {
        let reconcile_generation = self
            .local_reconcile_fence
            .generation
            .load(Ordering::Acquire);
        self.store.initialize()?;
        // Keep one SQLite connection for this whole observation. Its
        // connection-local data_version ignores our own bounded batches but
        // detects every hook/action/second-process commit since observation
        // began. Each write checks it while holding SQLite's writer gate.
        let mut store_reconcile = self.store.begin_reconcile_session()?;
        let observation = self.observe_processes();
        let processes = require_complete_processes(&observation, "reconcile ownership")?;
        let panes = match self.tmux.list_panes() {
            Ok(panes) => panes,
            Err(_) if !self.tmux.available() => Vec::new(),
            Err(error) => {
                return Err(error).context(
                    "Pika refused to reconcile ownership because tmux could not be observed",
                );
            }
        };
        let providers = Providers::new(&self.paths, &self.config);
        let mut stored = self.store.list_sessions()?;
        let provider_hidden = self.store.list_provider_hidden_sessions()?;
        let provider_hidden_keys = provider_hidden
            .iter()
            .map(|session| (session.provider, session.session_id.clone()))
            .collect::<BTreeSet<_>>();
        stored.extend(provider_hidden);
        let source_states = providers.source_states(&stored);
        let removed = stored
            .iter()
            .filter(|session| {
                let key = (session.provider, session.session_id.clone());
                provider_hidden_keys.contains(&key)
                    && source_states.get(&key)
                        != Some(&crate::providers::ProviderSourceState::Present)
                    || matches!(
                        source_states.get(&key),
                        Some(
                            crate::providers::ProviderSourceState::Archived
                                | crate::providers::ProviderSourceState::Deleted
                        )
                    )
            })
            .cloned()
            .collect::<Vec<_>>();
        let removed_keys = removed
            .iter()
            .map(|session| (session.provider, session.session_id.clone()))
            .collect::<BTreeSet<_>>();
        stored.retain(|session| {
            !removed_keys.contains(&(session.provider, session.session_id.clone()))
        });
        let named_candidates = self.named_discovery.candidates(&providers);
        self.admit_named_candidates(
            reconcile_generation,
            &mut store_reconcile,
            &mut stored,
            &named_candidates,
            &provider_hidden_keys,
        )?;
        // Provider-proven archive or deletion removes the row from daily
        // observation. Retain any exact pane tag as dormant recovery evidence:
        // reconciliation must never hold SQLite's writer lock across tmux I/O,
        // and clearing an observed tag after commit would race an exact reopen
        // in another process. If the provider restores the conversation, the
        // next observation revalidates this tag against live process identity;
        // otherwise no exact action can attach through the hidden row.
        let mut wanted = BTreeMap::<Provider, BTreeMap<String, f64>>::new();
        for session in &stored {
            let activity = wanted.entry(session.provider).or_default();
            for identity in identity_strings(session) {
                activity
                    .entry(identity.to_owned())
                    .and_modify(|value| *value = value.max(session.last_activity_at))
                    .or_insert(session.last_activity_at);
            }
        }
        let mut candidate_map = BTreeMap::new();
        for (provider, activity) in &wanted {
            for candidate in providers.reconcile_records(*provider, activity) {
                candidate_map.insert(
                    (candidate.provider, candidate.session_id.clone()),
                    candidate,
                );
            }
        }
        let known: BTreeSet<_> = stored
            .iter()
            .map(|session| (session.provider, session.session_id.clone()))
            .collect();
        let mut fork_imports = Vec::new();
        for candidate in candidate_map.values() {
            let Some(parent_id) = candidate.parent_session_id.as_deref() else {
                continue;
            };
            let Some(parent_index) = stored.iter().position(|session| {
                session.provider == Provider::Codex
                    && (session.session_id == parent_id
                        || session.active_thread_id.as_deref() == Some(parent_id))
            }) else {
                continue;
            };
            let parent = &stored[parent_index];
            if skip_independent_codex_fork(candidate, parent, &known) {
                continue;
            }
            let same_pane = panes
                .iter()
                .find(|pane| {
                    pane.pika_provider == Some(Provider::Codex)
                        && pane.pika_session_id.as_deref() == Some(&parent.session_id)
                })
                .is_some_and(|pane| {
                    let tree = process::process_tree(pane.pane_pid, processes);
                    process::find_session_processes(
                        &candidate.session_id,
                        Provider::Codex,
                        processes,
                    )
                    .iter()
                    .any(|pid| tree.contains(pid))
                });
            if same_pane {
                let parent = &mut stored[parent_index];
                parent.active_thread_id = Some(candidate.session_id.clone());
                merge_session_candidate(parent, candidate);
            } else {
                let mut fork = session_from_candidate(candidate);
                fork.managed = false;
                fork_imports.push(fork);
            }
        }
        // Reconciliation may project thousands of sessions, but hooks are the
        // latency-sensitive truth path. Commit bounded batches so a hook never
        // waits behind an inventory-sized SQLite writer transaction. Every
        // individual projection remains atomic inside its batch; the final
        // reload below observes hook writes that interleaved between batches.
        for batch in removed.chunks(RECONCILE_WRITE_BATCH) {
            self.reconciled_write(reconcile_generation, &mut store_reconcile, |ledger| {
                for session in batch {
                    let state =
                        match source_states.get(&(session.provider, session.session_id.clone())) {
                            Some(crate::providers::ProviderSourceState::Archived) => "archived",
                            Some(crate::providers::ProviderSourceState::Deleted) => "deleted",
                            _ => "source-unavailable",
                        };
                    ledger.hide_provider_session(session.provider, &session.session_id, state)?;
                }
                Ok(())
            })?;
            std::thread::sleep(Duration::from_millis(1));
        }
        let restorations = stored
            .iter()
            .filter(|session| {
                let key = (session.provider, session.session_id.clone());
                provider_hidden_keys.contains(&key)
                    && source_states.get(&key)
                        == Some(&crate::providers::ProviderSourceState::Present)
            })
            .collect::<Vec<_>>();
        for batch in restorations.chunks(RECONCILE_WRITE_BATCH) {
            self.reconciled_write(reconcile_generation, &mut store_reconcile, |ledger| {
                for session in batch {
                    ledger.restore_provider_session(session.provider, &session.session_id)?;
                }
                Ok(())
            })?;
            std::thread::sleep(Duration::from_millis(1));
        }
        let mut remaining = stored.into_iter();
        let mut runtime_sessions = BTreeMap::new();
        loop {
            let batch = remaining
                .by_ref()
                .take(RECONCILE_WRITE_BATCH)
                .collect::<Vec<_>>();
            if batch.is_empty() {
                break;
            }
            let projected =
                self.reconciled_write(reconcile_generation, &mut store_reconcile, |ledger| {
                    let mut projected = Vec::with_capacity(batch.len());
                    for mut session in batch {
                        let (candidate, continuation_conflicts) = continuation_candidate(
                            &session,
                            &candidate_map,
                            &panes,
                            processes,
                            now(),
                        );
                        if let Some(candidate) = candidate {
                            if candidate.session_id != session.session_id {
                                session.active_thread_id = Some(candidate.session_id.clone());
                            }
                            merge_session_candidate(&mut session, candidate);
                            if let Some(status) = candidate.lifecycle_status {
                                let attached = panes.iter().any(|pane| {
                                    pane.attached
                                        && pane.pika_provider == Some(session.provider)
                                        && pane.pika_session_id.as_deref()
                                            == Some(&session.session_id)
                                });
                                let observation = crate::model::StatusObservation {
                                    kind: ObservationKind::Lifecycle,
                                    status,
                                    unread: status == Status::Ready
                                        && candidate.updated_at > session.last_event_at
                                        && !attached,
                                    attention_reason: (status == Status::Ready)
                                        .then(|| "completed".into()),
                                    error: None,
                                    observed_at: candidate.updated_at.max(session.last_activity_at),
                                    source: candidate.source.clone(),
                                };
                                ledger.record_status_observation(
                                    session.provider,
                                    &session.session_id,
                                    &observation,
                                )?;
                            }
                        }
                        self.reconcile_one_in(
                            &mut session,
                            &panes,
                            processes,
                            ledger,
                            &continuation_conflicts,
                        )?;
                        recover_confirmed_pending(&session, &panes, processes, ledger)?;
                        ledger.upsert_session(&session, false)?;
                        projected.push(session);
                    }
                    Ok(projected)
                })?;
            runtime_sessions.extend(
                projected
                    .into_iter()
                    .map(|session| ((session.provider, session.session_id.clone()), session)),
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        for batch in fork_imports.chunks(RECONCILE_WRITE_BATCH) {
            self.reconciled_write(reconcile_generation, &mut store_reconcile, |ledger| {
                for fork in batch {
                    let _ = ledger.upsert_session(fork, false)?;
                }
                Ok(())
            })?;
            std::thread::sleep(Duration::from_millis(1));
        }
        let mut sessions = self.store.list_sessions()?;
        for session in &mut sessions {
            if let Some(runtime) =
                runtime_sessions.get(&(session.provider, session.session_id.clone()))
            {
                session.live = runtime.live;
                session.attached = runtime.attached;
                session.home_state.clone_from(&runtime.home_state);
                session.cpu_percent = runtime.cpu_percent;
                session.rss_kb = runtime.rss_kb;
            }
        }
        Ok(ReconciledInventory {
            inventory: Inventory {
                sessions,
                pending: self.store.list_visible_pending()?,
            },
            candidates: candidate_map,
        })
    }

    /// Grant watched membership to provider-verified personal names only.
    /// Membership writes use the same generation-fenced, small transactions as
    /// the rest of reconciliation; the provider cache is evidence, never a
    /// lifecycle/state snapshot to replay over a session that already exists.
    fn admit_named_candidates(
        &self,
        generation: u64,
        store_reconcile: &mut ReconcileSession,
        stored: &mut Vec<Session>,
        candidates: &[Candidate],
        provider_hidden: &BTreeSet<(Provider, String)>,
    ) -> Result<()> {
        let mut known = stored
            .iter()
            .flat_map(|session| {
                identity_strings(session)
                    .into_iter()
                    .map(move |identity| (session.provider, identity.to_owned()))
            })
            .collect::<BTreeSet<_>>();
        let eligible = candidates
            .iter()
            .filter(|candidate| {
                candidate
                    .name
                    .as_deref()
                    .is_some_and(|name| !name.trim().is_empty())
                    && !provider_hidden
                        .contains(&(candidate.provider, candidate.session_id.clone()))
                    && known.insert((candidate.provider, candidate.session_id.clone()))
            })
            .cloned()
            .collect::<Vec<_>>();
        for batch in eligible.chunks(RECONCILE_WRITE_BATCH) {
            let results = self.reconciled_write(generation, store_reconcile, |ledger| {
                let mut admitted = Vec::new();
                for candidate in batch {
                    let session = session_from_candidate(candidate);
                    if ledger.watch_named_session(&session)? {
                        if let Some(existing) =
                            ledger.get_session(candidate.provider, &candidate.session_id)?
                        {
                            admitted.push(existing);
                        }
                    }
                }
                Ok(admitted)
            })?;
            // The ledger snapshot retains an existing external observation's
            // unread, lifecycle, and ownership state for this projection.
            stored.extend(results);
            std::thread::sleep(Duration::from_millis(1));
        }
        Ok(())
    }

    #[cfg(test)]
    fn reconcile_one(
        &self,
        session: &mut Session,
        panes: &[Pane],
        processes: &BTreeMap<i64, ProcessRecord>,
    ) -> Result<()> {
        self.store.reconcile_transaction(|ledger| {
            self.reconcile_one_in(session, panes, processes, ledger, &[])
        })
    }

    fn reconcile_one_in(
        &self,
        session: &mut Session,
        panes: &[Pane],
        processes: &BTreeMap<i64, ProcessRecord>,
        ledger: &ReconcileLedger<'_>,
        continuation_conflicts: &[String],
    ) -> Result<()> {
        let tagged: Vec<&Pane> = panes
            .iter()
            .filter(|pane| {
                pane.pika_provider == Some(session.provider)
                    && pane.pika_session_id.as_deref() == Some(&session.session_id)
            })
            .collect();
        let owned: BTreeSet<i64> = tagged
            .iter()
            .flat_map(|pane| process::process_tree(pane.pane_pid, processes))
            .collect();

        let timestamp = now();
        let owners = identity_owners(session, processes, ledger, timestamp)?;
        let identities = owners.pids();
        let outside: Vec<i64> = identities.difference(&owned).copied().collect();
        let pane_selection = process::identity_pane_candidates(
            tagged.iter().copied(),
            session.provider,
            &identities,
            processes,
            |pid, pane| owners.proves_pane(pid, pane),
        );
        let candidate_panes = pane_selection.candidates;
        let ambiguous_provider = pane_selection.ambiguous_provider;
        let incomplete_root = pane_selection.incomplete_root;

        let safety = if !continuation_conflicts.is_empty()
            || identities.len() > 1
            || candidate_panes.len() > 1
            || (!candidate_panes.is_empty() && !outside.is_empty())
        {
            Some(Status::OpenTwice)
        } else if ambiguous_provider || incomplete_root {
            Some(Status::Error)
        } else {
            None
        };
        let exact = if safety.is_none() {
            candidate_panes.first().copied().filter(|(pane, _)| {
                let tree = process::process_tree(pane.pane_pid, processes)
                    .into_iter()
                    .collect::<BTreeSet<_>>();
                identities.is_subset(&tree)
            })
        } else {
            None
        };
        session.live = exact.is_some() || !outside.is_empty();
        session.attached = exact.is_some_and(|(pane, _)| pane.attached);
        session.home_state = if safety == Some(Status::OpenTwice) {
            "open_twice"
        } else if ambiguous_provider {
            "identity_unproven"
        } else if incomplete_root {
            "identity_incomplete"
        } else if exact.is_some() {
            "exact"
        } else if !outside.is_empty() {
            "outside"
        } else {
            "missing"
        }
        .into();
        if let Some((pane, pid)) = exact {
            session.tmux_session = Some(pane.session_name.clone());
            session.tmux_pane = Some(pane.pane_id.clone());
            session.root_pid = Some(pid);
        } else {
            if let Some(pid) = outside
                .iter()
                .copied()
                .find(|pid| exclusive_outside_owner(session.provider, *pid, processes))
            {
                session.root_pid = Some(pid);
            } else {
                session.root_pid = None;
                // Clear the exclusive root projection, not advisory leases.
                ledger.clear_session_runtime(session.provider, &session.session_id, timestamp)?;
            }
            if tagged.is_empty() {
                session.tmux_session = None;
                session.tmux_pane = None;
                ledger.clear_session_home(session.provider, &session.session_id, timestamp)?;
            } else if tagged.len() == 1 {
                session.tmux_session = Some(tagged[0].session_name.clone());
                session.tmux_pane = Some(tagged[0].pane_id.clone());
            }
        }

        let mut observations = ledger.status_observations(session.provider, &session.session_id)?;
        if let Some(status) = safety {
            let error = if incomplete_root {
                "a tagged pane's process root was missing from the complete observation".into()
            } else if ambiguous_provider {
                "a tagged pane contains an unverified live provider process".into()
            } else if continuation_conflicts.is_empty() {
                "the exact conversation has more than one live owner".into()
            } else {
                format!(
                    "multiple active Codex continuation threads: {}",
                    continuation_conflicts
                        .iter()
                        .map(|value| value.chars().take(8).collect::<String>())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            };
            let observation = crate::model::StatusObservation {
                kind: ObservationKind::Safety,
                status,
                unread: true,
                attention_reason: Some("identity".into()),
                error: Some(error),
                observed_at: timestamp,
                source: "reconcile".into(),
            };
            ledger.record_status_observation(
                session.provider,
                &session.session_id,
                &observation,
            )?;
            observations.retain(|value| value.kind != ObservationKind::Safety);
            observations.push(observation);
        } else {
            ledger.clear_status_observation(
                session.provider,
                &session.session_id,
                ObservationKind::Safety,
            )?;
            ledger.clear_identity_interruption(session.provider, &session.session_id)?;
            observations.retain(|value| value.kind != ObservationKind::Safety);
        }
        let fallback = ProjectionFallback {
            status: session.status,
            unread: session.unread,
            attention_reason: session.attention_reason.as_deref(),
            error: session.error.as_deref(),
            observed_at: session.last_event_at,
        };
        let projected = project_status(&observations, session.live, &session.home_state, fallback);
        session.status = projected.status;
        session.unread = projected.unread;
        session.attention_reason = projected.attention_reason;
        session.error = projected.error;
        session.last_event_at = projected.observed_at;
        if safety == Some(Status::OpenTwice) {
            session.attention_reason = Some("identity".into());
            session.error = Some(if continuation_conflicts.is_empty() {
                "the exact conversation has more than one live owner".into()
            } else {
                format!(
                    "multiple active Codex continuation threads: {}",
                    continuation_conflicts
                        .iter()
                        .map(|value| value.chars().take(8).collect::<String>())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            });
        } else if ambiguous_provider {
            session.attention_reason = Some("identity".into());
            session.error =
                Some("a tagged pane contains an unverified live provider process".into());
        } else if incomplete_root {
            session.attention_reason = Some("identity".into());
            session.error = Some(
                "a tagged pane's process root was missing from the complete observation".into(),
            );
        }
        Ok(())
    }

    pub fn import_named(&self) -> Result<Vec<Candidate>> {
        let providers = Providers::new(&self.paths, &self.config);
        let excluded: BTreeSet<(Provider, String)> = self
            .store
            .list_sessions()?
            .into_iter()
            .chain(self.store.list_untracked_sessions()?)
            .flat_map(|session| {
                let mut identities = vec![(session.provider, session.session_id)];
                if let Some(active) = session.active_thread_id {
                    identities.push((session.provider, active));
                }
                identities
            })
            .collect();
        let mut candidates = Vec::new();
        for provider in Provider::ALL {
            candidates.extend(providers.import_candidates(provider).into_iter().filter(
                |candidate| !excluded.contains(&(candidate.provider, candidate.session_id.clone())),
            ));
        }
        let mut seen = BTreeSet::new();
        candidates
            .retain(|candidate| seen.insert((candidate.provider, candidate.session_id.clone())));
        candidates.sort_by(|left, right| right.updated_at.total_cmp(&left.updated_at));
        candidates.dedup_by(|left, right| {
            left.provider == right.provider && left.session_id == right.session_id
        });
        Ok(candidates)
    }

    fn unconfirmed_candidates(&self) -> Result<Vec<Candidate>> {
        let providers = Providers::new(&self.paths, &self.config);
        let mut sessions = self.store.list_unconfirmed_sessions()?;
        sessions.sort_by(|a, b| b.last_activity_at.total_cmp(&a.last_activity_at));
        sessions.truncate(200);
        let states = providers.source_states(&sessions);
        Ok(sessions
            .into_iter()
            .filter(|session| {
                !matches!(
                    states.get(&(session.provider, session.session_id.clone())),
                    Some(
                        crate::providers::ProviderSourceState::Archived
                            | crate::providers::ProviderSourceState::Deleted
                    )
                )
            })
            .map(|session| Candidate {
                provider: session.provider,
                session_id: session.session_id,
                name: session.name,
                cwd: session.cwd,
                branch: session.branch,
                transcript_path: session.transcript_path,
                model: session.model,
                created_at: session.created_at,
                updated_at: session.last_activity_at,
                live: false,
                pid: None,
                source: "unconfirmed-observation".into(),
                parent_session_id: None,
                lifecycle_status: None,
            })
            .collect())
    }

    /// Return a bounded second-screen inventory of conversations without an
    /// explicit provider name. Provider-generated labels may remain on the
    /// candidate for orientation, but an identity present in the named pass is
    /// never repeated here.
    pub fn import_recent_unnamed(&self, limit: usize) -> Result<Vec<Candidate>> {
        let providers = Providers::new(&self.paths, &self.config);
        let excluded: BTreeSet<(Provider, String)> = self
            .store
            .list_sessions()?
            .into_iter()
            .chain(self.store.list_untracked_sessions()?)
            .map(|session| (session.provider, session.session_id))
            .chain(
                self.import_named()?
                    .into_iter()
                    .map(|candidate| (candidate.provider, candidate.session_id)),
            )
            .collect();
        let mut candidates = Provider::ALL
            .into_iter()
            .flat_map(|provider| providers.browse(provider))
            .chain(self.unconfirmed_candidates()?)
            .filter(|candidate| {
                !excluded.contains(&(candidate.provider, candidate.session_id.clone()))
            })
            .collect::<Vec<_>>();
        let mut seen = BTreeSet::new();
        candidates
            .retain(|candidate| seen.insert((candidate.provider, candidate.session_id.clone())));
        candidates.sort_by(|left, right| right.updated_at.total_cmp(&left.updated_at));
        candidates.dedup_by(|left, right| {
            left.provider == right.provider && left.session_id == right.session_id
        });
        candidates.truncate(limit);
        Ok(candidates)
    }

    pub fn adopt_candidate(&self, candidate: &Candidate) -> Result<bool> {
        self.store.initialize()?;
        self.store.adopt_session(&session_from_candidate(candidate))
    }

    pub fn resolve_local(&self, query: &str) -> Result<Vec<Session>> {
        let providers = Providers::new(&self.paths, &self.config);
        // Persisted rows deliberately have no live ownership. Reduce daily
        // names only from the fresh ownership snapshot, keeping provider clocks
        // separate from Pika's resume/hook/reconciliation activity clocks.
        let ReconciledInventory {
            inventory,
            candidates: mut metadata,
        } = fresh_observation(|| self.reconcile_local_with_candidates())?;
        let mut sessions = inventory.sessions;
        // An explicit daily-name or UUID lookup is also the recovery path for
        // a conversation the user previously stopped watching. Keep the
        // tombstone in place while resolving so read-only lookups and
        // ambiguous choices have no side effect; `open_session` removes only
        // the exact selected tombstone immediately before the open action.
        sessions.extend(self.store.list_untracked_sessions()?);
        sessions.extend(self.store.list_unconfirmed_sessions()?);
        let mut known: BTreeSet<(Provider, String)> = sessions
            .iter()
            .map(|item| (item.provider, item.session_id.clone()))
            .collect();
        let (provider_filter, raw_query) = split_provider_query(query);
        for provider in Provider::ALL {
            if provider_filter.is_some_and(|expected| expected != provider) {
                continue;
            }
            for candidate in providers.find(provider, raw_query) {
                if let Some(existing) = sessions.iter_mut().find(|session| {
                    session.provider == candidate.provider
                        && (session.session_id == candidate.session_id
                            || session.active_thread_id.as_deref()
                                == Some(candidate.session_id.as_str()))
                }) {
                    // Keep an untracked row's stable Pika identity while using
                    // current provider metadata for explicit lookup (including
                    // a native rename made while it was unwatched).
                    merge_session_candidate(existing, &candidate);
                } else if known.insert((candidate.provider, candidate.session_id.clone())) {
                    sessions.push(session_from_candidate(&candidate));
                }
                metadata.insert(
                    (candidate.provider, candidate.session_id.clone()),
                    candidate,
                );
            }
        }
        if let Some(provider) = provider_filter {
            sessions.retain(|session| session.provider == provider);
        }
        let source_index =
            crate::experts::LocalSourceIndex::read(&self.paths, &self.config, &sessions);
        let choices: Vec<NameCandidate> = sessions
            .iter()
            .map(|session| {
                let state = match source_index.availability(session) {
                    crate::experts::SourceAvailability::SourceAvailable => EvidenceState::Available,
                    crate::experts::SourceAvailability::Archived => EvidenceState::Archived,
                    crate::experts::SourceAvailability::Deleted => EvidenceState::Missing,
                    crate::experts::SourceAvailability::SourceUnavailable
                    | crate::experts::SourceAvailability::RequiresReconciliation => {
                        EvidenceState::Unknown
                    }
                };
                NameCandidate {
                    provider: session.provider,
                    session_id: session.session_id.clone(),
                    active_thread_id: session.active_thread_id.clone(),
                    name: session.name.clone(),
                    live: session.live,
                    exact_home: session.has_exact_home(),
                    status: session.status,
                    local: true,
                    evidence: SelectionEvidence {
                        state,
                        canonical_cwd: canonical_directory(session.cwd.as_deref()),
                        provider_updated_at: metadata
                            .get(&(session.provider, session.provider_thread_id().to_owned()))
                            .map(|candidate| candidate.updated_at),
                    },
                }
            })
            .collect();
        match resolve_name(raw_query, &choices) {
            Ok(indexes) => Ok(indexes
                .into_iter()
                .filter_map(|index| sessions.get(index).cloned())
                .collect()),
            Err(NameResolutionError::NotFound) => Ok(Vec::new()),
            Err(error) => Err(error.into()),
        }
    }

    /// Prove the exact tagged pane and provider identity before allowing a
    /// conversation to publish expertise about itself.
    pub fn current_exact_session(&self) -> Result<Session> {
        let provider: Provider = std::env::var("PIKA_PROVIDER")
            .context("this shell has no Pika provider identity")?
            .parse()
            .map_err(anyhow::Error::msg)?;
        let identity = std::env::var("PIKA_SESSION_ID")
            .context("this shell has no Pika conversation identity")?;
        let pane_id =
            std::env::var("TMUX_PANE").context("this command is not running inside a Pika pane")?;
        let session = self
            .store
            .get_session_by_thread(provider, &identity)?
            .context("the calling conversation is not tracked")?;
        self.exact_pane_binding(&session, Some(&pane_id))?;
        Ok(session)
    }

    /// Bind one exact provider identity to a current process generation using
    /// UUID argv or the provider-hook-certified launch generation. A remembered
    /// pane ID, title, lease, or launch token alone is never sufficient.
    pub fn exact_pane_binding(
        &self,
        session: &Session,
        expected_pane: Option<&str>,
    ) -> Result<ExactPaneBinding> {
        let observation = self.observe_processes();
        let processes = require_complete_processes(&observation, "verify the exact pane")?;
        let panes = self
            .tmux
            .list_panes()
            .context("Pika refused the pane action because tmux could not be observed")?;
        let matches = panes
            .iter()
            .filter(|pane| {
                pane.pika_provider == Some(session.provider)
                    && pane.pika_session_id.as_deref() == Some(&session.session_id)
            })
            .collect::<Vec<_>>();
        if matches.is_empty() {
            bail!(
                "Pika cannot bind one exact pane to {} {} (found 0). No pane action was performed.",
                session.provider,
                session.session_id,
            );
        }
        let owners = self
            .store
            .reconcile_transaction(|ledger| identity_owners(session, processes, ledger, now()))?;
        let identity_pids = owners.pids();
        if identity_pids.len() > 1 {
            let pids = identity_pids
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(",");
            bail!(
                "OPEN TWICE · {}\nMore than one {} client claims this conversation.\nInspect the observed copies: `ps -p {} -o pid,ppid,tty,args`\nExit the extra copy normally, then run: `pika open {}`\nNo process was stopped.",
                receipt_text(&session.display_name()),
                session.provider,
                pids,
                shell_words::quote(&format!("{}:{}", session.provider, session.session_id)),
            );
        }
        let pane_selection = process::identity_pane_candidates(
            matches.iter().copied(),
            session.provider,
            &identity_pids,
            processes,
            |pid, pane| owners.proves_pane(pid, pane),
        );
        let candidate_panes = pane_selection.candidates;
        let ambiguous_provider = pane_selection.ambiguous_provider;
        let incomplete_root = pane_selection.incomplete_root;
        if incomplete_root {
            bail!(
                "Pika could not verify every tagged pane's process root for {} {}; no pane action was performed.",
                session.provider,
                session.session_id
            );
        }
        if ambiguous_provider {
            bail!(
                "Pika found an unverified live {} process in a tagged pane competing with the exact identity; no pane action was performed.",
                session.provider
            );
        }
        if candidate_panes.len() != 1 {
            bail!(
                "Pika cannot bind one exact pane to {} {} (found {}). No pane action was performed.",
                session.provider,
                session.session_id,
                candidate_panes.len()
            );
        }
        let (pane, provider_pid) = candidate_panes[0];
        if expected_pane.is_some_and(|expected| pane.pane_id != expected) {
            bail!(
                "Pika cannot bind the expected exact pane {} to {} {}. No pane action was performed.",
                expected_pane.unwrap_or_default(),
                session.provider,
                session.session_id,
            );
        }
        let pane_generation = processes
            .get(&pane.pane_pid)
            .map(ProcessRecord::generation)
            .context("the tmux pane root disappeared from the complete observation")?;
        if process::process_generation(pane.pane_pid) != Some(pane_generation) {
            bail!(
                "the tmux pane root generation changed after observation; no pane action was performed"
            );
        }
        let ignored = matches
            .iter()
            .copied()
            .filter(|other| other.pane_id != pane.pane_id)
            .collect::<Vec<_>>();
        if !ignored.is_empty() {
            let fresh_observation = self.observe_processes();
            let fresh_processes = require_complete_processes(
                &fresh_observation,
                "recheck ignored tagged panes before an exact action",
            )?;
            if fresh_processes
                .get(&provider_pid)
                .map(ProcessRecord::generation)
                != processes.get(&provider_pid).map(ProcessRecord::generation)
            {
                bail!(
                    "the exact provider generation changed while ignored tagged panes were rechecked; no pane action was performed"
                );
            }
            for ignored_pane in ignored {
                let fresh = self
                    .tmux
                    .get_pane(&ignored_pane.pane_id)?
                    .context("an ignored tagged pane disappeared during identity recheck")?;
                if !same_pane_generation(ignored_pane, &fresh)
                    || fresh.pika_provider != Some(session.provider)
                    || fresh.pika_session_id.as_deref() != Some(&session.session_id)
                    || fresh.pika_launch_token != ignored_pane.pika_launch_token
                {
                    bail!(
                        "an ignored tagged pane changed during identity recheck; no pane action was performed"
                    );
                }
                let fresh_root = fresh_processes
                    .get(&fresh.pane_pid)
                    .map(ProcessRecord::generation)
                    .context("an ignored tagged pane root disappeared during identity recheck")?;
                let ignored_generation = processes
                    .get(&ignored_pane.pane_pid)
                    .map(ProcessRecord::generation)
                    .context("an ignored tagged pane root disappeared after observation")?;
                if fresh_root != ignored_generation {
                    bail!(
                        "an ignored tagged pane process generation changed during identity recheck; no pane action was performed"
                    );
                }
                let tree = process::process_tree_generation(fresh_root, fresh_processes);
                if tree.iter().any(|pid| {
                    fresh_processes.get(pid).and_then(ProcessRecord::provider)
                        == Some(session.provider)
                }) {
                    bail!(
                        "an ignored tagged pane acquired a live {} process; no pane action was performed",
                        session.provider
                    );
                }
            }
        }
        let tree = process::process_tree_generation(pane_generation, processes);
        if !tree.contains(&provider_pid) {
            bail!(
                "the exact provider process is outside the tagged pane; no pane action was performed"
            );
        }
        let provider_start_time = processes
            .get(&provider_pid)
            .map(|record| record.start_time)
            .context("the exact provider process disappeared from the complete observation")?;
        let provider_generation = process::ProcessGeneration {
            pid: provider_pid,
            start_time: provider_start_time,
        };
        process::revalidate_ancestry(pane_generation, provider_generation, processes)
            .map_err(anyhow::Error::msg)
            .context("the exact pane ancestry changed after tmux observation; no pane action was performed")?;
        let fresh = self
            .tmux
            .get_pane(&pane.pane_id)?
            .context("the exact pane disappeared before the action")?;
        if !same_pane_generation(pane, &fresh)
            || fresh.pika_provider != Some(session.provider)
            || fresh.pika_session_id.as_deref() != Some(&session.session_id)
            || fresh.pika_launch_token != pane.pika_launch_token
        {
            bail!("the exact pane or provider generation changed; no pane action was performed");
        }
        process::revalidate_ancestry(pane_generation, provider_generation, processes)
            .map_err(anyhow::Error::msg)
            .context("the exact pane ancestry changed immediately before the action; no pane action was performed")?;
        if !owners.direct.contains(&provider_pid) {
            let before = owners.recovery.as_ref().expect("certified pane proof");
            let current = self
                .store
                .get_recovery_owner(session.provider, &session.session_id)?;
            if !current.as_ref().is_some_and(|owner| {
                owner.pid == before.pid
                    && owner.start_time == before.start_time
                    && owner.launch_token == before.launch_token
            }) || self.store.get_launch_binding(&before.launch_token)?
                != Some((session.provider, session.session_id.clone()))
            {
                bail!(
                    "the exact provider launch certificate changed; no pane action was performed"
                );
            }
        }
        Ok(ExactPaneBinding {
            pane: fresh,
            pane_start_time: pane_generation.start_time,
            provider_pid,
            provider_start_time,
            provider_launcher: process::runtime_launcher_generation(provider_pid, processes),
        })
    }

    pub fn capture_exact(&self, session: &Session, lines: usize) -> Result<String> {
        let before = self.exact_pane_binding(session, session.tmux_pane.as_deref())?;
        let output = self.tmux.capture(&before.pane.pane_id, lines)?;
        let after = self.exact_pane_binding(session, Some(&before.pane.pane_id))?;
        if !same_exact_binding(&before, &after) {
            bail!("the exact pane changed during capture; captured output was discarded")
        }
        Ok(output)
    }

    pub fn clear_exact_tags(&self, session: &Session) -> Result<()> {
        let binding = self.exact_pane_binding(session, session.tmux_pane.as_deref())?;
        self.tmux.clear_tags_if_unchanged(&binding.pane)
    }

    fn confirm_exact_handoff(&self, session: &Session, expected: &ExactPaneBinding) -> Result<()> {
        let current = self.exact_pane_binding(session, Some(&expected.pane.pane_id))?;
        if !same_handoff_binding(expected, &current) {
            bail!(
                "exact conversation ownership changed during terminal handoff; Pika detached without acknowledging it"
            );
        }
        Ok(())
    }

    fn record_exact_handoff(
        &self,
        session: &Session,
        expected: &ExactPaneBinding,
        expected_event_at: f64,
    ) -> Result<()> {
        self.confirm_exact_handoff(session, expected)?;
        open_history::record_session(&self.store, session.provider, &session.session_id)?;
        self.store.acknowledge_attention(
            session.provider,
            &session.session_id,
            expected_event_at,
            true,
        )?;
        Ok(())
    }

    pub fn open_name(&self, query: &str, attach: bool, allow_create: bool) -> Result<OpenReceipt> {
        self.store.initialize()?;
        let matches = self.resolve_local(query)?;
        if matches.len() > 1 {
            return Err(OpenError::Ambiguous(ambiguity_message(query, &matches)).into());
        }
        if let Some(session) = matches.into_iter().next() {
            if self
                .store
                .get_session(session.provider, &session.session_id)?
                .is_none()
            {
                self.store.upsert_session(&session, false)?;
            }
            return self.open_session(session, attach);
        }
        let pending = self.resolve_pending(query)?;
        match pending.as_slice() {
            [pending] => return self.open_pending(&pending.launch_token, attach),
            [] => {}
            _ => bail!(
                "{query:?} matches several pending launches. Use `pika PROVIDER:NAME` or choose the existing terminal in the board. No new client was launched."
            ),
        }
        if !allow_create {
            return Err(OpenError::NotFound(query.to_owned()).into());
        }
        self.new_session(query, self.config.default_provider, attach)
    }

    /// Pending launches are recovery handles, not fabricated conversations.
    /// Hidden launches remain addressable by their exact daily name.
    pub fn resolve_pending(&self, query: &str) -> Result<Vec<PendingLaunch>> {
        let (provider, name) = split_provider_query(query);
        Ok(self
            .store
            .list_pending()?
            .into_iter()
            .filter(|pending| {
                provider.is_none_or(|provider| provider == pending.provider)
                    && pending.name.eq_ignore_ascii_case(name)
            })
            .collect())
    }

    /// Shared pending-state projection for the board, activity feed and CLI.
    pub fn pending_session(&self, pending: &PendingLaunch) -> Result<Session> {
        let mut session = session_from_pending(pending);
        if let Some(exit) = self
            .store
            .get_pending_exit(&pending.launch_token)?
            .filter(|exit| {
                exit.provider == pending.provider && exit.pending_created_at == pending.created_at
            })
        {
            session.status = Status::Error;
            session.home_state = "startup-exited".into();
            session.attention_reason = Some("startup exited before confirmation".into());
            session.error = Some(format!(
                "{} exited during startup (code {}). Enter reopens the existing terminal to inspect or finish the update. No second client was started; the conversation identity is still unconfirmed.",
                pending.provider, exit.code
            ));
            session.last_event_at = exit.observed_at;
            session.updated_at = exit.observed_at;
        }
        Ok(session)
    }

    pub fn open_session(&self, mut session: Session, attach: bool) -> Result<OpenReceipt> {
        // Selection is complete by the time this boundary is entered. Match
        // the frozen daily-command contract: explicitly opening an unwatched
        // conversation resumes watching that exact provider UUID, even when a
        // later provider/tmux check prevents the actual attach or resume.
        if self
            .store
            .restore_tracking(session.provider, &session.session_id)?
        {
            self.store.upsert_session(&session, false)?;
        }
        let inventory = self.reconcile_for_action()?;
        if let Some(current) = inventory.sessions.into_iter().find(|candidate| {
            candidate.provider == session.provider && candidate.session_id == session.session_id
        }) {
            session = current;
        }
        if session.status == Status::OpenTwice {
            return Err(OpenError::Identity(format!(
                "OPEN TWICE · {} has multiple live {} clients. Close the unintended copy, then run exactly: `pika {}`.",
                session.display_name(),
                session.provider,
                shell_words::quote(&session.display_name())
            ))
            .into());
        }
        if matches!(
            session.home_state.as_str(),
            "identity_unproven" | "identity_incomplete"
        ) {
            return Err(OpenError::IdentityUnproven.into());
        }
        if session.has_exact_home() {
            let binding = self.exact_pane_binding(&session, session.tmux_pane.as_deref())?;
            let event = session.last_event_at;
            let (code, receipt_delivery) = if attach {
                let receipt = continuity_receipt(&session, "ATTACHED LIVE");
                let handoff = self.tmux.attach_exact_with_observed_receipt(
                    &binding.pane,
                    &receipt,
                    || self.record_exact_handoff(&session, &binding, event),
                )?;
                (handoff.exit_code, handoff.delivery)
            } else {
                (0, None)
            };
            return Ok(OpenReceipt {
                target: OpenTarget::Session(Box::new(session)),
                kind: "ATTACHED LIVE",
                exit_code: code,
                receipt_delivery,
            });
        }

        let observation = self.observe_processes();
        let processes = require_complete_processes(&observation, "resume the conversation")?;
        let panes = self
            .tmux
            .list_panes()
            .context("Pika refused to resume because tmux could not be observed")?;
        let owned: BTreeSet<i64> = panes
            .iter()
            .filter(|pane| {
                pane.pika_provider == Some(session.provider)
                    && pane.pika_session_id.as_deref() == Some(&session.session_id)
            })
            .flat_map(|pane| process::process_tree(pane.pane_pid, processes))
            .collect();
        let mut outside = BTreeSet::new();
        for identity in identity_strings(&session) {
            outside.extend(
                process::find_session_processes(identity, session.provider, processes)
                    .into_iter()
                    .filter(|pid| !owned.contains(pid)),
            );
        }
        if !outside.is_empty() {
            let pids = outside
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            let display_name = session.display_name();
            let name = shell_words::quote(&display_name);
            return Err(OpenError::OutsideLive(format!(
                "{} is already running outside its Pika home (PID {pids}) and cannot be moved safely while live. Required steps: 1) return to that terminal and exit {} normally; 2) run exactly: `pika {name}`. Pika will resume the exact UUID.",
                session.display_name(), session.provider
            ))
            .into());
        }
        if session.live && session.home_state == "outside" {
            let owners = self
                .store
                .live_owners(session.provider, &session.session_id)?
                .into_iter()
                .map(|owner| owner.pid.to_string())
                .collect::<Vec<_>>();
            let pids = if owners.is_empty() {
                "unknown".into()
            } else {
                owners.join(", ")
            };
            let display_name = session.display_name();
            return Err(OpenError::OutsideLive(format!(
                "{display_name} has live ownership outside its Pika home (PID {pids}). Required steps: 1) return to that terminal and exit {} normally; 2) run exactly: `pika {}`. Pika will resume the exact UUID.",
                session.provider,
                shell_words::quote(&display_name)
            ))
            .into());
        }
        let availability =
            crate::experts::local_source_availability(&self.paths, &self.config, &session);
        if !availability.permits_resume() {
            let display_name = session.display_name();
            let quoted_name = shell_words::quote(&display_name);
            if availability == crate::experts::SourceAvailability::Archived
                && session.provider == Provider::Codex
            {
                bail!(
                    "{display_name} is archived. Required steps: 1) run exactly: `codex unarchive {}`; 2) run exactly: `pika {quoted_name}`. Pika did not start a provider.",
                    shell_words::quote(session.provider_thread_id()),
                )
            }
            bail!(
                "Cannot resume {display_name}: {}. Pika did not start a provider. Restore the exact conversation in {}, then run exactly: `pika {quoted_name}`.",
                availability.as_str(),
                session.provider,
            )
        }
        self.resume_session(session, attach)
    }

    /// Reconcile once and attach only if the existing pane becomes exactly
    /// provable. This recovery path never reserves, launches, or resumes.
    /// Only a proven exact handoff records the open and acknowledges its event.
    pub fn recover_existing_session(&self, session: Session, attach: bool) -> Result<OpenReceipt> {
        let inventory = self.reconcile_for_action()?;
        let session = inventory
            .sessions
            .into_iter()
            .find(|candidate| {
                candidate.provider == session.provider && candidate.session_id == session.session_id
            })
            .ok_or_else(|| OpenError::NotFound(session.display_name()))?;
        if session.status == Status::OpenTwice {
            return Err(OpenError::Identity(
                "Pika still sees multiple live clients for this conversation; no provider was launched."
                    .into(),
            )
            .into());
        }
        if !session.has_exact_home() {
            return Err(OpenError::IdentityUnproven.into());
        }
        let binding = self.exact_pane_binding(&session, session.tmux_pane.as_deref())?;
        let (exit_code, receipt_delivery) = if attach {
            let receipt = continuity_receipt(&session, "ATTACHED LIVE");
            let handoff =
                self.tmux
                    .attach_exact_with_observed_receipt(&binding.pane, &receipt, || {
                        self.record_exact_handoff(&session, &binding, session.last_event_at)
                    })?;
            (handoff.exit_code, handoff.delivery)
        } else {
            (0, None)
        };
        Ok(OpenReceipt {
            target: OpenTarget::Session(Box::new(session)),
            kind: "ATTACHED LIVE",
            exit_code,
            receipt_delivery,
        })
    }

    /// Return dedicated UUID-bearing processes that are outside every tmux
    /// pane, with their platform-native birth stamps. This is evidence for an
    /// explicit clean-and-attach choice, never an automatic takeover.
    pub fn outside_identity_generations(&self, session: &Session) -> Result<Vec<(i64, u64)>> {
        let observation = self.observe_processes();
        let processes = require_complete_processes(&observation, "inspect outside ownership")?;
        let panes = self.tmux.list_panes().context(
            "Pika refused to inspect outside ownership because tmux could not be observed",
        )?;
        let inside: BTreeSet<i64> = panes
            .iter()
            .flat_map(|pane| process::process_tree(pane.pane_pid, processes))
            .collect();
        let mut outside = BTreeSet::new();
        for identity in identity_strings(session) {
            outside.extend(
                process::find_session_processes(identity, session.provider, processes)
                    .into_iter()
                    .filter(|pid| !inside.contains(pid)),
            );
        }
        outside
            .into_iter()
            .map(|pid| {
                let record = processes
                    .get(&pid)
                    .context("outside provider process disappeared during identity proof")?;
                if process::shared_provider_process(record, session.provider) {
                    bail!("Pika will never terminate shared provider infrastructure")
                }
                Ok((pid, record.start_time))
            })
            .collect()
    }

    /// Capture one tagged live pane for an explicit, unverified terminal
    /// handoff. Provider UUIDs, leases and launch tokens are intentionally not
    /// treated as identity proof here; they only select the already-tagged
    /// terminal the user may choose to inspect.
    pub fn unverified_pane_binding(&self, session: &Session) -> Result<UnverifiedPaneBinding> {
        let observation = self.observe_processes();
        let processes = require_complete_processes(&observation, "inspect the existing terminal")?;
        let panes = self.tmux.list_panes().context(
            "Pika refused to inspect the existing terminal because tmux could not be observed",
        )?;
        let tagged = panes
            .iter()
            .filter(|pane| {
                crate::tmux::is_pika_session(&pane.session_name)
                    && !pane.dead
                    && pane.pika_provider == Some(session.provider)
                    && pane.pika_session_id.as_deref() == Some(&session.session_id)
            })
            .collect::<Vec<_>>();
        if tagged.len() != 1 {
            bail!(
                "Pika found {} tagged terminals for this conversation; it will not offer an unverified handoff",
                tagged.len()
            );
        }
        let pane = tagged[0];
        let pane_record = processes
            .get(&pane.pane_pid)
            .context("the tagged terminal root is no longer live")?;
        let pane_generation = pane_record.generation();
        let provider_candidates = process::process_tree(pane.pane_pid, processes)
            .into_iter()
            .filter(|pid| {
                processes.get(pid).and_then(ProcessRecord::provider) == Some(session.provider)
                    && processes.get(pid).is_none_or(|record| {
                        !process::shared_provider_process(record, session.provider)
                    })
            })
            .collect::<BTreeSet<_>>();
        let provider_pids = process::canonical_identity_pids(&provider_candidates, processes);
        if provider_pids.len() != 1 {
            bail!(
                "Pika could not find one unique non-helper provider process in the tagged terminal"
            );
        }
        let provider_pid = provider_pids[0];
        let provider_record = processes
            .get(&provider_pid)
            .context("the provider process disappeared during terminal inspection")?;
        process::revalidate_ancestry(pane_generation, provider_record.generation(), processes)
            .map_err(anyhow::Error::msg)
            .context("the tagged terminal changed during inspection")?;
        Ok(UnverifiedPaneBinding {
            pane: pane.clone(),
            pane_start_time: pane_generation.start_time,
            provider_pid,
            provider_start_time: provider_record.start_time,
        })
    }

    fn revalidate_unverified_pane(
        &self,
        session: &Session,
        expected: &UnverifiedPaneBinding,
    ) -> Result<()> {
        let current = self.unverified_pane_binding(session)?;
        if !same_unverified_binding(expected, &current) {
            bail!("the existing terminal changed; Pika refused the unverified handoff")
        }
        Ok(())
    }

    /// Attach to a user-confirmed existing terminal without launching a
    /// provider or mutating Pika identity/attention state.
    pub fn open_unverified_terminal(
        &self,
        session: Session,
        expected: UnverifiedPaneBinding,
    ) -> Result<OpenReceipt> {
        self.revalidate_unverified_pane(&session, &expected)?;
        let receipt = format!(
            "UNVERIFIED TERMINAL · {} · conversation identity not certified",
            receipt_text(&session.display_name())
        );
        let exit_code = self
            .tmux
            .attach_unverified_with_started(&expected.pane, || {
                // The tmux generation guard closes the check/use gap for the
                // pane. Recheck process ancestry at the actual handoff too;
                // failure is reported after attach and never auto-recovered.
                self.revalidate_unverified_pane(&session, &expected)?;
                Ok(Some(receipt))
            })?;
        Ok(OpenReceipt {
            target: OpenTarget::Session(Box::new(session)),
            kind: "UNVERIFIED TERMINAL",
            exit_code,
            receipt_delivery: None,
        })
    }

    /// Gracefully stop one explicitly selected, generation-pinned outside
    /// client, prove it is gone, then resume the same UUID in Pika. No force
    /// kill or name-based fallback is permitted.
    pub fn clean_and_attach(
        &self,
        session: Session,
        expected: (i64, u64),
        attach: bool,
    ) -> Result<OpenReceipt> {
        let before = self.outside_identity_generations(&session)?;
        if before != [expected] {
            let found = if before.is_empty() {
                "none".to_owned()
            } else {
                before
                    .iter()
                    .map(|(pid, _)| pid.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            bail!(
                "CLEAN AND ATTACH REFUSED: exact ownership changed (expected PID {}, now {found}). No signal was sent.",
                expected.0
            )
        }
        process::terminate_generation(expected.0, expected.1, Duration::from_secs(8))
            .map_err(anyhow::Error::msg)
            .context("CLEAN AND ATTACH REFUSED")?;
        let remaining = self.outside_identity_generations(&session)?;
        if !remaining.is_empty() {
            bail!(
                "CLEAN AND ATTACH REFUSED: exact provider identity still has a live owner. Pika did not resume a second client."
            )
        }
        self.open_session(session, attach)
    }

    pub fn open_pending(&self, launch_token: &str, attach: bool) -> Result<OpenReceipt> {
        let pending = self
            .store
            .get_pending(launch_token)?
            .context("That launch is no longer pending. Refresh Pika and select it again.")?;
        if let Some((provider, session_id)) = self.store.get_launch_binding(launch_token)?
            && let Some(session) = self.store.get_session(provider, &session_id)?
        {
            return self.open_session(session, attach);
        }
        self.open_pending_terminal(pending, launch_token, attach)
    }

    fn open_pending_terminal(
        &self,
        pending: PendingLaunch,
        launch_token: &str,
        attach: bool,
    ) -> Result<OpenReceipt> {
        let pane_id = pending
            .tmux_pane
            .as_deref()
            .context("This conversation is still starting and has no terminal home yet.")?;
        let pane = self
            .tmux
            .get_pane(pane_id)?
            .context("The pending terminal home disappeared. Run `pika doctor --repair-stale`.")?;
        if pane.pika_launch_token.as_deref() != Some(launch_token)
            || pane
                .pika_provider
                .is_some_and(|provider| provider != pending.provider)
        {
            bail!("Pika refused a pending home whose exact launch identity changed")
        }
        let kind = if self.store.get_pending_exit(launch_token)?.is_some() {
            "STARTUP TERMINAL"
        } else {
            "ATTACHED STARTING"
        };
        let (code, receipt_delivery) = if attach {
            let receipt = pending_receipt(&pending, kind);
            let handoff = self
                .tmux
                .attach_exact_with_observed_receipt(&pane, &receipt, || {
                    open_history::record_pending(&self.store, &pending)
                })?;
            (handoff.exit_code, handoff.delivery)
        } else {
            (0, None)
        };
        Ok(OpenReceipt {
            target: OpenTarget::Pending(Box::new(pending)),
            kind,
            exit_code: code,
            receipt_delivery,
        })
    }

    fn resume_session(&self, mut session: Session, attach: bool) -> Result<OpenReceipt> {
        // History-only Claude records can legitimately omit cwd. Match the
        // pinned provider recovery contract by using this invocation's current
        // directory only when it is absent, never when a saved path is invalid.
        if session.cwd.is_none() {
            session.cwd = Some(std::env::current_dir()?.to_string_lossy().into_owned());
        }
        existing_cwd(session.cwd.as_deref())?;
        let token = Uuid::new_v4().to_string();
        let selected_event = session.last_event_at;
        let identity = session.provider_thread_id().to_owned();
        let tmux_name = Tmux::internal_name(session.provider, &session.session_id);
        let owner_pid = i64::from(std::process::id());
        let owner_start = process::process_start_time(owner_pid)
            .and_then(|value| i64::try_from(value).ok())
            .context("Pika cannot pin its own launch reservation process generation")?;
        if !self.store.reserve_resume(
            session.provider,
            &session.session_id,
            &token,
            owner_pid,
            owner_start,
        )? {
            bail!(
                "{} is already being resumed. Wait a moment, then run exactly: `pika {}`.",
                session.display_name(),
                shell_words::quote(&session.display_name())
            );
        }
        let pending = PendingLaunch {
            launch_token: token.clone(),
            provider: session.provider,
            name: session.display_name(),
            cwd: existing_cwd(session.cwd.as_deref())?.to_owned(),
            tmux_session: Some(tmux_name.clone()),
            tmux_pane: None,
            expected_session_id: Some(session.session_id.clone()),
            root_pid: None,
            root_pid_start: None,
            preexisting_session_ids: None,
            candidate_session_id: None,
            candidate_observed_at: None,
            created_at: now(),
        };
        if !self.store.add_pending(&pending)? {
            self.store
                .release_resume(session.provider, &session.session_id, &token)?;
            bail!(
                "{} is already starting. Wait a moment, then run exactly: `pika {}`.",
                session.display_name(),
                shell_words::quote(&session.display_name())
            )
        }
        let result = (|| {
            let observation = self.observe_processes();
            let processes =
                require_complete_processes(&observation, "start the exact provider process")?;
            let panes = self
                .tmux
                .list_panes()
                .context("Pika refused to resume because tmux could not be observed")?;
            let all_pane_pids = panes
                .iter()
                .flat_map(|pane| process::process_tree(pane.pane_pid, processes))
                .collect::<BTreeSet<_>>();
            let exact_pids =
                process::find_session_processes(&identity, session.provider, processes);
            let outside = exact_pids
                .iter()
                .filter(|pid| !all_pane_pids.contains(pid))
                .copied()
                .collect::<Vec<_>>();
            if !outside.is_empty() {
                bail!(
                    "{} started elsewhere while Pika was reserving its exact home (PID {}). No second provider was launched.",
                    session.display_name(),
                    outside
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }
            if !exact_pids.is_empty() {
                let binding = self.exact_pane_binding(&session, None)?;
                return self.attach_existing_reserved(
                    &session,
                    &binding,
                    selected_event,
                    &token,
                    attach,
                );
            }
            let providers = Providers::new(&self.paths, &self.config);
            let argv = providers.resume_argv(session.provider, &identity);
            let environment = self.register_launch_wrapper(
                session.provider,
                Some(&session.session_id),
                &token,
                &session.display_name(),
            )?;
            let cwd = existing_cwd(session.cwd.as_deref())?;
            // Never destructively reuse a pane that has been exposed outside
            // this launch. A fresh private holder makes `respawn-pane -k`
            // retire only Pika's own inert process; stale tagged panes remain
            // recoverable evidence until reconciliation clears them safely.
            let free_name = free_tmux_name(&tmux_name, &panes);
            let allocated = self.tmux.create_holding_session(&free_name, cwd)?;
            self.store.finalize_pending_pane(
                &token,
                &allocated.session_name,
                &allocated.pane_id,
                Some(allocated.pane_pid),
                process::process_start_time(allocated.pane_pid)
                    .and_then(|value| i64::try_from(value).ok()),
            )?;
            self.tmux.configure_exact_home(&allocated)?;
            let prepared = self.tmux.prepare_agent_pane(
                &allocated,
                session.provider,
                Some(&session.session_id),
                &session.display_name(),
                &token,
            )?;
            if !self
                .store
                .set_launch_phase(&token, crate::store::LaunchPhase::PanePrepared)?
            {
                bail!("the recoverable launch record disappeared before provider execution")
            }
            if !self
                .store
                .bind_launch(&token, session.provider, &session.session_id)?
            {
                bail!("the recoverable launch token was already bound to another conversation")
            }
            // Persist intent before the only command that can start a provider.
            if !self
                .store
                .set_launch_phase(&token, crate::store::LaunchPhase::ProviderStarting)?
            {
                bail!("the recoverable launch record disappeared before provider execution")
            }
            // A process could have appeared after the first snapshot. Recheck
            // the idle tree after tagging and immediately before respawn.
            let ready_observation = self.observe_processes();
            let ready_processes = require_complete_processes(
                &ready_observation,
                "execute the reserved provider launch",
            )?;
            let appeared =
                process::find_session_processes(&identity, session.provider, ready_processes);
            if !appeared.is_empty() {
                bail!(
                    "{} acquired another exact owner while Pika was preparing its home (PID {}). No second provider was launched.",
                    session.display_name(),
                    appeared
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }
            require_idle_pane(&prepared, ready_processes, true)?;
            let pane = self.tmux.start_prepared_agent(
                &prepared,
                cwd,
                session.provider,
                &argv,
                &environment,
                Some(&session.session_id),
                &session.display_name(),
                &token,
            )?;
            let binding = wait_for_exact_binding(self, &session, &pane.pane_id)?;
            let start_time = i64::try_from(binding.provider_start_time)
                .context("provider generation does not fit the state store")?;
            self.store
                .observe_launched_generation(&token, binding.provider_pid, start_time)?;
            session.tmux_session = Some(pane.session_name.clone());
            session.tmux_pane = Some(pane.pane_id.clone());
            session.root_pid = Some(binding.provider_pid);
            session.status = Status::Starting;
            session.live = true;
            session.home_state = "starting".into();
            session.last_activity_at = now();
            self.store.upsert_session(&session, true)?;
            if self.store.get_pending(&token)?.is_some()
                && !self.store.certify_launch(
                    &token,
                    session.provider,
                    &session.session_id,
                    binding.provider_pid,
                    start_time,
                )?
            {
                bail!("the launched provider generation could not be certified")
            }
            let (code, receipt_delivery) = if attach {
                let receipt = continuity_receipt(&session, "RESUMED EXACT");
                let handoff = self.tmux.attach_exact_with_observed_receipt(
                    &binding.pane,
                    &receipt,
                    || self.record_exact_handoff(&session, &binding, selected_event),
                )?;
                (handoff.exit_code, handoff.delivery)
            } else {
                (0, None)
            };
            Ok(OpenReceipt {
                target: OpenTarget::Session(Box::new(session.clone())),
                kind: "RESUMED EXACT",
                exit_code: code,
                receipt_delivery,
            })
        })();
        self.store
            .release_resume(session.provider, &session.session_id, &token)?;
        result
    }

    pub fn new_session(&self, name: &str, provider: Provider, attach: bool) -> Result<OpenReceipt> {
        validate_daily_name(name)?;
        let observation = self.observe_processes();
        require_complete_processes(&observation, "start a new conversation")?;
        let existing_panes = self
            .tmux
            .list_panes()
            .context("Pika refused to start a conversation because tmux could not be observed")?;
        if !self.resolve_local(name)?.is_empty() {
            bail!(
                "{name:?} already names a saved conversation; run exactly: `pika {}`",
                shell_words::quote(name)
            );
        }
        let token = Uuid::new_v4().to_string();
        let reserved = (provider == Provider::Claude).then(|| Uuid::new_v4().to_string());
        let providers = Providers::new(&self.paths, &self.config);
        let argv = providers.new_argv(provider, name, reserved.as_deref());
        let cwd = std::env::current_dir()?.to_string_lossy().into_owned();
        let internal = free_tmux_name(
            &Tmux::internal_name(provider, reserved.as_deref().unwrap_or(&token)),
            &existing_panes,
        );
        let pending = PendingLaunch {
            launch_token: token.clone(),
            provider,
            name: name.into(),
            cwd: cwd.clone(),
            tmux_session: Some(internal.clone()),
            tmux_pane: None,
            expected_session_id: reserved.clone(),
            root_pid: None,
            root_pid_start: None,
            preexisting_session_ids: Some(
                providers
                    .browse(provider)
                    .into_iter()
                    .map(|candidate| candidate.session_id)
                    .collect(),
            ),
            candidate_session_id: None,
            candidate_observed_at: None,
            created_at: now(),
        };
        if !self.store.add_pending(&pending)? {
            bail!(
                "{name:?} is already starting; run exactly: `pika {}`",
                shell_words::quote(name)
            );
        }
        // Failures deliberately retain the phase-stamped pending record. It is
        // the recovery handle if tmux accepted provider execution before a
        // later readback/store operation failed.
        (|| {
            let pane = self.start_new_provider(&pending, &internal, &argv)?;
            let mut exact_new_home = None;
            if let Some(session_id) = reserved.as_deref() {
                let mut provisional = Session {
                    provider,
                    session_id: session_id.to_owned(),
                    name: Some(name.to_owned()),
                    cwd: Some(cwd.clone()),
                    branch: None,
                    transcript_path: None,
                    tmux_session: Some(pane.session_name.clone()),
                    tmux_pane: Some(pane.pane_id.clone()),
                    root_pid: None,
                    status: Status::Starting,
                    unread: false,
                    model: None,
                    source: "pending-launch".into(),
                    managed: true,
                    error: None,
                    attention_reason: None,
                    created_at: pending.created_at,
                    updated_at: pending.created_at,
                    last_event_at: pending.created_at,
                    last_activity_at: pending.created_at,
                    live: true,
                    attached: false,
                    home_state: "starting".into(),
                    cpu_percent: None,
                    rss_kb: None,
                    input_tokens: None,
                    output_tokens: None,
                    cached_input_tokens: None,
                    cache_write_tokens: None,
                    total_tokens: None,
                    estimated_cost_usd: None,
                    active_thread_id: None,
                };
                if let Ok(binding) = wait_for_exact_binding(self, &provisional, &pane.pane_id) {
                    let start_time = i64::try_from(binding.provider_start_time)
                        .context("provider generation does not fit the state store")?;
                    provisional.root_pid = Some(binding.provider_pid);
                    self.store.observe_launched_generation(
                        &token,
                        binding.provider_pid,
                        start_time,
                    )?;
                    self.store.upsert_session(&provisional, true)?;
                    let _ = self.store.certify_launch(
                        &token,
                        provider,
                        session_id,
                        binding.provider_pid,
                        start_time,
                    )?;
                    exact_new_home = Some((provisional, binding));
                }
            }
            let current = self
                .store
                .get_pending(&token)?
                .unwrap_or_else(|| PendingLaunch {
                    tmux_session: Some(pane.session_name.clone()),
                    tmux_pane: Some(pane.pane_id.clone()),
                    root_pid: Some(pane.pane_pid),
                    root_pid_start: process::process_start_time(pane.pane_pid)
                        .and_then(|value| i64::try_from(value).ok()),
                    ..pending.clone()
                });
            let (code, receipt_delivery) = if attach {
                if let Some((session, binding)) = exact_new_home.as_ref() {
                    let receipt = continuity_receipt(session, "NEW HOME");
                    let handoff = self.tmux.attach_exact_with_observed_receipt(
                        &binding.pane,
                        &receipt,
                        || self.record_exact_handoff(session, binding, session.last_event_at),
                    )?;
                    (handoff.exit_code, handoff.delivery)
                } else {
                    let receipt = pending_receipt(&current, "NEW HOME");
                    let handoff =
                        self.tmux
                            .attach_exact_with_observed_receipt(&pane, &receipt, || {
                                open_history::record_pending(&self.store, &current)
                            })?;
                    (handoff.exit_code, handoff.delivery)
                }
            } else {
                (0, None)
            };
            Ok(OpenReceipt {
                target: OpenTarget::Pending(Box::new(current)),
                kind: "NEW HOME",
                exit_code: code,
                receipt_delivery,
            })
        })()
    }

    fn register_launch_wrapper(
        &self,
        provider: Provider,
        session_id: Option<&str>,
        token: &str,
        name: &str,
    ) -> Result<BTreeMap<String, String>> {
        let environment = launch_environment(provider, session_id, token, name);
        if !self
            .store
            .register_pending_wrapper_owner(token, &environment["PIKA_OWNER_TOKEN"])?
        {
            bail!("the launch reservation changed before its wrapper could be registered")
        }
        Ok(environment)
    }

    fn attach_existing_reserved(
        &self,
        session: &Session,
        binding: &ExactPaneBinding,
        selected_event: f64,
        token: &str,
        attach: bool,
    ) -> Result<OpenReceipt> {
        self.store.delete_pending(token)?;
        let (code, receipt_delivery) = if attach {
            let receipt = continuity_receipt(session, "ATTACHED LIVE");
            let handoff =
                self.tmux
                    .attach_exact_with_observed_receipt(&binding.pane, &receipt, || {
                        self.record_exact_handoff(session, binding, selected_event)
                    })?;
            (handoff.exit_code, handoff.delivery)
        } else {
            (0, None)
        };
        Ok(OpenReceipt {
            target: OpenTarget::Session(Box::new(session.clone())),
            kind: "ATTACHED LIVE",
            exit_code: code,
            receipt_delivery,
        })
    }

    fn set_required_launch_phase(
        &self,
        token: &str,
        phase: crate::store::LaunchPhase,
    ) -> Result<()> {
        if !self.store.set_launch_phase(token, phase)? {
            bail!("the recoverable launch record disappeared before provider execution")
        }
        Ok(())
    }

    fn start_new_provider(
        &self,
        pending: &PendingLaunch,
        internal: &str,
        argv: &[String],
    ) -> Result<Pane> {
        let provider = pending.provider;
        let reserved = pending.expected_session_id.as_deref();
        let token = pending.launch_token.as_str();
        let name = pending.name.as_str();
        let cwd = pending.cwd.as_str();
        let environment = self.register_launch_wrapper(provider, reserved, token, name)?;
        let allocated = self.tmux.create_holding_session(internal, cwd)?;
        self.store.finalize_pending_pane(
            token,
            &allocated.session_name,
            &allocated.pane_id,
            Some(allocated.pane_pid),
            process::process_start_time(allocated.pane_pid)
                .and_then(|value| i64::try_from(value).ok()),
        )?;
        self.tmux.configure_exact_home(&allocated)?;
        let prepared = self
            .tmux
            .prepare_agent_pane(&allocated, provider, reserved, name, token)?;
        self.set_required_launch_phase(token, crate::store::LaunchPhase::PanePrepared)?;
        if let Some(session_id) = reserved
            && !self.store.bind_launch(token, provider, session_id)?
        {
            bail!("the recoverable launch token was already bound to another conversation")
        }
        self.set_required_launch_phase(token, crate::store::LaunchPhase::ProviderStarting)?;
        let observation = self.observe_processes();
        let processes =
            require_complete_processes(&observation, "execute the reserved provider launch")?;
        ensure_new_identity_unowned(reserved, provider, processes)?;
        require_idle_pane(&prepared, processes, true)?;
        let pane = self.tmux.start_prepared_agent(
            &prepared,
            cwd,
            provider,
            argv,
            &environment,
            reserved,
            name,
            token,
        )?;
        Ok(pane)
    }
}

fn recover_confirmed_pending(
    session: &Session,
    panes: &[Pane],
    processes: &BTreeMap<i64, ProcessRecord>,
    ledger: &crate::store::ReconcileLedger<'_>,
) -> Result<bool> {
    if !session.has_exact_home() {
        return Ok(false);
    }
    let Some(pane) = panes.iter().find(|pane| {
        session.tmux_pane.as_deref() == Some(pane.pane_id.as_str())
            && session.tmux_session.as_deref() == Some(pane.session_name.as_str())
            && !pane.dead
            && pane.pika_provider == Some(session.provider)
            && pane.pika_session_id.as_deref() == Some(session.session_id.as_str())
    }) else {
        return Ok(false);
    };
    let Some(token) = pane.pika_launch_token.as_deref() else {
        return Ok(false);
    };
    let Some(owner) = session.root_pid.and_then(|pid| processes.get(&pid)) else {
        return Ok(false);
    };
    let Ok(start) = i64::try_from(owner.start_time) else {
        return Ok(false);
    };
    ledger.recover_confirmed_launch(token, session, owner.pid, start)
}

pub fn session_from_pending(pending: &PendingLaunch) -> Session {
    // Age can prove that startup is overdue, never that a provider has stopped
    // or that a conversation identity is known. Keep the recovery row intact.
    let overdue = now() - pending.created_at > 120.0;
    Session {
        provider: pending.provider,
        session_id: pending
            .expected_session_id
            .clone()
            .unwrap_or_else(|| format!("pending:{}", pending.launch_token)),
        name: Some(pending.name.clone()),
        cwd: Some(pending.cwd.clone()),
        branch: None,
        transcript_path: None,
        tmux_session: pending.tmux_session.clone(),
        tmux_pane: pending.tmux_pane.clone(),
        root_pid: pending.root_pid,
        status: if overdue { Status::Error } else { Status::Starting },
        unread: false,
        model: None,
        source: "pending-launch".into(),
        managed: true,
        error: overdue.then(|| "Pika has not confirmed this launch. Enter checks its existing terminal; X hides this launch entry without stopping the agent.".into()),
        attention_reason: Some(if overdue { "launch not confirmed" } else { "starting provider conversation" }.into()),
        created_at: pending.created_at,
        updated_at: pending.created_at,
        last_event_at: pending.created_at,
        last_activity_at: pending.created_at,
        live: pending.tmux_pane.is_some(),
        attached: false,
        home_state: "starting".into(),
        cpu_percent: None,
        rss_kb: None,
        input_tokens: None,
        output_tokens: None,
        cached_input_tokens: None,
        cache_write_tokens: None,
        total_tokens: None,
        estimated_cost_usd: None,
        active_thread_id: None,
    }
}

fn continuation_candidate<'a>(
    session: &Session,
    candidates: &'a BTreeMap<(Provider, String), Candidate>,
    panes: &[Pane],
    processes: &BTreeMap<i64, ProcessRecord>,
    timestamp: f64,
) -> (Option<&'a Candidate>, Vec<String>) {
    let current_id = session.provider_thread_id();
    let current = candidates
        .get(&(session.provider, current_id.to_owned()))
        .or_else(|| candidates.get(&(session.provider, session.session_id.clone())));
    if session.provider != Provider::Codex {
        return (current, Vec::new());
    }
    let pane = panes.iter().find(|pane| {
        (pane.pika_provider == Some(session.provider)
            && pane.pika_session_id.as_deref() == Some(&session.session_id))
            || (session.tmux_pane.as_deref() == Some(&pane.pane_id)
                && pane.pika_provider.is_none()
                && pane.pika_session_id.is_none())
    });
    if pane.is_none_or(|pane| {
        process::provider_process(pane.pane_pid, Some(Provider::Codex), processes).is_none()
    }) {
        return (current, Vec::new());
    }

    let cutoff = timestamp - CONTINUATION_FRESH_SECONDS;
    let same_identity = |candidate: &Candidate| {
        let same_name = session
            .name
            .as_deref()
            .zip(candidate.name.as_deref())
            .is_some_and(|(left, right)| left.eq_ignore_ascii_case(right));
        if !same_name {
            return false;
        }
        match (session.cwd.as_deref(), candidate.cwd.as_deref()) {
            (Some(left), Some(right)) => canonical_directory(Some(left))
                .zip(canonical_directory(Some(right)))
                .map_or_else(|| left == right, |(left, right)| left == right),
            _ => true,
        }
    };
    let mut working = BTreeMap::<String, &Candidate>::new();
    if let Some(candidate) = current.filter(|candidate| {
        candidate.lifecycle_status == Some(Status::Working) && candidate.updated_at >= cutoff
    }) {
        working.insert(candidate.session_id.clone(), candidate);
    }
    for candidate in candidates.values().filter(|candidate| {
        candidate.provider == Provider::Codex
            && candidate
                .parent_session_id
                .as_deref()
                .is_some_and(|parent| parent == session.session_id || parent == current_id)
            && candidate.lifecycle_status == Some(Status::Working)
            && candidate.updated_at >= cutoff
            && same_identity(candidate)
    }) {
        working.insert(candidate.session_id.clone(), candidate);
    }
    if working.len() > 1 {
        let mut conflicts = working.into_values().collect::<Vec<_>>();
        conflicts.sort_by(|left, right| right.updated_at.total_cmp(&left.updated_at));
        return (
            current,
            conflicts
                .into_iter()
                .map(|candidate| candidate.session_id.clone())
                .collect(),
        );
    }
    (working.into_values().next().or(current), Vec::new())
}

fn merge_session_candidate(session: &mut Session, candidate: &Candidate) {
    // Provider-native user names are authoritative. This is how a rename made
    // inside Codex/Claude reaches an already watched Pika row.
    session.name = candidate.name.clone().or_else(|| session.name.clone());
    session.cwd = candidate.cwd.clone().or_else(|| session.cwd.clone());
    session.branch = candidate.branch.clone().or_else(|| session.branch.clone());
    session.transcript_path = candidate
        .transcript_path
        .clone()
        .or_else(|| session.transcript_path.clone());
    session.model = candidate.model.clone().or_else(|| session.model.clone());
    session.last_activity_at = session.last_activity_at.max(candidate.updated_at);
    session.live |= candidate.live;
}

pub fn session_from_candidate(candidate: &Candidate) -> Session {
    Session {
        provider: candidate.provider,
        session_id: candidate.session_id.clone(),
        name: candidate.name.clone(),
        cwd: candidate.cwd.clone(),
        branch: candidate.branch.clone(),
        transcript_path: candidate.transcript_path.clone(),
        tmux_session: None,
        tmux_pane: None,
        root_pid: candidate.pid,
        status: candidate.lifecycle_status.unwrap_or(if candidate.live {
            Status::Ready
        } else {
            Status::Parked
        }),
        unread: false,
        model: candidate.model.clone(),
        source: candidate.source.clone(),
        managed: true,
        error: None,
        attention_reason: None,
        created_at: candidate.created_at,
        updated_at: candidate.updated_at,
        last_event_at: candidate.updated_at,
        last_activity_at: candidate.updated_at,
        live: candidate.live,
        attached: false,
        home_state: if candidate.live { "outside" } else { "missing" }.into(),
        cpu_percent: None,
        rss_kb: None,
        input_tokens: None,
        output_tokens: None,
        cached_input_tokens: None,
        cache_write_tokens: None,
        total_tokens: None,
        estimated_cost_usd: None,
        active_thread_id: None,
    }
}

fn identity_strings(session: &Session) -> Vec<&str> {
    let mut identities = vec![session.session_id.as_str()];
    if let Some(active) = session
        .active_thread_id
        .as_deref()
        .filter(|active| *active != session.session_id)
    {
        identities.push(active);
    }
    identities
}

fn require_complete_processes<'a>(
    observation: &'a ProcessObservation,
    operation: &str,
) -> Result<&'a BTreeMap<i64, ProcessRecord>> {
    observation
        .require_complete(operation)
        .map_err(anyhow::Error::msg)
}

fn same_pane_generation(left: &Pane, right: &Pane) -> bool {
    left.pane_id == right.pane_id
        && left.session_name == right.session_name
        && left.pane_pid == right.pane_pid
        && left.created == right.created
}

fn same_exact_binding(left: &ExactPaneBinding, right: &ExactPaneBinding) -> bool {
    same_pane_generation(&left.pane, &right.pane)
        && left.pane_start_time == right.pane_start_time
        && left.provider_pid == right.provider_pid
        && left.provider_start_time == right.provider_start_time
}

fn same_unverified_binding(left: &UnverifiedPaneBinding, right: &UnverifiedPaneBinding) -> bool {
    same_pane_generation(&left.pane, &right.pane)
        && left.pane.pika_provider == right.pane.pika_provider
        && left.pane.pika_session_id == right.pane.pika_session_id
        && left.pane.pika_name == right.pane.pika_name
        && left.pane.pika_launch_token == right.pane.pika_launch_token
        && left.pane_start_time == right.pane_start_time
        && left.provider_pid == right.provider_pid
        && left.provider_start_time == right.provider_start_time
}

fn same_handoff_binding(expected: &ExactPaneBinding, current: &ExactPaneBinding) -> bool {
    // Startup may first expose the Node/Bun launcher, then its native provider.
    // Accept only that one-way transition from the still-live, same-generation
    // launcher. A native client replacement, reused PID, new launch or second
    // client is not continuity. Exact binding has already revalidated uniqueness,
    // UUID evidence and every live ancestry edge before reaching this check.
    same_pane_generation(&expected.pane, &current.pane)
        && expected.pane_start_time == current.pane_start_time
        && expected.pane.pika_provider == current.pane.pika_provider
        && expected.pane.pika_session_id == current.pane.pika_session_id
        && expected.pane.pika_launch_token == current.pane.pika_launch_token
        && (same_exact_binding(expected, current)
            || current.provider_launcher
                == Some(process::ProcessGeneration {
                    pid: expected.provider_pid,
                    start_time: expected.provider_start_time,
                }))
}

fn require_idle_pane(
    pane: &Pane,
    processes: &BTreeMap<i64, ProcessRecord>,
    allow_launch_holder: bool,
) -> Result<()> {
    if pane.dead {
        bail!("Pika will not reuse a dead pane without a live generation to bind")
    }
    let shell = Path::new(&pane.current_command)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(&pane.current_command)
        .to_ascii_lowercase();
    let shell_idle = matches!(
        shell.as_str(),
        "bash" | "dash" | "fish" | "ksh" | "sh" | "tcsh" | "zsh"
    );
    let launch_holder = allow_launch_holder && shell == "sleep";
    if !shell_idle && !launch_holder {
        bail!(
            "the saved Pika pane is running {:?}; Pika refused to replace it",
            pane.current_command
        )
    }
    let tree = process::process_tree(pane.pane_pid, processes);
    if tree != [pane.pane_pid] {
        bail!("the saved Pika pane acquired another process; Pika refused to replace it")
    }
    Ok(())
}

fn free_tmux_name(base: &str, panes: &[Pane]) -> String {
    let names = panes
        .iter()
        .map(|pane| pane.session_name.as_str())
        .collect::<BTreeSet<_>>();
    if !names.contains(base) {
        return base.to_owned();
    }
    for suffix in 2_u32.. {
        let candidate = format!("{base}-{suffix}");
        if !names.contains(candidate.as_str()) {
            return candidate;
        }
    }
    unreachable!("u32 tmux-name suffix space exhausted")
}

fn wait_for_exact_binding(pika: &Pika, session: &Session, pane: &str) -> Result<ExactPaneBinding> {
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        match pika.exact_pane_binding(session, Some(pane)) {
            Ok(binding) => return Ok(binding),
            Err(error) if std::time::Instant::now() < deadline => {
                let _ = error;
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => {
                return Err(error).context(
                    "provider execution was accepted, but exact UUID ownership was not established; the recoverable pending launch was retained",
                );
            }
        }
    }
}

/// Shared owner evidence for reconciliation and exact actions. Recovery is a
/// provider-confirmed immutable launch binding plus a still-current process
/// generation, not a pane label or a transferable token by itself.
fn identity_owners(
    session: &Session,
    processes: &BTreeMap<i64, ProcessRecord>,
    ledger: &ReconcileLedger<'_>,
    timestamp: f64,
) -> Result<IdentityOwners> {
    let mut owners = IdentityOwners::default();
    for identity in identity_strings(session) {
        owners.direct.extend(process::find_session_processes(
            identity,
            session.provider,
            processes,
        ));
    }
    if let Some(owner) = ledger.get_recovery_owner(session.provider, &session.session_id)? {
        let valid = processes.get(&owner.pid).is_some_and(|record| {
            u64::try_from(owner.start_time).ok() == Some(record.start_time)
                && record.provider() == Some(session.provider)
        }) && ledger.get_launch_binding(&owner.launch_token)?
            == Some((session.provider, session.session_id.clone()));
        if valid {
            owners.recovery = Some(owner);
        } else {
            ledger.delete_recovery_owner(session.provider, &session.session_id)?;
        }
    }
    for owner in ledger.live_owners(session.provider, &session.session_id)? {
        let valid_generation = processes.get(&owner.pid).is_some_and(|record| {
            record.provider() == Some(session.provider)
                && owner
                    .start_time
                    .is_none_or(|start| u64::try_from(start).ok() == Some(record.start_time))
        });
        let shared = processes
            .get(&owner.pid)
            .is_some_and(|record| process::shared_provider_process(record, session.provider));
        if !valid_generation || (shared && timestamp - owner.last_seen > LIVE_OWNER_LEASE_SECONDS) {
            ledger.delete_live_owner(
                owner.provider,
                &owner.session_id,
                owner.pid,
                &owner.owner_token,
            )?;
        } else if !(session.provider == Provider::Codex
            && (!owners.direct.is_empty() || owners.recovery.is_some())
            && shared)
        {
            owners.leases.insert(owner.pid);
        }
    }
    // Direct argv detection already folds runtime launchers into their native
    // children. A hook lease or recovery certificate must not reintroduce the
    // same launcher as a second client when the evidence sets are combined.
    let canonical: BTreeSet<_> = process::canonical_identity_pids(&owners.pids(), processes)
        .into_iter()
        .collect();
    owners.direct.retain(|pid| canonical.contains(pid));
    owners.leases.retain(|pid| canonical.contains(pid));
    if owners
        .recovery
        .as_ref()
        .is_some_and(|owner| !canonical.contains(&owner.pid))
    {
        owners.recovery = None;
    }
    Ok(owners)
}

fn canonical_directory(value: Option<&str>) -> Option<String> {
    let path = Path::new(value?);
    (path.is_absolute() && path.is_dir())
        .then(|| path.canonicalize().ok())
        .flatten()
        .map(|path| path.to_string_lossy().into_owned())
}

fn existing_cwd(value: Option<&str>) -> Result<&str> {
    value
        .filter(|path| Path::new(path).is_dir())
        .context("the conversation's saved working directory no longer exists")
}

fn receipt_text(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .take(120)
        .collect::<String>()
        .trim()
        .to_owned()
}

fn fingerprint(value: &str) -> String {
    value.chars().take(8).collect()
}

fn continuity_receipt(session: &Session, kind: &str) -> String {
    let meaning = match kind {
        "ATTACHED LIVE" => "same live home",
        "RESUMED EXACT" => "same conversation resumed",
        "NEW HOME" => "same conversation · protected new home",
        _ => "exact conversation",
    };
    let provider_id = session.provider_thread_id();
    let mut parts = vec![
        "CONTINUITY PROVEN".to_owned(),
        receipt_text(&session.display_name()),
        kind.to_owned(),
        meaning.to_owned(),
        session.provider.to_string(),
        format!("exact id {}", fingerprint(provider_id)),
    ];
    if provider_id != session.session_id {
        parts.push(format!("home {}", fingerprint(&session.session_id)));
    }
    parts.join(" · ")
}

fn pending_receipt(pending: &PendingLaunch, kind: &str) -> String {
    let mut parts = vec![
        if kind == "STARTUP TERMINAL" {
            "STARTUP EXITED · existing terminal only"
        } else {
            "PIKA HOME READY"
        }
        .to_owned(),
        receipt_text(&pending.name),
        kind.to_owned(),
        "identity pending".to_owned(),
        pending.provider.to_string(),
    ];
    if let Some(expected) = pending.expected_session_id.as_deref() {
        parts.push(format!("expected id {}", fingerprint(expected)));
    } else {
        parts.push(format!("launch {}", fingerprint(&pending.launch_token)));
    }
    parts.join(" · ")
}

fn split_provider_query(query: &str) -> (Option<Provider>, &str) {
    let Some((prefix, value)) = query.split_once(':') else {
        return (None, query);
    };
    match prefix.parse() {
        Ok(provider) => (Some(provider), value),
        Err(_) => (None, query),
    }
}

fn ambiguity_message(query: &str, sessions: &[Session]) -> String {
    let mut message = format!("{query:?} matches more than one exact conversation:\n");
    for (index, session) in sessions.iter().enumerate() {
        message.push_str(&format!(
            "  {}) {} {} · {} · {}\n",
            index + 1,
            session.provider,
            &session.session_id[..session.session_id.len().min(8)],
            session.cwd.as_deref().unwrap_or("unknown path"),
            session.status
        ));
    }
    message.push_str("Run exactly: `pika PROVIDER:NAME`, or choose the conversation in `pika`.");
    message
}

fn validate_daily_name(name: &str) -> Result<()> {
    let trimmed = name.trim();
    if trimmed.is_empty() || trimmed.len() > 120 || trimmed.chars().any(char::is_control) {
        bail!("conversation names must contain 1-120 printable characters");
    }
    if trimmed != name {
        bail!("conversation names cannot start or end with whitespace");
    }
    Ok(())
}

fn launch_environment(
    provider: Provider,
    session_id: Option<&str>,
    token: &str,
    name: &str,
) -> BTreeMap<String, String> {
    let mut values = BTreeMap::from([
        ("PIKA_PROVIDER".into(), provider.to_string()),
        ("PIKA_LAUNCH_TOKEN".into(), token.into()),
        ("PIKA_OWNER_TOKEN".into(), Uuid::new_v4().to_string()),
        ("PIKA_NAME".into(), name.into()),
    ]);
    if let Some(session_id) = session_id {
        values.insert("PIKA_SESSION_ID".into(), session_id.into());
    }
    values.extend(crate::terminal::palette_environment());
    values
}

fn ensure_new_identity_unowned(
    reserved: Option<&str>,
    provider: Provider,
    processes: &BTreeMap<i64, ProcessRecord>,
) -> Result<()> {
    if let Some(session_id) = reserved
        && !process::find_session_processes(session_id, provider, processes).is_empty()
    {
        bail!(
            "the reserved conversation UUID acquired another owner before provider execution. No second provider was launched."
        )
    }
    Ok(())
}

fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        hooks::{HookContext, handle_hook, parse_hook_payload},
        store::LiveOwner,
    };
    #[cfg(unix)]
    use crate::{model::ExpertProfile, store::StoredExpertProfile};
    use std::sync::{Arc, Barrier};
    fn test_pika() -> (tempfile::TempDir, Pika) {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: root.path().join("config"),
            state_dir: root.path().join("state"),
            config: root.path().join("config/config.json"),
            database: root.path().join("state/pika.db"),
            codex_home: root.path().join("codex"),
            claude_home: root.path().join("claude"),
            opencode_data_home: root.path().join("opencode-data"),
            opencode_config_home: root.path().join("opencode-config"),
            muse_data_home: root.path().join("muse-data"),
            muse_config_home: root.path().join("muse-config"),
        };
        let store = Store::from_paths(&paths);
        store.initialize().unwrap();
        let pika = Pika::with_components(
            paths,
            Config::default(),
            store,
            Tmux::with_executable("/usr/bin/true", Some("never-used".into())),
        );
        (root, pika)
    }

    fn test_session(identity: &str) -> Session {
        session_from_candidate(&Candidate {
            provider: Provider::Codex,
            session_id: identity.to_owned(),
            name: Some("portfolio_review".into()),
            cwd: Some("/tmp".into()),
            branch: None,
            transcript_path: None,
            model: None,
            updated_at: 1.0,
            live: true,
            pid: Some(2),
            source: "test".into(),
            parent_session_id: None,
            created_at: 1.0,
            lifecycle_status: Some(Status::Working),
        })
    }

    fn pending_fixture(token: &str, provider: Provider) -> PendingLaunch {
        PendingLaunch {
            launch_token: token.into(),
            provider,
            name: "update_test".into(),
            cwd: "/tmp".into(),
            tmux_session: Some("pika-startup".into()),
            tmux_pane: Some("%1".into()),
            expected_session_id: None,
            root_pid: None,
            root_pid_start: None,
            preexisting_session_ids: Some(vec![]),
            candidate_session_id: None,
            candidate_observed_at: None,
            created_at: now(),
        }
    }

    #[cfg(unix)]
    #[test]
    fn daily_name_reopens_pending_home_without_spawning_or_requiring_create() {
        let (root, mut pika) = test_pika();
        let pending = pending_fixture("update-launch", Provider::Codex);
        let mut pane = tagged_pane("unused");
        pane.session_name = "pika-startup".into();
        pane.pika_session_id = None;
        pane.pika_launch_token = Some(pending.launch_token.clone());
        pika.tmux = fixture_tmux(root.path(), &[pane]);
        pika.process_observer = Arc::new(|| ProcessObservation::complete(BTreeMap::new()));
        pika.store.add_pending(&pending).unwrap();
        pika.store
            .hide_pending(&pending.launch_token, pending.provider, pending.created_at)
            .unwrap();
        for query in ["update_test", "UPDATE_TEST", "codex:update_test"] {
            let receipt = pika.open_name(query, false, false).unwrap();
            assert_eq!(receipt.kind, "ATTACHED STARTING");
            let OpenTarget::Pending(opened) = receipt.target else {
                panic!("fabricated session")
            };
            assert_eq!(opened.launch_token, pending.launch_token);
        }
        assert!(pika.store.list_sessions().unwrap().is_empty());
        assert_eq!(pika.store.list_pending().unwrap(), vec![pending]);
    }

    #[test]
    fn pending_daily_name_requires_exact_name_and_provider_disambiguation() {
        let (_root, mut pika) = test_pika();
        pika.process_observer = Arc::new(|| ProcessObservation::complete(BTreeMap::new()));
        for (token, provider) in [("one", Provider::Codex), ("two", Provider::Claude)] {
            pika.store
                .add_pending(&pending_fixture(token, provider))
                .unwrap();
        }
        assert_eq!(pika.resolve_pending("update_test").unwrap().len(), 2);
        assert_eq!(
            pika.resolve_pending("claude:update_test").unwrap()[0].launch_token,
            "two"
        );
        assert!(pika.resolve_pending("update").unwrap().is_empty());
        assert!(pika.resolve_pending("muse:update_test").unwrap().is_empty());
        let error = pika.open_name("update_test", false, true).unwrap_err();
        assert!(
            error.to_string().contains("several pending launches"),
            "{error:#}"
        );
        assert_eq!(pika.store.list_pending().unwrap().len(), 2);
        assert!(pika.store.list_sessions().unwrap().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn pending_retry_refuses_a_reused_terminal_token() {
        let (root, mut pika) = test_pika();
        let pending = pending_fixture("old-launch", Provider::Codex);
        let mut pane = tagged_pane("unused");
        pane.pika_launch_token = Some("replacement-launch".into());
        pika.tmux = fixture_tmux(root.path(), &[pane]);
        pika.process_observer = Arc::new(|| ProcessObservation::complete(BTreeMap::new()));
        pika.store.add_pending(&pending).unwrap();
        let error = pika.open_name("update_test", false, true).unwrap_err();
        assert!(error.to_string().contains("exact launch identity changed"));
        assert_eq!(pika.store.list_pending().unwrap(), vec![pending]);
        assert!(pika.store.list_sessions().unwrap().is_empty());
    }

    #[test]
    fn unconfirmed_labels_stay_in_recent_history_until_selected() {
        let (_root, pika) = test_pika();
        let mut named = test_session("named");
        named.managed = false;
        named.source = "external".into();
        pika.store.upsert_session(&named, false).unwrap();
        let mut unnamed = named.clone();
        unnamed.session_id = "unnamed".into();
        unnamed.name = None;
        pika.store.upsert_session(&unnamed, false).unwrap();
        assert!(pika.store.list_sessions().unwrap().is_empty());
        let choices = pika.import_named().unwrap();
        assert!(choices.is_empty());
        let recent = pika.import_recent_unnamed(20).unwrap();
        assert_eq!(recent.len(), 2);
        let named = recent.iter().find(|row| row.session_id == "named").unwrap();
        pika.adopt_candidate(named).unwrap();
        assert_eq!(pika.store.list_sessions().unwrap().len(), 1);
        assert!(pika.import_named().unwrap().is_empty());
        assert_eq!(pika.import_recent_unnamed(20).unwrap().len(), 1);
        assert!(pika.store.list_untracked_sessions().unwrap().is_empty());
    }

    #[test]
    fn explicit_open_confirms_unconfirmed_identity_even_if_launch_is_blocked() {
        let (_root, pika) = test_pika();
        let mut external = test_session("bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb");
        external.managed = false;
        external.source = "external".into();
        pika.store.upsert_session(&external, false).unwrap();
        let choices = pika.resolve_local("portfolio_review").unwrap();
        assert_eq!(choices.len(), 1);
        assert!(
            pika.store.list_sessions().unwrap().is_empty(),
            "lookup is read-only"
        );
        // The fixture tmux cannot create a pane. Choice is durable even when
        // the later launch fails; there is no automatic launch retry.
        assert!(pika.open_session(choices[0].clone(), false).is_err());
        assert!(
            pika.store
                .is_watched(external.provider, &external.session_id)
                .unwrap()
        );
        assert!(pika.store.list_unconfirmed_sessions().unwrap().is_empty());
    }

    #[test]
    fn foreground_reobserves_after_another_connection_commits() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::at(temp.path().join("state.db"));
        store.initialize().unwrap();
        let mut scans = 0;
        fresh_observation(|| {
            scans += 1;
            let mut observed = store.begin_reconcile_session()?;
            if scans == 1 {
                store.ensure_local_node_id()?;
            }
            observed.transaction(|_| Ok(()))
        })
        .unwrap();
        assert_eq!(scans, 2, "a new scan and snapshot fence are required");
    }

    #[test]
    fn foreground_retry_is_bounded_and_never_retries_identity_or_closed_board_fences() {
        let mut scans = 0;
        let result: Result<()> = fresh_observation(|| {
            scans += 1;
            Err(crate::store::ReconcileSuperseded.into())
        });
        assert_eq!(scans, 3);
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("retry the same command")
        );
        for reason in [
            "OPEN TWICE",
            "local reconciliation was superseded before it could commit",
        ] {
            let mut scans = 0;
            let _: Result<()> = fresh_observation(|| {
                scans += 1;
                bail!("{reason}")
            });
            assert_eq!(scans, 1);
        }
    }

    #[test]
    fn continuity_receipt_names_kind_provider_and_both_distinct_identities() {
        let mut session = test_session("11111111-1111-4111-8111-111111111111");
        session.active_thread_id = Some("22222222-2222-4222-8222-222222222222".into());
        assert_eq!(
            continuity_receipt(&session, "RESUMED EXACT"),
            "CONTINUITY PROVEN · portfolio_review · RESUMED EXACT · same conversation resumed · codex · exact id 22222222 · home 11111111"
        );
        session.active_thread_id = None;
        let same = continuity_receipt(&session, "ATTACHED LIVE");
        assert!(same.contains("ATTACHED LIVE · same live home · codex · exact id 11111111"));
        assert!(!same.contains(" · home "));
    }

    fn record(pid: i64, parent_pid: Option<i64>, start_time: u64, argv: &[&str]) -> ProcessRecord {
        ProcessRecord {
            pid,
            parent_pid,
            start_time,
            argv: argv.iter().map(|value| (*value).to_owned()).collect(),
        }
    }

    fn tagged_pane(identity: &str) -> Pane {
        Pane {
            session_name: "pika-c-masterhf".into(),
            pane_id: "%1".into(),
            pane_pid: 1,
            cwd: "/tmp".into(),
            current_command: "sh".into(),
            attached: false,
            dead: false,
            dead_status: None,
            activity: 1.0,
            created: 1.0,
            pika_provider: Some(Provider::Codex),
            pika_session_id: Some(identity.into()),
            pika_name: Some("portfolio_review".into()),
            pika_launch_token: None,
        }
    }

    #[cfg(unix)]
    fn unverified_fixture(identity: &str) -> (tempfile::TempDir, Pika, Session) {
        let (root, mut pika) = test_pika();
        let session = test_session(identity);
        pika.store.upsert_session(&session, false).unwrap();
        let pid = i64::from(std::process::id());
        let start_time = process::process_start_time(pid).unwrap();
        let mut pane = tagged_pane(identity);
        pane.pane_pid = pid;
        pika.tmux = fixture_tmux(root.path(), &[pane]);
        pika.process_observer = Arc::new(move || {
            ProcessObservation::complete(BTreeMap::from([(
                pid,
                record(pid, None, start_time, &["codex", "resume"]),
            )]))
        });
        (root, pika, session)
    }

    #[cfg(unix)]
    #[test]
    fn unverified_binding_accepts_one_tagged_live_provider_without_uuid_argv() {
        let (_root, pika, session) = unverified_fixture("abababab-abab-4bab-8bab-abababababab");
        let binding = pika.unverified_pane_binding(&session).unwrap();
        assert_eq!(binding.pane.pane_id, "%1");
        assert_eq!(binding.provider_pid, i64::from(std::process::id()));
    }

    #[cfg(unix)]
    #[test]
    fn reconciliation_completes_only_an_independently_proven_pending_launch() {
        for case in [
            "exact",
            "unknown",
            "wrong-token",
            "wrong-binding",
            "duplicate",
        ] {
            let identity = "cdcdcdcd-cdcd-4dcd-8dcd-cdcdcdcdcdcd";
            let (root, mut pika, mut session) = unverified_fixture(identity);
            let pid = i64::from(std::process::id());
            let start = process::process_start_time(pid).unwrap();
            let mut pane = tagged_pane(identity);
            pane.pane_pid = pid;
            pane.pika_launch_token = Some(
                if case == "wrong-token" {
                    "other"
                } else {
                    "stuck"
                }
                .into(),
            );
            pika.tmux = fixture_tmux(root.path(), &[pane.clone()]);
            let duplicate = case == "duplicate";
            pika.process_observer = Arc::new(move || {
                let mut records = BTreeMap::from([(
                    pid,
                    record(pid, None, start, &["codex", "resume", identity]),
                )]);
                if duplicate {
                    records.insert(
                        999_998,
                        record(999_998, None, 88, &["codex", "resume", identity]),
                    );
                }
                ProcessObservation::complete(records)
            });
            session.status = Status::Ready;
            session.unread = true;
            pika.store.upsert_session(&session, false).unwrap();
            pika.store
                .record_status_observation(
                    session.provider,
                    identity,
                    &crate::model::StatusObservation {
                        kind: ObservationKind::Lifecycle,
                        status: Status::Ready,
                        unread: true,
                        attention_reason: Some("completed".into()),
                        error: None,
                        observed_at: 42.0,
                        source: "fixture".into(),
                    },
                )
                .unwrap();
            let pending = PendingLaunch {
                launch_token: "stuck".into(),
                provider: session.provider,
                name: "delayed".into(),
                cwd: "/tmp".into(),
                tmux_session: Some(pane.session_name),
                tmux_pane: Some(pane.pane_id),
                expected_session_id: (case != "unknown").then(|| identity.into()),
                root_pid: Some(999_999),
                root_pid_start: Some(10),
                preexisting_session_ids: None,
                candidate_session_id: None,
                candidate_observed_at: None,
                created_at: 1.0,
            };
            pika.store.add_pending(&pending).unwrap();
            pika.store
                .bind_launch(
                    "stuck",
                    session.provider,
                    if case == "wrong-binding" {
                        "other"
                    } else {
                        identity
                    },
                )
                .unwrap();
            pika.store
                .set_launch_phase("stuck", crate::store::LaunchPhase::ProviderStarting)
                .unwrap();
            pika.reconcile_for_action().unwrap();
            if case == "exact" {
                assert!(pika.store.get_pending("stuck").unwrap().is_none());
                let owner = pika
                    .store
                    .get_recovery_owner(session.provider, identity)
                    .unwrap()
                    .unwrap();
                assert_eq!((owner.pid, owner.start_time), (pid, start as i64));
                assert!(
                    pika.store
                        .get_session(session.provider, identity)
                        .unwrap()
                        .unwrap()
                        .unread
                );
                assert!(pika.store.get_meta("last_attached").unwrap().is_none());
            } else {
                assert_eq!(
                    pika.store.get_pending("stuck").unwrap(),
                    Some(pending),
                    "{case}"
                );
                assert!(
                    pika.store
                        .get_recovery_owner(session.provider, identity)
                        .unwrap()
                        .is_none(),
                    "{case}"
                );
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn unverified_binding_rejects_duplicate_tagged_panes() {
        let (root, mut pika, session) = unverified_fixture("acacacac-acac-4cac-8cac-acacacacacac");
        let mut duplicate = tagged_pane(&session.session_id);
        duplicate.pane_id = "%2".into();
        duplicate.pane_pid = 10;
        let mut first = tagged_pane(&session.session_id);
        first.pane_pid = 10;
        pika.tmux = fixture_tmux(root.path(), &[first, duplicate]);
        let error = pika.unverified_pane_binding(&session).unwrap_err();
        assert!(error.to_string().contains("2 tagged terminals"));
    }

    #[cfg(unix)]
    #[test]
    fn unverified_binding_rejects_dead_or_non_pika_tagged_panes() {
        let (root, mut pika, session) = unverified_fixture("adadadad-adad-4dad-8dad-adadadadadad");
        let mut dead = tagged_pane(&session.session_id);
        dead.dead = true;
        pika.tmux = fixture_tmux(root.path(), &[dead]);
        assert!(pika.unverified_pane_binding(&session).is_err());

        let mut non_pika = tagged_pane(&session.session_id);
        non_pika.session_name = "user-shell".into();
        pika.tmux = fixture_tmux(root.path(), &[non_pika]);
        assert!(pika.unverified_pane_binding(&session).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn unverified_binding_rejects_replaced_process_generation_and_retags() {
        let (root, mut pika, session) = unverified_fixture("aeaeaeae-aeae-4eae-8eae-aeaeaeaeaeae");
        let expected = pika.unverified_pane_binding(&session).unwrap();
        let pid = expected.provider_pid;
        let start_time = expected.provider_start_time;
        pika.process_observer = Arc::new(move || {
            ProcessObservation::complete(BTreeMap::from([(
                pid,
                record(pid, None, 999, &["codex", "resume"]),
            )]))
        });
        assert!(
            pika.revalidate_unverified_pane(&session, &expected)
                .is_err()
        );

        let mut retagged = tagged_pane(&session.session_id);
        retagged.pika_name = Some("different_conversation".into());
        retagged.pane_pid = pid;
        let current_start = start_time;
        pika.tmux = fixture_tmux(root.path(), &[retagged]);
        pika.process_observer = Arc::new(move || {
            ProcessObservation::complete(BTreeMap::from([(
                pid,
                record(pid, None, current_start, &["codex", "resume"]),
            )]))
        });
        assert!(
            pika.revalidate_unverified_pane(&session, &expected)
                .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn unverified_binding_rejects_shared_codex_daemon_without_terminal_provider() {
        let (root, mut pika, session) = unverified_fixture("afafafaf-afaf-4faf-8faf-afafafafafaf");
        let pid = i64::from(std::process::id());
        let start_time = process::process_start_time(pid).unwrap();
        pika.process_observer = Arc::new(move || {
            ProcessObservation::complete(BTreeMap::from([(
                pid,
                record(pid, None, start_time, &["codex", "app-server"]),
            )]))
        });
        let mut pane = tagged_pane(&session.session_id);
        pane.pane_pid = pid;
        pika.tmux = fixture_tmux(root.path(), &[pane]);
        assert!(pika.unverified_pane_binding(&session).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn recover_existing_session_with_vanished_pane_never_creates_pending_launch() {
        let (root, mut pika) = test_pika();
        let identity = "babababa-baba-4bab-8bab-babababababa";
        let mut session = test_session(identity);
        session.home_state = "identity_unproven".into();
        pika.store.upsert_session(&session, false).unwrap();
        pika.tmux = fixture_tmux(root.path(), &[]);
        pika.process_observer = Arc::new(|| ProcessObservation::complete(BTreeMap::new()));

        let error = pika
            .recover_existing_session(session.clone(), true)
            .unwrap_err();
        assert!(matches!(
            error.downcast_ref::<OpenError>(),
            Some(OpenError::IdentityUnproven)
        ));
        assert!(pika.store.list_pending().unwrap().is_empty());
        assert!(
            pika.store
                .get_recovery_owner(Provider::Codex, identity)
                .unwrap()
                .is_none()
        );
    }

    #[cfg(unix)]
    #[test]
    fn recovery_rejects_a_session_removed_from_fresh_inventory() {
        let (root, mut pika) = test_pika();
        let session = test_session("bcbcbcbc-bcbc-4cbc-8cbc-bcbcbcbcbcbc");
        pika.store.upsert_session(&session, false).unwrap();
        pika.store
            .untrack_session(session.provider, &session.session_id)
            .unwrap();
        pika.tmux = fixture_tmux(root.path(), &[]);
        pika.process_observer = Arc::new(|| ProcessObservation::complete(BTreeMap::new()));
        let error = pika
            .recover_existing_session(session.clone(), true)
            .unwrap_err();
        assert!(matches!(
            error.downcast_ref::<OpenError>(),
            Some(OpenError::NotFound(_))
        ));
        assert!(
            pika.store
                .is_untracked(session.provider, &session.session_id)
                .unwrap()
        );
        assert!(pika.store.list_pending().unwrap().is_empty());
    }

    #[test]
    fn another_process_commit_supersedes_stale_board_observation() {
        let (_root, mut pika) = test_pika();
        let identity = "abababab-abab-4bab-8bab-abababababab";
        let mut stale = test_session(identity);
        stale.live = false;
        stale.root_pid = None;
        stale.tmux_session = None;
        stale.tmux_pane = None;
        pika.store.upsert_session(&stale, false).unwrap();

        let observed = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let worker_observed = Arc::clone(&observed);
        let worker_release = Arc::clone(&release);
        pika.process_observer = Arc::new(move || {
            worker_observed.wait();
            worker_release.wait();
            ProcessObservation::complete(BTreeMap::new())
        });

        let worker = pika.clone();
        let reconcile = std::thread::spawn(move || worker.reconcile_local());
        observed.wait();

        // Construct a second Pika rather than cloning the first: its in-memory
        // fence is independent, just like a hook or separate CLI process. The
        // connection-local SQLite cursor must still reject the older snapshot.
        let other = Pika::with_components(
            pika.paths.clone(),
            pika.config.clone(),
            Store::from_paths(&pika.paths),
            pika.tmux.clone(),
        );
        let mut exact = stale;
        exact.live = true;
        exact.root_pid = Some(99_999);
        exact.tmux_session = Some("pika-c-new-home".into());
        exact.tmux_pane = Some("%99".into());
        other.store.upsert_session(&exact, false).unwrap();

        release.wait();
        let error = reconcile.join().unwrap().unwrap_err().to_string();
        assert!(error.contains("superseded"));
        let preserved = pika
            .store
            .get_session(Provider::Codex, identity)
            .unwrap()
            .unwrap();
        assert_eq!(preserved.root_pid, Some(99_999));
        assert_eq!(preserved.tmux_pane.as_deref(), Some("%99"));
    }

    #[test]
    fn two_thousand_row_reconcile_yields_to_hook_bursts_without_losing_truth() {
        let (_root, mut pika) = test_pika();
        pika.process_observer = Arc::new(|| ProcessObservation::complete(BTreeMap::new()));
        let target_id = "00000000-0000-4000-8000-000000000000";
        for index in 0..2_000 {
            let id = format!("00000000-0000-4000-8000-{index:012x}");
            let mut session = test_session(&id);
            session.name = Some(format!("session-{index:04}"));
            session.live = false;
            session.root_pid = None;
            session.status = Status::Parked;
            pika.store.upsert_session(&session, false).unwrap();
        }

        let participants = 7;
        let barrier = Arc::new(Barrier::new(participants));
        let reconcile_barrier = Arc::clone(&barrier);
        let reconcile_pika = pika.clone();
        let reconcile = std::thread::spawn(move || {
            reconcile_barrier.wait();
            reconcile_pika.reconcile_local()
        });
        let mut hooks = Vec::new();
        for index in 0..5 {
            let store = pika.store.clone();
            let hook_barrier = Arc::clone(&barrier);
            hooks.push(std::thread::spawn(move || {
                let payload = parse_hook_payload(
                    serde_json::to_vec(&serde_json::json!({
                        "session_id":target_id,
                        "hook_event_name":"Stop",
                        "cwd":"/tmp",
                    }))
                    .unwrap()
                    .as_slice(),
                    Provider::Codex,
                )
                .unwrap();
                hook_barrier.wait();
                handle_hook(
                    &store,
                    Provider::Codex,
                    &payload,
                    &HookContext::at(10_000.0 + index as f64),
                )
            }));
        }
        barrier.wait();
        let stale_result = reconcile.join().unwrap();
        for hook in hooks {
            hook.join().unwrap().unwrap();
        }

        // A hook committed through another SQLite connection while the slow
        // observation was in flight, so that observation must either have
        // completed before the hook burst or fail closed. A fresh pass then
        // projects the settled truth without losing any hook event.
        if let Err(error) = stale_result {
            assert!(error.to_string().contains("superseded"));
        }
        let inventory = pika.reconcile_local().unwrap();

        assert_eq!(inventory.sessions.len(), 2_000);
        let target = pika
            .store
            .get_session(Provider::Codex, target_id)
            .unwrap()
            .unwrap();
        assert_eq!(target.status, Status::Ready);
        assert!(target.unread);
        assert_eq!(target.last_event_at, 10_004.0);
    }

    #[test]
    fn daily_name_and_uuid_restore_only_the_exact_selected_tombstone() {
        for query_by_name in [true, false] {
            let (root, pika) = test_pika();
            let identity = if query_by_name {
                "11111111-1111-4111-8111-111111111111"
            } else {
                "22222222-2222-4222-8222-222222222222"
            };
            let mut session = test_session(identity);
            if query_by_name {
                session.name = Some("old_provider_name".into());
                std::fs::create_dir_all(&pika.paths.codex_home).unwrap();
                let transcript = root.path().join("renamed.jsonl");
                std::fs::write(&transcript, "{}\n").unwrap();
                let db =
                    rusqlite::Connection::open(pika.paths.codex_home.join("state_renamed.sqlite"))
                        .unwrap();
                db.execute_batch("CREATE TABLE threads(id TEXT PRIMARY KEY,name TEXT,cwd TEXT,rollout_path TEXT,updated_at INTEGER,archived INTEGER)").unwrap();
                db.execute(
                    "INSERT INTO threads VALUES(?1,'restored_name','/tmp',?2,10,0)",
                    rusqlite::params![identity, transcript.display().to_string()],
                )
                .unwrap();
            }
            session.live = false;
            session.root_pid = None;
            session.status = Status::Parked;
            pika.store.upsert_session(&session, false).unwrap();
            pika.store
                .untrack_session(session.provider, &session.session_id)
                .unwrap();

            let query = if query_by_name {
                "restored_name".to_owned()
            } else {
                session.session_id.clone()
            };
            let matches = pika.resolve_local(&query).unwrap();
            assert_eq!(matches.len(), 1);
            assert_eq!(matches[0].session_id, session.session_id);
            assert!(
                pika.store
                    .is_untracked(session.provider, &session.session_id)
                    .unwrap(),
                "resolution alone must preserve the tombstone"
            );

            // The isolated fixture cannot complete a real provider/tmux open.
            // Frozen Pika semantics still restore watching immediately after
            // the exact daily-command selection.
            assert!(pika.open_name(&query, false, false).is_err());
            assert!(
                !pika
                    .store
                    .is_untracked(session.provider, &session.session_id)
                    .unwrap()
            );
            assert!(
                pika.cached_inventory()
                    .unwrap()
                    .sessions
                    .iter()
                    .any(|item| {
                        item.provider == session.provider && item.session_id == session.session_id
                    })
            );
        }
    }

    #[test]
    fn ambiguous_daily_name_does_not_restore_any_tombstone() {
        let (root, pika) = test_pika();
        let first_dir = root.path().join("first");
        let second_dir = root.path().join("second");
        std::fs::create_dir_all(&first_dir).unwrap();
        std::fs::create_dir_all(&second_dir).unwrap();
        let mut identities = Vec::new();
        for (identity, cwd) in [
            ("11111111-1111-4111-8111-111111111111", first_dir),
            ("22222222-2222-4222-8222-222222222222", second_dir),
        ] {
            let mut session = test_session(identity);
            session.cwd = Some(cwd.to_string_lossy().into_owned());
            session.live = false;
            session.root_pid = None;
            session.status = Status::Parked;
            pika.store.upsert_session(&session, false).unwrap();
            pika.store
                .untrack_session(session.provider, &session.session_id)
                .unwrap();
            identities.push(session.session_id);
        }

        let error = pika
            .open_name("portfolio_review", false, false)
            .unwrap_err();
        assert!(error.downcast_ref::<OpenError>().is_some());
        for identity in identities {
            assert!(pika.store.is_untracked(Provider::Codex, &identity).unwrap());
        }
    }

    #[cfg(unix)]
    fn fixture_tmux(root: &Path, panes: &[Pane]) -> Tmux {
        use std::os::unix::fs::PermissionsExt;
        let rows = panes
            .iter()
            .map(|pane| {
                [
                    pane.session_name.clone(),
                    pane.pane_id.clone(),
                    pane.pane_pid.to_string(),
                    pane.cwd.clone(),
                    pane.current_command.clone(),
                    "0".into(),
                    "1".into(),
                    "1".into(),
                    if pane.dead { "1" } else { "0" }.into(),
                    String::new(),
                    "1".into(),
                    "1".into(),
                    pane.pika_provider
                        .map(|provider| provider.to_string())
                        .unwrap_or_default(),
                    pane.pika_session_id.clone().unwrap_or_default(),
                    pane.pika_name.clone().unwrap_or_default(),
                    pane.pika_launch_token.clone().unwrap_or_default(),
                ]
                .join("\u{1f}")
            })
            .collect::<Vec<_>>()
            .join("\n");
        let executable = root.join("tmux-fixture");
        std::fs::write(&executable, format!(
            "#!/bin/sh\ncase \"$*\" in\n*list-panes*) printf '%s\\n' {} ;;\n*capture-pane*) printf 'exact fixture output\\n' ;;\n*) exit 0 ;;\nesac\n",
            shell_words::quote(&rows)
        )).unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        Tmux::with_executable(executable.to_string_lossy(), None)
    }

    #[cfg(unix)]
    #[test]
    fn daily_name_uses_fresh_ownership_and_provider_clocks_not_saved_activity() {
        let (root, mut pika) = test_pika();
        let first = "11111111-1111-4111-8111-111111111111";
        let second = "22222222-2222-4222-8222-222222222222";
        std::fs::create_dir_all(&pika.paths.codex_home).unwrap();
        let db = rusqlite::Connection::open(pika.paths.codex_home.join("state_1.sqlite")).unwrap();
        db.execute_batch("CREATE TABLE threads(id TEXT PRIMARY KEY,name TEXT,cwd TEXT,rollout_path TEXT,updated_at INTEGER,archived INTEGER)").unwrap();
        for (identity, updated, saved_activity) in [(first, 10, 9000.0), (second, 20, 100.0)] {
            let path = root.path().join(format!("{identity}.jsonl"));
            std::fs::write(&path, "{}\n").unwrap();
            db.execute(
                "INSERT INTO threads VALUES(?1,'portfolio_review',?2,?3,?4,0)",
                rusqlite::params![
                    identity,
                    root.path().display().to_string(),
                    path.display().to_string(),
                    updated
                ],
            )
            .unwrap();
            let mut session = test_session(identity);
            session.cwd = Some(root.path().display().to_string());
            session.transcript_path = Some(path.display().to_string());
            session.last_activity_at = saved_activity;
            session.status = Status::Parked;
            pika.store.upsert_session(&session, false).unwrap();
        }
        let pane = tagged_pane(first);
        pika.tmux = fixture_tmux(root.path(), &[pane]);
        pika.process_observer = Arc::new(move || {
            ProcessObservation::complete(BTreeMap::from([
                (1, record(1, None, 1, &["sh"])),
                (2, record(2, Some(1), 2, &["codex", "resume", first])),
            ]))
        });
        let chosen = pika.resolve_local("portfolio_review").unwrap();
        assert_eq!(chosen.len(), 1);
        assert_eq!(
            chosen[0].session_id, first,
            "the exact live home wins over newer idle history"
        );
        assert!(chosen[0].has_exact_home());

        pika.process_observer = Arc::new(|| ProcessObservation::complete(BTreeMap::new()));
        pika.tmux = Tmux::with_executable("/usr/bin/false", None);
        let chosen = pika.resolve_local("portfolio_review").unwrap();
        assert_eq!(chosen.len(), 1);
        assert_eq!(
            chosen[0].session_id, second,
            "idle reduction uses provider time, not Pika's newer resume/activity time"
        );
        db.execute("UPDATE threads SET updated_at=20", []).unwrap();
        assert_eq!(
            pika.resolve_local("portfolio_review").unwrap().len(),
            2,
            "equal provider times stay ambiguous"
        );
    }

    #[cfg(unix)]
    #[test]
    fn newly_certified_provider_homes_support_return_peek_and_profile_without_uuid_argv() {
        let (root, mut pika) = test_pika();
        let pid = i64::from(std::process::id());
        let start_time = process::process_start_time(pid).unwrap();
        for (provider, identity) in [
            (Provider::Codex, "33333333-3333-4333-8333-333333333333"),
            (Provider::Claude, "44444444-4444-4444-8444-444444444444"),
            (Provider::Opencode, "ses_newfixture"),
        ] {
            let mut session = test_session(identity);
            session.provider = provider;
            session.cwd = Some(root.path().display().to_string());
            session.transcript_path = None;
            let token = format!("launch-{provider}");
            let mut pane = tagged_pane(identity);
            pane.pane_pid = pid;
            pane.pika_provider = Some(provider);
            pane.pika_launch_token = Some(token.clone());
            pika.store.upsert_session(&session, false).unwrap();
            pika.store.bind_launch(&token, provider, identity).unwrap();
            let certificate = crate::store::RecoveryOwner {
                provider,
                session_id: identity.into(),
                pid,
                start_time: start_time as i64,
                launch_token: token.clone(),
                created_at: now(),
            };
            pika.store.set_recovery_owner(&certificate).unwrap();
            let argv = Providers::new(&pika.paths, &pika.config).new_argv(
                provider,
                "fresh",
                Some(identity),
            );
            let process = ProcessRecord {
                pid,
                parent_pid: None,
                start_time,
                argv,
            };
            let observed = process.clone();
            pika.process_observer = Arc::new(move || {
                ProcessObservation::complete(BTreeMap::from([(pid, observed.clone())]))
            });
            pika.tmux = fixture_tmux(root.path(), &[pane.clone()]);
            assert_eq!(
                pika.open_session(session.clone(), false).unwrap().kind,
                "ATTACHED LIVE"
            );
            assert_eq!(
                pika.capture_exact(&session, 20).unwrap(),
                "exact fixture output"
            );
            pika.exact_pane_binding(&session, Some(&pane.pane_id))
                .unwrap();
            let proof = crate::experts::PublisherProof::from_verified_identity(
                &session, provider, identity, identity,
            )
            .unwrap();
            crate::experts::publish(
                &pika.store,
                &session,
                &proof,
                crate::experts::PublishInput {
                    scope: "Verified new provider home".into(),
                    current_state: "Testing exact recovery".into(),
                    topics: vec!["recovery".into()],
                    ..Default::default()
                },
            )
            .unwrap();

            // A valid launch certificate cannot hide an independent UUID owner.
            let duplicate = record(
                pid + 1,
                None,
                start_time,
                &[provider.as_str(), "resume", identity],
            );
            let original = process.clone();
            pika.process_observer = Arc::new(move || {
                ProcessObservation::complete(BTreeMap::from([
                    (pid, original.clone()),
                    (pid + 1, duplicate.clone()),
                ]))
            });
            assert!(
                pika.exact_pane_binding(&session, Some(&pane.pane_id))
                    .is_err()
            );
            let original = process.clone();
            pika.process_observer = Arc::new(move || {
                ProcessObservation::complete(BTreeMap::from([(pid, original.clone())]))
            });
            if provider != Provider::Claude {
                pane.pika_launch_token = Some("copied-or-reused-pane".into());
                pika.tmux = fixture_tmux(root.path(), &[pane.clone()]);
                assert!(
                    pika.exact_pane_binding(&session, Some(&pane.pane_id))
                        .is_err()
                );
                pane.pika_launch_token = Some(token);
                pika.tmux = fixture_tmux(root.path(), &[pane.clone()]);
                pika.store
                    .set_recovery_owner(&crate::store::RecoveryOwner {
                        start_time: certificate.start_time - 1,
                        ..certificate
                    })
                    .unwrap();
                assert!(
                    pika.exact_pane_binding(&session, Some(&pane.pane_id))
                        .is_err(),
                    "a reused PID cannot inherit the launch certificate"
                );
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn exact_binding_chooses_verified_pane_when_stale_duplicate_tag_is_shell_only() {
        let (root, mut pika) = test_pika();
        let identity = "15151515-1515-4151-8151-151515151515";
        let provider_pid = i64::from(std::process::id());
        let provider_start = process::process_start_time(provider_pid).unwrap();
        let stale_child = std::process::Command::new("/bin/sleep")
            .arg("60")
            .spawn()
            .unwrap();
        let stale_pid = i64::from(stale_child.id());
        let stale_start = process::process_start_time(stale_pid).unwrap();
        struct Reap(std::process::Child);
        impl Drop for Reap {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let _stale_guard = Reap(stale_child);

        let mut healthy = tagged_pane(identity);
        healthy.pane_id = "%29".into();
        healthy.pane_pid = provider_pid;
        healthy.current_command = "codex".into();
        let mut stale = tagged_pane(identity);
        stale.pane_id = "%stale".into();
        stale.pane_pid = stale_pid;
        stale.current_command = "zsh".into();
        pika.tmux = fixture_tmux(root.path(), &[healthy.clone(), stale]);
        pika.process_observer = Arc::new(move || {
            ProcessObservation::complete(BTreeMap::from([
                (
                    provider_pid,
                    record(
                        provider_pid,
                        None,
                        provider_start,
                        &["codex", "resume", identity],
                    ),
                ),
                (stale_pid, record(stale_pid, None, stale_start, &["zsh"])),
            ]))
        });
        let session = test_session(identity);
        pika.store.upsert_session(&session, false).unwrap();

        let binding = pika.exact_pane_binding(&session, Some("%29")).unwrap();
        assert_eq!(binding.pane.pane_id, "%29");
        assert_eq!(binding.provider_pid, provider_pid);
    }

    #[cfg(unix)]
    #[test]
    fn exact_binding_rechecks_ignored_tag_before_accepting_it() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let (root, mut pika) = test_pika();
        let identity = "16161616-1616-4161-8161-161616161616";
        let provider_pid = i64::from(std::process::id());
        let provider_start = process::process_start_time(provider_pid).unwrap();
        let stale_child = std::process::Command::new("/bin/sleep")
            .arg("60")
            .spawn()
            .unwrap();
        let stale_pid = i64::from(stale_child.id());
        let stale_start = process::process_start_time(stale_pid).unwrap();
        struct Reap(std::process::Child);
        impl Drop for Reap {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let _stale_guard = Reap(stale_child);

        let mut healthy = tagged_pane(identity);
        healthy.pane_id = "%29".into();
        healthy.pane_pid = provider_pid;
        let mut stale = tagged_pane(identity);
        stale.pane_id = "%stale".into();
        stale.pane_pid = stale_pid;
        stale.current_command = "zsh".into();
        pika.tmux = fixture_tmux(root.path(), &[healthy, stale]);
        let phase = Arc::new(AtomicUsize::new(0));
        let observed_phase = Arc::clone(&phase);
        pika.process_observer = Arc::new(move || {
            let mut records = BTreeMap::from([
                (
                    provider_pid,
                    record(
                        provider_pid,
                        None,
                        provider_start,
                        &["codex", "resume", identity],
                    ),
                ),
                (stale_pid, record(stale_pid, None, stale_start, &["zsh"])),
            ]);
            if observed_phase.fetch_add(1, Ordering::SeqCst) > 0 {
                records.insert(
                    stale_pid + 1,
                    record(
                        stale_pid + 1,
                        Some(stale_pid),
                        stale_start + 1,
                        &["codex", "app-server", "--stdio"],
                    ),
                );
            }
            ProcessObservation::complete(records)
        });
        let session = test_session(identity);
        pika.store.upsert_session(&session, false).unwrap();

        let error = pika
            .exact_pane_binding(&session, Some("%29"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("acquired a live codex process"), "{error}");
    }

    fn continuation(identity: &str, parent: Option<&str>, updated_at: f64) -> Candidate {
        Candidate {
            provider: Provider::Codex,
            session_id: identity.into(),
            name: Some("portfolio_review".into()),
            cwd: Some("/tmp".into()),
            branch: None,
            transcript_path: None,
            model: None,
            updated_at,
            live: true,
            pid: None,
            source: "test".into(),
            parent_session_id: parent.map(str::to_owned),
            created_at: updated_at,
            lifecycle_status: Some(Status::Working),
        }
    }

    #[test]
    fn fresh_same_name_codex_continuations_fail_closed_then_recover() {
        let root = "10000000-0000-4000-8000-000000000000";
        let child_a = "20000000-0000-4000-8000-000000000000";
        let child_b = "30000000-0000-4000-8000-000000000000";
        let session = test_session(root);
        let mut candidates = [
            continuation(root, None, 9_990.0),
            continuation(child_a, Some(root), 9_995.0),
            continuation(child_b, Some(root), 9_999.0),
        ]
        .into_iter()
        .map(|candidate| {
            (
                (candidate.provider, candidate.session_id.clone()),
                candidate,
            )
        })
        .collect::<BTreeMap<_, _>>();
        let processes = BTreeMap::from([
            (1, record(1, None, 10, &["sh"])),
            (2, record(2, Some(1), 20, &["codex", "resume", root])),
        ]);

        let (selected, conflicts) = continuation_candidate(
            &session,
            &candidates,
            &[tagged_pane(root)],
            &processes,
            10_000.0,
        );
        assert_eq!(selected.map(|item| item.session_id.as_str()), Some(root));
        assert_eq!(conflicts, [child_b, child_a, root]);

        candidates
            .get_mut(&(Provider::Codex, root.into()))
            .unwrap()
            .lifecycle_status = Some(Status::Ready);
        candidates
            .get_mut(&(Provider::Codex, child_a.into()))
            .unwrap()
            .updated_at = 1.0;
        let (selected, conflicts) = continuation_candidate(
            &session,
            &candidates,
            &[tagged_pane(root)],
            &processes,
            10_000.0,
        );
        assert_eq!(selected.map(|item| item.session_id.as_str()), Some(child_b));
        assert!(conflicts.is_empty());
    }

    #[test]
    fn qualified_names_choose_provider_without_consuming_colons_in_normal_names() {
        assert_eq!(
            split_provider_query("codex:alpha"),
            (Some(Provider::Codex), "alpha")
        );
        assert_eq!(
            split_provider_query("research:alpha"),
            (None, "research:alpha")
        );
    }

    #[test]
    fn launch_environment_contains_identity_but_no_parent_thread_aliases() {
        let values = launch_environment(Provider::Codex, Some("id"), "token", "name");
        assert_eq!(
            values.get("PIKA_SESSION_ID").map(String::as_str),
            Some("id")
        );
        assert!(!values.contains_key("CODEX_THREAD_ID"));
    }

    #[test]
    fn stale_shared_server_and_pid_reuse_recover_the_exact_tagged_pane() {
        let (_root, pika) = test_pika();
        let identity = "11111111-1111-4111-8111-111111111111";
        let mut session = test_session(identity);
        pika.store.upsert_session(&session, false).unwrap();
        for (token, start_time) in [("stale-server", 30), ("reused-pid", 29)] {
            pika.store
                .set_live_owner(&LiveOwner {
                    provider: Provider::Codex,
                    session_id: identity.into(),
                    pid: 3,
                    start_time: Some(start_time),
                    owner_token: token.into(),
                    last_seen: 0.0,
                })
                .unwrap();
        }
        let processes = BTreeMap::from([
            (1, record(1, None, 10, &["sh"])),
            (2, record(2, Some(1), 20, &["codex", "resume", identity])),
            (3, record(3, None, 30, &["codex", "app-server"])),
        ]);
        pika.reconcile_one(&mut session, &[tagged_pane(identity)], &processes)
            .unwrap();
        assert_eq!(session.home_state, "exact");
        assert!(session.live);
        assert_ne!(session.status, Status::OpenTwice);
        assert!(
            pika.store
                .live_owners(Provider::Codex, identity)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn stale_shell_tag_does_not_veto_one_exact_owner() {
        let (_root, pika) = test_pika();
        let identity = "12121212-1212-4121-8121-121212121212";
        let mut session = test_session(identity);
        pika.store.upsert_session(&session, false).unwrap();
        let mut stale = tagged_pane(identity);
        stale.pane_id = "%stale".into();
        stale.pane_pid = 4;
        stale.current_command = "zsh".into();
        let processes = BTreeMap::from([
            (1, record(1, None, 10, &["sh"])),
            (2, record(2, Some(1), 20, &["codex", "resume", identity])),
            (4, record(4, None, 40, &["zsh"])),
        ]);
        pika.reconcile_one(&mut session, &[tagged_pane(identity), stale], &processes)
            .unwrap();
        assert_eq!(session.home_state, "exact");
        assert!(session.live);
        assert_ne!(session.status, Status::OpenTwice);
        assert_eq!(session.tmux_pane.as_deref(), Some("%1"));
        assert_eq!(session.root_pid, Some(2));
    }

    #[test]
    fn nested_provider_helper_does_not_steal_exact_owner() {
        let (_root, pika) = test_pika();
        let identity = "13131313-1313-4131-8131-131313131313";
        let mut session = test_session(identity);
        pika.store.upsert_session(&session, false).unwrap();
        let processes = BTreeMap::from([
            (1, record(1, None, 10, &["sh"])),
            (2, record(2, Some(1), 20, &["codex", "resume", identity])),
            (
                3,
                record(3, Some(2), 30, &["codex", "app-server", "--stdio"]),
            ),
        ]);
        pika.reconcile_one(&mut session, &[tagged_pane(identity)], &processes)
            .unwrap();
        assert_eq!(session.home_state, "exact");
        assert!(session.live);
        assert_eq!(session.root_pid, Some(2));
    }

    #[test]
    fn helper_only_tagged_pane_fails_closed() {
        let (_root, pika) = test_pika();
        let identity = "14141414-1414-4141-8141-141414141414";
        let mut session = test_session(identity);
        pika.store.upsert_session(&session, false).unwrap();
        let processes = BTreeMap::from([
            (1, record(1, None, 10, &["sh"])),
            (
                2,
                record(2, Some(1), 20, &["codex", "app-server", "--stdio"]),
            ),
        ]);
        pika.reconcile_one(&mut session, &[tagged_pane(identity)], &processes)
            .unwrap();
        assert_eq!(session.home_state, "identity_unproven");
        assert_eq!(session.status, Status::Error);
        assert!(!session.live);
    }

    #[test]
    fn shared_codex_daemon_clears_root_projection_but_keeps_advisory_lease() {
        let (_root, pika) = test_pika();
        let identity = "17171717-1717-4171-8171-171717171717";
        let mut session = test_session(identity);
        session.root_pid = Some(3);
        session.tmux_session = None;
        session.tmux_pane = None;
        pika.store.upsert_session(&session, false).unwrap();
        pika.store
            .set_live_owner(&LiveOwner {
                provider: Provider::Codex,
                session_id: identity.into(),
                pid: 3,
                start_time: Some(30),
                owner_token: "shared-daemon-lease".into(),
                last_seen: now(),
            })
            .unwrap();
        let processes = BTreeMap::from([(
            3,
            record(3, None, 30, &["codex", "app-server", "--remote-control"]),
        )]);

        pika.reconcile_one(&mut session, &[], &processes).unwrap();

        assert!(
            session.live,
            "the advisory shared owner still proves liveness"
        );
        assert_eq!(
            session.root_pid, None,
            "shared daemon is not a terminal root"
        );
        assert_eq!(
            pika.store
                .get_session(Provider::Codex, identity)
                .unwrap()
                .unwrap()
                .root_pid,
            None
        );
        let leases = pika.store.live_owners(Provider::Codex, identity).unwrap();
        assert_eq!(leases.len(), 1);
        assert_eq!(leases[0].pid, 3);
        assert!(pika.store.list_pending().unwrap().is_empty());
    }

    #[test]
    fn certified_launcher_does_not_reappear_as_a_second_native_client() {
        let (_root, pika) = test_pika();
        let identity = "11111111-1111-4111-8111-111111111111";
        let mut session = test_session(identity);
        pika.store.upsert_session(&session, false).unwrap();
        pika.store
            .bind_launch("launcher-token", Provider::Codex, identity)
            .unwrap();
        let certificate = crate::store::RecoveryOwner {
            provider: Provider::Codex,
            session_id: identity.into(),
            pid: 2,
            start_time: 20,
            launch_token: "launcher-token".into(),
            created_at: now(),
        };
        pika.store.set_recovery_owner(&certificate).unwrap();
        pika.store
            .set_live_owner(&LiveOwner {
                provider: Provider::Codex,
                session_id: identity.into(),
                pid: 2,
                start_time: Some(20),
                owner_token: "launcher-lease".into(),
                last_seen: now(),
            })
            .unwrap();
        let mut processes = BTreeMap::from([
            (1, record(1, None, 10, &["sh"])),
            (
                2,
                record(2, Some(1), 20, &["node", "/bin/codex", "resume", identity]),
            ),
            (
                3,
                record(3, Some(2), 30, &["/vendor/codex", "resume", identity]),
            ),
            (4, record(4, None, 40, &["codex", "resume", identity])),
        ]);
        let owners = pika
            .store
            .reconcile_transaction(|ledger| identity_owners(&session, &processes, ledger, now()))
            .unwrap();
        assert_eq!(owners.pids(), BTreeSet::from([3, 4]));
        pika.reconcile_one(&mut session, &[tagged_pane(identity)], &processes)
            .unwrap();
        assert_eq!(session.status, Status::OpenTwice);
        processes.remove(&4);
        pika.reconcile_one(&mut session, &[tagged_pane(identity)], &processes)
            .unwrap();
        assert_eq!(session.home_state, "exact");
        assert_ne!(session.status, Status::OpenTwice);
        let owners = pika
            .store
            .reconcile_transaction(|ledger| identity_owners(&session, &processes, ledger, now()))
            .unwrap();
        assert_eq!(owners.pids(), BTreeSet::from([3]));
        assert!(owners.proves_pane(3, &tagged_pane(identity)));
        // Suppress only duplicate observation; don't destroy the launch receipt.
        assert!(
            pika.store
                .get_recovery_owner(Provider::Codex, identity)
                .unwrap()
                .is_some()
        );
        // A reused PID invalidates the stored generation even if a new runtime
        // launcher happens to own the same argv. Fresh direct evidence is separate.
        processes.get_mut(&2).unwrap().start_time = 25;
        let owners = pika
            .store
            .reconcile_transaction(|ledger| identity_owners(&session, &processes, ledger, now()))
            .unwrap();
        assert_eq!(owners.pids(), BTreeSet::from([3]));
        assert!(
            pika.store
                .get_recovery_owner(Provider::Codex, identity)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn genuine_duplicate_is_open_twice_then_recovers_automatically() {
        let (_root, pika) = test_pika();
        let identity = "22222222-2222-4222-8222-222222222222";
        let mut session = test_session(identity);
        pika.store.upsert_session(&session, false).unwrap();
        let mut processes = BTreeMap::from([
            (1, record(1, None, 10, &["sh"])),
            (2, record(2, Some(1), 20, &["codex", "resume", identity])),
            (4, record(4, None, 40, &["codex", "resume", identity])),
        ]);
        pika.reconcile_one(&mut session, &[tagged_pane(identity)], &processes)
            .unwrap();
        assert_eq!(session.status, Status::OpenTwice);
        assert_eq!(session.home_state, "open_twice");

        processes.remove(&4);
        pika.reconcile_one(&mut session, &[tagged_pane(identity)], &processes)
            .unwrap();
        assert_eq!(session.home_state, "exact");
        assert_ne!(session.status, Status::OpenTwice);
        assert!(
            pika.store
                .status_observations(Provider::Codex, identity)
                .unwrap()
                .iter()
                .all(|observation| observation.kind != ObservationKind::Safety)
        );
    }

    #[test]
    fn old_session_end_open_twice_stops_blocking_after_duplicate_is_gone() {
        let (_root, pika) = test_pika();
        let identity = "33333333-3333-4333-8333-333333333333";
        let mut session = test_session(identity);
        session.status = Status::OpenTwice;
        session.unread = true;
        session.attention_reason = Some("identity".into());
        session.error = Some("the exact conversation has more than one live owner".into());
        pika.store.upsert_session(&session, false).unwrap();
        pika.store
            .record_status_observation(
                Provider::Codex,
                identity,
                &crate::model::StatusObservation {
                    kind: ObservationKind::Lifecycle,
                    status: Status::OpenTwice,
                    unread: true,
                    attention_reason: Some("identity".into()),
                    error: session.error.clone(),
                    observed_at: 1.0,
                    source: "hook:SessionEnd".into(),
                },
            )
            .unwrap();
        let mut processes = BTreeMap::from([
            (1, record(1, None, 10, &["sh"])),
            (2, record(2, Some(1), 20, &["codex", "resume", identity])),
            (4, record(4, None, 40, &["codex", "resume", identity])),
        ]);
        pika.reconcile_one(&mut session, &[tagged_pane(identity)], &processes)
            .unwrap();
        assert_eq!(session.status, Status::OpenTwice);
        processes.remove(&4);
        pika.reconcile_one(&mut session, &[tagged_pane(identity)], &processes)
            .unwrap();
        assert_eq!(session.home_state, "exact");
        assert_eq!(session.status, Status::Ready);
        assert_eq!(session.error, None);
        assert!(!session.unread);
    }

    #[test]
    fn absent_process_clears_sticky_live_and_runtime_pid() {
        let (_root, pika) = test_pika();
        let identity = "33333333-3333-4333-8333-333333333333";
        let mut session = test_session(identity);
        pika.store.upsert_session(&session, false).unwrap();
        pika.reconcile_one(&mut session, &[], &BTreeMap::new())
            .unwrap();
        assert!(!session.live);
        assert_eq!(session.root_pid, None);
        assert_eq!(session.status, Status::Parked);
        assert_eq!(
            pika.store
                .get_session(Provider::Codex, identity)
                .unwrap()
                .unwrap()
                .root_pid,
            None
        );
    }

    #[test]
    fn reused_pane_id_with_missing_tags_is_never_rebound_from_saved_state() {
        let (_root, pika) = test_pika();
        let identity = "44444444-4444-4444-8444-444444444444";
        let mut session = test_session(identity);
        session.tmux_session = Some("old-server".into());
        session.tmux_pane = Some("%1".into());
        pika.store.upsert_session(&session, false).unwrap();
        let mut replacement = tagged_pane(identity);
        replacement.session_name = "unrelated-after-restart".into();
        replacement.pika_provider = None;
        replacement.pika_session_id = None;
        let processes = BTreeMap::from([
            (1, record(1, None, 100, &["zsh"])),
            (2, record(2, Some(1), 200, &["codex", "resume", identity])),
        ]);

        pika.reconcile_one(&mut session, &[replacement], &processes)
            .unwrap();
        assert_eq!(session.home_state, "outside");
        assert_eq!(session.tmux_pane, None);
        assert_eq!(session.tmux_session, None);
        let stored = pika
            .store
            .get_session(Provider::Codex, identity)
            .unwrap()
            .unwrap();
        assert_eq!(stored.tmux_pane, None);
        assert_eq!(stored.tmux_session, None);
    }

    #[test]
    fn reused_pane_id_with_wrong_tags_is_not_touched_or_treated_as_home() {
        let (_root, pika) = test_pika();
        let identity = "55555555-5555-4555-8555-555555555555";
        let mut session = test_session(identity);
        session.tmux_pane = Some("%1".into());
        pika.store.upsert_session(&session, false).unwrap();
        let mut replacement = tagged_pane("66666666-6666-4666-8666-666666666666");
        replacement.pika_name = Some("someone_else".into());
        let processes = BTreeMap::from([
            (1, record(1, None, 100, &["zsh"])),
            (2, record(2, Some(1), 200, &["codex", "resume", identity])),
        ]);

        pika.reconcile_one(&mut session, &[replacement], &processes)
            .unwrap();
        assert_eq!(session.home_state, "outside");
        assert_eq!(session.tmux_pane, None);
        assert_eq!(session.root_pid, Some(2));
    }

    #[test]
    fn partial_process_observation_cannot_clear_state_or_start_a_provider() {
        let (_root, mut pika) = test_pika();
        let identity = "77777777-7777-4777-8777-777777777777";
        let mut session = test_session(identity);
        session.tmux_session = Some("pika-c-existing".into());
        session.tmux_pane = Some("%9".into());
        session.root_pid = Some(900);
        pika.store.upsert_session(&session, false).unwrap();
        pika.process_observer = Arc::new(|| {
            ProcessObservation::partial(BTreeMap::new(), vec!["PID 900: permission denied".into()])
        });

        let error = pika.reconcile_local().unwrap_err().to_string();
        assert!(error.contains("process identity observation was partial"));
        let unchanged = pika
            .store
            .get_session(Provider::Codex, identity)
            .unwrap()
            .unwrap();
        assert_eq!(unchanged.tmux_pane.as_deref(), Some("%9"));
        assert_eq!(unchanged.root_pid, Some(900));
        assert!(pika.open_session(unchanged, false).is_err());
        assert!(pika.store.list_pending().unwrap().is_empty());
    }

    #[test]
    fn failed_enumeration_blocks_new_launch_then_a_complete_scan_recovers() {
        let (_root, mut pika) = test_pika();
        pika.process_observer = Arc::new(|| ProcessObservation::error("enumeration denied"));
        let error = pika
            .new_session("blocked_new", Provider::Codex, false)
            .unwrap_err()
            .to_string();
        assert!(error.contains("process identity could not be observed"));
        assert!(pika.store.list_pending().unwrap().is_empty());

        pika.process_observer = Arc::new(|| ProcessObservation::complete(BTreeMap::new()));
        pika.tmux = Tmux::with_executable("/usr/bin/false", None);
        // Recovery reaches the next independent precondition instead of
        // retaining a sticky observation failure.
        let error = pika
            .new_session("blocked_new", Provider::Codex, false)
            .unwrap_err()
            .to_string();
        assert!(!error.contains("process identity could not be observed"));
    }

    #[cfg(unix)]
    #[test]
    fn provider_proven_archive_is_removed_from_daily_inventory() {
        let (root, mut pika) = test_pika();
        pika.process_observer = Arc::new(|| ProcessObservation::complete(BTreeMap::new()));
        let identity = "88888888-8888-4888-8888-888888888888";
        let archived = root.path().join("archived_sessions/thread.jsonl");
        let tag_cleanup = root.path().join("unexpected-tag-cleanup");
        let tmux_fixture = root.path().join("tmux-fixture");
        let pane_line = [
            "pika-c-archived",
            "%9",
            "1",
            "/tmp",
            "sh",
            "0",
            "1",
            "1",
            "0",
            "",
            "10",
            "9",
            "codex",
            identity,
            "archived_work",
            "",
        ]
        .join("\u{1f}");
        std::fs::write(
            &tmux_fixture,
            format!(
                "#!/bin/sh\ncase \"$*\" in\n  *list-panes*) printf '%s\\n' {};;\n  *if-shell*) printf '%s' \"$*\" > {};;\nesac\n",
                shell_words::quote(&pane_line),
                shell_words::quote(&tag_cleanup.to_string_lossy()),
            ),
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&tmux_fixture).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o700);
        std::fs::set_permissions(&tmux_fixture, permissions).unwrap();
        pika.tmux = Tmux::with_executable(tmux_fixture.to_string_lossy(), None);
        std::fs::create_dir_all(archived.parent().unwrap()).unwrap();
        std::fs::write(&archived, "retained history\n").unwrap();
        let mut session = test_session(identity);
        session.name = Some("archived_work".into());
        session.live = false;
        session.status = Status::Parked;
        session.root_pid = None;
        session.tmux_session = Some("pika-c-archived".into());
        session.tmux_pane = Some("%9".into());
        session.transcript_path = Some(archived.display().to_string());
        pika.store.upsert_session(&session, false).unwrap();
        pika.store
            .put_expert_profile(&StoredExpertProfile {
                profile: ExpertProfile {
                    provider: Provider::Codex,
                    session_id: identity.into(),
                    summary: "knows archived work".into(),
                    current_state: "complete".into(),
                    topics: vec!["archive".into()],
                    artifacts: vec!["result.md".into()],
                    source: "fixture".into(),
                    updated_at: 10.0,
                    scope_updated_at: 10.0,
                    current_state_updated_at: 10.0,
                },
                transcript_mtime_ns: Some(1),
                transcript_size: Some(1),
                current_state_mtime_ns: Some(1),
                current_state_size: Some(1),
            })
            .unwrap();
        std::fs::create_dir_all(&pika.paths.codex_home).unwrap();
        let db =
            rusqlite::Connection::open(pika.paths.codex_home.join("state_archive.sqlite")).unwrap();
        db.execute_batch("CREATE TABLE threads(id TEXT PRIMARY KEY,name TEXT,cwd TEXT,rollout_path TEXT,updated_at INTEGER,archived INTEGER)").unwrap();
        db.execute(
            "INSERT INTO threads VALUES(?1,'archived_work','/tmp',?2,10,1)",
            rusqlite::params![identity, archived.display().to_string()],
        )
        .unwrap();

        assert!(pika.reconcile_local().unwrap().sessions.is_empty());
        assert!(pika.cached_inventory().unwrap().sessions.is_empty());
        assert!(
            pika.store
                .get_session(Provider::Codex, identity)
                .unwrap()
                .is_some()
        );
        assert_eq!(pika.store.list_untracked_sessions().unwrap().len(), 1);
        assert!(
            pika.store
                .get_stored_expert_profile(Provider::Codex, identity)
                .unwrap()
                .is_some()
        );
        assert!(
            pika.resolve_local("archived_work")
                .unwrap_err()
                .to_string()
                .contains("archived")
        );
        assert!(pika.store.list_pending().unwrap().is_empty());
        assert!(
            !tag_cleanup.exists(),
            "archive reconciliation must not race an exact reopen through tmux mutation"
        );

        db.execute("UPDATE threads SET archived=0 WHERE id=?1", [identity])
            .unwrap();
        let restored = pika.reconcile_local().unwrap().sessions;
        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].tmux_pane.as_deref(), Some("%9"));
        assert!(pika.store.list_untracked_sessions().unwrap().is_empty());
        assert!(
            pika.store
                .get_stored_expert_profile(Provider::Codex, identity)
                .unwrap()
                .is_some()
        );
    }

    #[cfg(unix)]
    #[test]
    fn archived_pane_cleanup_never_holds_the_store_or_local_action_gate() {
        let (root, mut pika) = test_pika();
        pika.process_observer = Arc::new(|| ProcessObservation::complete(BTreeMap::new()));
        let panes_path = root.path().join("panes");
        let listed_path = root.path().join("panes-listed");
        let cleanup_path = root.path().join("unexpected-cleanup");
        let tmux_fixture = root.path().join("tmux-stalled-cleanup");
        std::fs::create_dir_all(&pika.paths.codex_home).unwrap();
        let db =
            rusqlite::Connection::open(pika.paths.codex_home.join("state_archive.sqlite")).unwrap();
        db.execute_batch("CREATE TABLE threads(id TEXT PRIMARY KEY,name TEXT,cwd TEXT,rollout_path TEXT,updated_at INTEGER,archived INTEGER)").unwrap();

        let mut pane_lines = Vec::new();
        for index in 0..128 {
            let identity = format!("00000000-0000-4000-8001-{index:012x}");
            let transcript = root.path().join(format!("archive-{index}.jsonl"));
            std::fs::write(&transcript, b"history\n").unwrap();
            db.execute(
                "INSERT INTO threads VALUES(?1,?2,'/tmp',?3,10,1)",
                rusqlite::params![
                    identity,
                    format!("archived-{index}"),
                    transcript.display().to_string()
                ],
            )
            .unwrap();
            let mut session = test_session(&identity);
            session.name = Some(format!("archived-{index}"));
            session.live = false;
            session.root_pid = None;
            session.tmux_session = Some(format!("pika-c-{index}"));
            session.tmux_pane = Some(format!("%{index}"));
            session.transcript_path = Some(transcript.display().to_string());
            pika.store.upsert_session(&session, false).unwrap();
            pane_lines.push(
                [
                    format!("pika-c-{index}"),
                    format!("%{index}"),
                    format!("{}", index + 10),
                    "/tmp".into(),
                    "sh".into(),
                    "0".into(),
                    "1".into(),
                    "1".into(),
                    "0".into(),
                    String::new(),
                    "10".into(),
                    "9".into(),
                    "codex".into(),
                    identity,
                    format!("archived-{index}"),
                    String::new(),
                ]
                .join("\u{1f}"),
            );
        }
        std::fs::write(&panes_path, pane_lines.join("\n")).unwrap();
        std::fs::write(
            &tmux_fixture,
            format!(
                "#!/bin/sh\ncase \"$*\" in\n  *list-panes*) : > {}; cat {};;\n  *if-shell*) : > {}; sleep 5;;\nesac\n",
                shell_words::quote(&listed_path.to_string_lossy()),
                shell_words::quote(&panes_path.to_string_lossy()),
                shell_words::quote(&cleanup_path.to_string_lossy()),
            ),
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&tmux_fixture).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o700);
        std::fs::set_permissions(&tmux_fixture, permissions).unwrap();
        pika.tmux = Tmux::with_executable(tmux_fixture.to_string_lossy(), None);
        let hook_id = "99999999-9999-4999-8999-999999999999";
        let mut hook_session = test_session(hook_id);
        hook_session.live = false;
        hook_session.root_pid = None;
        pika.store.upsert_session(&hook_session, false).unwrap();

        let worker = pika.clone();
        let reconcile = std::thread::spawn(move || worker.reconcile_local());
        for _ in 0..500 {
            if listed_path.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(
            listed_path.exists(),
            "reconciliation never observed the panes"
        );

        let invalidated_at = std::time::Instant::now();
        pika.invalidate_local_reconciliation();
        assert!(invalidated_at.elapsed() < Duration::from_millis(500));

        let payload = parse_hook_payload(
            serde_json::to_vec(&serde_json::json!({
                "session_id":hook_id,
                "hook_event_name":"Stop",
                "cwd":"/tmp",
            }))
            .unwrap()
            .as_slice(),
            Provider::Codex,
        )
        .unwrap();
        let hook_at = std::time::Instant::now();
        handle_hook(
            &pika.store,
            Provider::Codex,
            &payload,
            &HookContext::at(20_000.0),
        )
        .unwrap();
        assert!(hook_at.elapsed() < Duration::from_millis(500));
        let _ = reconcile.join().unwrap();
        assert!(
            !cleanup_path.exists(),
            "reconciliation invoked external tmux cleanup"
        );
        assert_eq!(
            pika.store
                .get_session(Provider::Codex, hook_id)
                .unwrap()
                .unwrap()
                .status,
            Status::Ready
        );
    }

    #[test]
    fn concurrent_exact_reopen_supersedes_archive_hiding_without_losing_binding() {
        let (root, mut pika) = test_pika();
        let identity = "77777777-7777-4777-8777-777777777770";
        let transcript = root.path().join("archived-reopen.jsonl");
        std::fs::write(&transcript, b"history\n").unwrap();
        std::fs::create_dir_all(&pika.paths.codex_home).unwrap();
        let db =
            rusqlite::Connection::open(pika.paths.codex_home.join("state_archive.sqlite")).unwrap();
        db.execute_batch("CREATE TABLE threads(id TEXT PRIMARY KEY,name TEXT,cwd TEXT,rollout_path TEXT,updated_at INTEGER,archived INTEGER)").unwrap();
        db.execute(
            "INSERT INTO threads VALUES(?1,'archived-reopen','/tmp',?2,10,1)",
            rusqlite::params![identity, transcript.display().to_string()],
        )
        .unwrap();
        let mut stale = test_session(identity);
        stale.name = Some("archived-reopen".into());
        stale.live = false;
        stale.root_pid = None;
        stale.tmux_session = Some("pika-c-old".into());
        stale.tmux_pane = Some("%1".into());
        stale.transcript_path = Some(transcript.display().to_string());
        pika.store.upsert_session(&stale, false).unwrap();

        let observed = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let worker_observed = Arc::clone(&observed);
        let worker_release = Arc::clone(&release);
        pika.process_observer = Arc::new(move || {
            worker_observed.wait();
            worker_release.wait();
            ProcessObservation::complete(BTreeMap::new())
        });
        let worker = pika.clone();
        let reconcile = std::thread::spawn(move || worker.reconcile_local());
        observed.wait();

        let other = Store::from_paths(&pika.paths);
        let mut reopened = stale;
        reopened.live = true;
        reopened.root_pid = Some(44_444);
        reopened.tmux_session = Some("pika-c-reopened".into());
        reopened.tmux_pane = Some("%44".into());
        other.upsert_session(&reopened, false).unwrap();
        release.wait();

        let error = reconcile.join().unwrap().unwrap_err().to_string();
        assert!(error.contains("superseded"));
        let preserved = pika
            .store
            .get_session(Provider::Codex, identity)
            .unwrap()
            .unwrap();
        assert_eq!(preserved.root_pid, Some(44_444));
        assert_eq!(preserved.tmux_session.as_deref(), Some("pika-c-reopened"));
        assert_eq!(preserved.tmux_pane.as_deref(), Some("%44"));
        assert!(
            !pika.store.is_untracked(Provider::Codex, identity).unwrap(),
            "stale archive observation hid the concurrently reopened exact row"
        );
    }

    #[test]
    fn missing_saved_directory_is_not_replaced_or_left_reserved() {
        let (root, pika) = test_pika();
        let mut session = test_session("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa");
        session.provider = Provider::Claude;
        session.cwd = Some(root.path().join("deleted-project").display().to_string());
        let error = pika
            .resume_session(session.clone(), false)
            .unwrap_err()
            .to_string();
        assert!(error.contains("saved working directory no longer exists"));
        assert!(
            pika.store
                .get_resume_reservation(session.provider, &session.session_id)
                .unwrap()
                .is_none()
        );
        assert!(pika.store.list_pending().unwrap().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn launcher_to_native_handoff_preserves_exact_identity_and_duplicate_guard() {
        use std::os::unix::fs::PermissionsExt;
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct Child(std::process::Child);
        impl Drop for Child {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let child = Child(
            std::process::Command::new("/bin/sleep")
                .arg("60")
                .spawn()
                .unwrap(),
        );
        let other = Child(
            std::process::Command::new("/bin/sleep")
                .arg("60")
                .spawn()
                .unwrap(),
        );
        let (root, mut pika) = test_pika();
        let identity = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
        let pid = i64::from(std::process::id());
        let start = process::process_start_time(pid).unwrap();
        let native_pid = i64::from(child.0.id());
        let other_pid = i64::from(other.0.id());
        let native_start = process::process_start_time(native_pid).unwrap();
        let other_start = process::process_start_time(other_pid).unwrap();
        let row = [
            "pika-c-test".to_owned(),
            "%1".into(),
            pid.to_string(),
            "/tmp".into(),
            "codex".into(),
            "0".into(),
            "1".into(),
            "1".into(),
            "0".into(),
            String::new(),
            "1".into(),
            "1".into(),
            "codex".into(),
            identity.into(),
            "test".into(),
            "launch-a".into(),
        ]
        .join("\u{1f}");
        let executable = root.path().join("tmux-fixture");
        std::fs::write(&executable, format!(
            "#!/bin/sh\ncase \"$*\" in\n *list-panes*) printf '%s\\n' '{row}' ;;\n *) exit 0 ;;\nesac\n"
        )).unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        pika.tmux = Tmux::with_executable(executable.to_string_lossy(), None);
        let phase = Arc::new(AtomicUsize::new(0));
        let observed_phase = Arc::clone(&phase);
        pika.process_observer = Arc::new(move || {
            let mut records = BTreeMap::from([(
                pid,
                record(pid, None, start, &["node", "codex", "resume", identity]),
            )]);
            if observed_phase.load(Ordering::SeqCst) > 0 {
                records.insert(
                    native_pid,
                    record(
                        native_pid,
                        Some(pid),
                        native_start,
                        &["codex", "resume", identity],
                    ),
                );
            }
            if observed_phase.load(Ordering::SeqCst) > 1 {
                records.insert(
                    other_pid,
                    record(
                        other_pid,
                        Some(pid),
                        other_start,
                        &["codex", "resume", identity],
                    ),
                );
            }
            ProcessObservation::complete(records)
        });
        let mut session = test_session(identity);
        session.tmux_pane = Some("%1".into());
        session.status = Status::Ready;
        session.unread = true;
        session.last_event_at = 42.0;
        pika.store.upsert_session(&session, true).unwrap();
        let launcher = pika.exact_pane_binding(&session, Some("%1")).unwrap();
        assert_eq!(launcher.provider_pid, pid);
        phase.store(1, Ordering::SeqCst);
        let native = pika.exact_pane_binding(&session, Some("%1")).unwrap();
        assert_eq!(native.provider_pid, native_pid);
        assert!(
            !same_exact_binding(&launcher, &native),
            "capture must still detect a changed process"
        );
        assert!(
            !same_handoff_binding(&native, &launcher),
            "no reverse transition"
        );
        let mut replacement = native.clone();
        replacement.provider_pid = other_pid;
        replacement.provider_start_time = other_start;
        assert!(
            !same_handoff_binding(&native, &replacement),
            "a sibling restart is not the original native client"
        );
        let mut retagged = native.clone();
        retagged.pane.pika_launch_token = Some("launch-b".into());
        assert!(!same_handoff_binding(&launcher, &retagged));
        pika.confirm_exact_handoff(&session, &launcher)
            .expect("same launcher generation handing off to its exact native child must attach");
        let mut reused = launcher.clone();
        reused.provider_start_time += 1;
        assert!(pika.confirm_exact_handoff(&session, &reused).is_err());
        let mut replaced_pane = launcher.clone();
        replaced_pane.pane_start_time += 1;
        assert!(
            pika.confirm_exact_handoff(&session, &replaced_pane)
                .is_err()
        );
        phase.store(2, Ordering::SeqCst);
        let error = pika
            .record_exact_handoff(&session, &launcher, 42.0)
            .unwrap_err()
            .to_string();
        assert!(error.contains("OPEN TWICE"), "{error}");
        assert!(
            pika.store
                .get_session(session.provider, identity)
                .unwrap()
                .unwrap()
                .unread
        );
        phase.store(1, Ordering::SeqCst);
        pika.record_exact_handoff(&session, &launcher, 42.0)
            .unwrap();
        assert!(
            !pika
                .store
                .get_session(session.provider, identity)
                .unwrap()
                .unwrap()
                .unread
        );
    }

    #[cfg(unix)]
    #[test]
    fn rejected_exact_attach_preserves_unread_attention() {
        use std::os::unix::fs::PermissionsExt;

        let (root, mut pika) = test_pika();
        let identity = "99999999-9999-4999-8999-999999999999";
        let pid = i64::from(std::process::id());
        let start_time = process::process_start_time(pid).expect("test process has a generation");
        let separator = '\u{1f}'.to_string();
        let row = [
            "pika-c-portfolio".to_owned(),
            "%1".to_owned(),
            pid.to_string(),
            "/tmp".to_owned(),
            "codex".to_owned(),
            "0".to_owned(),
            "1".to_owned(),
            "1".to_owned(),
            "0".to_owned(),
            String::new(),
            "1".to_owned(),
            "1".to_owned(),
            "codex".to_owned(),
            identity.to_owned(),
            "portfolio_review".to_owned(),
            String::new(),
        ]
        .join(&separator);
        let executable = root.path().join("tmux-fixture");
        std::fs::write(
            &executable,
            format!(
                "#!/bin/sh\ncase \"$*\" in\n  *list-panes*) printf '%s\\n' '{row}' ;;\n  *if-shell*attach-session*) sleep 0.25; exit 75 ;;\n  *) exit 0 ;;\nesac\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        pika.tmux = Tmux::with_executable(executable.to_string_lossy(), None);
        pika.process_observer = Arc::new(move || {
            ProcessObservation::complete(BTreeMap::from([(
                pid,
                record(pid, None, start_time, &["codex", "resume", identity]),
            )]))
        });

        let mut session = test_session(identity);
        session.tmux_session = Some("pika-c-portfolio".into());
        session.tmux_pane = Some("%1".into());
        session.root_pid = Some(pid);
        session.status = Status::Ready;
        session.unread = true;
        session.last_event_at = 42.0;
        pika.store.upsert_session(&session, false).unwrap();
        pika.store
            .record_status_observation(
                Provider::Codex,
                identity,
                &crate::model::StatusObservation {
                    kind: ObservationKind::Lifecycle,
                    status: Status::Ready,
                    unread: true,
                    attention_reason: Some("result ready".into()),
                    error: None,
                    observed_at: 42.0,
                    source: "test".into(),
                },
            )
            .unwrap();

        let receipt = pika.open_session(session, true).unwrap();
        assert_eq!(receipt.exit_code, 75);
        let stored = pika
            .store
            .get_session(Provider::Codex, identity)
            .unwrap()
            .unwrap();
        assert_eq!(stored.status, Status::Ready);
        assert!(
            stored.unread,
            "a rejected handoff must not acknowledge attention"
        );
    }

    #[cfg(unix)]
    #[test]
    fn proven_exact_attach_acknowledges_only_the_selected_event() {
        assert_exact_attach_event_boundary(false);
    }

    #[cfg(unix)]
    #[test]
    fn recovered_exact_attach_records_open_and_preserves_newer_event() {
        assert_exact_attach_event_boundary(true);
    }

    #[cfg(unix)]
    fn assert_exact_attach_event_boundary(recovery: bool) {
        use std::os::unix::fs::PermissionsExt;
        use std::sync::atomic::{AtomicBool, Ordering};

        let (root, mut pika) = test_pika();
        let identity = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
        let provider_pid = i64::from(std::process::id());
        let start_time =
            process::process_start_time(provider_pid).expect("test process has a generation");
        let separator = '\u{1f}'.to_string();
        let row = [
            "pika-c-portfolio".to_owned(),
            "%1".to_owned(),
            provider_pid.to_string(),
            "/tmp".to_owned(),
            "codex".to_owned(),
            "0".to_owned(),
            "1".to_owned(),
            "1".to_owned(),
            "0".to_owned(),
            String::new(),
            "1".to_owned(),
            "1".to_owned(),
            "codex".to_owned(),
            identity.to_owned(),
            "portfolio_review".to_owned(),
            String::new(),
        ]
        .join(&separator);
        let client_pid = root.path().join("client-pid");
        let handoff_seen = root.path().join("handoff-seen");
        let executable = root.path().join("tmux-fixture");
        std::fs::write(
            &executable,
            format!(
                "#!/bin/sh\ncase \"$*\" in\n  *list-panes*) printf '%s\\n' '{row}' ;;\n  *'list-clients -F #{{client_name}}'*) if [ -f {client_pid} ]; then touch {handoff_seen}; printf 'invoking-client\\037%s\\037%%1\\n' \"$(cat {client_pid})\"; fi ;;\n  *list-clients*) if [ -f {client_pid} ]; then touch {handoff_seen}; printf '%s\\t%%1\\n' \"$(cat {client_pid})\"; fi ;;\n  *if-shell*attach-session*) printf '%s' \"$$\" > {client_pid}; sleep 0.25; exit 0 ;;\n  *) exit 0 ;;\nesac\n",
                client_pid = shell_words::quote(&client_pid.to_string_lossy()),
                handoff_seen = shell_words::quote(&handoff_seen.to_string_lossy()),
            ),
        )
        .unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        pika.tmux = Tmux::with_executable(executable.to_string_lossy(), None);

        let mut session = test_session(identity);
        session.tmux_session = Some("pika-c-portfolio".into());
        session.tmux_pane = Some("%1".into());
        session.root_pid = Some(provider_pid);
        session.status = Status::Ready;
        session.unread = true;
        session.last_event_at = 42.0;
        let mut newer_session = session.clone();
        newer_session.last_event_at = 43.0;
        newer_session.attention_reason = Some("newer result ready".into());

        let store = pika.store.clone();
        let marker = handoff_seen.clone();
        let inserted = Arc::new(AtomicBool::new(false));
        let inserted_from_observer = Arc::clone(&inserted);
        let observed = record(
            provider_pid,
            None,
            start_time,
            &["codex", "resume", identity],
        );
        pika.process_observer = Arc::new(move || {
            if marker.exists() && !inserted_from_observer.swap(true, Ordering::SeqCst) {
                store.upsert_session(&newer_session, false).unwrap();
                store
                    .record_status_observation(
                        Provider::Codex,
                        identity,
                        &crate::model::StatusObservation {
                            kind: ObservationKind::Lifecycle,
                            status: Status::Ready,
                            unread: true,
                            attention_reason: Some("newer result ready".into()),
                            error: None,
                            observed_at: 43.0,
                            source: "test-newer-event".into(),
                        },
                    )
                    .unwrap();
            }
            ProcessObservation::complete(BTreeMap::from([(provider_pid, observed.clone())]))
        });

        pika.store.upsert_session(&session, false).unwrap();
        pika.store
            .record_status_observation(
                Provider::Codex,
                identity,
                &crate::model::StatusObservation {
                    kind: ObservationKind::Lifecycle,
                    status: Status::Ready,
                    unread: true,
                    attention_reason: Some("selected result ready".into()),
                    error: None,
                    observed_at: 42.0,
                    source: "test-selected-event".into(),
                },
            )
            .unwrap();

        let receipt = if recovery {
            pika.recover_existing_session(session, true)
        } else {
            pika.open_session(session, true)
        }
        .unwrap();
        assert_eq!(receipt.exit_code, 0);
        assert!(inserted.load(Ordering::SeqCst));
        assert_eq!(
            pika.store.get_meta("last_attached").unwrap(),
            Some(serde_json::to_string(&("codex", identity)).unwrap())
        );
        let stored = pika
            .store
            .get_session(Provider::Codex, identity)
            .unwrap()
            .unwrap();
        assert_eq!(stored.last_event_at, 43.0);
        assert!(
            stored.unread,
            "the newer concurrent event must survive handoff"
        );
        assert_eq!(
            stored.attention_reason.as_deref(),
            Some("newer result ready")
        );
    }
}
