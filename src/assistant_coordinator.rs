//! Bounded, provider-neutral coordinator for the persistent assistant.
//!
//! The coordinator consumes already-authorized activity observations.  It does
//! not inspect transcripts, files, provider processes, or the live board.
//! Model/provider execution is an injected worker and is never performed while
//! constructing a view snapshot.

use crate::assistant_memory::{
    DecisionState, MemoryError, NewRecord, Origin, Record, RecordKind, Scope, Store,
};
use crate::assistant_policy::{AssistantPolicy, DeliveryOutcome, PolicyError, ReservationState};
use rusqlite::{Connection, params};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use thiserror::Error;

pub const MAX_OBSERVATIONS: usize = 256;
pub const MAX_JOBS: usize = 128;
pub const MAX_WORKERS: usize = 8;
pub const MAX_SUMMARY_BYTES: usize = 4 * 1024;

/// An operational observation supplied by the shared activity feed.  It is not
/// a transcript and deliberately has no free-form provider output field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OperationalObservation {
    pub node_id: String,
    pub provider: String,
    pub conversation_id: String,
    pub project: Option<String>,
    pub status: ObservationStatus,
    pub summary: String,
    pub observed_at: i64,
    pub material_revision: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObservationStatus {
    Working,
    Ready,
    NeedsYou,
    Error,
    Parked,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MaterialChange {
    pub key: ObservationKey,
    pub previous_revision: Option<u64>,
    pub revision: u64,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct ObservationKey {
    pub node_id: String,
    pub provider: String,
    pub conversation_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ViewSnapshot {
    pub observations: Vec<OperationalObservation>,
    pub memory: Vec<Record>,
    pub revision: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkerRequest {
    pub job_id: String,
    pub root_id: String,
    pub scope: Scope,
    pub prompt: String,
    /// Exact assistant-memory records permitted as context; never inferred
    /// from the prompt or a provider transcript.
    pub dependency_ids: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkerResult {
    Completed { text: String },
    Failed { message: String },
    Unknown,
}

pub trait CancellableWorker {
    fn execute(&mut self, request: &WorkerRequest, cancelled: &AtomicBool) -> WorkerResult;
}

#[derive(Debug, Error)]
pub enum CoordinatorError {
    #[error("memory: {0}")]
    Memory(#[from] MemoryError),
    #[error("policy: {0}")]
    Policy(#[from] PolicyError),
    #[error("invalid observation: {0}")]
    InvalidObservation(String),
    #[error("job queue is full")]
    QueueFull,
    #[error("job already exists")]
    DuplicateJob,
    #[error("root must be an envelope and cannot be dispatched")]
    RootDispatch,
}

#[derive(Clone, Debug)]
struct QueuedJob {
    request: WorkerRequest,
    child_id: String,
    cancelled: std::sync::Arc<AtomicBool>,
    forget_epoch: u64,
}

struct ChildIntent<'a> {
    request: &'a WorkerRequest,
    root_id: &'a str,
    child_id: &'a str,
    scope_json: &'a str,
    dependency_json: &'a str,
    forget_epoch: u64,
    now: i64,
}

pub struct AssistantCoordinator {
    memory: Store,
    policy: AssistantPolicy,
    observation_db: Connection,
    queue_db: Connection,
    observations: HashMap<ObservationKey, OperationalObservation>,
    jobs: VecDeque<QueuedJob>,
    running: HashMap<String, Arc<AtomicBool>>,
    quarantined: Vec<String>,
    active: usize,
    revision: u64,
}

impl AssistantCoordinator {
    pub fn reserve_root_envelope(
        &mut self,
        root_id: &str,
        calls: u64,
        now: i64,
    ) -> Result<(), CoordinatorError> {
        self.policy.reserve_root(root_id, calls, false, now, None)?;
        Ok(())
    }
    pub fn new(memory: Store, mut policy: AssistantPolicy) -> Result<Self, CoordinatorError> {
        let observation_db =
            open_observation_db(memory.path().with_extension("observations.sqlite"))?;
        let queue_db = open_queue_db(memory.path().with_extension("queue.sqlite"))?;
        let current_forget_epoch = memory.forget_epoch()?;
        let (jobs, quarantined) =
            recover_queued_jobs(&queue_db, &mut policy, current_forget_epoch)?;
        let observations = load_observations(&observation_db)?;
        Ok(Self {
            memory,
            policy,
            observation_db,
            queue_db,
            observations,
            jobs,
            running: HashMap::new(),
            quarantined,
            active: 0,
            revision: 0,
        })
    }

    pub fn observe(
        &mut self,
        input: Vec<OperationalObservation>,
    ) -> Result<Vec<MaterialChange>, CoordinatorError> {
        if input.len() > MAX_OBSERVATIONS {
            return Err(CoordinatorError::InvalidObservation(
                "observation batch is too large".into(),
            ));
        }
        for item in &input {
            validate_observation(item)?;
        }
        let mut incoming_keys = std::collections::HashSet::new();
        for item in &input {
            incoming_keys.insert((
                item.node_id.clone(),
                item.provider.clone(),
                item.conversation_id.clone(),
            ));
        }
        if self.observations.len()
            + incoming_keys.len().saturating_sub(
                incoming_keys
                    .iter()
                    .filter(|(n, p, c)| {
                        self.observations.contains_key(&ObservationKey {
                            node_id: (*n).clone(),
                            provider: (*p).clone(),
                            conversation_id: (*c).clone(),
                        })
                    })
                    .count(),
            )
            > MAX_OBSERVATIONS
        {
            return Err(CoordinatorError::InvalidObservation(
                "cumulative observation bound exceeded".into(),
            ));
        }
        let mut changes = Vec::new();
        for item in input {
            let key = ObservationKey {
                node_id: item.node_id.clone(),
                provider: item.provider.clone(),
                conversation_id: item.conversation_id.clone(),
            };
            let old = self.observations.get(&key);
            if old.is_some_and(|previous| item.observed_at < previous.observed_at) {
                continue;
            }
            // Timestamp-only refreshes coalesce: they update freshness but never
            // create a durable briefing or a paid worker job.
            let material = old.is_none_or(|previous| {
                previous.material_revision != item.material_revision
                    || previous.status != item.status
                    || previous.summary != item.summary
                    || previous.project != item.project
            });
            let previous_revision = old.map(|v| v.material_revision);
            let payload_hash = material_hash(&item);
            if material {
                self.revision = self.revision.saturating_add(1);
                changes.push(MaterialChange {
                    key: key.clone(),
                    previous_revision,
                    revision: item.material_revision,
                });
                let scope = Scope {
                    node: Some(item.node_id.clone()),
                    project: item.project.clone(),
                    provider: Some(item.provider.clone()),
                    conversation: Some(item.conversation_id.clone()),
                };
                let body = format!("{}: {}", status_name(item.status), item.summary);
                self.memory.append_idempotent(
                    &format!(
                        "observation:{}:{}:{}:{}",
                        item.node_id,
                        item.provider,
                        item.conversation_id,
                        material_hash(&item)
                    ),
                    NewRecord {
                        kind: RecordKind::Briefing,
                        origin: Origin::System,
                        scope,
                        body,
                        provenance: format!("shared activity feed; node={}", item.node_id),
                        timestamp: item.observed_at,
                        supersedes: None,
                        dependencies: vec![],
                        decision_state: None,
                        protected_policy: false,
                    },
                )?;
            }
            self.observation_db.execute("INSERT INTO assistant_observations(node_id,provider,conversation_id,project,status,summary,observed_at,material_revision,payload_hash) VALUES(?,?,?,?,?,?,?,?,?) ON CONFLICT(node_id,provider,conversation_id) DO UPDATE SET project=excluded.project,status=excluded.status,summary=excluded.summary,observed_at=excluded.observed_at,material_revision=excluded.material_revision,payload_hash=excluded.payload_hash", params![&item.node_id, &item.provider, &item.conversation_id, &item.project, status_name(item.status), &item.summary, item.observed_at, item.material_revision as i64, payload_hash]).map_err(MemoryError::Database)?;
            self.observations.insert(key.clone(), item.clone());
        }
        Ok(changes)
    }

    /// Read-only UI operation. This never reserves policy budget or invokes a worker.
    pub fn snapshot(&self, scope: &Scope, limit: usize) -> Result<ViewSnapshot, CoordinatorError> {
        Ok(ViewSnapshot {
            observations: self
                .observations
                .values()
                .filter(|item| scope_matches(scope, item))
                .cloned()
                .collect(),
            memory: self.memory.recent(scope, limit)?,
            revision: self.revision,
        })
    }

    /// Explicitly enqueue a bounded child worker under a non-dispatched root envelope.
    pub fn enqueue_child(
        &mut self,
        root_id: &str,
        request: WorkerRequest,
        now: i64,
    ) -> Result<(), CoordinatorError> {
        self.validate_child_request(root_id, &request)?;
        let child_id = format!("{}:child:{}", root_id, request.job_id);
        let scope_json =
            serde_json::to_string(&request.scope).map_err(MemoryError::Serialization)?;
        let dependency_json =
            serde_json::to_string(&request.dependency_ids).map_err(MemoryError::Serialization)?;
        let forget_epoch = self.memory.forget_epoch()?;
        self.persist_child_intent(ChildIntent {
            request: &request,
            root_id,
            child_id: &child_id,
            scope_json: &scope_json,
            dependency_json: &dependency_json,
            forget_epoch,
            now,
        })?;
        self.policy
            .reserve_child(&child_id, root_id, 1, now, None)?;
        self.mark_child_reserved(&request.job_id)?;
        self.jobs.push_back(QueuedJob {
            request,
            child_id,
            cancelled: Arc::new(AtomicBool::new(false)),
            forget_epoch,
        });
        Ok(())
    }

    fn validate_child_request(
        &self,
        root_id: &str,
        request: &WorkerRequest,
    ) -> Result<(), CoordinatorError> {
        if self.jobs.len() >= MAX_JOBS {
            return Err(CoordinatorError::QueueFull);
        }
        if request.root_id != root_id
            || request.job_id.is_empty()
            || request.prompt.len() > MAX_SUMMARY_BYTES
        {
            return Err(CoordinatorError::InvalidObservation(
                "invalid worker request".into(),
            ));
        }
        if self.jobs.iter().any(|j| j.request.job_id == request.job_id) {
            return Err(CoordinatorError::DuplicateJob);
        }
        self.validate_child_dependencies(request)?;
        self.validate_root_envelope(root_id)?;
        Ok(())
    }

    fn validate_child_dependencies(&self, request: &WorkerRequest) -> Result<(), CoordinatorError> {
        let records = self.memory.recent(&request.scope, 256)?;
        if request.dependency_ids.len() > 64
            || request
                .dependency_ids
                .iter()
                .any(|id| !records.iter().any(|record| &record.id == id))
        {
            return Err(CoordinatorError::InvalidObservation(
                "worker dependency is outside exact memory scope".into(),
            ));
        }
        Ok(())
    }

    fn validate_root_envelope(&self, root_id: &str) -> Result<(), CoordinatorError> {
        let root = self
            .policy
            .reservation(root_id)?
            .ok_or_else(|| PolicyError::UnknownReservation(root_id.into()))?;
        if root.parent_id.is_some() || root.state != ReservationState::Reserved {
            return Err(CoordinatorError::RootDispatch);
        }
        Ok(())
    }

    fn persist_child_intent(&self, intent: ChildIntent<'_>) -> Result<(), CoordinatorError> {
        self.queue_db.execute("INSERT INTO assistant_jobs(job_id,root_id,scope_json,prompt,dependency_json,forget_epoch,child_id,state,created_at) VALUES(?,?,?,?,?,?,?,'intent',?)", params![&intent.request.job_id, intent.root_id, intent.scope_json, &intent.request.prompt, intent.dependency_json, intent.forget_epoch as i64, intent.child_id, intent.now]).map_err(MemoryError::Database)?;
        Ok(())
    }

    fn mark_child_reserved(&self, job_id: &str) -> Result<(), CoordinatorError> {
        self.queue_db
            .execute(
                "UPDATE assistant_jobs SET state='reserved' WHERE job_id=? AND state='intent'",
                [job_id],
            )
            .map_err(MemoryError::Database)?;
        Ok(())
    }

    pub fn cancel_job(&mut self, job_id: &str) -> bool {
        self.jobs
            .iter()
            .find(|j| j.request.job_id == job_id)
            .map(|j| {
                j.cancelled.store(true, Ordering::Release);
                true
            })
            .or_else(|| {
                self.running.get(job_id).map(|flag| {
                    flag.store(true, Ordering::Release);
                    true
                })
            })
            .unwrap_or(false)
    }
    pub fn cancellation_handle(&self, job_id: &str) -> Option<Arc<AtomicBool>> {
        self.jobs
            .iter()
            .find(|j| j.request.job_id == job_id)
            .map(|j| j.cancelled.clone())
            .or_else(|| self.running.get(job_id).cloned())
    }
    pub fn queued_jobs(&self) -> usize {
        self.jobs.len()
    }
    /// Forgetting memory cancels all owned work: prompts are free-form and may
    /// contain forgotten content without declaring a dependency.
    pub fn forget(&mut self, record_id: &str, now: i64) -> Result<usize, CoordinatorError> {
        let removed = self.memory.forget(record_id)?;
        for job in self.jobs.drain(..) {
            let _ = self.policy.release_before_dispatch(&job.child_id, now);
        }
        for flag in self.running.values() {
            flag.store(true, Ordering::Release);
        }
        self.queue_db
            .execute(
                "DELETE FROM assistant_jobs WHERE state='reserved' OR state='intent'",
                [],
            )
            .map_err(MemoryError::Database)?;
        Ok(removed)
    }
    pub fn quarantined_jobs(&self) -> &[String] {
        &self.quarantined
    }

    /// Run at most one bounded worker in this synchronous skeleton. Production
    /// hosts can call this from a bounded executor; no thread is created here.
    pub fn run_one<W: CancellableWorker>(
        &mut self,
        worker: &mut W,
        now: i64,
    ) -> Result<Option<WorkerResult>, CoordinatorError> {
        let Some(job) = self.take_ready_job(now)? else {
            return Ok(None);
        };
        self.run_ready_job(worker, job, now).map(Some)
    }

    fn take_ready_job(&mut self, now: i64) -> Result<Option<QueuedJob>, CoordinatorError> {
        if self.active >= MAX_WORKERS {
            return Ok(None);
        }
        let Some(job) = self.jobs.pop_front() else {
            return Ok(None);
        };
        if self.memory.forget_epoch()? != job.forget_epoch {
            self.release_and_delete_job(&job, now)?;
            return Ok(None);
        }
        let records = self.memory.recent(&job.request.scope, 256)?;
        if job
            .request
            .dependency_ids
            .iter()
            .any(|id| !records.iter().any(|record| &record.id == id))
        {
            self.release_and_delete_job(&job, now)?;
            return Ok(None);
        }
        if job.cancelled.load(Ordering::Acquire) {
            self.release_and_delete_job(&job, now)?;
            return Ok(None);
        }
        Ok(Some(job))
    }

    fn release_and_delete_job(
        &mut self,
        job: &QueuedJob,
        now: i64,
    ) -> Result<(), CoordinatorError> {
        let _ = self.policy.release_before_dispatch(&job.child_id, now);
        self.queue_db
            .execute(
                "DELETE FROM assistant_jobs WHERE job_id=? AND state='reserved'",
                [&job.request.job_id],
            )
            .map_err(MemoryError::Database)?;
        Ok(())
    }

    fn run_ready_job<W: CancellableWorker>(
        &mut self,
        worker: &mut W,
        job: QueuedJob,
        now: i64,
    ) -> Result<WorkerResult, CoordinatorError> {
        self.begin_dispatch(&job, now)?;
        let result = worker.execute(&job.request, &job.cancelled);
        self.finish_dispatch(&job, result, now)
    }

    fn begin_dispatch(&mut self, job: &QueuedJob, now: i64) -> Result<(), CoordinatorError> {
        self.active += 1;
        self.running
            .insert(job.request.job_id.clone(), job.cancelled.clone());
        if let Err(error) = self.policy.mark_dispatched(&job.child_id, now) {
            self.running.remove(&job.request.job_id);
            self.active -= 1;
            return Err(error.into());
        }
        if let Err(error) = self.queue_db.execute(
            "UPDATE assistant_jobs SET state='dispatched' WHERE job_id=? AND state='reserved'",
            [&job.request.job_id],
        ) {
            let _ = self
                .policy
                .record_outcome(&job.child_id, DeliveryOutcome::Unknown, now);
            self.running.remove(&job.request.job_id);
            self.active -= 1;
            return Err(MemoryError::Database(error).into());
        }
        Ok(())
    }

    fn finish_dispatch(
        &mut self,
        job: &QueuedJob,
        result: WorkerResult,
        now: i64,
    ) -> Result<WorkerResult, CoordinatorError> {
        let outcome = match result {
            WorkerResult::Completed { .. } => DeliveryOutcome::Completed,
            WorkerResult::Failed { .. } => DeliveryOutcome::Failed,
            WorkerResult::Unknown => DeliveryOutcome::Unknown,
        };
        if let Err(error) = self.policy.record_outcome(&job.child_id, outcome, now) {
            self.running.remove(&job.request.job_id);
            self.active -= 1;
            return Err(error.into());
        }
        let state = match outcome {
            DeliveryOutcome::Completed => "completed",
            DeliveryOutcome::Failed => "failed",
            DeliveryOutcome::Unknown => "unknown",
        };
        if let Err(error) = self.queue_db.execute(
            "UPDATE assistant_jobs SET state=? WHERE job_id=? AND state='dispatched'",
            params![state, job.request.job_id],
        ) {
            self.running.remove(&job.request.job_id);
            self.active -= 1;
            return Err(MemoryError::Database(error).into());
        }
        self.running.remove(&job.request.job_id);
        self.active -= 1;
        Ok(result)
    }

    pub fn record_user_correction(
        &mut self,
        request_id: &str,
        scope: Scope,
        body: String,
        dependency: String,
        timestamp: i64,
    ) -> Result<Record, CoordinatorError> {
        Ok(self.memory.append_idempotent(
            request_id,
            NewRecord {
                kind: RecordKind::Correction,
                origin: Origin::Human,
                scope,
                body,
                provenance: "explicit user correction".into(),
                timestamp,
                supersedes: None,
                dependencies: vec![dependency],
                decision_state: None,
                protected_policy: false,
            },
        )?)
    }
    pub fn record_grasp(
        &mut self,
        request_id: &str,
        scope: Scope,
        body: String,
        timestamp: i64,
    ) -> Result<Record, CoordinatorError> {
        Ok(self.memory.append_idempotent(
            request_id,
            NewRecord {
                kind: RecordKind::GraspInteraction,
                origin: Origin::Human,
                scope,
                body,
                provenance: "explicit user context; skip/defer/explain-back".into(),
                timestamp,
                supersedes: None,
                dependencies: vec![],
                decision_state: Some(DecisionState::Deferred),
                protected_policy: false,
            },
        )?)
    }
}

type JobIdentity = (String, String);
type StoredJob = (String, String, String, String, String, String);

fn open_observation_db(path: std::path::PathBuf) -> Result<Connection, CoordinatorError> {
    crate::assistant_storage::database(&path).map_err(MemoryError::Filesystem)?;
    let connection = Connection::open(path).map_err(MemoryError::Database)?;
    connection.execute_batch("CREATE TABLE IF NOT EXISTS assistant_observations (node_id TEXT NOT NULL, provider TEXT NOT NULL, conversation_id TEXT NOT NULL, project TEXT, status TEXT NOT NULL, summary TEXT NOT NULL, observed_at INTEGER NOT NULL, material_revision INTEGER NOT NULL, payload_hash TEXT NOT NULL, PRIMARY KEY(node_id,provider,conversation_id));").map_err(MemoryError::Database)?;
    Ok(connection)
}

fn open_queue_db(path: std::path::PathBuf) -> Result<Connection, CoordinatorError> {
    crate::assistant_storage::database(&path).map_err(MemoryError::Filesystem)?;
    let connection = Connection::open(path).map_err(MemoryError::Database)?;
    connection.execute_batch("CREATE TABLE IF NOT EXISTS assistant_jobs (job_id TEXT PRIMARY KEY, root_id TEXT NOT NULL, scope_json TEXT NOT NULL, prompt TEXT NOT NULL, dependency_json TEXT NOT NULL DEFAULT '[]', forget_epoch INTEGER NOT NULL DEFAULT 0, child_id TEXT NOT NULL UNIQUE, state TEXT NOT NULL CHECK(state IN ('intent','reserved','dispatched','completed','failed','unknown')), created_at INTEGER NOT NULL)").map_err(MemoryError::Database)?;
    migrate_queue_column(
        &connection,
        "dependency_json",
        "ALTER TABLE assistant_jobs ADD COLUMN dependency_json TEXT NOT NULL DEFAULT '[]'",
    )?;
    migrate_queue_column(
        &connection,
        "forget_epoch",
        "ALTER TABLE assistant_jobs ADD COLUMN forget_epoch INTEGER NOT NULL DEFAULT 0",
    )?;
    Ok(connection)
}

fn migrate_queue_column(
    connection: &Connection,
    column: &str,
    migration: &str,
) -> Result<(), CoordinatorError> {
    let has_column = connection
        .prepare("PRAGMA table_info(assistant_jobs)")
        .map_err(MemoryError::Database)?
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(MemoryError::Database)?
        .filter_map(Result::ok)
        .any(|name| name == column);
    if !has_column {
        connection
            .execute(migration, [])
            .map_err(MemoryError::Database)?;
    }
    Ok(())
}

fn recover_queued_jobs(
    queue_db: &Connection,
    policy: &mut AssistantPolicy,
    forget_epoch: u64,
) -> Result<(VecDeque<QueuedJob>, Vec<String>), CoordinatorError> {
    queue_db
        .execute(
            "UPDATE assistant_jobs SET state='unknown' WHERE state='dispatched'",
            [],
        )
        .map_err(MemoryError::Database)?;
    let mut quarantined = quarantine_unknown(queue_db, policy)?;
    recover_intents(queue_db, policy, &mut quarantined)?;
    queue_db
        .execute("DELETE FROM assistant_jobs WHERE state='intent'", [])
        .map_err(MemoryError::Database)?;
    recover_stale_reservations(queue_db, policy, forget_epoch, &mut quarantined)?;
    let jobs = restore_reserved_jobs(queue_db, policy, forget_epoch, &mut quarantined)?;
    Ok((jobs, quarantined))
}

fn quarantine_unknown(
    queue_db: &Connection,
    policy: &mut AssistantPolicy,
) -> Result<Vec<String>, CoordinatorError> {
    let mut statement = queue_db
        .prepare("SELECT job_id,child_id FROM assistant_jobs WHERE state='unknown'")
        .map_err(MemoryError::Database)?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(MemoryError::Database)?;
    let mut quarantined = Vec::new();
    for row in rows {
        let (job, child) = row.map_err(MemoryError::Database)?;
        if policy
            .reservation(&child)?
            .is_some_and(|reservation| reservation.state == ReservationState::Dispatched)
        {
            policy.record_outcome(&child, DeliveryOutcome::Unknown, 0)?;
        }
        quarantined.push(job);
    }
    Ok(quarantined)
}

fn recover_intents(
    queue_db: &Connection,
    policy: &mut AssistantPolicy,
    quarantined: &mut Vec<String>,
) -> Result<(), CoordinatorError> {
    let mut statement = queue_db
        .prepare("SELECT job_id,child_id FROM assistant_jobs WHERE state='intent'")
        .map_err(MemoryError::Database)?;
    let ids: Vec<JobIdentity> = statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .map_err(MemoryError::Database)?
        .filter_map(Result::ok)
        .collect();
    for (job_id, child_id) in ids {
        recover_intent(queue_db, policy, &job_id, &child_id, quarantined)?;
    }
    Ok(())
}

fn recover_intent(
    queue_db: &Connection,
    policy: &mut AssistantPolicy,
    job_id: &str,
    child_id: &str,
    quarantined: &mut Vec<String>,
) -> Result<(), CoordinatorError> {
    let Some(reservation) = policy.reservation(child_id)? else {
        return Ok(());
    };
    match reservation.state {
        ReservationState::Reserved => policy.release_before_dispatch(child_id, 0)?,
        ReservationState::Dispatched => {
            policy.record_outcome(child_id, DeliveryOutcome::Unknown, 0)?;
            quarantine_job(queue_db, job_id)?;
            quarantined.push(job_id.into());
        }
        ReservationState::Unknown => {
            quarantine_job(queue_db, job_id)?;
            quarantined.push(job_id.into());
        }
        _ => {}
    }
    Ok(())
}

fn recover_stale_reservations(
    queue_db: &Connection,
    policy: &mut AssistantPolicy,
    forget_epoch: u64,
    quarantined: &mut Vec<String>,
) -> Result<(), CoordinatorError> {
    let mut statement = queue_db
        .prepare(
            "SELECT job_id,child_id FROM assistant_jobs WHERE state='reserved' AND forget_epoch<>?",
        )
        .map_err(MemoryError::Database)?;
    let stale: Vec<JobIdentity> = statement
        .query_map([forget_epoch as i64], |row| Ok((row.get(0)?, row.get(1)?)))
        .map_err(MemoryError::Database)?
        .filter_map(Result::ok)
        .collect();
    for (job_id, child_id) in stale {
        recover_stale_reservation(queue_db, policy, &job_id, &child_id, quarantined)?;
    }
    Ok(())
}

fn recover_stale_reservation(
    queue_db: &Connection,
    policy: &mut AssistantPolicy,
    job_id: &str,
    child_id: &str,
    quarantined: &mut Vec<String>,
) -> Result<(), CoordinatorError> {
    if let Some(reservation) = policy.reservation(child_id)? {
        if matches!(
            reservation.state,
            ReservationState::Dispatched | ReservationState::Unknown
        ) {
            if reservation.state == ReservationState::Dispatched {
                policy.record_outcome(child_id, DeliveryOutcome::Unknown, 0)?;
            }
            quarantine_job(queue_db, job_id)?;
            quarantined.push(job_id.into());
            return Ok(());
        }
        if reservation.state == ReservationState::Reserved {
            policy.release_before_dispatch(child_id, 0)?;
        }
    }
    queue_db
        .execute(
            "DELETE FROM assistant_jobs WHERE job_id=? AND state='reserved'",
            [job_id],
        )
        .map_err(MemoryError::Database)?;
    Ok(())
}

fn quarantine_job(queue_db: &Connection, job_id: &str) -> Result<(), CoordinatorError> {
    queue_db
        .execute(
            "UPDATE assistant_jobs SET state='unknown',prompt='',dependency_json='[]' WHERE job_id=?",
            [job_id],
        )
        .map_err(MemoryError::Database)?;
    Ok(())
}

fn restore_reserved_jobs(
    queue_db: &Connection,
    policy: &mut AssistantPolicy,
    forget_epoch: u64,
    quarantined: &mut Vec<String>,
) -> Result<VecDeque<QueuedJob>, CoordinatorError> {
    let mut stmt = queue_db.prepare("SELECT job_id,root_id,scope_json,prompt,dependency_json,child_id FROM assistant_jobs WHERE state='reserved' AND forget_epoch=? ORDER BY created_at,job_id").map_err(MemoryError::Database)?;
    let rows = stmt
        .query_map([forget_epoch as i64], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
            ))
        })
        .map_err(MemoryError::Database)?;
    let mut jobs = VecDeque::new();
    for row in rows {
        restore_reserved_job(
            queue_db,
            policy,
            row.map_err(MemoryError::Database)?,
            forget_epoch,
            &mut jobs,
            quarantined,
        )?;
    }
    Ok(jobs)
}

fn restore_reserved_job(
    queue_db: &Connection,
    policy: &mut AssistantPolicy,
    (job_id, root_id, scope_json, prompt, dependency_json, child_id): StoredJob,
    forget_epoch: u64,
    jobs: &mut VecDeque<QueuedJob>,
    quarantined: &mut Vec<String>,
) -> Result<(), CoordinatorError> {
    let reservation = policy.reservation(&child_id)?;
    if !reservation
        .as_ref()
        .is_some_and(|reservation| reservation.state == ReservationState::Reserved)
    {
        if reservation
            .as_ref()
            .is_some_and(|reservation| reservation.state == ReservationState::Dispatched)
        {
            policy.record_outcome(&child_id, DeliveryOutcome::Unknown, 0)?;
        }
        quarantine_job(queue_db, &job_id)?;
        quarantined.push(job_id);
        return Ok(());
    }
    let scope: Scope = serde_json::from_str(&scope_json).map_err(MemoryError::Serialization)?;
    let dependency_ids: Vec<String> =
        serde_json::from_str(&dependency_json).map_err(MemoryError::Serialization)?;
    jobs.push_back(QueuedJob {
        request: WorkerRequest {
            job_id,
            root_id,
            scope,
            prompt,
            dependency_ids,
        },
        child_id,
        cancelled: Arc::new(AtomicBool::new(false)),
        forget_epoch,
    });
    Ok(())
}

fn load_observations(
    observation_db: &Connection,
) -> Result<HashMap<ObservationKey, OperationalObservation>, CoordinatorError> {
    let mut observations = HashMap::new();
    let mut rows = observation_db.prepare("SELECT node_id,provider,conversation_id,project,status,summary,observed_at,material_revision FROM assistant_observations").map_err(MemoryError::Database)?;
    let rows = rows
        .query_map([], |row| {
            Ok(OperationalObservation {
                node_id: row.get(0)?,
                provider: row.get(1)?,
                conversation_id: row.get(2)?,
                project: row.get(3)?,
                status: parse_status(&row.get::<_, String>(4)?),
                summary: row.get(5)?,
                observed_at: row.get(6)?,
                material_revision: row.get::<_, i64>(7)? as u64,
            })
        })
        .map_err(MemoryError::Database)?;
    for row in rows {
        let item = row.map_err(MemoryError::Database)?;
        observations.insert(
            ObservationKey {
                node_id: item.node_id.clone(),
                provider: item.provider.clone(),
                conversation_id: item.conversation_id.clone(),
            },
            item,
        );
    }
    Ok(observations)
}

fn validate_observation(o: &OperationalObservation) -> Result<(), CoordinatorError> {
    for (name, value) in [
        ("node", &o.node_id),
        ("provider", &o.provider),
        ("conversation", &o.conversation_id),
    ] {
        if value.is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
            return Err(CoordinatorError::InvalidObservation(format!(
                "invalid {name} identity"
            )));
        }
    }
    if o.summary.len() > MAX_SUMMARY_BYTES || o.summary.chars().any(char::is_control) {
        return Err(CoordinatorError::InvalidObservation(
            "invalid bounded summary".into(),
        ));
    }
    Ok(())
}
fn status_name(status: ObservationStatus) -> &'static str {
    match status {
        ObservationStatus::Working => "working",
        ObservationStatus::Ready => "ready",
        ObservationStatus::NeedsYou => "needs-you",
        ObservationStatus::Error => "error",
        ObservationStatus::Parked => "parked",
    }
}
fn parse_status(value: &str) -> ObservationStatus {
    match value {
        "working" => ObservationStatus::Working,
        "needs-you" => ObservationStatus::NeedsYou,
        "error" => ObservationStatus::Error,
        "parked" => ObservationStatus::Parked,
        _ => ObservationStatus::Ready,
    }
}
fn material_hash(item: &OperationalObservation) -> String {
    let mut hash = Sha256::new();
    hash.update(item.node_id.as_bytes());
    hash.update([0]);
    hash.update(item.provider.as_bytes());
    hash.update([0]);
    hash.update(item.conversation_id.as_bytes());
    hash.update([0]);
    hash.update(item.project.as_deref().unwrap_or_default().as_bytes());
    hash.update([0]);
    hash.update(status_name(item.status).as_bytes());
    hash.update([0]);
    hash.update(item.summary.as_bytes());
    hash.update([0]);
    hash.update(item.material_revision.to_le_bytes());
    format!("{:x}", hash.finalize())
}
fn scope_matches(scope: &Scope, item: &OperationalObservation) -> bool {
    scope.node.as_deref().is_none_or(|v| v == item.node_id)
        && scope
            .project
            .as_deref()
            .is_none_or(|v| item.project.as_deref() == Some(v))
        && scope.provider.as_deref().is_none_or(|v| v == item.provider)
        && scope
            .conversation
            .as_deref()
            .is_none_or(|v| v == item.conversation_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    struct Fake {
        calls: usize,
        result: WorkerResult,
    }
    impl CancellableWorker for Fake {
        fn execute(&mut self, _: &WorkerRequest, _: &AtomicBool) -> WorkerResult {
            self.calls += 1;
            self.result.clone()
        }
    }
    fn coordinator() -> AssistantCoordinator {
        let d = tempdir().unwrap();
        let mut p = AssistantPolicy::open(d.path().join("private/policy.sqlite")).unwrap();
        p.configure(&crate::assistant_policy::PolicyConfig {
            max_total_calls: 4,
            max_concurrent: 2,
            ..Default::default()
        })
        .unwrap();
        let coordinator = AssistantCoordinator::new(
            Store::open(d.path().join("private/memory.sqlite")).unwrap(),
            p,
        )
        .unwrap();
        std::mem::forget(d);
        coordinator
    }
    fn obs(t: i64, rev: u64) -> OperationalObservation {
        OperationalObservation {
            node_id: "node-a".into(),
            provider: "codex".into(),
            conversation_id: "c1".into(),
            project: Some("p".into()),
            status: ObservationStatus::Ready,
            summary: "done".into(),
            observed_at: t,
            material_revision: rev,
        }
    }
    #[test]
    fn timestamp_noise_coalesces_and_snapshot_is_free() {
        let mut c = coordinator();
        assert_eq!(c.observe(vec![obs(1, 1)]).unwrap().len(), 1);
        assert!(c.observe(vec![obs(2, 1)]).unwrap().is_empty());
        let s = c
            .snapshot(
                &Scope {
                    project: Some("p".into()),
                    ..Default::default()
                },
                10,
            )
            .unwrap();
        assert_eq!(s.observations.len(), 1);
    }
    #[test]
    fn worker_unknown_is_not_acknowledged() {
        let mut c = coordinator();
        c.policy.reserve_root("r", 1, false, 1, None).unwrap();
        c.enqueue_child(
            "r",
            WorkerRequest {
                job_id: "j".into(),
                root_id: "r".into(),
                scope: Scope::default(),
                prompt: "x".into(),
                dependency_ids: vec![],
            },
            1,
        )
        .unwrap();
        let mut f = Fake {
            calls: 0,
            result: WorkerResult::Unknown,
        };
        assert!(matches!(
            c.run_one(&mut f, 2).unwrap(),
            Some(WorkerResult::Unknown)
        ));
        assert_eq!(
            c.policy.reservation("r:child:j").unwrap().unwrap().state,
            ReservationState::Unknown
        );
    }
    #[test]
    fn correction_survives_restart_store() {
        let d = tempdir().unwrap();
        let path = d.path().join("private/memory.sqlite");
        let mut s = Store::open(&path).unwrap();
        let scope = Scope {
            project: Some("p".into()),
            ..Default::default()
        };
        let root = s
            .append(NewRecord {
                kind: RecordKind::Finding,
                origin: Origin::Human,
                scope: scope.clone(),
                body: "old".into(),
                provenance: "u".into(),
                timestamp: 1,
                supersedes: None,
                dependencies: vec![],
                decision_state: None,
                protected_policy: false,
            })
            .unwrap();
        drop(s);
        let mut c = AssistantCoordinator::new(Store::open(&path).unwrap(), {
            let mut p = AssistantPolicy::open(d.path().join("private/policy.sqlite")).unwrap();
            p.configure(&crate::assistant_policy::PolicyConfig {
                max_total_calls: 1,
                ..Default::default()
            })
            .unwrap();
            p
        })
        .unwrap();
        c.record_user_correction("corr", scope, "new".into(), root.id, 2)
            .unwrap();
        assert_eq!(
            c.snapshot(
                &Scope {
                    project: Some("p".into()),
                    ..Default::default()
                },
                10
            )
            .unwrap()
            .memory
            .len(),
            2
        );
    }
    #[test]
    fn reserved_queue_reloads_once_and_stale_observation_cannot_regress() {
        let d = tempdir().unwrap();
        let memory_path = d.path().join("private/memory.sqlite");
        let policy_path = d.path().join("private/policy.sqlite");
        let mut policy = AssistantPolicy::open(&policy_path).unwrap();
        policy
            .configure(&crate::assistant_policy::PolicyConfig {
                max_total_calls: 2,
                max_concurrent: 2,
                ..Default::default()
            })
            .unwrap();
        let mut c = AssistantCoordinator::new(Store::open(&memory_path).unwrap(), policy).unwrap();
        c.policy.reserve_root("root", 1, false, 1, None).unwrap();
        c.enqueue_child(
            "root",
            WorkerRequest {
                job_id: "job".into(),
                root_id: "root".into(),
                scope: Scope::default(),
                prompt: "bounded".into(),
                dependency_ids: vec![],
            },
            1,
        )
        .unwrap();
        assert_eq!(c.queued_jobs(), 1);
        let mut newer = obs(20, 2);
        newer.summary = "new".into();
        c.observe(vec![newer]).unwrap();
        let mut older = obs(10, 1);
        older.summary = "old".into();
        assert!(c.observe(vec![older]).unwrap().is_empty());
        drop(c);
        let policy = AssistantPolicy::open(&policy_path).unwrap();
        let mut restarted =
            AssistantCoordinator::new(Store::open(&memory_path).unwrap(), policy).unwrap();
        assert_eq!(restarted.queued_jobs(), 1);
        let mut f = Fake {
            calls: 0,
            result: WorkerResult::Completed { text: "ok".into() },
        };
        restarted.run_one(&mut f, 2).unwrap();
        assert_eq!(f.calls, 1);
        drop(restarted);
        let policy = AssistantPolicy::open(&policy_path).unwrap();
        let restarted =
            AssistantCoordinator::new(Store::open(&memory_path).unwrap(), policy).unwrap();
        assert_eq!(restarted.queued_jobs(), 0);
    }

    #[test]
    fn startup_quarantines_dispatched_intent_and_refunds_only_reserved_intent() {
        let d = tempdir().unwrap();
        let memory_path = d.path().join("private/memory.sqlite");
        let policy_path = d.path().join("private/policy.sqlite");
        let mut policy = AssistantPolicy::open(&policy_path).unwrap();
        policy
            .configure(&crate::assistant_policy::PolicyConfig {
                max_total_calls: 4,
                ..Default::default()
            })
            .unwrap();
        let mut c = AssistantCoordinator::new(Store::open(&memory_path).unwrap(), policy).unwrap();
        c.policy.reserve_root("root", 2, false, 1, None).unwrap();
        c.enqueue_child(
            "root",
            WorkerRequest {
                job_id: "reserved-intent".into(),
                root_id: "root".into(),
                scope: Scope::default(),
                prompt: "x".into(),
                dependency_ids: vec![],
            },
            1,
        )
        .unwrap();
        c.queue_db
            .execute(
                "UPDATE assistant_jobs SET state='intent' WHERE job_id='reserved-intent'",
                [],
            )
            .unwrap();
        drop(c);
        let restarted = AssistantCoordinator::new(
            Store::open(&memory_path).unwrap(),
            AssistantPolicy::open(&policy_path).unwrap(),
        )
        .unwrap();
        assert!(restarted.quarantined_jobs().is_empty());
        assert_eq!(restarted.queued_jobs(), 0);
        assert_eq!(
            restarted
                .policy
                .reservation("root:child:reserved-intent")
                .unwrap()
                .unwrap()
                .state,
            ReservationState::Released
        );

        let d = tempdir().unwrap();
        let memory_path = d.path().join("private/memory.sqlite");
        let policy_path = d.path().join("private/policy.sqlite");
        let mut policy = AssistantPolicy::open(&policy_path).unwrap();
        policy
            .configure(&crate::assistant_policy::PolicyConfig {
                max_total_calls: 4,
                ..Default::default()
            })
            .unwrap();
        let mut c = AssistantCoordinator::new(Store::open(&memory_path).unwrap(), policy).unwrap();
        c.policy.reserve_root("root", 2, false, 1, None).unwrap();
        c.enqueue_child(
            "root",
            WorkerRequest {
                job_id: "dispatched-intent".into(),
                root_id: "root".into(),
                scope: Scope::default(),
                prompt: "x".into(),
                dependency_ids: vec![],
            },
            1,
        )
        .unwrap();
        c.policy
            .mark_dispatched("root:child:dispatched-intent", 2)
            .unwrap();
        c.queue_db
            .execute(
                "UPDATE assistant_jobs SET state='intent' WHERE job_id='dispatched-intent'",
                [],
            )
            .unwrap();
        drop(c);
        let restarted = AssistantCoordinator::new(
            Store::open(&memory_path).unwrap(),
            AssistantPolicy::open(&policy_path).unwrap(),
        )
        .unwrap();
        assert_eq!(restarted.queued_jobs(), 0);
        assert_eq!(
            restarted.quarantined_jobs(),
            &["dispatched-intent".to_string()]
        );
        assert_eq!(
            restarted
                .policy
                .reservation("root:child:dispatched-intent")
                .unwrap()
                .unwrap()
                .state,
            ReservationState::Unknown
        );
    }
}
