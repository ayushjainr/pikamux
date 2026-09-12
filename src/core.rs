use crate::{
    config::Config,
    model::{Candidate, ObservationKind, Pane, Provider, Session, Status},
    open_history,
    paths::Paths,
    process::{self, ProcessObservation, ProcessRecord},
    providers::Providers,
    resolve::{EvidenceState, NameCandidate, NameResolutionError, SelectionEvidence, resolve_name},
    status::{ProjectionFallback, project_status},
    store::{PendingLaunch, ReconcileLedger, Store},
    tmux::Tmux,
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
}

#[derive(Clone, Debug)]
pub struct ExactPaneBinding {
    pub pane: Pane,
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
}

#[derive(Default)]
struct LocalReconcileFence {
    generation: AtomicU64,
    write_gate: Mutex<()>,
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
            pending: self.store.list_pending()?,
        })
    }

    /// Reconcile local identity and lifecycle evidence once. Remote machines are
    /// deliberately outside this operation so an offline node cannot stall it.
    pub fn reconcile_local(&self) -> Result<Inventory> {
        self.reconcile_local_with_candidates()
            .map(|observed| observed.inventory)
    }

    /// Fence an observation that was started for a board which has now closed.
    ///
    /// Reconciliation deliberately performs slow provider/process reads before
    /// taking its write gate. Bumping this generation under that same gate lets
    /// an exact board action proceed immediately without allowing an older
    /// observation to commit after it.
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

    fn reconciled_write<T>(&self, generation: u64, write: impl FnOnce() -> Result<T>) -> Result<T> {
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
        write()
    }

    fn reconcile_local_with_candidates(&self) -> Result<ReconciledInventory> {
        let reconcile_generation = self
            .local_reconcile_fence
            .generation
            .load(Ordering::Acquire);
        self.store.initialize()?;
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
        // Match the frozen durable-state contract: provider-proven archive or
        // deletion removes the row from daily observation. Pane tags are cleared
        // only through a generation-guarded mutation and remain best effort.
        self.reconciled_write(reconcile_generation, || {
            for session in &removed {
                for pane in panes.iter().filter(|pane| {
                    pane.pika_provider == Some(session.provider)
                        && pane.pika_session_id.as_deref() == Some(&session.session_id)
                }) {
                    let _ = self.tmux.clear_tags_if_unchanged(pane);
                }
            }
            Ok(())
        })?;
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
            let distinct_name = candidate
                .name
                .as_deref()
                .zip(parent.name.as_deref())
                .is_some_and(|(child, parent)| !child.eq_ignore_ascii_case(parent));
            if !distinct_name
                || known.contains(&(Provider::Codex, candidate.session_id.clone()))
                || parent.active_thread_id.as_deref() == Some(&candidate.session_id)
            {
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
            self.reconciled_write(reconcile_generation, || {
                self.store.reconcile_transaction(|ledger| {
                    for session in batch {
                        let state = match source_states
                            .get(&(session.provider, session.session_id.clone()))
                        {
                            Some(crate::providers::ProviderSourceState::Archived) => "archived",
                            Some(crate::providers::ProviderSourceState::Deleted) => "deleted",
                            _ => "source-unavailable",
                        };
                        ledger.hide_provider_session(
                            session.provider,
                            &session.session_id,
                            state,
                        )?;
                    }
                    Ok(())
                })
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
            self.reconciled_write(reconcile_generation, || {
                self.store.reconcile_transaction(|ledger| {
                    for session in batch {
                        ledger.restore_provider_session(session.provider, &session.session_id)?;
                    }
                    Ok(())
                })
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
            let projected = self.reconciled_write(reconcile_generation, || {
                self.store.reconcile_transaction(|ledger| {
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
                        ledger.upsert_session(&session, false)?;
                        projected.push(session);
                    }
                    Ok(projected)
                })
            })?;
            runtime_sessions.extend(
                projected
                    .into_iter()
                    .map(|session| ((session.provider, session.session_id.clone()), session)),
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        for batch in fork_imports.chunks(RECONCILE_WRITE_BATCH) {
            self.reconciled_write(reconcile_generation, || {
                self.store.reconcile_transaction(|ledger| {
                    for fork in batch {
                        let _ = ledger.upsert_session(fork, false)?;
                    }
                    Ok(())
                })
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
                pending: self.store.list_pending()?,
            },
            candidates: candidate_map,
        })
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
        let candidate_panes: Vec<(&Pane, i64)> = tagged
            .iter()
            .filter_map(|pane| {
                let provider_pid =
                    process::provider_process(pane.pane_pid, Some(session.provider), processes)?;
                owners
                    .proves_pane(provider_pid, pane)
                    .then_some((*pane, provider_pid))
            })
            .collect();

        let safety = if !continuation_conflicts.is_empty()
            || identities.len() > 1
            || candidate_panes.len() > 1
            || (!candidate_panes.is_empty() && !outside.is_empty())
        {
            Some(Status::OpenTwice)
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
        session.home_state = if safety.is_some() {
            "open_twice"
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
            if let Some(pid) = outside.first() {
                session.root_pid = Some(*pid);
            } else {
                session.root_pid = None;
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
            let error = if continuation_conflicts.is_empty() {
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
            .map(|session| (session.provider, session.session_id))
            .collect();
        let mut candidates = Vec::new();
        for provider in Provider::ALL {
            candidates.extend(providers.import_candidates(provider).into_iter().filter(
                |candidate| !excluded.contains(&(candidate.provider, candidate.session_id.clone())),
            ));
        }
        candidates.sort_by(|left, right| right.updated_at.total_cmp(&left.updated_at));
        candidates.dedup_by(|left, right| {
            left.provider == right.provider && left.session_id == right.session_id
        });
        Ok(candidates)
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
            .chain(Provider::ALL.into_iter().flat_map(|provider| {
                providers
                    .import_candidates(provider)
                    .into_iter()
                    .map(move |candidate| (candidate.provider, candidate.session_id))
            }))
            .collect();
        let mut candidates = Provider::ALL
            .into_iter()
            .flat_map(|provider| providers.browse(provider))
            .filter(|candidate| {
                !excluded.contains(&(candidate.provider, candidate.session_id.clone()))
            })
            .collect::<Vec<_>>();
        candidates.sort_by(|left, right| right.updated_at.total_cmp(&left.updated_at));
        candidates.dedup_by(|left, right| {
            left.provider == right.provider && left.session_id == right.session_id
        });
        candidates.truncate(limit);
        Ok(candidates)
    }

    pub fn adopt_candidate(&self, candidate: &Candidate) -> Result<bool> {
        self.store.initialize()?;
        self.store
            .restore_tracking(candidate.provider, &candidate.session_id)?;
        self.store
            .upsert_session(&session_from_candidate(candidate), false)
    }

    pub fn resolve_local(&self, query: &str) -> Result<Vec<Session>> {
        let providers = Providers::new(&self.paths, &self.config);
        // Persisted rows deliberately have no live ownership. Reduce daily
        // names only from the fresh ownership snapshot, keeping provider clocks
        // separate from Pika's resume/hook/reconciliation activity clocks.
        let ReconciledInventory {
            inventory,
            candidates: mut metadata,
        } = self.reconcile_local_with_candidates()?;
        let mut sessions = inventory.sessions;
        // An explicit daily-name or UUID lookup is also the recovery path for
        // a conversation the user previously stopped watching. Keep the
        // tombstone in place while resolving so read-only lookups and
        // ambiguous choices have no side effect; `open_session` removes only
        // the exact selected tombstone immediately before the open action.
        sessions.extend(self.store.list_untracked_sessions()?);
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
        if matches.len() != 1
            || expected_pane.is_some_and(|expected| matches[0].pane_id != expected)
        {
            bail!(
                "Pika cannot bind one exact pane to {} {} (found {}). No pane action was performed.",
                session.provider,
                session.session_id,
                matches.len()
            );
        }
        let pane = matches[0];
        let owners = self
            .store
            .reconcile_transaction(|ledger| identity_owners(session, processes, ledger, now()))?;
        let identity_pids = owners.pids();
        if identity_pids.len() != 1 {
            bail!(
                "Pika cannot bind the pane to one exact {} process (found {}). No pane action was performed.",
                session.provider,
                identity_pids.len()
            );
        }
        let provider_pid = *identity_pids.first().expect("one identity PID");
        if !owners.proves_pane(provider_pid, pane) {
            bail!(
                "the exact pane has no UUID argv or matching certified launch generation; no pane action was performed"
            );
        }
        let tree = process::process_tree(pane.pane_pid, processes);
        if !tree.contains(&provider_pid) {
            bail!(
                "the exact provider process is outside the tagged pane; no pane action was performed"
            );
        }
        let provider_start_time = processes
            .get(&provider_pid)
            .map(|record| record.start_time)
            .context("the exact provider process disappeared from the complete observation")?;
        if process::process_start_time(provider_pid) != Some(provider_start_time) {
            bail!("the exact provider process generation changed; no pane action was performed");
        }
        let fresh = self
            .tmux
            .get_pane(&pane.pane_id)?
            .context("the exact pane disappeared before the action")?;
        if !same_pane_generation(pane, &fresh)
            || fresh.pika_provider != Some(session.provider)
            || fresh.pika_session_id.as_deref() != Some(&session.session_id)
            || fresh.pika_launch_token != pane.pika_launch_token
            || process::process_start_time(provider_pid) != Some(provider_start_time)
        {
            bail!("the exact pane or provider generation changed; no pane action was performed");
        }
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
            provider_pid,
            provider_start_time,
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
        if !same_exact_binding(expected, &current) {
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
        if !allow_create {
            return Err(OpenError::NotFound(query.to_owned()).into());
        }
        self.new_session(query, self.config.default_provider, attach)
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
        let inventory = self.reconcile_local()?;
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
        if session.has_exact_home() {
            let binding = self.exact_pane_binding(&session, session.tmux_pane.as_deref())?;
            let event = session.last_event_at;
            let code = if attach {
                self.tmux.attach_exact_with_started(&binding.pane, || {
                    self.record_exact_handoff(&session, &binding, event)
                })?
            } else {
                0
            };
            return Ok(OpenReceipt {
                target: OpenTarget::Session(Box::new(session)),
                kind: "ATTACHED LIVE",
                exit_code: code,
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
        let code = if attach {
            self.tmux.attach_exact_with_started(&pane, || {
                open_history::record_pending(&self.store, &pending)
            })?
        } else {
            0
        };
        Ok(OpenReceipt {
            target: OpenTarget::Pending(Box::new(pending)),
            kind: "ATTACHED STARTING",
            exit_code: code,
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
                self.store.delete_pending(&token)?;
                let code = if attach {
                    self.tmux.attach_exact_with_started(&binding.pane, || {
                        self.record_exact_handoff(&session, &binding, selected_event)
                    })?
                } else {
                    0
                };
                return Ok(OpenReceipt {
                    target: OpenTarget::Session(Box::new(session.clone())),
                    kind: "ATTACHED LIVE",
                    exit_code: code,
                });
            }
            let providers = Providers::new(&self.paths, &self.config);
            let argv = providers.resume_argv(session.provider, &identity);
            let environment = launch_environment(
                session.provider,
                Some(&session.session_id),
                &token,
                &session.display_name(),
            );
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
            let code = if attach {
                self.tmux.attach_exact_with_started(&binding.pane, || {
                    self.record_exact_handoff(&session, &binding, selected_event)
                })?
            } else {
                0
            };
            Ok(OpenReceipt {
                target: OpenTarget::Session(Box::new(session.clone())),
                kind: "RESUMED EXACT",
                exit_code: code,
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
            let environment = launch_environment(provider, reserved.as_deref(), &token, name);
            let allocated = self.tmux.create_holding_session(&internal, &cwd)?;
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
                provider,
                reserved.as_deref(),
                name,
                &token,
            )?;
            if !self
                .store
                .set_launch_phase(&token, crate::store::LaunchPhase::PanePrepared)?
            {
                bail!("the recoverable launch record disappeared before provider execution")
            }
            if let Some(session_id) = reserved.as_deref()
                && !self.store.bind_launch(&token, provider, session_id)?
            {
                bail!("the recoverable launch token was already bound to another conversation")
            }
            if !self
                .store
                .set_launch_phase(&token, crate::store::LaunchPhase::ProviderStarting)?
            {
                bail!("the recoverable launch record disappeared before provider execution")
            }
            let ready_observation = self.observe_processes();
            let ready_processes = require_complete_processes(
                &ready_observation,
                "execute the reserved provider launch",
            )?;
            if let Some(session_id) = reserved.as_deref() {
                let appeared =
                    process::find_session_processes(session_id, provider, ready_processes);
                if !appeared.is_empty() {
                    bail!(
                        "the reserved conversation UUID acquired another owner before provider execution. No second provider was launched."
                    )
                }
            }
            require_idle_pane(&prepared, ready_processes, true)?;
            let pane = self.tmux.start_prepared_agent(
                &prepared,
                &cwd,
                provider,
                &argv,
                &environment,
                reserved.as_deref(),
                name,
                &token,
            )?;
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
            let code = if attach {
                self.tmux.attach_exact_with_started(&pane, || {
                    open_history::record_pending(&self.store, &current)
                })?
            } else {
                0
            };
            Ok(OpenReceipt {
                target: OpenTarget::Pending(Box::new(current)),
                kind: "NEW HOME",
                exit_code: code,
            })
        })()
    }
}

pub fn session_from_pending(pending: &PendingLaunch) -> Session {
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
        status: Status::Starting,
        unread: false,
        model: None,
        source: "pending-launch".into(),
        managed: true,
        error: None,
        attention_reason: Some("starting provider conversation".into()),
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
        && left.provider_pid == right.provider_pid
        && left.provider_start_time == right.provider_start_time
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
        model::ExpertProfile,
        store::{LiveOwner, StoredExpertProfile},
    };
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

    #[test]
    fn superseded_board_observation_cannot_erase_new_exact_identity() {
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

        // This is the board action boundary: an exact action may now certify a
        // newer owner while the old process/provider observation is still
        // blocked. The old observation must never become authoritative later.
        pika.invalidate_local_reconciliation();
        let mut exact = stale;
        exact.live = true;
        exact.root_pid = Some(99_999);
        exact.tmux_session = Some("pika-c-new-home".into());
        exact.tmux_pane = Some("%99".into());
        pika.store.upsert_session(&exact, false).unwrap();

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
        let inventory = reconcile.join().unwrap().unwrap();
        for hook in hooks {
            hook.join().unwrap().unwrap();
        }

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
                    "0".into(),
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
        let tag_cleanup = root.path().join("tag-cleanup");
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
        let cleanup = std::fs::read_to_string(tag_cleanup).unwrap();
        assert!(cleanup.contains("if-shell -F -t %9"));
        assert!(
            cleanup.contains("set-option -p -u -t '%9' @pika_session_id"),
            "{cleanup}"
        );

        db.execute("UPDATE threads SET archived=0 WHERE id=?1", [identity])
            .unwrap();
        assert_eq!(pika.reconcile_local().unwrap().sessions.len(), 1);
        assert!(pika.store.list_untracked_sessions().unwrap().is_empty());
        assert!(
            pika.store
                .get_stored_expert_profile(Provider::Codex, identity)
                .unwrap()
                .is_some()
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
                "#!/bin/sh\ncase \"$*\" in\n  *list-panes*) printf '%s\\n' '{row}' ;;\n  *list-clients*) if [ -f {client_pid} ]; then touch {handoff_seen}; printf '%s\\t%%1\\n' \"$(cat {client_pid})\"; fi ;;\n  *if-shell*attach-session*) printf '%s' \"$$\" > {client_pid}; sleep 0.25; exit 0 ;;\n  *) exit 0 ;;\nesac\n",
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

        let receipt = pika.open_session(session, true).unwrap();
        assert_eq!(receipt.exit_code, 0);
        assert!(inserted.load(Ordering::SeqCst));
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
