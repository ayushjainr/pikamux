use crate::expert_search::{ExpertQuery, token_sequence};
use crate::model::{
    ExpertProfile, FleetNode, ObservationKind, Provider, Session, Status, StatusObservation,
};
use crate::paths::Paths;
use anyhow::{Context, Result, anyhow, bail};
use fs2::FileExt;
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Row, Transaction, TransactionBehavior, params,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

// Lifecycle hooks are latency-sensitive. Reconciliation transactions contain
// no provider/process I/O, so a writer held for longer than this is abnormal;
// report the lock explicitly instead of freezing a hook or board action.
const BUSY_TIMEOUT: Duration = Duration::from_millis(500);
pub const MAX_REMOTE_SNAPSHOT_BYTES: usize = 4 * 1024 * 1024;
const REMOTE_BOARD_CACHE_SCHEMA: u32 = 1;
const REMOTE_EXPERT_CACHE_SCHEMA: u32 = 2;
// The aggregate budget is divided fairly across as many as 64 machines. A
// 64-KiB page leaves room for the per-node manifest inside its 256-KiB slice,
// so every valid dense node can contribute at least one row.
const REMOTE_CACHE_CHUNK_BYTES: usize = 64 * 1024;
const MAX_REMOTE_CACHE_HEADER_BYTES: usize = 64 * 1024;
const MAX_REMOTE_BOARD_CACHE_BYTES: usize = MAX_REMOTE_SNAPSHOT_BYTES + 512 * 1024;
const MAX_REMOTE_EXPERT_INDEX_BYTES: usize = 2 * MAX_REMOTE_SNAPSHOT_BYTES + 512 * 1024;
const MAX_REMOTE_CACHE_CHUNKS: usize = 128;

const SCHEMA: &str = r#"
CREATE TABLE sessions (
    provider TEXT NOT NULL,
    session_id TEXT NOT NULL,
    active_thread_id TEXT,
    name TEXT,
    cwd TEXT,
    branch TEXT,
    transcript_path TEXT,
    tmux_session TEXT,
    tmux_pane TEXT,
    root_pid INTEGER,
    status TEXT NOT NULL DEFAULT 'PARKED',
    unread INTEGER NOT NULL DEFAULT 0,
    model TEXT,
    source TEXT NOT NULL DEFAULT 'managed',
    managed INTEGER NOT NULL DEFAULT 1,
    error TEXT,
    attention_reason TEXT,
    created_at REAL NOT NULL,
    updated_at REAL NOT NULL,
    last_event_at REAL NOT NULL,
    last_activity_at REAL NOT NULL,
    PRIMARY KEY (provider, session_id)
);
CREATE INDEX sessions_name_idx ON sessions(name COLLATE NOCASE);
CREATE INDEX sessions_status_idx ON sessions(status, unread);
CREATE INDEX sessions_active_thread_idx ON sessions(provider, active_thread_id);
CREATE TABLE pending_launches (
    launch_token TEXT PRIMARY KEY,
    provider TEXT NOT NULL,
    name TEXT NOT NULL,
    cwd TEXT NOT NULL,
    tmux_session TEXT,
    tmux_pane TEXT,
    expected_session_id TEXT,
    root_pid INTEGER,
    root_pid_start INTEGER,
    preexisting_session_ids_json TEXT,
    candidate_session_id TEXT,
    candidate_observed_at REAL,
    created_at REAL NOT NULL
);
CREATE TABLE launch_reservations (
    provider TEXT NOT NULL,
    session_id TEXT NOT NULL,
    token TEXT NOT NULL,
    owner_pid INTEGER,
    owner_start_time INTEGER,
    created_at REAL NOT NULL,
    PRIMARY KEY (provider, session_id)
);
CREATE TABLE launch_bindings (
    launch_token TEXT PRIMARY KEY,
    provider TEXT NOT NULL,
    session_id TEXT NOT NULL,
    created_at REAL NOT NULL
);
CREATE TABLE live_owners (
    provider TEXT NOT NULL,
    session_id TEXT NOT NULL,
    pid INTEGER NOT NULL,
    start_time INTEGER,
    owner_token TEXT NOT NULL DEFAULT '',
    last_seen REAL NOT NULL,
    PRIMARY KEY (provider, session_id, pid, owner_token)
);
CREATE TABLE recovery_owners (
    provider TEXT NOT NULL,
    session_id TEXT NOT NULL,
    pid INTEGER NOT NULL,
    start_time INTEGER NOT NULL,
    launch_token TEXT NOT NULL,
    created_at REAL NOT NULL,
    PRIMARY KEY (provider, session_id)
);
CREATE TABLE untracked_sessions (
    provider TEXT NOT NULL,
    session_id TEXT NOT NULL,
    untracked_at REAL NOT NULL,
    PRIMARY KEY (provider, session_id)
);
CREATE TABLE usage_cache (
    provider TEXT NOT NULL,
    session_id TEXT NOT NULL,
    source_path TEXT NOT NULL,
    source_mtime_ns INTEGER NOT NULL,
    source_size INTEGER NOT NULL,
    model TEXT,
    input_tokens INTEGER NOT NULL,
    output_tokens INTEGER NOT NULL,
    cached_input_tokens INTEGER NOT NULL,
    cache_write_tokens INTEGER NOT NULL,
    total_tokens INTEGER NOT NULL,
    estimated_cost_usd REAL,
    updated_at REAL NOT NULL,
    PRIMARY KEY (provider, session_id)
);
CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE hook_observations (
    provider TEXT PRIMARY KEY,
    fingerprint TEXT NOT NULL,
    event_name TEXT NOT NULL,
    session_id TEXT NOT NULL,
    observed_at REAL NOT NULL,
    source TEXT,
    managed INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE session_events (
    event_id INTEGER PRIMARY KEY AUTOINCREMENT,
    provider TEXT NOT NULL,
    session_id TEXT NOT NULL,
    event_at REAL NOT NULL,
    status TEXT NOT NULL,
    attention_reason TEXT,
    error TEXT,
    UNIQUE (provider, session_id, event_at, status)
);
CREATE INDEX session_events_time_idx ON session_events(event_at);
CREATE TABLE session_status_observations (
    provider TEXT NOT NULL,
    session_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    status TEXT NOT NULL,
    unread INTEGER NOT NULL DEFAULT 0,
    attention_reason TEXT,
    error TEXT,
    observed_at REAL NOT NULL,
    source TEXT NOT NULL,
    PRIMARY KEY (provider, session_id, kind)
);
CREATE INDEX session_status_observations_time_idx
ON session_status_observations(observed_at);
CREATE TABLE identity_interruptions (
    provider TEXT NOT NULL,
    session_id TEXT NOT NULL,
    status TEXT NOT NULL,
    unread INTEGER NOT NULL,
    attention_reason TEXT,
    error TEXT,
    last_event_at REAL NOT NULL,
    PRIMARY KEY (provider, session_id)
);
CREATE TABLE expert_profiles (
    provider TEXT NOT NULL,
    session_id TEXT NOT NULL,
    summary TEXT NOT NULL,
    current_state TEXT NOT NULL DEFAULT '',
    topics_json TEXT NOT NULL,
    artifacts_json TEXT NOT NULL,
    source TEXT NOT NULL DEFAULT 'self',
    transcript_mtime_ns INTEGER,
    transcript_size INTEGER,
    updated_at REAL NOT NULL,
    scope_updated_at REAL NOT NULL DEFAULT 0,
    current_state_updated_at REAL NOT NULL DEFAULT 0,
    current_state_mtime_ns INTEGER,
    current_state_size INTEGER,
    PRIMARY KEY (provider, session_id)
);
CREATE INDEX expert_profiles_updated_idx ON expert_profiles(updated_at DESC);
CREATE TABLE expert_refresh_attempts (
    provider TEXT NOT NULL,
    session_id TEXT NOT NULL,
    reset_at INTEGER NOT NULL,
    status TEXT NOT NULL,
    detail TEXT,
    attempted_at REAL NOT NULL,
    PRIMARY KEY (provider, session_id, reset_at)
);
CREATE TABLE fleet_nodes (
    node_id TEXT PRIMARY KEY,
    alias TEXT NOT NULL UNIQUE COLLATE NOCASE,
    ssh_target TEXT NOT NULL,
    sources_json TEXT NOT NULL DEFAULT '[]',
    status TEXT NOT NULL DEFAULT 'unknown',
    protocol_version INTEGER,
    package_version TEXT,
    capabilities_json TEXT NOT NULL DEFAULT '[]',
    last_seen REAL NOT NULL DEFAULT 0,
    last_attempt_at REAL NOT NULL DEFAULT 0,
    last_error TEXT,
    created_at REAL NOT NULL,
    updated_at REAL NOT NULL
);
CREATE TABLE remote_snapshots (
    node_id TEXT PRIMARY KEY,
    payload_json TEXT NOT NULL,
    captured_at REAL NOT NULL
);
CREATE TABLE ignored_node_candidates (
    candidate_key TEXT PRIMARY KEY,
    ignored_at REAL NOT NULL
);
"#;

#[derive(Clone, Debug, PartialEq)]
pub struct PendingLaunch {
    pub launch_token: String,
    pub provider: Provider,
    pub name: String,
    pub cwd: String,
    pub tmux_session: Option<String>,
    pub tmux_pane: Option<String>,
    pub expected_session_id: Option<String>,
    pub root_pid: Option<i64>,
    pub root_pid_start: Option<i64>,
    pub preexisting_session_ids: Option<Vec<String>>,
    pub candidate_session_id: Option<String>,
    pub candidate_observed_at: Option<f64>,
    pub created_at: f64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LaunchPhase {
    Reserved,
    PaneAllocated,
    PanePrepared,
    ProviderStarting,
    ProviderObserved,
}

impl LaunchPhase {
    fn as_str(self) -> &'static str {
        match self {
            Self::Reserved => "reserved",
            Self::PaneAllocated => "pane_allocated",
            Self::PanePrepared => "pane_prepared",
            Self::ProviderStarting => "provider_starting",
            Self::ProviderObserved => "provider_observed",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "reserved" => Some(Self::Reserved),
            "pane_allocated" => Some(Self::PaneAllocated),
            "pane_prepared" => Some(Self::PanePrepared),
            "provider_starting" => Some(Self::ProviderStarting),
            "provider_observed" => Some(Self::ProviderObserved),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct LaunchReservation {
    pub provider: Provider,
    pub session_id: String,
    pub token: String,
    pub owner_pid: Option<i64>,
    pub owner_start_time: Option<i64>,
    pub created_at: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LiveOwner {
    pub provider: Provider,
    pub session_id: String,
    pub pid: i64,
    pub start_time: Option<i64>,
    pub owner_token: String,
    pub last_seen: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RecoveryOwner {
    pub provider: Provider,
    pub session_id: String,
    pub pid: i64,
    pub start_time: i64,
    pub launch_token: String,
    pub created_at: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ActivityEvent {
    pub event_id: i64,
    pub provider: Provider,
    pub session_id: String,
    pub name: Option<String>,
    pub status: Status,
    pub attention_reason: Option<String>,
    pub error: Option<String>,
    pub event_at: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct IdentityInterruption {
    pub provider: Provider,
    pub session_id: String,
    pub status: Status,
    pub unread: bool,
    pub attention_reason: Option<String>,
    pub error: Option<String>,
    pub last_event_at: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StoredExpertProfile {
    pub profile: ExpertProfile,
    pub transcript_mtime_ns: Option<i64>,
    pub transcript_size: Option<i64>,
    pub current_state_mtime_ns: Option<i64>,
    pub current_state_size: Option<i64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ExpertRefreshAttempt {
    pub provider: Provider,
    pub session_id: String,
    pub reset_at: i64,
    pub status: String,
    pub detail: Option<String>,
    pub attempted_at: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RemoteSnapshot {
    pub node_id: String,
    pub payload: Value,
    pub captured_at: f64,
    pub encoded_bytes: usize,
}

/// A bounded board-only projection of one retained remote snapshot.
///
/// The complete protocol snapshot remains authoritative for exact actions and
/// expert search. Aggregate board reads consume only these small chunks, so a
/// fleet-sized cache cannot multiply pre-paint JSON parsing by every node's
/// full four-megabyte wire allowance.
#[derive(Clone, Debug, PartialEq)]
pub struct RemoteBoardProjection {
    pub rows: Vec<Value>,
    pub protocol: String,
    pub version: i64,
    pub source_sessions: usize,
    pub source_captured_at: f64,
    pub remote_captured_at: f64,
    pub source_encoded_bytes: usize,
    pub input_bytes: usize,
    pub directory_notices: Vec<Value>,
}

/// Query-ranked expert rows selected from a revision-bound SQLite search index.
/// SQLite considers the complete per-node expert set; only this fair result
/// slice crosses into the caller's aggregate input budget.
#[derive(Clone, Debug, PartialEq)]
pub struct RemoteExpertProjection {
    pub rows: Vec<Value>,
    pub protocol: String,
    pub version: i64,
    pub source_experts: usize,
    pub matching_experts: usize,
    pub source_captured_at: f64,
    pub remote_captured_at: f64,
    pub source_encoded_bytes: usize,
    pub input_bytes: usize,
    pub directory_notices: Vec<Value>,
}

/// Result of a size-gated full-snapshot read. When `snapshot` is `None`, SQLite
/// returned only metadata and never materialized or parsed the oversized JSON.
#[derive(Clone, Debug, PartialEq)]
pub struct BoundedRemoteSnapshot {
    pub snapshot: Option<RemoteSnapshot>,
    pub captured_at: f64,
    pub encoded_bytes: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RemoteBoardCacheHeader {
    schema: u32,
    node_id: String,
    protocol: String,
    version: i64,
    source_captured_at: f64,
    remote_captured_at: f64,
    source_encoded_bytes: usize,
    source_sessions: usize,
    chunks: Vec<RemoteBoardCacheChunk>,
    directory_notices: Vec<Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RemoteExpertCacheHeader {
    schema: u32,
    node_id: String,
    protocol: String,
    version: i64,
    source_captured_at: f64,
    remote_captured_at: f64,
    source_encoded_bytes: usize,
    source_experts: usize,
    indexed_bytes: usize,
    directory_notices: Vec<Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RemoteBoardCacheChunk {
    bytes: usize,
    rows: usize,
}

struct PreparedRemoteSnapshot {
    revision: String,
    node_id: String,
    encoded: String,
    board_cache: Option<PreparedRemoteBoardCache>,
    expert_cache: Option<PreparedRemoteExpertCache>,
}

struct PreparedRemoteBoardCache {
    header: String,
    chunks: Vec<String>,
}

struct PreparedRemoteExpertCache {
    header: String,
    rows: Vec<PreparedRemoteExpertRow>,
}

struct PreparedRemoteExpertRow {
    ordinal: usize,
    row_json: String,
    live: bool,
    profile_updated_at: f64,
    session_id: String,
    topic_tokens: String,
    scope_tokens: String,
    current_tokens: String,
    name_tokens: String,
    project_tokens: String,
    artifact_tokens: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct UsageCacheRecord {
    pub provider: Provider,
    pub session_id: String,
    pub source_path: String,
    pub source_mtime_ns: i64,
    pub source_size: i64,
    pub model: Option<String>,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cached_input_tokens: i64,
    pub cache_write_tokens: i64,
    pub total_tokens: i64,
    pub estimated_cost_usd: Option<f64>,
    pub updated_at: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct HookObservation {
    pub provider: Provider,
    pub fingerprint: String,
    pub event_name: String,
    pub session_id: String,
    pub observed_at: f64,
    pub source: Option<String>,
    pub managed: bool,
}

#[derive(Clone, Debug)]
pub struct Store {
    path: PathBuf,
    validated: Arc<AtomicBool>,
}

/// A cheap, connection-local SQLite commit cursor for dynamic views. SQLite's
/// `data_version` advances only when another connection commits, so polling it
/// does not scan conversations or provider history.
pub struct StoreChangeWatcher {
    db: Connection,
    version: i64,
}

impl StoreChangeWatcher {
    pub fn changed(&mut self) -> Result<bool> {
        let version = self
            .db
            .query_row("PRAGMA data_version", [], |row| row.get(0))?;
        let changed = version != self.version;
        self.version = version;
        Ok(changed)
    }
}

/// One short, writer-serialized view shared by reconciliation and hook ingestion.
/// Identity checks, authoritative observations, projections and session writes
/// must use this same transaction so overlapping writers cannot publish stale
/// projections. Provider/process I/O belongs outside the transaction.
pub(crate) struct ReconcileLedger<'a> {
    tx: &'a Transaction<'a>,
}

/// One reconciliation's cross-process snapshot fence. SQLite's connection-local
/// `data_version` changes for commits made by every *other* connection but not
/// for this connection's own bounded batches. Beginning each batch with an
/// immediate transaction makes the version check and write one atomic gate.
pub(crate) struct ReconcileSession {
    db: Connection,
    version: i64,
}

impl ReconcileSession {
    pub(crate) fn transaction<T>(
        &mut self,
        operation: impl FnOnce(&ReconcileLedger<'_>) -> Result<T>,
    ) -> Result<T> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = tx.query_row("PRAGMA data_version", [], |row| row.get::<_, i64>(0))?;
        if current != self.version {
            bail!("local reconciliation was superseded by another process before it could commit")
        }
        let result = operation(&ReconcileLedger { tx: &tx })?;
        tx.commit()?;
        Ok(result)
    }
}

impl Store {
    pub fn from_paths(paths: &Paths) -> Self {
        Self {
            path: paths.database.clone(),
            validated: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn at(path: impl AsRef<Path>) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
            validated: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn exists(&self) -> bool {
        self.path.is_file()
    }

    pub fn change_watcher(&self) -> Result<StoreChangeWatcher> {
        let db = self.open_read()?;
        let version = db.query_row("PRAGMA data_version", [], |row| row.get(0))?;
        Ok(StoreChangeWatcher { db, version })
    }

    /// Create only the frozen current schema. Existing partial or legacy schemas are rejected.
    pub fn initialize(&self) -> Result<()> {
        if self.validated.load(Ordering::Acquire) && self.path.is_file() {
            return Ok(());
        }
        let parent = self
            .path
            .parent()
            .context("Pika database path has no parent directory")?;
        fs::create_dir_all(parent)
            .with_context(|| format!("cannot create Pika state directory {}", parent.display()))?;
        set_mode(parent, 0o700)?;

        let file_name = self
            .path
            .file_name()
            .and_then(|value| value.to_str())
            .context("Pika database path has no valid file name")?;
        let lock_path = parent.join(format!(".{file_name}.initialize.lock"));
        let mut options = OpenOptions::new();
        options.create(true).read(true).write(true);
        #[cfg(unix)]
        options.mode(0o600);
        let lock = options
            .open(&lock_path)
            .with_context(|| format!("cannot open schema lock {}", lock_path.display()))?;
        set_mode(&lock_path, 0o600)?;
        lock.lock_exclusive().context("cannot lock Pika schema")?;

        let result = self.initialize_locked();
        let unlock_result = FileExt::unlock(&lock).context("cannot unlock Pika schema");
        let result = result.and(unlock_result);
        if result.is_ok() {
            self.validated.store(true, Ordering::Release);
        }
        result
    }

    fn initialize_locked(&self) -> Result<()> {
        let existed = self.path.exists();
        let mut db = self.open_write_raw()?;
        let table_count: i64 = db.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
            [],
            |row| row.get(0),
        )?;
        if table_count == 0 {
            db.pragma_update(None, "journal_mode", "WAL")?;
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            tx.execute_batch(SCHEMA)?;
            tx.commit()?;
        } else {
            validate_schema(&db)?;
            db.pragma_update(None, "journal_mode", "WAL")?;
        }
        set_mode(&self.path, 0o600)?;
        if !existed {
            validate_schema(&db)?;
        }
        Ok(())
    }

    fn open_read(&self) -> Result<Connection> {
        let db = Connection::open_with_flags(
            &self.path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("cannot open Pika database at {}", self.path.display()))?;
        db.busy_timeout(BUSY_TIMEOUT)?;
        if !self.validated.load(Ordering::Acquire) {
            validate_schema(&db)?;
            self.validated.store(true, Ordering::Release);
        }
        Ok(db)
    }

    fn open_write_raw(&self) -> Result<Connection> {
        let db = Connection::open_with_flags(
            &self.path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("cannot open Pika database at {}", self.path.display()))?;
        db.busy_timeout(BUSY_TIMEOUT)?;
        Ok(db)
    }

    fn open_write(&self) -> Result<Connection> {
        self.initialize()?;
        self.open_write_raw()
    }

    fn open_fleet_cache(&self) -> Result<Connection> {
        let parent = self
            .path
            .parent()
            .context("Pika database path has no parent directory")?;
        fs::create_dir_all(parent)?;
        let file = self
            .path
            .file_name()
            .and_then(|value| value.to_str())
            .context("Pika database path has no valid file name")?;
        let path = parent.join(format!(".{file}.fleet-cache.sqlite3"));
        let db = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        db.busy_timeout(Duration::from_secs(30))?;
        db.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE IF NOT EXISTS projections(
               node_id TEXT NOT NULL, revision TEXT NOT NULL, kind TEXT NOT NULL,
               header TEXT NOT NULL, PRIMARY KEY(node_id,revision,kind));
             CREATE TABLE IF NOT EXISTS projection_chunks(
               node_id TEXT NOT NULL, revision TEXT NOT NULL, kind TEXT NOT NULL,
               chunk_index INTEGER NOT NULL, value TEXT NOT NULL,
               PRIMARY KEY(node_id,revision,kind,chunk_index));
             CREATE TABLE IF NOT EXISTS expert_rows(
               node_id TEXT NOT NULL,
               revision TEXT NOT NULL,
               ordinal INTEGER NOT NULL CHECK(ordinal >= 0 AND ordinal < 2000),
               row_json TEXT NOT NULL,
               row_bytes INTEGER NOT NULL CHECK(row_bytes > 0 AND row_bytes <= 4194304),
               live INTEGER NOT NULL CHECK(live IN (0,1)),
               profile_updated_at REAL NOT NULL,
               session_id TEXT NOT NULL,
               PRIMARY KEY(node_id,revision,ordinal)
             ) WITHOUT ROWID;
             CREATE VIRTUAL TABLE IF NOT EXISTS expert_search_fts_v2 USING fts5(
               node_id,
               revision,
               ordinal UNINDEXED,
               topic_tokens,
               scope_tokens,
               current_tokens,
               name_tokens,
               project_tokens,
               artifact_tokens,
               tokenize='unicode61 remove_diacritics 0'
             );",
        )?;
        set_mode(&path, 0o600)?;
        Ok(db)
    }

    fn stage_remote_cache(&self, prepared: &PreparedRemoteSnapshot) -> Result<()> {
        let mut db = self.open_fleet_cache()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(cache) = prepared.board_cache.as_ref() {
            tx.execute(
                "INSERT INTO projections(node_id,revision,kind,header) VALUES (?,?,?,?)",
                params![prepared.node_id, prepared.revision, "board", cache.header],
            )?;
            for (index, chunk) in cache.chunks.iter().enumerate() {
                tx.execute(
                    "INSERT INTO projection_chunks(node_id,revision,kind,chunk_index,value) VALUES (?,?,?,?,?)",
                    params![prepared.node_id, prepared.revision, "board", index, chunk],
                )?;
            }
        }
        if let Some(cache) = prepared.expert_cache.as_ref() {
            tx.execute(
                "INSERT INTO projections(node_id,revision,kind,header) VALUES (?,?,?,?)",
                params![prepared.node_id, prepared.revision, "expert", cache.header],
            )?;
            let indexed_node = fts_identity("n", &prepared.node_id);
            let indexed_revision = fts_identity("r", &prepared.revision);
            {
                let mut rich = tx.prepare(
                    "INSERT INTO expert_rows(node_id,revision,ordinal,row_json,row_bytes,live,profile_updated_at,session_id) VALUES (?,?,?,?,?,?,?,?)",
                )?;
                let mut search = tx.prepare(
                    "INSERT INTO expert_search_fts_v2(node_id,revision,ordinal,topic_tokens,scope_tokens,current_tokens,name_tokens,project_tokens,artifact_tokens) VALUES (?,?,?,?,?,?,?,?,?)",
                )?;
                for row in &cache.rows {
                    rich.execute(params![
                        prepared.node_id,
                        prepared.revision,
                        row.ordinal,
                        row.row_json,
                        row.row_json.len(),
                        row.live,
                        row.profile_updated_at,
                        row.session_id,
                    ])?;
                    search.execute(params![
                        indexed_node,
                        indexed_revision,
                        row.ordinal,
                        row.topic_tokens,
                        row.scope_tokens,
                        row.current_tokens,
                        row.name_tokens,
                        row.project_tokens,
                        row.artifact_tokens,
                    ])?;
                }
            }
        }
        tx.commit()?;
        Ok(())
    }

    fn discard_remote_cache_revision(&self, node_id: &str, revision: &str) {
        let Ok(mut db) = self.open_fleet_cache() else {
            return;
        };
        let Ok(tx) = db.transaction_with_behavior(TransactionBehavior::Immediate) else {
            return;
        };
        let _ = tx.execute(
            "DELETE FROM projection_chunks WHERE node_id=? AND revision=?",
            params![node_id, revision],
        );
        let _ = tx.execute(
            "DELETE FROM expert_search_fts_v2 WHERE node_id=? AND revision=?",
            params![fts_identity("n", node_id), fts_identity("r", revision)],
        );
        let _ = tx.execute(
            "DELETE FROM expert_rows WHERE node_id=? AND revision=?",
            params![node_id, revision],
        );
        let _ = tx.execute(
            "DELETE FROM projections WHERE node_id=? AND revision=?",
            params![node_id, revision],
        );
        let _ = tx.commit();
    }

    pub(crate) fn reconcile_transaction<T>(
        &self,
        operation: impl FnOnce(&ReconcileLedger<'_>) -> Result<T>,
    ) -> Result<T> {
        let mut db = self.open_write()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let result = operation(&ReconcileLedger { tx: &tx })?;
        tx.commit()?;
        Ok(result)
    }

    /// Capture a cross-process commit cursor before any slow OS/provider reads.
    /// All resulting reconciliation writes must use the returned connection.
    pub(crate) fn begin_reconcile_session(&self) -> Result<ReconcileSession> {
        let db = self.open_write()?;
        let version = db.query_row("PRAGMA data_version", [], |row| row.get(0))?;
        Ok(ReconcileSession { db, version })
    }

    pub fn upsert_session(&self, session: &Session, preserve_name: bool) -> Result<bool> {
        let mut db = self.open_write()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = upsert_session_tx(&tx, session, preserve_name)?;
        tx.commit()?;
        Ok(changed)
    }

    /// Clear verified process ownership while retaining the durable tmux home.
    /// Ordinary discovery upserts coalesce nullable runtime fields because a
    /// missing observation is not proof of an exit; callers use this only when
    /// a provider-exit callback supplies that proof.
    pub fn clear_session_runtime(
        &self,
        provider: Provider,
        session_id: &str,
        observed_at: f64,
    ) -> Result<bool> {
        if !self.exists() {
            return Ok(false);
        }
        let db = self.open_write()?;
        Ok(db.execute(
            "UPDATE sessions SET root_pid=NULL,updated_at=? WHERE provider=? AND session_id=?",
            params![observed_at, provider.as_str(), session_id],
        )? == 1)
    }

    pub fn get_session(&self, provider: Provider, session_id: &str) -> Result<Option<Session>> {
        if !self.exists() {
            return Ok(None);
        }
        let db = self.open_read()?;
        db.query_row(
            "SELECT * FROM sessions WHERE provider=? AND session_id=?",
            params![provider.as_str(), session_id],
            session_from_row,
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn get_session_by_thread(
        &self,
        provider: Provider,
        thread_id: &str,
    ) -> Result<Option<Session>> {
        if !self.exists() {
            return Ok(None);
        }
        let db = self.open_read()?;
        db.query_row(
            "SELECT * FROM sessions WHERE provider=? AND (session_id=? OR active_thread_id=?) ORDER BY CASE WHEN session_id=? THEN 0 ELSE 1 END LIMIT 1",
            params![provider.as_str(), thread_id, thread_id, thread_id],
            session_from_row,
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn list_sessions(&self) -> Result<Vec<Session>> {
        self.list_sessions_query(false)
    }

    pub fn list_untracked_sessions(&self) -> Result<Vec<Session>> {
        self.list_sessions_query(true)
    }

    pub fn list_provider_hidden_sessions(&self) -> Result<Vec<Session>> {
        if !self.exists() {
            return Ok(Vec::new());
        }
        let db = self.open_read()?;
        let mut statement = db.prepare(
            "SELECT sessions.* FROM sessions JOIN meta ON meta.key=('provider-hidden:' || sessions.provider || ':' || sessions.session_id)",
        )?;
        Ok(statement
            .query_map([], session_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    fn list_sessions_query(&self, untracked: bool) -> Result<Vec<Session>> {
        if !self.exists() {
            return Ok(Vec::new());
        }
        let db = self.open_read()?;
        let sql = if untracked {
            "SELECT sessions.* FROM sessions JOIN untracked_sessions USING(provider,session_id)"
        } else {
            "SELECT sessions.* FROM sessions LEFT JOIN untracked_sessions USING(provider,session_id) WHERE untracked_sessions.session_id IS NULL"
        };
        let mut statement = db.prepare(sql)?;
        let mut sessions = statement
            .query_map([], session_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        sessions.sort_by(|a, b| {
            a.status
                .attention_order()
                .cmp(&b.status.attention_order())
                .then_with(|| b.last_event_at.total_cmp(&a.last_event_at))
                .then_with(|| {
                    a.display_name()
                        .to_lowercase()
                        .cmp(&b.display_name().to_lowercase())
                })
        });
        Ok(sessions)
    }

    pub fn find_named(&self, name: &str) -> Result<Vec<Session>> {
        Ok(self
            .list_sessions()?
            .into_iter()
            .filter(|session| {
                session
                    .name
                    .as_deref()
                    .is_some_and(|candidate| candidate.eq_ignore_ascii_case(name))
                    || session.session_id.eq_ignore_ascii_case(name)
                    || session
                        .active_thread_id
                        .as_deref()
                        .is_some_and(|candidate| candidate.eq_ignore_ascii_case(name))
            })
            .collect())
    }

    pub fn is_untracked(&self, provider: Provider, session_id: &str) -> Result<bool> {
        if !self.exists() {
            return Ok(false);
        }
        let db = self.open_read()?;
        is_untracked_connection(&db, provider, session_id).map_err(Into::into)
    }

    pub fn untrack_session(&self, provider: Provider, session_id: &str) -> Result<()> {
        let mut db = self.open_write()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO untracked_sessions(provider,session_id,untracked_at) VALUES (?,?,?) ON CONFLICT(provider,session_id) DO UPDATE SET untracked_at=excluded.untracked_at",
            params![provider.as_str(), session_id, now()],
        )?;
        tx.execute(
            "DELETE FROM meta WHERE key=?",
            [provider_hidden_key(provider, session_id)],
        )?;
        for table in [
            "usage_cache",
            "session_events",
            "session_status_observations",
            "identity_interruptions",
            "expert_refresh_attempts",
            "live_owners",
            "recovery_owners",
            "launch_reservations",
        ] {
            tx.execute(
                &format!("DELETE FROM {table} WHERE provider=? AND session_id=?"),
                params![provider.as_str(), session_id],
            )?;
        }
        tx.execute(
            "DELETE FROM launch_bindings WHERE provider=? AND session_id=?",
            params![provider.as_str(), session_id],
        )?;
        tx.execute(
            "UPDATE sessions SET tmux_session=NULL,tmux_pane=NULL,root_pid=NULL,status='PARKED',unread=0,error=NULL,attention_reason=NULL,updated_at=? WHERE provider=? AND session_id=?",
            params![now(), provider.as_str(), session_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn restore_tracking(&self, provider: Provider, session_id: &str) -> Result<bool> {
        let mut db = self.open_write()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let restored = tx.execute(
            "DELETE FROM untracked_sessions WHERE provider=? AND session_id=?",
            params![provider.as_str(), session_id],
        )? == 1;
        tx.execute(
            "DELETE FROM meta WHERE key=?",
            [provider_hidden_key(provider, session_id)],
        )?;
        tx.commit()?;
        Ok(restored)
    }

    pub fn delete_session(
        &self,
        provider: Provider,
        session_id: &str,
        preserve_live_owners: bool,
    ) -> Result<bool> {
        self.reconcile_transaction(|ledger| {
            ledger.delete_session(provider, session_id, preserve_live_owners)
        })
    }

    pub fn record_status_observation(
        &self,
        provider: Provider,
        session_id: &str,
        observation: &StatusObservation,
    ) -> Result<bool> {
        let db = self.open_write()?;
        let changed = db.execute(
            r#"INSERT INTO session_status_observations(provider,session_id,kind,status,unread,attention_reason,error,observed_at,source)
            VALUES (?,?,?,?,?,?,?,?,?) ON CONFLICT(provider,session_id,kind) DO UPDATE SET
            status=excluded.status,unread=excluded.unread,attention_reason=excluded.attention_reason,
            error=excluded.error,observed_at=excluded.observed_at,source=excluded.source
            WHERE excluded.observed_at>=session_status_observations.observed_at"#,
            params![provider.as_str(), session_id, observation_kind_str(observation.kind), observation.status.as_str(), bool_i64(observation.unread), observation.attention_reason, observation.error, observation.observed_at, observation.source],
        )? == 1;
        Ok(changed)
    }

    pub fn status_observations(
        &self,
        provider: Provider,
        session_id: &str,
    ) -> Result<Vec<StatusObservation>> {
        if !self.exists() {
            return Ok(Vec::new());
        }
        let db = self.open_read()?;
        let mut statement = db.prepare(
            "SELECT kind,status,unread,attention_reason,error,observed_at,source FROM session_status_observations WHERE provider=? AND session_id=? ORDER BY kind",
        )?;
        Ok(statement
            .query_map(params![provider.as_str(), session_id], |row| {
                Ok(StatusObservation {
                    kind: parse_observation_kind(row.get_ref(0)?.as_str()?, 0)?,
                    status: parse_status(row.get_ref(1)?.as_str()?, 1)?,
                    unread: row.get::<_, i64>(2)? != 0,
                    attention_reason: row.get(3)?,
                    error: row.get(4)?,
                    observed_at: row.get(5)?,
                    source: row.get(6)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn clear_status_observation(
        &self,
        provider: Provider,
        session_id: &str,
        kind: ObservationKind,
    ) -> Result<bool> {
        let db = self.open_write()?;
        Ok(db.execute(
            "DELETE FROM session_status_observations WHERE provider=? AND session_id=? AND kind=?",
            params![provider.as_str(), session_id, observation_kind_str(kind)],
        )? == 1)
    }

    pub fn acknowledge_attention(
        &self,
        provider: Provider,
        session_id: &str,
        expected_event_at: f64,
        attaching: bool,
    ) -> Result<bool> {
        let mut db = self.open_write()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let statuses = if attaching {
            &[Status::Ready, Status::Error][..]
        } else {
            &[Status::Ready][..]
        };
        let current: Option<(Status, bool, f64)> = tx
            .query_row(
                "SELECT status,unread,last_event_at FROM sessions WHERE provider=? AND session_id=?",
                params![provider.as_str(), session_id],
                |row| {
                    Ok((
                        parse_status(row.get_ref(0)?.as_str()?, 0)?,
                        row.get::<_, i64>(1)? != 0,
                        row.get(2)?,
                    ))
                },
            )
            .optional()?;
        let allowed = current.as_ref().is_some_and(|(status, unread, event_at)| {
            *unread && *event_at == expected_event_at && statuses.contains(status)
        });
        if !allowed {
            tx.commit()?;
            return Ok(false);
        }
        let status = current.expect("validated current session").0;
        let changed = tx.execute(
            "UPDATE sessions SET unread=0,updated_at=? WHERE provider=? AND session_id=? AND unread=1 AND last_event_at=? AND status=?",
            params![now(), provider.as_str(), session_id, expected_event_at, status.as_str()],
        )? == 1;
        if changed {
            tx.execute(
                "UPDATE session_status_observations SET unread=0 WHERE provider=? AND session_id=? AND unread=1 AND observed_at=? AND status=?",
                params![provider.as_str(), session_id, expected_event_at, status.as_str()],
            )?;
        }
        tx.commit()?;
        Ok(changed)
    }

    pub fn collect_result(
        &self,
        provider: Provider,
        session_id: &str,
        expected_event_at: f64,
    ) -> Result<Option<i64>> {
        let mut db = self.open_write()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            "UPDATE sessions SET unread=0,updated_at=? WHERE provider=? AND session_id=? AND status='READY' AND unread=1 AND last_event_at=?",
            params![now(), provider.as_str(), session_id, expected_event_at],
        )? == 1;
        if !changed {
            tx.commit()?;
            return Ok(None);
        }
        tx.execute(
            "UPDATE session_status_observations SET unread=0 WHERE provider=? AND session_id=? AND status='READY' AND observed_at=?",
            params![provider.as_str(), session_id, expected_event_at],
        )?;
        let remaining = tx.query_row(
            "SELECT COUNT(*) FROM sessions WHERE status='READY' AND unread=1",
            [],
            |row| row.get(0),
        )?;
        tx.commit()?;
        Ok(Some(remaining))
    }

    pub fn list_activity_events(&self, limit: usize) -> Result<Vec<ActivityEvent>> {
        if !(1..=500).contains(&limit) {
            bail!("activity event limit must be between 1 and 500");
        }
        if !self.exists() {
            return Ok(Vec::new());
        }
        let db = self.open_read()?;
        let mut statement = db.prepare(
            "SELECT events.event_id,events.provider,events.session_id,sessions.name,events.status,events.attention_reason,events.error,events.event_at FROM session_events AS events LEFT JOIN sessions USING(provider,session_id) ORDER BY events.event_id DESC LIMIT ?",
        )?;
        Ok(statement
            .query_map(params![limit as i64], |row| {
                Ok(ActivityEvent {
                    event_id: row.get(0)?,
                    provider: parse_provider(row.get_ref(1)?.as_str()?, 1)?,
                    session_id: row.get(2)?,
                    name: row.get(3)?,
                    status: parse_status(row.get_ref(4)?.as_str()?, 4)?,
                    attention_reason: row.get(5)?,
                    error: row.get(6)?,
                    event_at: row.get(7)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn attention_event_counts(&self, since: f64, until: f64) -> Result<BTreeMap<String, i64>> {
        if !self.exists() {
            return Ok(BTreeMap::new());
        }
        let db = self.open_read()?;
        let mut statement = db.prepare(
            "SELECT status,COUNT(*) FROM session_events WHERE event_at>? AND event_at<=? GROUP BY status",
        )?;
        let rows = statement.query_map(params![since, until], |row| {
            let encoded = row.get_ref(0)?.as_str()?;
            parse_status(encoded, 0)?;
            Ok((encoded.to_owned(), row.get(1)?))
        })?;
        Ok(rows.collect::<rusqlite::Result<BTreeMap<_, _>>>()?)
    }

    pub fn capture_identity_interruption(
        &self,
        provider: Provider,
        session_id: &str,
    ) -> Result<()> {
        self.reconcile_transaction(|ledger| {
            ledger.capture_identity_interruption(provider, session_id)
        })
    }

    pub fn get_identity_interruption(
        &self,
        provider: Provider,
        session_id: &str,
    ) -> Result<Option<IdentityInterruption>> {
        if !self.exists() {
            return Ok(None);
        }
        let db = self.open_read()?;
        db.query_row(
            "SELECT provider,session_id,status,unread,attention_reason,error,last_event_at FROM identity_interruptions WHERE provider=? AND session_id=?",
            params![provider.as_str(), session_id],
            |row| {
                Ok(IdentityInterruption {
                    provider: parse_provider(row.get_ref(0)?.as_str()?, 0)?,
                    session_id: row.get(1)?,
                    status: parse_status(row.get_ref(2)?.as_str()?, 2)?,
                    unread: row.get::<_, i64>(3)? != 0,
                    attention_reason: row.get(4)?,
                    error: row.get(5)?,
                    last_event_at: row.get(6)?,
                })
            },
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn clear_identity_interruption(
        &self,
        provider: Provider,
        session_id: &str,
    ) -> Result<bool> {
        let db = self.open_write()?;
        Ok(db.execute(
            "DELETE FROM identity_interruptions WHERE provider=? AND session_id=?",
            params![provider.as_str(), session_id],
        )? == 1)
    }

    pub fn add_pending(&self, pending: &PendingLaunch) -> Result<bool> {
        let mut db = self.open_write()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let competing = tx
            .query_row(
                "SELECT 1 FROM pending_launches WHERE provider=? AND name=? COLLATE NOCASE AND launch_token<>? LIMIT 1",
                params![pending.provider.as_str(), pending.name, pending.launch_token],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if competing {
            tx.commit()?;
            return Ok(false);
        }
        let preexisting = encode_optional_strings(&pending.preexisting_session_ids)?;
        tx.execute(
            r#"INSERT INTO pending_launches(launch_token,provider,name,cwd,tmux_session,tmux_pane,expected_session_id,root_pid,root_pid_start,preexisting_session_ids_json,candidate_session_id,candidate_observed_at,created_at)
            VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?) ON CONFLICT(launch_token) DO UPDATE SET
            tmux_session=COALESCE(excluded.tmux_session,pending_launches.tmux_session),
            tmux_pane=COALESCE(excluded.tmux_pane,pending_launches.tmux_pane),
            expected_session_id=COALESCE(excluded.expected_session_id,pending_launches.expected_session_id),
            root_pid=COALESCE(excluded.root_pid,pending_launches.root_pid),
            root_pid_start=COALESCE(excluded.root_pid_start,pending_launches.root_pid_start),
            preexisting_session_ids_json=COALESCE(excluded.preexisting_session_ids_json,pending_launches.preexisting_session_ids_json)"#,
            params![pending.launch_token, pending.provider.as_str(), pending.name, pending.cwd, pending.tmux_session, pending.tmux_pane, pending.expected_session_id, pending.root_pid, pending.root_pid_start, preexisting, pending.candidate_session_id, pending.candidate_observed_at, nonzero_or(pending.created_at, now())],
        )?;
        tx.execute(
            "INSERT INTO meta(key,value) VALUES (?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![launch_phase_key(&pending.launch_token), LaunchPhase::Reserved.as_str()],
        )?;
        tx.commit()?;
        Ok(true)
    }

    pub fn list_pending(&self) -> Result<Vec<PendingLaunch>> {
        if !self.exists() {
            return Ok(Vec::new());
        }
        let db = self.open_read()?;
        let mut statement = db.prepare("SELECT * FROM pending_launches ORDER BY created_at")?;
        Ok(statement
            .query_map([], pending_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn delete_pending_if_created(&self, launch_token: &str, created_at: f64) -> Result<bool> {
        let mut db = self.open_write()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let deleted = tx.execute(
            "DELETE FROM pending_launches WHERE launch_token=? AND created_at=?",
            params![launch_token, created_at],
        )? == 1;
        if deleted {
            tx.execute(
                "DELETE FROM meta WHERE key=?",
                [launch_phase_key(launch_token)],
            )?;
        }
        tx.commit()?;
        Ok(deleted)
    }

    pub fn get_pending(&self, launch_token: &str) -> Result<Option<PendingLaunch>> {
        if !self.exists() {
            return Ok(None);
        }
        let db = self.open_read()?;
        db.query_row(
            "SELECT * FROM pending_launches WHERE launch_token=?",
            [launch_token],
            pending_from_row,
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn find_pending_for_pane(&self, pane: &str) -> Result<Option<PendingLaunch>> {
        if !self.exists() {
            return Ok(None);
        }
        let db = self.open_read()?;
        db.query_row(
            "SELECT * FROM pending_launches WHERE tmux_pane=? ORDER BY created_at DESC LIMIT 1",
            [pane],
            pending_from_row,
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn observe_pending_candidate(
        &self,
        launch_token: &str,
        session_id: &str,
        observed_at: f64,
    ) -> Result<Option<f64>> {
        let mut db = self.open_write()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current: Option<(Option<String>, Option<f64>)> = tx
            .query_row(
                "SELECT candidate_session_id,candidate_observed_at FROM pending_launches WHERE launch_token=?",
                [launch_token],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        match current {
            None => {
                tx.commit()?;
                Ok(None)
            }
            Some((candidate, first_seen)) if candidate.as_deref() == Some(session_id) => {
                tx.commit()?;
                Ok(first_seen)
            }
            Some(_) => {
                tx.execute(
                    "UPDATE pending_launches SET candidate_session_id=?,candidate_observed_at=? WHERE launch_token=?",
                    params![session_id, observed_at, launch_token],
                )?;
                tx.commit()?;
                Ok(None)
            }
        }
    }

    pub fn finalize_pending_pane(
        &self,
        launch_token: &str,
        tmux_session: &str,
        tmux_pane: &str,
        root_pid: Option<i64>,
        root_pid_start: Option<i64>,
    ) -> Result<Option<(Provider, String)>> {
        let mut db = self.open_write()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let binding = launch_binding_tx(&tx, launch_token)?;
        tx.execute(
            "UPDATE pending_launches SET tmux_session=?,tmux_pane=?,root_pid=?,root_pid_start=? WHERE launch_token=?",
            params![tmux_session, tmux_pane, root_pid, root_pid_start, launch_token],
        )?;
        tx.execute(
            "INSERT INTO meta(key,value) VALUES (?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![launch_phase_key(launch_token), LaunchPhase::PaneAllocated.as_str()],
        )?;
        tx.commit()?;
        Ok(binding)
    }

    pub fn set_launch_phase(&self, launch_token: &str, phase: LaunchPhase) -> Result<bool> {
        let mut db = self.open_write()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let exists = tx
            .query_row(
                "SELECT 1 FROM pending_launches WHERE launch_token=?",
                [launch_token],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if exists {
            tx.execute(
                "INSERT INTO meta(key,value) VALUES (?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                params![launch_phase_key(launch_token), phase.as_str()],
            )?;
        }
        tx.commit()?;
        Ok(exists)
    }

    pub fn observe_launched_generation(
        &self,
        launch_token: &str,
        pid: i64,
        start_time: i64,
    ) -> Result<bool> {
        let mut db = self.open_write()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            "UPDATE pending_launches SET root_pid=?,root_pid_start=? WHERE launch_token=?",
            params![pid, start_time, launch_token],
        )? == 1;
        if changed {
            tx.execute(
                "INSERT INTO meta(key,value) VALUES (?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                params![launch_phase_key(launch_token), LaunchPhase::ProviderObserved.as_str()],
            )?;
        }
        tx.commit()?;
        Ok(changed)
    }

    pub fn get_launch_phase(&self, launch_token: &str) -> Result<Option<LaunchPhase>> {
        if !self.exists() {
            return Ok(None);
        }
        let db = self.open_read()?;
        let value: Option<String> = db
            .query_row(
                "SELECT value FROM meta WHERE key=?",
                [launch_phase_key(launch_token)],
                |row| row.get(0),
            )
            .optional()?;
        Ok(value.as_deref().and_then(LaunchPhase::parse))
    }

    pub fn delete_pending(&self, launch_token: &str) -> Result<bool> {
        self.reconcile_transaction(|ledger| ledger.delete_pending(launch_token))
    }

    pub fn prune_pending(&self, pending_before: f64, binding_before: f64) -> Result<usize> {
        let mut db = self.open_write()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let stale_tokens = {
            let mut statement =
                tx.prepare("SELECT launch_token FROM pending_launches WHERE created_at<?")?;
            statement
                .query_map([pending_before], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let deleted = tx.execute(
            "DELETE FROM pending_launches WHERE created_at<?",
            [pending_before],
        )?;
        for token in stale_tokens {
            tx.execute("DELETE FROM meta WHERE key=?", [launch_phase_key(&token)])?;
        }
        tx.execute(
            "DELETE FROM launch_bindings WHERE created_at<?",
            [binding_before],
        )?;
        tx.commit()?;
        Ok(deleted)
    }

    pub fn bind_launch(
        &self,
        launch_token: &str,
        provider: Provider,
        session_id: &str,
    ) -> Result<bool> {
        self.reconcile_transaction(|ledger| ledger.bind_launch(launch_token, provider, session_id))
    }

    pub fn get_launch_binding(&self, launch_token: &str) -> Result<Option<(Provider, String)>> {
        if !self.exists() {
            return Ok(None);
        }
        let db = self.open_read()?;
        launch_binding_connection(&db, launch_token)
    }

    pub fn delete_launch_binding(&self, launch_token: &str) -> Result<bool> {
        let db = self.open_write()?;
        Ok(db.execute(
            "DELETE FROM launch_bindings WHERE launch_token=?",
            [launch_token],
        )? == 1)
    }

    pub fn certify_launch(
        &self,
        launch_token: &str,
        provider: Provider,
        session_id: &str,
        pid: i64,
        start_time: i64,
    ) -> Result<bool> {
        self.reconcile_transaction(|ledger| {
            ledger.certify_launch(launch_token, provider, session_id, pid, start_time)
        })
    }

    /// Atomically retarget a certified launch. Liveness checks belong to the caller.
    #[allow(clippy::too_many_arguments)]
    pub fn switch_launch_binding(
        &self,
        launch_token: &str,
        provider: Provider,
        from_session_id: &str,
        to_session_id: &str,
        pid: i64,
        start_time: i64,
    ) -> Result<bool> {
        self.reconcile_transaction(|ledger| {
            ledger.switch_launch_binding(
                launch_token,
                provider,
                from_session_id,
                to_session_id,
                pid,
                start_time,
            )
        })
    }

    pub fn reserve_resume(
        &self,
        provider: Provider,
        session_id: &str,
        token: &str,
        owner_pid: i64,
        owner_start_time: i64,
    ) -> Result<bool> {
        let db = self.open_write()?;
        Ok(db.execute(
            "INSERT OR IGNORE INTO launch_reservations(provider,session_id,token,owner_pid,owner_start_time,created_at) VALUES (?,?,?,?,?,?)",
            params![provider.as_str(), session_id, token, owner_pid, owner_start_time, now()],
        )? == 1)
    }

    /// Reclaim only the exact stale PID generation that the caller has independently disproved.
    #[allow(clippy::too_many_arguments)]
    pub fn reclaim_resume(
        &self,
        provider: Provider,
        session_id: &str,
        stale_pid: i64,
        stale_start_time: i64,
        token: &str,
        owner_pid: i64,
        owner_start_time: i64,
    ) -> Result<bool> {
        let mut db = self.open_write()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let removed = tx.execute(
            "DELETE FROM launch_reservations WHERE provider=? AND session_id=? AND owner_pid=? AND owner_start_time=?",
            params![provider.as_str(), session_id, stale_pid, stale_start_time],
        )? == 1;
        if !removed {
            tx.commit()?;
            return Ok(false);
        }
        let inserted = tx.execute(
            "INSERT OR IGNORE INTO launch_reservations(provider,session_id,token,owner_pid,owner_start_time,created_at) VALUES (?,?,?,?,?,?)",
            params![provider.as_str(), session_id, token, owner_pid, owner_start_time, now()],
        )? == 1;
        if inserted {
            tx.commit()?;
            Ok(true)
        } else {
            // Dropping the transaction rolls the compare-and-swap deletion back.
            Ok(false)
        }
    }

    pub fn get_resume_reservation(
        &self,
        provider: Provider,
        session_id: &str,
    ) -> Result<Option<LaunchReservation>> {
        if !self.exists() {
            return Ok(None);
        }
        let db = self.open_read()?;
        db.query_row(
            "SELECT provider,session_id,token,owner_pid,owner_start_time,created_at FROM launch_reservations WHERE provider=? AND session_id=?",
            params![provider.as_str(), session_id],
            |row| {
                Ok(LaunchReservation {
                    provider: parse_provider(row.get_ref(0)?.as_str()?, 0)?,
                    session_id: row.get(1)?,
                    token: row.get(2)?,
                    owner_pid: row.get(3)?,
                    owner_start_time: row.get(4)?,
                    created_at: row.get(5)?,
                })
            },
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn list_resume_reservations(&self) -> Result<Vec<LaunchReservation>> {
        if !self.exists() {
            return Ok(Vec::new());
        }
        let db = self.open_read()?;
        let mut statement = db.prepare(
            "SELECT provider,session_id,token,owner_pid,owner_start_time,created_at FROM launch_reservations ORDER BY created_at",
        )?;
        Ok(statement
            .query_map([], |row| {
                Ok(LaunchReservation {
                    provider: parse_provider(row.get_ref(0)?.as_str()?, 0)?,
                    session_id: row.get(1)?,
                    token: row.get(2)?,
                    owner_pid: row.get(3)?,
                    owner_start_time: row.get(4)?,
                    created_at: row.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn release_resume_if_generation(&self, reservation: &LaunchReservation) -> Result<bool> {
        let db = self.open_write()?;
        Ok(db.execute(
            "DELETE FROM launch_reservations WHERE provider=? AND session_id=? AND token=? AND owner_pid IS ? AND owner_start_time IS ? AND created_at=?",
            params![reservation.provider.as_str(), reservation.session_id, reservation.token, reservation.owner_pid, reservation.owner_start_time, reservation.created_at],
        )? == 1)
    }

    pub fn release_resume(
        &self,
        provider: Provider,
        session_id: &str,
        token: &str,
    ) -> Result<bool> {
        let db = self.open_write()?;
        Ok(db.execute(
            "DELETE FROM launch_reservations WHERE provider=? AND session_id=? AND token=?",
            params![provider.as_str(), session_id, token],
        )? == 1)
    }

    pub fn set_live_owner(&self, owner: &LiveOwner) -> Result<bool> {
        self.reconcile_transaction(|ledger| ledger.set_live_owner(owner))
    }

    pub fn live_owners(&self, provider: Provider, session_id: &str) -> Result<Vec<LiveOwner>> {
        if !self.exists() {
            return Ok(Vec::new());
        }
        let db = self.open_read()?;
        let mut statement = db.prepare(
            "SELECT provider,session_id,pid,start_time,owner_token,last_seen FROM live_owners WHERE provider=? AND session_id=? ORDER BY pid,owner_token",
        )?;
        Ok(statement
            .query_map(params![provider.as_str(), session_id], live_owner_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn list_live_owners(&self) -> Result<Vec<LiveOwner>> {
        if !self.exists() {
            return Ok(Vec::new());
        }
        let db = self.open_read()?;
        let mut statement = db.prepare(
            "SELECT provider,session_id,pid,start_time,owner_token,last_seen FROM live_owners ORDER BY provider,session_id,pid,owner_token",
        )?;
        Ok(statement
            .query_map([], live_owner_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn delete_live_owner(
        &self,
        provider: Provider,
        session_id: &str,
        pid: Option<i64>,
        owner_token: Option<&str>,
    ) -> Result<usize> {
        self.reconcile_transaction(|ledger| {
            ledger.delete_live_owners(provider, session_id, pid, owner_token)
        })
    }

    pub fn delete_other_live_owner_sessions(
        &self,
        provider: Provider,
        pid: i64,
        keep_session_id: &str,
    ) -> Result<usize> {
        self.reconcile_transaction(|ledger| {
            ledger.delete_other_live_owner_sessions(provider, pid, keep_session_id)
        })
    }

    pub fn set_recovery_owner(&self, owner: &RecoveryOwner) -> Result<()> {
        let db = self.open_write()?;
        put_recovery_owner_connection(&db, owner)?;
        Ok(())
    }

    pub fn get_recovery_owner(
        &self,
        provider: Provider,
        session_id: &str,
    ) -> Result<Option<RecoveryOwner>> {
        if !self.exists() {
            return Ok(None);
        }
        let db = self.open_read()?;
        recovery_owner_connection(&db, provider, session_id)
    }

    pub fn delete_recovery_owner(&self, provider: Provider, session_id: &str) -> Result<bool> {
        let db = self.open_write()?;
        Ok(db.execute(
            "DELETE FROM recovery_owners WHERE provider=? AND session_id=?",
            params![provider.as_str(), session_id],
        )? == 1)
    }

    pub fn list_experts(&self) -> Result<Vec<ExpertProfile>> {
        if !self.exists() {
            return Ok(Vec::new());
        }
        let db = self.open_read()?;
        let mut statement = db.prepare("SELECT * FROM expert_profiles ORDER BY updated_at DESC")?;
        Ok(statement
            .query_map([], expert_profile_from_row)?
            .map(|result| result.map(|stored| stored.profile))
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn list_nodes(&self) -> Result<Vec<FleetNode>> {
        if !self.exists() {
            return Ok(Vec::new());
        }
        let db = self.open_read()?;
        let mut statement =
            db.prepare("SELECT * FROM fleet_nodes ORDER BY alias COLLATE NOCASE")?;
        Ok(statement
            .query_map([], fleet_node_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn local_node_id(&self) -> Result<Option<String>> {
        if !self.exists() {
            return Ok(None);
        }
        let db = self.open_read()?;
        let value: Option<String> = db
            .query_row(
                "SELECT value FROM meta WHERE key='fleet:node_id'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        value
            .map(|node_id| {
                Uuid::parse_str(&node_id)
                    .map(|id| id.to_string())
                    .context("Pika's stored fleet node UUID is invalid")
            })
            .transpose()
    }
}

impl Store {
    pub fn put_expert_profile(&self, stored: &StoredExpertProfile) -> Result<StoredExpertProfile> {
        let mut db = self.open_write()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let tracked = tx
            .query_row(
                "SELECT 1 FROM sessions WHERE provider=? AND session_id=?",
                params![stored.profile.provider.as_str(), stored.profile.session_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if !tracked {
            bail!("expert profile requires a tracked Pika session");
        }
        let existing = expert_profile_tx(&tx, stored.profile.provider, &stored.profile.session_id)?;
        let scope_changed = existing.as_ref().is_none_or(|old| {
            old.profile.summary != stored.profile.summary
                || old.profile.topics != stored.profile.topics
                || old.profile.artifacts != stored.profile.artifacts
        });
        let work_changed = existing
            .as_ref()
            .is_none_or(|old| old.profile.current_state != stored.profile.current_state);
        let verified = stored.profile.source == "interview"
            && stored.transcript_mtime_ns.is_some()
            && stored.transcript_size.is_some();
        if let Some(old) = &existing
            && !scope_changed
            && !work_changed
        {
            if verified {
                tx.execute(
                    "UPDATE expert_profiles SET transcript_mtime_ns=?,transcript_size=?,current_state_mtime_ns=?,current_state_size=? WHERE provider=? AND session_id=?",
                    params![stored.transcript_mtime_ns, stored.transcript_size, stored.transcript_mtime_ns, stored.transcript_size, stored.profile.provider.as_str(), stored.profile.session_id],
                )?;
                let mut reaffirmed = old.clone();
                reaffirmed.transcript_mtime_ns = stored.transcript_mtime_ns;
                reaffirmed.transcript_size = stored.transcript_size;
                reaffirmed.current_state_mtime_ns = stored.transcript_mtime_ns;
                reaffirmed.current_state_size = stored.transcript_size;
                tx.commit()?;
                return Ok(reaffirmed);
            }
            let unchanged = old.clone();
            tx.commit()?;
            return Ok(unchanged);
        }
        let updated_at = nonzero_or(stored.profile.updated_at, now());
        let scope_updated_at = if scope_changed {
            updated_at
        } else {
            existing
                .as_ref()
                .map_or(0.0, |old| old.profile.scope_updated_at)
        };
        let current_state_updated_at = if work_changed && !stored.profile.current_state.is_empty() {
            updated_at
        } else {
            existing
                .as_ref()
                .map_or(0.0, |old| old.profile.current_state_updated_at)
        };
        let current_state_mtime_ns = if work_changed || verified {
            stored.transcript_mtime_ns
        } else {
            existing.as_ref().and_then(|old| old.current_state_mtime_ns)
        };
        let current_state_size = if work_changed || verified {
            stored.transcript_size
        } else {
            existing.as_ref().and_then(|old| old.current_state_size)
        };
        let topics = serde_json::to_string(&stored.profile.topics)?;
        let artifacts = serde_json::to_string(&stored.profile.artifacts)?;
        tx.execute(
            r#"INSERT INTO expert_profiles(provider,session_id,summary,current_state,topics_json,artifacts_json,source,transcript_mtime_ns,transcript_size,updated_at,scope_updated_at,current_state_updated_at,current_state_mtime_ns,current_state_size)
            VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?) ON CONFLICT(provider,session_id) DO UPDATE SET
            summary=excluded.summary,current_state=excluded.current_state,topics_json=excluded.topics_json,
            artifacts_json=excluded.artifacts_json,source=excluded.source,
            transcript_mtime_ns=excluded.transcript_mtime_ns,transcript_size=excluded.transcript_size,
            updated_at=excluded.updated_at,scope_updated_at=excluded.scope_updated_at,
            current_state_updated_at=excluded.current_state_updated_at,
            current_state_mtime_ns=excluded.current_state_mtime_ns,current_state_size=excluded.current_state_size"#,
            params![stored.profile.provider.as_str(), stored.profile.session_id, stored.profile.summary, stored.profile.current_state, topics, artifacts, stored.profile.source, stored.transcript_mtime_ns, stored.transcript_size, updated_at, scope_updated_at, current_state_updated_at, current_state_mtime_ns, current_state_size],
        )?;
        let mut result = stored.clone();
        result.profile.updated_at = updated_at;
        result.profile.scope_updated_at = scope_updated_at;
        result.profile.current_state_updated_at = current_state_updated_at;
        result.current_state_mtime_ns = current_state_mtime_ns;
        result.current_state_size = current_state_size;
        tx.commit()?;
        Ok(result)
    }

    pub fn get_stored_expert_profile(
        &self,
        provider: Provider,
        session_id: &str,
    ) -> Result<Option<StoredExpertProfile>> {
        if !self.exists() {
            return Ok(None);
        }
        let db = self.open_read()?;
        expert_profile_connection(&db, provider, session_id)
    }

    pub fn list_stored_expert_profiles(&self) -> Result<Vec<StoredExpertProfile>> {
        if !self.exists() {
            return Ok(Vec::new());
        }
        let db = self.open_read()?;
        let mut statement = db.prepare("SELECT * FROM expert_profiles ORDER BY updated_at DESC")?;
        Ok(statement
            .query_map([], expert_profile_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn delete_expert_profile(&self, provider: Provider, session_id: &str) -> Result<bool> {
        let db = self.open_write()?;
        Ok(db.execute(
            "DELETE FROM expert_profiles WHERE provider=? AND session_id=?",
            params![provider.as_str(), session_id],
        )? == 1)
    }

    pub fn put_expert_refresh_attempt(
        &self,
        attempt: &ExpertRefreshAttempt,
    ) -> Result<ExpertRefreshAttempt> {
        let db = self.open_write()?;
        let attempted_at = nonzero_or(attempt.attempted_at, now());
        db.execute(
            r#"INSERT INTO expert_refresh_attempts(provider,session_id,reset_at,status,detail,attempted_at)
            VALUES (?,?,?,?,?,?) ON CONFLICT(provider,session_id,reset_at) DO UPDATE SET
            status=excluded.status,detail=excluded.detail,attempted_at=excluded.attempted_at"#,
            params![attempt.provider.as_str(), attempt.session_id, attempt.reset_at, attempt.status, attempt.detail, attempted_at],
        )?;
        let mut result = attempt.clone();
        result.attempted_at = attempted_at;
        Ok(result)
    }

    /// Atomically reserve one provider/reset-cycle interview before quota is spent.
    /// Returns `false` when another refresher already claimed the same card.
    pub fn claim_expert_refresh_attempt(&self, attempt: &ExpertRefreshAttempt) -> Result<bool> {
        let mut db = self.open_write()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let attempted_at = nonzero_or(attempt.attempted_at, now());
        let inserted = tx.execute(
            r#"INSERT OR IGNORE INTO expert_refresh_attempts(
                provider,session_id,reset_at,status,detail,attempted_at
            ) VALUES (?,?,?,?,?,?)"#,
            params![
                attempt.provider.as_str(),
                attempt.session_id,
                attempt.reset_at,
                attempt.status,
                attempt.detail,
                attempted_at
            ],
        )? == 1;
        tx.commit()?;
        Ok(inserted)
    }

    pub fn get_expert_refresh_attempt(
        &self,
        provider: Provider,
        session_id: &str,
        reset_at: i64,
    ) -> Result<Option<ExpertRefreshAttempt>> {
        if !self.exists() {
            return Ok(None);
        }
        let db = self.open_read()?;
        db.query_row(
            "SELECT provider,session_id,reset_at,status,detail,attempted_at FROM expert_refresh_attempts WHERE provider=? AND session_id=? AND reset_at=?",
            params![provider.as_str(), session_id, reset_at],
            |row| {
                Ok(ExpertRefreshAttempt {
                    provider: parse_provider(row.get_ref(0)?.as_str()?, 0)?,
                    session_id: row.get(1)?,
                    reset_at: row.get(2)?,
                    status: row.get(3)?,
                    detail: row.get(4)?,
                    attempted_at: row.get(5)?,
                })
            },
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn upsert_fleet_node(&self, node: &FleetNode) -> Result<FleetNode> {
        Uuid::parse_str(&node.node_id).context("fleet node_id is not a UUID")?;
        let mut db = self.open_write()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(collision) = tx
            .query_row(
                "SELECT node_id FROM fleet_nodes WHERE alias=? COLLATE NOCASE",
                [&node.alias],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            && collision != node.node_id
        {
            bail!(
                "machine alias {:?} already belongs to another node",
                node.alias
            );
        }
        let existing: Option<(f64, f64, f64)> = tx
            .query_row(
                "SELECT last_seen,last_attempt_at,created_at FROM fleet_nodes WHERE node_id=?",
                [&node.node_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let timestamp = now();
        let created_at = nonzero_or(
            node.created_at,
            existing.map_or(timestamp, |(_, _, created)| created),
        );
        let updated_at = nonzero_or(node.updated_at, timestamp);
        let last_seen = nonzero_or(node.last_seen, existing.map_or(0.0, |old| old.0));
        let last_attempt_at = nonzero_or(
            node.last_attempt_at,
            existing.map_or(updated_at, |old| old.1),
        );
        let sources = serde_json::to_string(&node.sources)?;
        let capabilities = serde_json::to_string(&node.capabilities)?;
        tx.execute(
            r#"INSERT INTO fleet_nodes(node_id,alias,ssh_target,sources_json,status,protocol_version,package_version,capabilities_json,last_seen,last_attempt_at,last_error,created_at,updated_at)
            VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?) ON CONFLICT(node_id) DO UPDATE SET
            alias=excluded.alias,ssh_target=excluded.ssh_target,sources_json=excluded.sources_json,
            status=excluded.status,protocol_version=excluded.protocol_version,
            package_version=excluded.package_version,capabilities_json=excluded.capabilities_json,
            last_seen=excluded.last_seen,last_attempt_at=excluded.last_attempt_at,
            last_error=excluded.last_error,updated_at=excluded.updated_at"#,
            params![node.node_id, node.alias, node.ssh_target, sources, node.status, node.protocol_version, node.package_version, capabilities, last_seen, last_attempt_at, node.last_error, created_at, updated_at],
        )?;
        tx.commit()?;
        let mut result = node.clone();
        result.created_at = created_at;
        result.updated_at = updated_at;
        result.last_seen = last_seen;
        result.last_attempt_at = last_attempt_at;
        Ok(result)
    }

    pub fn get_fleet_node(&self, value: &str) -> Result<Option<FleetNode>> {
        if !self.exists() {
            return Ok(None);
        }
        let db = self.open_read()?;
        let mut statement =
            db.prepare("SELECT * FROM fleet_nodes WHERE node_id=? OR alias=? COLLATE NOCASE")?;
        let nodes = statement
            .query_map(params![value, value], fleet_node_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        match nodes.len() {
            0 => Ok(None),
            1 => Ok(nodes.into_iter().next()),
            _ => bail!("ambiguous machine identity {value:?}"),
        }
    }

    pub fn delete_fleet_node(&self, node_id: &str) -> Result<bool> {
        let mut db = self.open_write()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute("DELETE FROM remote_snapshots WHERE node_id=?", [node_id])?;
        tx.execute(
            "DELETE FROM meta WHERE key LIKE ? OR key LIKE ? OR key LIKE ? OR key LIKE ? OR key=? OR key=?",
            params![
                format!("fleet:pending-adopt:{node_id}:%"),
                format!("fleet:pending-untrack:{node_id}:%"),
                remote_board_cache_pattern(node_id),
                remote_expert_cache_pattern(node_id),
                fleet_refresh_generation_key(node_id),
                remote_cache_revision_key(node_id),
            ],
        )?;
        let deleted = tx.execute("DELETE FROM fleet_nodes WHERE node_id=?", [node_id])? == 1;
        tx.commit()?;
        if deleted {
            if let Ok(cache) = self.open_fleet_cache() {
                let _ = cache.execute("DELETE FROM projection_chunks WHERE node_id=?", [node_id]);
                let _ = cache.execute(
                    "DELETE FROM expert_search_fts_v2 WHERE node_id=?",
                    [fts_identity("n", node_id)],
                );
                let _ = cache.execute("DELETE FROM expert_rows WHERE node_id=?", [node_id]);
                let _ = cache.execute("DELETE FROM projections WHERE node_id=?", [node_id]);
            }
        }
        Ok(deleted)
    }

    pub fn mark_fleet_node_error(&self, node_id: &str, status: &str, error: &str) -> Result<bool> {
        if ![
            "unreachable",
            "auth",
            "incompatible",
            "quarantined",
            "error",
        ]
        .contains(&status)
        {
            bail!("unsupported fleet node status: {status}");
        }
        let timestamp = now();
        let db = self.open_write()?;
        Ok(db.execute(
            "UPDATE fleet_nodes SET status=?,last_error=?,last_attempt_at=?,updated_at=? WHERE node_id=?",
            params![status, error, timestamp, timestamp, node_id],
        )? == 1)
    }

    /// Reserve a monotonically increasing refresh generation before remote I/O.
    /// The durable claim coordinates boards and exact actions in other Pika
    /// processes without holding SQLite open across SSH.
    pub fn claim_fleet_refresh(&self, node_id: &str) -> Result<u64> {
        let mut db = self.open_write()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let trusted = tx
            .query_row(
                "SELECT 1 FROM fleet_nodes WHERE node_id=?",
                [node_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if !trusted {
            bail!("fleet refresh requires an adopted node");
        }
        let key = fleet_refresh_generation_key(node_id);
        let current = tx
            .query_row("SELECT value FROM meta WHERE key=?", [&key], |row| {
                row.get::<_, String>(0)
            })
            .optional()?
            .map(|value| {
                value
                    .parse::<u64>()
                    .context("stored fleet refresh generation is invalid")
            })
            .transpose()?
            .unwrap_or(0);
        let generation = current
            .checked_add(1)
            .context("fleet refresh generation exhausted")?;
        tx.execute(
            "INSERT INTO meta(key,value) VALUES (?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![key, generation.to_string()],
        )?;
        let timestamp = now();
        tx.execute(
            "UPDATE fleet_nodes SET last_attempt_at=?,updated_at=? WHERE node_id=?",
            params![timestamp, timestamp, node_id],
        )?;
        tx.commit()?;
        Ok(generation)
    }

    /// Reserve the same monotonic generation used by refresh without trusting
    /// a new machine yet. The node and its first snapshot become visible only
    /// together in `commit_fleet_onboarding_if_current`.
    pub fn claim_fleet_onboarding(&self, node_id: &str) -> Result<u64> {
        Uuid::parse_str(node_id).context("fleet node_id is not a UUID")?;
        let mut db = self.open_write()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let key = fleet_refresh_generation_key(node_id);
        let current = tx
            .query_row("SELECT value FROM meta WHERE key=?", [&key], |row| {
                row.get::<_, String>(0)
            })
            .optional()?
            .map(|value| {
                value
                    .parse::<u64>()
                    .context("stored fleet refresh generation is invalid")
            })
            .transpose()?
            .unwrap_or(0);
        let generation = current
            .checked_add(1)
            .context("fleet refresh generation exhausted")?;
        tx.execute(
            "INSERT INTO meta(key,value) VALUES (?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![key, generation.to_string()],
        )?;
        let timestamp = now();
        tx.execute(
            "UPDATE fleet_nodes SET last_attempt_at=?,updated_at=? WHERE node_id=?",
            params![timestamp, timestamp, node_id],
        )?;
        tx.commit()?;
        Ok(generation)
    }

    pub fn current_fleet_refresh_generation(&self, node_id: &str) -> Result<u64> {
        if !self.exists() {
            return Ok(0);
        }
        self.get_meta(&fleet_refresh_generation_key(node_id))?
            .map(|value| {
                value
                    .parse::<u64>()
                    .context("stored fleet refresh generation is invalid")
            })
            .transpose()
            .map(|value| value.unwrap_or(0))
    }

    /// Record an error only if this request remains the newest refresh claim.
    pub fn mark_fleet_node_error_if_current(
        &self,
        node_id: &str,
        generation: u64,
        status: &str,
        error: &str,
    ) -> Result<bool> {
        if ![
            "unreachable",
            "auth",
            "incompatible",
            "quarantined",
            "error",
        ]
        .contains(&status)
        {
            bail!("unsupported fleet node status: {status}");
        }
        let mut db = self.open_write()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if !fleet_refresh_is_current(&tx, node_id, generation)? {
            tx.commit()?;
            return Ok(false);
        }
        let timestamp = now();
        let changed = tx.execute(
            "UPDATE fleet_nodes SET status=?,last_error=?,last_attempt_at=?,updated_at=? WHERE node_id=?",
            params![status, error, timestamp, timestamp, node_id],
        )? == 1;
        tx.commit()?;
        Ok(changed)
    }

    pub fn put_remote_snapshot(
        &self,
        node_id: &str,
        payload: &Value,
        captured_at: f64,
    ) -> Result<()> {
        let prepared = prepare_remote_snapshot(node_id, payload, captured_at)?;
        self.stage_remote_cache(&prepared)?;
        let mut db = self.open_write()?;
        let result = (|| -> Result<Option<String>> {
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let trusted = tx
                .query_row(
                    "SELECT 1 FROM fleet_nodes WHERE node_id=?",
                    [node_id],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            if !trusted {
                bail!("remote snapshot requires an adopted fleet node");
            }
            let previous_revision = write_remote_snapshot_tx(&tx, node_id, &prepared, captured_at)?;
            tx.execute(
            "UPDATE fleet_nodes SET status='ready',last_seen=?,last_error=NULL,last_attempt_at=?,updated_at=? WHERE node_id=?",
            params![captured_at, captured_at, now(), node_id],
        )?;
            tx.commit()?;
            Ok(previous_revision)
        })();
        match &result {
            Ok(Some(previous)) if previous != &prepared.revision => {
                self.discard_remote_cache_revision(node_id, previous);
            }
            Ok(_) => {}
            Err(_) => self.discard_remote_cache_revision(node_id, &prepared.revision),
        }
        result.map(drop)
    }

    /// Atomically make an onboarded node and its validated first snapshot
    /// visible, but only if no newer add/refresh/delete operation superseded
    /// the pre-I/O generation claim.
    pub fn commit_fleet_onboarding_if_current(
        &self,
        node: &FleetNode,
        payload: &Value,
        captured_at: f64,
        generation: u64,
    ) -> Result<bool> {
        Uuid::parse_str(&node.node_id).context("fleet node_id is not a UUID")?;
        let prepared = prepare_remote_snapshot(&node.node_id, payload, captured_at)?;
        self.stage_remote_cache(&prepared)?;
        let mut db = self.open_write()?;
        let result = (|| -> Result<(bool, Option<String>)> {
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            if !fleet_refresh_is_current(&tx, &node.node_id, generation)? {
                tx.commit()?;
                return Ok((false, None));
            }
            if let Some(collision) = tx
                .query_row(
                    "SELECT node_id FROM fleet_nodes WHERE alias=? COLLATE NOCASE",
                    [&node.alias],
                    |row| row.get::<_, String>(0),
                )
                .optional()?
                && collision != node.node_id
            {
                bail!(
                    "machine alias {:?} already belongs to another node",
                    node.alias
                );
            }
            let existing: Option<(f64, f64, f64)> = tx
                .query_row(
                    "SELECT last_seen,last_attempt_at,created_at FROM fleet_nodes WHERE node_id=?",
                    [&node.node_id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()?;
            let timestamp = now();
            let created_at = nonzero_or(
                node.created_at,
                existing.map_or(timestamp, |(_, _, created)| created),
            );
            let updated_at = nonzero_or(node.updated_at, timestamp);
            let last_seen = nonzero_or(node.last_seen, existing.map_or(0.0, |old| old.0));
            let last_attempt_at = nonzero_or(
                node.last_attempt_at,
                existing.map_or(updated_at, |old| old.1),
            );
            let sources = serde_json::to_string(&node.sources)?;
            let capabilities = serde_json::to_string(&node.capabilities)?;
            tx.execute(
            r#"INSERT INTO fleet_nodes(node_id,alias,ssh_target,sources_json,status,protocol_version,package_version,capabilities_json,last_seen,last_attempt_at,last_error,created_at,updated_at)
            VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?) ON CONFLICT(node_id) DO UPDATE SET
            alias=excluded.alias,ssh_target=excluded.ssh_target,sources_json=excluded.sources_json,
            status=excluded.status,protocol_version=excluded.protocol_version,
            package_version=excluded.package_version,capabilities_json=excluded.capabilities_json,
            last_seen=excluded.last_seen,last_attempt_at=excluded.last_attempt_at,
            last_error=excluded.last_error,updated_at=excluded.updated_at"#,
            params![node.node_id, node.alias, node.ssh_target, sources, node.status, node.protocol_version, node.package_version, capabilities, last_seen, last_attempt_at, node.last_error, created_at, updated_at],
        )?;
            let previous_revision =
                write_remote_snapshot_tx(&tx, &node.node_id, &prepared, captured_at)?;
            tx.execute(
            "UPDATE fleet_nodes SET status='ready',last_seen=?,last_error=NULL,last_attempt_at=?,updated_at=? WHERE node_id=?",
            params![captured_at, captured_at, timestamp, node.node_id],
        )?;
            tx.commit()?;
            Ok((true, previous_revision))
        })();
        match &result {
            Ok((true, Some(previous))) if previous != &prepared.revision => {
                self.discard_remote_cache_revision(&node.node_id, previous);
            }
            Ok((true, _)) => {}
            _ => self.discard_remote_cache_revision(&node.node_id, &prepared.revision),
        }
        result.map(|(committed, _)| committed)
    }

    /// Commit a remote snapshot only while its pre-I/O refresh claim is still
    /// current. A later-started request therefore wins even if an older SSH
    /// response arrives last.
    pub fn put_remote_snapshot_if_current(
        &self,
        node_id: &str,
        payload: &Value,
        captured_at: f64,
        generation: u64,
    ) -> Result<bool> {
        let prepared = prepare_remote_snapshot(node_id, payload, captured_at)?;
        self.stage_remote_cache(&prepared)?;
        let mut db = self.open_write()?;
        let result = (|| -> Result<(bool, Option<String>)> {
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
            if !fleet_refresh_is_current(&tx, node_id, generation)? {
                tx.commit()?;
                return Ok((false, None));
            }
            let trusted = tx
                .query_row(
                    "SELECT 1 FROM fleet_nodes WHERE node_id=?",
                    [node_id],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            if !trusted {
                bail!("remote snapshot requires an adopted fleet node");
            }
            let previous_revision = write_remote_snapshot_tx(&tx, node_id, &prepared, captured_at)?;
            tx.execute(
            "UPDATE fleet_nodes SET status='ready',last_seen=?,last_error=NULL,last_attempt_at=?,updated_at=? WHERE node_id=?",
            params![captured_at, captured_at, now(), node_id],
        )?;
            tx.commit()?;
            Ok((true, previous_revision))
        })();
        match &result {
            Ok((true, Some(previous))) if previous != &prepared.revision => {
                self.discard_remote_cache_revision(node_id, previous);
            }
            Ok((true, _)) => {}
            _ => self.discard_remote_cache_revision(node_id, &prepared.revision),
        }
        result.map(|(committed, _)| committed)
    }

    /// Check only whether an authoritative snapshot row exists. Board health
    /// uses this constant-size query so a 64-node fleet cannot re-materialize
    /// every full JSON payload after reading the bounded projections.
    pub fn has_remote_snapshot(&self, node_id: &str) -> Result<bool> {
        if !self.exists() {
            return Ok(false);
        }
        let db = self.open_read()?;
        Ok(db
            .query_row(
                "SELECT 1 FROM remote_snapshots WHERE node_id=?",
                [node_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    pub fn get_remote_snapshot(&self, node_id: &str) -> Result<Option<RemoteSnapshot>> {
        if !self.exists() {
            return Ok(None);
        }
        let db = self.open_read()?;
        let snapshot = db
            .query_row(
                "SELECT node_id,payload_json,captured_at,length(CAST(payload_json AS BLOB)) FROM remote_snapshots WHERE node_id=?",
                [node_id],
                |row| {
                    let encoded_bytes: i64 = row.get(3)?;
                    if encoded_bytes < 0
                        || usize::try_from(encoded_bytes)
                            .ok()
                            .is_none_or(|size| size > MAX_REMOTE_SNAPSHOT_BYTES)
                    {
                        return Err(conversion_error(
                            1,
                            "stored remote snapshot exceeds the 4 MiB safety limit",
                        ));
                    }
                    let encoded: String = row.get(1)?;
                    let payload = parse_json_value(&encoded, 1)?;
                    if !payload.is_object() {
                        return Err(conversion_error(
                            1,
                            "stored remote snapshot is not an object",
                        ));
                    }
                    Ok(RemoteSnapshot {
                        node_id: row.get(0)?,
                        payload,
                        captured_at: row.get(2)?,
                        encoded_bytes: encoded_bytes as usize,
                    })
                },
            )
            .optional()?;
        Ok(snapshot)
    }

    /// Read a complete snapshot only when SQLite can prove its encoded payload
    /// fits the caller's budget. The CASE expression prevents an oversized TEXT
    /// value from crossing the SQLite/Rust boundary merely to be rejected.
    pub fn get_remote_snapshot_bounded(
        &self,
        node_id: &str,
        max_bytes: usize,
    ) -> Result<Option<BoundedRemoteSnapshot>> {
        if !self.exists() {
            return Ok(None);
        }
        let db = self.open_read()?;
        let gate = max_bytes.min(MAX_REMOTE_SNAPSHOT_BYTES);
        let row = db
            .query_row(
                "SELECT CASE WHEN length(CAST(payload_json AS BLOB))<=? THEN payload_json ELSE NULL END,captured_at,length(CAST(payload_json AS BLOB)) FROM remote_snapshots WHERE node_id=?",
                params![i64::try_from(gate)?, node_id],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, f64>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )
            .optional()?;
        let Some((encoded, captured_at, encoded_bytes)) = row else {
            return Ok(None);
        };
        let encoded_bytes =
            usize::try_from(encoded_bytes).context("stored remote snapshot has a negative size")?;
        let snapshot = encoded
            .map(|encoded| -> Result<RemoteSnapshot> {
                let payload: Value = serde_json::from_str(&encoded)?;
                if !payload.is_object() {
                    bail!("stored remote snapshot is not an object");
                }
                Ok(RemoteSnapshot {
                    node_id: node_id.to_owned(),
                    payload,
                    captured_at,
                    encoded_bytes,
                })
            })
            .transpose()?;
        Ok(Some(BoundedRemoteSnapshot {
            snapshot,
            captured_at,
            encoded_bytes,
        }))
    }

    /// Load only complete, prevalidated board-cache chunks which fit the
    /// caller's per-node row and byte slice. Full remote snapshots remain in
    /// `remote_snapshots` for exact actions and expert discovery.
    pub fn get_remote_board_projection(
        &self,
        node_id: &str,
        max_bytes: usize,
        max_rows: usize,
    ) -> Result<Option<RemoteBoardProjection>> {
        if !self.exists() {
            return Ok(None);
        }
        let mut db = self.open_read()?;
        let tx = db.transaction()?;
        let source = tx
            .query_row(
                "SELECT captured_at,length(CAST(payload_json AS BLOB)) FROM remote_snapshots WHERE node_id=?",
                [node_id],
                |row| Ok((row.get::<_, f64>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()?;
        let Some((source_captured_at, source_encoded_bytes)) = source else {
            tx.commit()?;
            return Ok(None);
        };
        let source_encoded_bytes = usize::try_from(source_encoded_bytes)
            .context("stored remote snapshot has a negative size")?;
        let revision = tx
            .query_row(
                "SELECT value FROM meta WHERE key=?",
                [remote_cache_revision_key(node_id)],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if let Some(revision) = revision {
            tx.commit()?;
            let cache = self.open_fleet_cache()?;
            let encoded_header = cache
                .query_row(
                    "SELECT CASE WHEN length(CAST(header AS BLOB))<=? THEN header ELSE NULL END FROM projections WHERE node_id=? AND revision=? AND kind='board'",
                    params![MAX_REMOTE_CACHE_HEADER_BYTES as i64, node_id, revision],
                    |row| row.get::<_, Option<String>>(0),
                )
                .optional()?
                .flatten();
            let Some(encoded_header) = encoded_header else {
                return Ok(None);
            };
            let header_bytes = encoded_header.len();
            if header_bytes > max_bytes {
                bail!("stored remote board cache header exceeds its per-node input budget");
            }
            let header: RemoteBoardCacheHeader = serde_json::from_str(&encoded_header)
                .context("stored remote board cache header is malformed")?;
            if header.schema != REMOTE_BOARD_CACHE_SCHEMA
                || header.node_id != node_id
                || header.source_captured_at != source_captured_at
                || header.source_encoded_bytes != source_encoded_bytes
                || header.chunks.len() > MAX_REMOTE_CACHE_CHUNKS
                || header.source_sessions > 2_000
            {
                bail!("stored remote board cache does not match its source snapshot");
            }
            let mut input_bytes = header_bytes;
            let mut rows = Vec::new();
            for (index, chunk) in header.chunks.iter().enumerate() {
                if chunk.bytes == 0
                    || chunk.bytes > MAX_REMOTE_BOARD_CACHE_BYTES
                    || chunk.rows == 0
                    || chunk.rows > 2_000
                {
                    bail!("stored remote board cache chunk metadata is invalid");
                }
                if rows.len() >= max_rows || chunk.bytes > max_bytes.saturating_sub(input_bytes) {
                    break;
                }
                let encoded = cache
                    .query_row(
                        "SELECT CASE WHEN length(CAST(value AS BLOB))=? THEN value ELSE NULL END FROM projection_chunks WHERE node_id=? AND revision=? AND kind='board' AND chunk_index=?",
                        params![i64::try_from(chunk.bytes)?, node_id, revision, index],
                        |row| row.get::<_, Option<String>>(0),
                    )
                    .optional()?
                    .flatten()
                    .context("stored remote board cache chunk is missing or changed")?;
                let mut values: Vec<Value> = serde_json::from_str(&encoded)
                    .context("stored remote board cache chunk is malformed")?;
                if values.len() != chunk.rows {
                    bail!("stored remote board cache chunk row count changed");
                }
                input_bytes += chunk.bytes;
                values.truncate(max_rows.saturating_sub(rows.len()));
                rows.extend(values);
            }
            return Ok(Some(RemoteBoardProjection {
                rows,
                protocol: header.protocol,
                version: header.version,
                source_sessions: header.source_sessions,
                source_captured_at,
                remote_captured_at: header.remote_captured_at,
                source_encoded_bytes,
                input_bytes,
                directory_notices: header.directory_notices,
            }));
        }
        let header_key = remote_board_cache_header_key(node_id);
        let header = tx
            .query_row(
                "SELECT CASE WHEN length(CAST(value AS BLOB))<=? THEN value ELSE NULL END,length(CAST(value AS BLOB)) FROM meta WHERE key=?",
                params![MAX_REMOTE_CACHE_HEADER_BYTES as i64, header_key],
                |row| Ok((row.get::<_, Option<String>>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()?;
        let Some((Some(encoded_header), header_bytes)) = header else {
            tx.commit()?;
            return Ok(None);
        };
        let header_bytes = usize::try_from(header_bytes)
            .context("stored remote board cache header has a negative size")?;
        if header_bytes > max_bytes {
            bail!("stored remote board cache header exceeds its per-node input budget");
        }
        let header: RemoteBoardCacheHeader = serde_json::from_str(&encoded_header)
            .context("stored remote board cache header is malformed")?;
        if header.schema != REMOTE_BOARD_CACHE_SCHEMA
            || header.node_id != node_id
            || header.source_captured_at != source_captured_at
            || header.source_encoded_bytes != source_encoded_bytes
            || header.chunks.len() > MAX_REMOTE_CACHE_CHUNKS
            || header.source_sessions > 2_000
        {
            bail!("stored remote board cache does not match its source snapshot");
        }
        let mut input_bytes = header_bytes;
        let mut rows = Vec::new();
        for (index, chunk) in header.chunks.iter().enumerate() {
            if chunk.bytes == 0
                || chunk.bytes > MAX_REMOTE_BOARD_CACHE_BYTES
                || chunk.rows == 0
                || chunk.rows > 2_000
            {
                bail!("stored remote board cache chunk metadata is invalid");
            }
            if rows.len() >= max_rows || chunk.bytes > max_bytes.saturating_sub(input_bytes) {
                break;
            }
            let key = remote_board_cache_chunk_key(node_id, index);
            let encoded = tx
                .query_row(
                    "SELECT CASE WHEN length(CAST(value AS BLOB))=? THEN value ELSE NULL END FROM meta WHERE key=?",
                    params![i64::try_from(chunk.bytes)?, key],
                    |row| row.get::<_, Option<String>>(0),
                )
                .optional()?
                .flatten()
                .context("stored remote board cache chunk is missing or changed")?;
            let mut values: Vec<Value> = serde_json::from_str(&encoded)
                .context("stored remote board cache chunk is malformed")?;
            if values.len() != chunk.rows {
                bail!("stored remote board cache chunk row count changed");
            }
            input_bytes += chunk.bytes;
            let remaining_rows = max_rows.saturating_sub(rows.len());
            values.truncate(remaining_rows);
            rows.extend(values);
        }
        tx.commit()?;
        Ok(Some(RemoteBoardProjection {
            rows,
            protocol: header.protocol,
            version: header.version,
            source_sessions: header.source_sessions,
            source_captured_at,
            remote_captured_at: header.remote_captured_at,
            source_encoded_bytes,
            input_bytes,
            directory_notices: header.directory_notices,
        }))
    }

    /// Rank the complete persisted expert set for one machine, then materialize
    /// only the fair result slice. Full remote snapshots and nonmatching rich
    /// rows never enter the caller's aggregate input budget.
    pub fn get_remote_expert_projection(
        &self,
        node_id: &str,
        query: &str,
        max_bytes: usize,
        max_rows: usize,
    ) -> Result<Option<RemoteExpertProjection>> {
        if !self.exists() {
            return Ok(None);
        }
        let mut db = self.open_read()?;
        let tx = db.transaction()?;
        let source = tx
            .query_row(
                "SELECT captured_at,length(CAST(payload_json AS BLOB)) FROM remote_snapshots WHERE node_id=?",
                [node_id],
                |row| Ok((row.get::<_, f64>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()?;
        let Some((source_captured_at, source_encoded_bytes)) = source else {
            tx.commit()?;
            return Ok(None);
        };
        let source_encoded_bytes = usize::try_from(source_encoded_bytes)
            .context("stored remote snapshot has a negative size")?;
        let revision = tx
            .query_row(
                "SELECT value FROM meta WHERE key=?",
                [remote_cache_revision_key(node_id)],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        tx.commit()?;
        let Some(revision) = revision else {
            // Frozen Python and pre-index Rust snapshots have no revision-bound
            // searchable projection. The caller may size-gate and search the
            // complete legacy snapshot, but must never treat a prefix cache as
            // a complete expert directory.
            return Ok(None);
        };
        let cache = self.open_fleet_cache()?;
        let encoded_header = cache
            .query_row(
                "SELECT CASE WHEN length(CAST(header AS BLOB))<=? THEN header ELSE NULL END FROM projections WHERE node_id=? AND revision=? AND kind='expert'",
                params![MAX_REMOTE_CACHE_HEADER_BYTES as i64, node_id, revision],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        let Some(encoded_header) = encoded_header else {
            return Ok(None);
        };
        let header_bytes = encoded_header.len();
        if header_bytes > max_bytes {
            bail!("stored remote expert cache header exceeds its per-node input budget");
        }
        let header: RemoteExpertCacheHeader = serde_json::from_str(&encoded_header)
            .context("stored remote expert cache header is malformed")?;
        if header.schema != REMOTE_EXPERT_CACHE_SCHEMA
            || header.node_id != node_id
            || header.source_captured_at != source_captured_at
            || header.source_encoded_bytes != source_encoded_bytes
            || header.source_experts > 2_000
            || header.indexed_bytes > MAX_REMOTE_EXPERT_INDEX_BYTES
        {
            bail!("stored remote expert cache does not match its source snapshot");
        }
        let indexed_node = fts_identity("n", node_id);
        let indexed_revision = fts_identity("r", &revision);
        let identity_query =
            format!("node_id : \"{indexed_node}\" AND revision : \"{indexed_revision}\"");

        let (row_count, search_count): (i64, i64) = cache.query_row(
            "SELECT
               (SELECT COUNT(*) FROM expert_rows WHERE node_id=?1 AND revision=?2),
               (SELECT COUNT(*) FROM expert_search_fts_v2 WHERE expert_search_fts_v2 MATCH ?3)",
            params![node_id, revision, identity_query],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if usize::try_from(row_count).ok() != Some(header.source_experts)
            || row_count != search_count
        {
            bail!("stored remote expert search index is incomplete or changed");
        }

        let parsed = ExpertQuery::parse(query);
        if parsed.raw_nonempty && parsed.terms.is_empty() {
            return Ok(Some(RemoteExpertProjection {
                rows: Vec::new(),
                protocol: header.protocol,
                version: header.version,
                source_experts: header.source_experts,
                matching_experts: 0,
                source_captured_at,
                remote_captured_at: header.remote_captured_at,
                source_encoded_bytes,
                input_bytes: header_bytes,
                directory_notices: header.directory_notices,
            }));
        }
        let terms_json = serde_json::to_string(&parsed.terms)?;
        let phrase = parsed.phrase();
        let expert_terms = parsed
            .terms
            .iter()
            .map(|term| format!("\"{}\"", term.replace('"', "\"\"")))
            .collect::<Vec<_>>()
            .join(" AND ");
        let fts_query = if expert_terms.is_empty() {
            format!("node_id : \"{indexed_node}\" AND revision : \"{indexed_revision}\"")
        } else {
            format!(
                "node_id : \"{indexed_node}\" AND revision : \"{indexed_revision}\" AND \
                 {{topic_tokens scope_tokens current_tokens name_tokens project_tokens artifact_tokens}} : ({expert_terms})"
            )
        };
        let available_bytes = max_bytes.saturating_sub(header_bytes);
        let score = "COALESCE((
            SELECT SUM(
              CASE WHEN instr(s.topic_tokens, ' ' || CAST(term.value AS TEXT) || ' ')>0 THEN 10 ELSE 0 END +
              CASE WHEN instr(s.scope_tokens, ' ' || CAST(term.value AS TEXT) || ' ')>0 THEN 6 ELSE 0 END +
              CASE WHEN instr(s.current_tokens, ' ' || CAST(term.value AS TEXT) || ' ')>0 THEN 5 ELSE 0 END +
              CASE WHEN instr(s.name_tokens, ' ' || CAST(term.value AS TEXT) || ' ')>0 THEN 4 ELSE 0 END +
              CASE WHEN instr(s.project_tokens, ' ' || CAST(term.value AS TEXT) || ' ')>0 THEN 3 ELSE 0 END +
              CASE WHEN instr(s.artifact_tokens, ' ' || CAST(term.value AS TEXT) || ' ')>0 THEN 2 ELSE 0 END
            ) FROM json_each(?1) term
          ),0) +
          CASE WHEN ?5=1 AND instr(s.topic_tokens, ?6)>0 THEN 20 ELSE 0 END +
          CASE WHEN ?5=1 AND instr(s.scope_tokens, ?6)>0 THEN 12 ELSE 0 END +
          CASE WHEN ?5=1 AND instr(s.current_tokens, ?6)>0 THEN 10 ELSE 0 END +
          CASE WHEN ?5=1 AND instr(s.name_tokens, ?6)>0 THEN 8 ELSE 0 END +
          CASE WHEN ?5=1 AND instr(s.project_tokens, ?6)>0 THEN 6 ELSE 0 END +
          CASE WHEN ?5=1 AND instr(s.artifact_tokens, ?6)>0 THEN 4 ELSE 0 END";
        let rows_sql = format!(
            "WITH scored AS (
               SELECT r.ordinal,r.row_bytes,r.live,r.profile_updated_at,r.session_id,{score} AS score
               FROM expert_rows r JOIN expert_search_fts_v2 s ON s.ordinal=r.ordinal
               WHERE r.node_id=?3 AND r.revision=?4
                 AND expert_search_fts_v2 MATCH ?2
             ), eligible AS (
               SELECT * FROM scored WHERE row_bytes<=?7
             ), ranked AS (
               SELECT *,
                 ROW_NUMBER() OVER (
                   ORDER BY score DESC,live DESC,profile_updated_at DESC,session_id DESC
                 ) AS result_rank,
                 SUM(row_bytes) OVER (
                   ORDER BY score DESC,live DESC,profile_updated_at DESC,session_id DESC
                   ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW
                 ) AS cumulative_bytes
               FROM eligible
             ), totals AS (
               SELECT COUNT(*) AS matching_experts FROM scored
             )
             SELECT CASE
                      WHEN ranked.ordinal IS NULL THEN NULL
                      WHEN length(CAST(r.row_json AS BLOB))=ranked.row_bytes THEN r.row_json
                      ELSE NULL
                    END,
                    ranked.row_bytes,
                    totals.matching_experts
             FROM totals
             LEFT JOIN ranked
               ON ranked.result_rank<=?8 AND ranked.cumulative_bytes<=?7
             LEFT JOIN expert_rows r
               ON r.node_id=?3 AND r.revision=?4 AND r.ordinal=ranked.ordinal
             ORDER BY ranked.result_rank"
        );
        let mut statement = cache.prepare(&rows_sql)?;
        let selected = statement
            .query_map(
                params![
                    terms_json,
                    fts_query,
                    node_id,
                    revision,
                    !parsed.tokens.is_empty(),
                    phrase,
                    i64::try_from(available_bytes)?,
                    i64::try_from(max_rows)?,
                ],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<i64>>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut input_bytes = header_bytes;
        let mut rows = Vec::with_capacity(selected.len());
        let matching_experts = selected
            .first()
            .and_then(|(_, _, count)| usize::try_from(*count).ok())
            .context("stored remote expert match count is invalid")?;
        for (encoded, row_bytes, _) in selected {
            let Some(row_bytes) = row_bytes else {
                continue;
            };
            let encoded = encoded.context("stored remote expert row is missing or changed")?;
            let row_bytes = usize::try_from(row_bytes)
                .context("stored remote expert row has an invalid size")?;
            input_bytes = input_bytes
                .checked_add(row_bytes)
                .context("remote expert projection input size overflowed")?;
            if input_bytes > max_bytes {
                bail!("stored remote expert projection exceeded its input budget");
            }
            rows.push(
                serde_json::from_str(&encoded).context("stored remote expert row is malformed")?,
            );
        }
        Ok(Some(RemoteExpertProjection {
            rows,
            protocol: header.protocol,
            version: header.version,
            source_experts: header.source_experts,
            matching_experts,
            source_captured_at,
            remote_captured_at: header.remote_captured_at,
            source_encoded_bytes,
            input_bytes,
            directory_notices: header.directory_notices,
        }))
    }

    pub fn ignore_node_candidate(&self, candidate_key: &str, ignored_at: f64) -> Result<()> {
        let db = self.open_write()?;
        db.execute(
            "INSERT INTO ignored_node_candidates(candidate_key,ignored_at) VALUES (?,?) ON CONFLICT(candidate_key) DO UPDATE SET ignored_at=excluded.ignored_at",
            params![candidate_key.to_lowercase(), ignored_at],
        )?;
        Ok(())
    }

    pub fn ignored_node_candidate_keys(&self) -> Result<BTreeSet<String>> {
        if !self.exists() {
            return Ok(BTreeSet::new());
        }
        let db = self.open_read()?;
        let mut statement = db.prepare("SELECT candidate_key FROM ignored_node_candidates")?;
        Ok(statement
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<BTreeSet<_>>>()?)
    }

    pub fn ensure_local_node_id(&self) -> Result<String> {
        let mut db = self.open_write()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let proposed = Uuid::new_v4().to_string();
        tx.execute(
            "INSERT OR IGNORE INTO meta(key,value) VALUES ('fleet:node_id',?)",
            [&proposed],
        )?;
        let stored: String = tx.query_row(
            "SELECT value FROM meta WHERE key='fleet:node_id'",
            [],
            |row| row.get(0),
        )?;
        let canonical = Uuid::parse_str(&stored)
            .context("Pika's stored fleet node UUID is invalid")?
            .to_string();
        tx.commit()?;
        Ok(canonical)
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.reconcile_transaction(|ledger| ledger.set_meta(key, value))
    }

    pub fn get_meta(&self, key: &str) -> Result<Option<String>> {
        if !self.exists() {
            return Ok(None);
        }
        let db = self.open_read()?;
        db.query_row("SELECT value FROM meta WHERE key=?", [key], |row| {
            row.get(0)
        })
        .optional()
        .map_err(Into::into)
    }

    pub fn delete_meta(&self, key: &str) -> Result<bool> {
        self.reconcile_transaction(|ledger| ledger.delete_meta(key))
    }

    pub fn record_attach(&self, provider: Provider, session_id: &str) -> Result<()> {
        self.reconcile_transaction(|ledger| ledger.record_attach(provider, session_id))
    }

    pub fn previous_attached(&self) -> Result<Option<(Provider, String)>> {
        let Some(encoded) = self.get_meta("previous_attached")? else {
            return Ok(None);
        };
        let (provider, session_id): (String, String) =
            serde_json::from_str(&encoded).context("stored previous_attached value is invalid")?;
        Ok(Some((
            provider
                .parse()
                .map_err(|message: String| anyhow!(message))?,
            session_id,
        )))
    }

    pub fn record_hook_observation(&self, observation: &HookObservation) -> Result<()> {
        self.reconcile_transaction(|ledger| ledger.record_hook_observation(observation))
    }

    pub fn get_hook_observation(&self, provider: Provider) -> Result<Option<HookObservation>> {
        if !self.exists() {
            return Ok(None);
        }
        let db = self.open_read()?;
        db.query_row(
            "SELECT provider,fingerprint,event_name,session_id,observed_at,source,managed FROM hook_observations WHERE provider=?",
            [provider.as_str()],
            |row| {
                Ok(HookObservation {
                    provider: parse_provider(row.get_ref(0)?.as_str()?, 0)?,
                    fingerprint: row.get(1)?,
                    event_name: row.get(2)?,
                    session_id: row.get(3)?,
                    observed_at: row.get(4)?,
                    source: row.get(5)?,
                    managed: row.get::<_, i64>(6)? != 0,
                })
            },
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn put_cached_usage(&self, usage: &UsageCacheRecord) -> Result<()> {
        let db = self.open_write()?;
        db.execute(
            r#"INSERT INTO usage_cache(provider,session_id,source_path,source_mtime_ns,source_size,model,input_tokens,output_tokens,cached_input_tokens,cache_write_tokens,total_tokens,estimated_cost_usd,updated_at)
            VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?) ON CONFLICT(provider,session_id) DO UPDATE SET
            source_path=excluded.source_path,source_mtime_ns=excluded.source_mtime_ns,
            source_size=excluded.source_size,model=excluded.model,input_tokens=excluded.input_tokens,
            output_tokens=excluded.output_tokens,cached_input_tokens=excluded.cached_input_tokens,
            cache_write_tokens=excluded.cache_write_tokens,total_tokens=excluded.total_tokens,
            estimated_cost_usd=excluded.estimated_cost_usd,updated_at=excluded.updated_at"#,
            params![usage.provider.as_str(), usage.session_id, usage.source_path, usage.source_mtime_ns, usage.source_size, usage.model, usage.input_tokens, usage.output_tokens, usage.cached_input_tokens, usage.cache_write_tokens, usage.total_tokens, usage.estimated_cost_usd, usage.updated_at],
        )?;
        Ok(())
    }

    pub fn list_cached_usage(&self) -> Result<Vec<UsageCacheRecord>> {
        if !self.exists() {
            return Ok(Vec::new());
        }
        let db = self.open_read()?;
        let mut statement = db.prepare(
            "SELECT provider,session_id,source_path,source_mtime_ns,source_size,model,input_tokens,output_tokens,cached_input_tokens,cache_write_tokens,total_tokens,estimated_cost_usd,updated_at FROM usage_cache ORDER BY updated_at",
        )?;
        Ok(statement
            .query_map([], usage_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn get_cached_usage(
        &self,
        provider: Provider,
        session_id: &str,
        source_path: &str,
        source_mtime_ns: i64,
        source_size: i64,
    ) -> Result<Option<UsageCacheRecord>> {
        if !self.exists() {
            return Ok(None);
        }
        let db = self.open_read()?;
        db.query_row(
            "SELECT provider,session_id,source_path,source_mtime_ns,source_size,model,input_tokens,output_tokens,cached_input_tokens,cache_write_tokens,total_tokens,estimated_cost_usd,updated_at FROM usage_cache WHERE provider=? AND session_id=? AND source_path=? AND source_mtime_ns=? AND source_size=?",
            params![provider.as_str(), session_id, source_path, source_mtime_ns, source_size],
            usage_from_row,
        )
        .optional()
        .map_err(Into::into)
    }

    /// Return the newest cached accounting checkpoint for a transcript even
    /// when the file has subsequently grown. Callers must independently prove
    /// that the source is append-only before treating `source_size` as a safe
    /// parsing offset.
    pub fn get_latest_cached_usage(
        &self,
        provider: Provider,
        session_id: &str,
        source_path: &str,
    ) -> Result<Option<UsageCacheRecord>> {
        if !self.exists() {
            return Ok(None);
        }
        let db = self.open_read()?;
        db.query_row(
            "SELECT provider,session_id,source_path,source_mtime_ns,source_size,model,input_tokens,output_tokens,cached_input_tokens,cache_write_tokens,total_tokens,estimated_cost_usd,updated_at FROM usage_cache WHERE provider=? AND session_id=? AND source_path=?",
            params![provider.as_str(), session_id, source_path],
            usage_from_row,
        )
        .optional()
        .map_err(Into::into)
    }
}

#[derive(Clone)]
struct AttentionRow {
    name: Option<String>,
    status: Status,
    unread: bool,
    attention_reason: Option<String>,
    error: Option<String>,
}

fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

fn launch_phase_key(launch_token: &str) -> String {
    format!("launch_phase:{launch_token}")
}

fn provider_hidden_key(provider: Provider, session_id: &str) -> String {
    format!("provider-hidden:{}:{session_id}", provider.as_str())
}

fn nonzero_or(value: f64, fallback: f64) -> f64 {
    if value == 0.0 { fallback } else { value }
}

impl ReconcileLedger<'_> {
    pub(crate) fn hide_provider_session(
        &self,
        provider: Provider,
        session_id: &str,
        state: &str,
    ) -> Result<()> {
        self.tx.execute(
            "INSERT INTO untracked_sessions(provider,session_id,untracked_at) VALUES (?,?,?) ON CONFLICT(provider,session_id) DO UPDATE SET untracked_at=excluded.untracked_at",
            params![provider.as_str(), session_id, now()],
        )?;
        self.tx.execute(
            "INSERT INTO meta(key,value) VALUES (?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![provider_hidden_key(provider, session_id), state],
        )?;
        for table in [
            "usage_cache",
            "session_events",
            "session_status_observations",
            "identity_interruptions",
            "expert_refresh_attempts",
            "live_owners",
            "recovery_owners",
            "launch_reservations",
        ] {
            self.tx.execute(
                &format!("DELETE FROM {table} WHERE provider=? AND session_id=?"),
                params![provider.as_str(), session_id],
            )?;
        }
        self.tx.execute(
            "DELETE FROM launch_bindings WHERE provider=? AND session_id=?",
            params![provider.as_str(), session_id],
        )?;
        self.tx.execute(
            "UPDATE sessions SET tmux_session=NULL,tmux_pane=NULL,root_pid=NULL,status='PARKED',unread=0,error=NULL,attention_reason=NULL,updated_at=? WHERE provider=? AND session_id=?",
            params![now(), provider.as_str(), session_id],
        )?;
        Ok(())
    }

    pub(crate) fn restore_provider_session(
        &self,
        provider: Provider,
        session_id: &str,
    ) -> Result<bool> {
        let key = provider_hidden_key(provider, session_id);
        if self.tx.execute("DELETE FROM meta WHERE key=?", [&key])? != 1 {
            return Ok(false);
        }
        self.tx.execute(
            "DELETE FROM untracked_sessions WHERE provider=? AND session_id=?",
            params![provider.as_str(), session_id],
        )?;
        Ok(true)
    }

    pub(crate) fn delete_session(
        &self,
        provider: Provider,
        session_id: &str,
        preserve_live_owners: bool,
    ) -> Result<bool> {
        let tx = self.tx;
        for table in [
            "usage_cache",
            "session_events",
            "session_status_observations",
            "identity_interruptions",
            "expert_profiles",
            "expert_refresh_attempts",
            "recovery_owners",
            "launch_reservations",
        ] {
            tx.execute(
                &format!("DELETE FROM {table} WHERE provider=? AND session_id=?"),
                params![provider.as_str(), session_id],
            )?;
        }
        if !preserve_live_owners {
            tx.execute(
                "DELETE FROM live_owners WHERE provider=? AND session_id=?",
                params![provider.as_str(), session_id],
            )?;
        }
        tx.execute(
            "DELETE FROM launch_bindings WHERE provider=? AND session_id=?",
            params![provider.as_str(), session_id],
        )?;
        let deleted = tx.execute(
            "DELETE FROM sessions WHERE provider=? AND session_id=?",
            params![provider.as_str(), session_id],
        )? == 1;
        Ok(deleted)
    }

    pub(crate) fn capture_identity_interruption(
        &self,
        provider: Provider,
        session_id: &str,
    ) -> Result<()> {
        let tx = self.tx;
        tx.execute(
            "INSERT OR IGNORE INTO identity_interruptions(provider,session_id,status,unread,attention_reason,error,last_event_at) SELECT provider,session_id,status,unread,attention_reason,error,last_event_at FROM sessions WHERE provider=? AND session_id=?",
            params![provider.as_str(), session_id],
        )?;
        tx.execute(
            "INSERT OR IGNORE INTO session_status_observations(provider,session_id,kind,status,unread,attention_reason,error,observed_at,source) SELECT provider,session_id,'lifecycle',status,unread,attention_reason,error,last_event_at,'identity-interruption' FROM sessions WHERE provider=? AND session_id=?",
            params![provider.as_str(), session_id],
        )?;
        Ok(())
    }

    pub(crate) fn delete_pending(&self, launch_token: &str) -> Result<bool> {
        let tx = self.tx;
        let deleted = tx.execute(
            "DELETE FROM pending_launches WHERE launch_token=?",
            [launch_token],
        )? == 1;
        tx.execute(
            "DELETE FROM meta WHERE key=?",
            [launch_phase_key(launch_token)],
        )?;
        Ok(deleted)
    }

    pub(crate) fn bind_launch(
        &self,
        launch_token: &str,
        provider: Provider,
        session_id: &str,
    ) -> Result<bool> {
        let tx = self.tx;
        if let Some(existing) = launch_binding_tx(tx, launch_token)? {
            return Ok(existing == (provider, session_id.to_owned()));
        }
        tx.execute(
            "INSERT INTO launch_bindings(launch_token,provider,session_id,created_at) VALUES (?,?,?,?)",
            params![launch_token, provider.as_str(), session_id, now()],
        )?;
        Ok(true)
    }

    pub(crate) fn certify_launch(
        &self,
        launch_token: &str,
        provider: Provider,
        session_id: &str,
        pid: i64,
        start_time: i64,
    ) -> Result<bool> {
        let tx = self.tx;
        if launch_binding_tx(tx, launch_token)? != Some((provider, session_id.to_owned())) {
            return Ok(false);
        }
        let pending: Option<(Provider, Option<i64>, Option<i64>)> = tx
            .query_row(
                "SELECT provider,root_pid,root_pid_start FROM pending_launches WHERE launch_token=?",
                [launch_token],
                |row| {
                    Ok((
                        parse_provider(row.get_ref(0)?.as_str()?, 0)?,
                        row.get(1)?,
                        row.get(2)?,
                    ))
                },
            )
            .optional()?;
        if pending.is_some_and(|candidate| candidate != (provider, Some(pid), Some(start_time))) {
            return Ok(false);
        }
        put_recovery_owner_tx(
            tx,
            provider,
            session_id,
            pid,
            start_time,
            launch_token,
            now(),
        )?;
        tx.execute(
            "DELETE FROM pending_launches WHERE launch_token=?",
            [launch_token],
        )?;
        tx.execute(
            "DELETE FROM meta WHERE key=?",
            [launch_phase_key(launch_token)],
        )?;
        Ok(true)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn switch_launch_binding(
        &self,
        launch_token: &str,
        provider: Provider,
        from_session_id: &str,
        to_session_id: &str,
        pid: i64,
        start_time: i64,
    ) -> Result<bool> {
        let tx = self.tx;
        let binding = launch_binding_tx(tx, launch_token)?;
        let proof = recovery_owner_tx(tx, provider, from_session_id)?;
        let target = recovery_owner_tx(tx, provider, to_session_id)?;
        let competing_target = target.is_some_and(|owner| {
            owner.pid != pid || owner.start_time != start_time || owner.launch_token != launch_token
        });
        if binding != Some((provider, from_session_id.to_owned()))
            || proof.as_ref().is_none_or(|owner| {
                owner.pid != pid
                    || owner.start_time != start_time
                    || owner.launch_token != launch_token
            })
            || competing_target
        {
            return Ok(false);
        }
        let changed = tx.execute(
            "UPDATE launch_bindings SET session_id=?,created_at=? WHERE launch_token=? AND provider=? AND session_id=?",
            params![to_session_id, now(), launch_token, provider.as_str(), from_session_id],
        )? == 1;
        if changed {
            tx.execute(
                "DELETE FROM recovery_owners WHERE provider=? AND session_id=?",
                params![provider.as_str(), from_session_id],
            )?;
            put_recovery_owner_tx(
                tx,
                provider,
                to_session_id,
                pid,
                start_time,
                launch_token,
                now(),
            )?;
        }
        Ok(changed)
    }

    pub(crate) fn set_live_owner(&self, owner: &LiveOwner) -> Result<bool> {
        let db = self.tx;
        if is_untracked_connection(db, owner.provider, &owner.session_id)? {
            return Ok(false);
        }
        Ok(db.execute(
            r#"INSERT INTO live_owners(provider,session_id,pid,start_time,owner_token,last_seen)
            VALUES (?,?,?,?,?,?) ON CONFLICT(provider,session_id,pid,owner_token) DO UPDATE SET
            start_time=excluded.start_time,last_seen=excluded.last_seen
            WHERE excluded.last_seen>=live_owners.last_seen"#,
            params![
                owner.provider.as_str(),
                owner.session_id,
                owner.pid,
                owner.start_time,
                owner.owner_token,
                owner.last_seen
            ],
        )? == 1)
    }

    pub(crate) fn delete_live_owners(
        &self,
        provider: Provider,
        session_id: &str,
        pid: Option<i64>,
        owner_token: Option<&str>,
    ) -> Result<usize> {
        let db = self.tx;
        let deleted = match (pid, owner_token) {
            (None, None) => db.execute(
                "DELETE FROM live_owners WHERE provider=? AND session_id=?",
                params![provider.as_str(), session_id],
            )?,
            (Some(pid), None) => db.execute(
                "DELETE FROM live_owners WHERE provider=? AND session_id=? AND pid=?",
                params![provider.as_str(), session_id, pid],
            )?,
            (None, Some(token)) => db.execute(
                "DELETE FROM live_owners WHERE provider=? AND session_id=? AND owner_token=?",
                params![provider.as_str(), session_id, token],
            )?,
            (Some(pid), Some(token)) => db.execute(
                "DELETE FROM live_owners WHERE provider=? AND session_id=? AND pid=? AND owner_token=?",
                params![provider.as_str(), session_id, pid, token],
            )?,
        };
        Ok(deleted)
    }

    pub(crate) fn delete_live_owners_observed_through(
        &self,
        provider: Provider,
        session_id: &str,
        pid: Option<i64>,
        owner_token: Option<&str>,
        observed_at: f64,
    ) -> Result<usize> {
        let db = self.tx;
        let deleted = match (pid, owner_token) {
            (None, None) => db.execute(
                "DELETE FROM live_owners WHERE provider=? AND session_id=? AND last_seen<=?",
                params![provider.as_str(), session_id, observed_at],
            )?,
            (Some(pid), None) => db.execute(
                "DELETE FROM live_owners WHERE provider=? AND session_id=? AND pid=? AND last_seen<=?",
                params![provider.as_str(), session_id, pid, observed_at],
            )?,
            (None, Some(token)) => db.execute(
                "DELETE FROM live_owners WHERE provider=? AND session_id=? AND owner_token=? AND last_seen<=?",
                params![provider.as_str(), session_id, token, observed_at],
            )?,
            (Some(pid), Some(token)) => db.execute(
                "DELETE FROM live_owners WHERE provider=? AND session_id=? AND pid=? AND owner_token=? AND last_seen<=?",
                params![provider.as_str(), session_id, pid, token, observed_at],
            )?,
        };
        Ok(deleted)
    }

    pub(crate) fn delete_other_live_owner_sessions(
        &self,
        provider: Provider,
        pid: i64,
        keep_session_id: &str,
    ) -> Result<usize> {
        let db = self.tx;
        Ok(db.execute(
            "DELETE FROM live_owners WHERE provider=? AND pid=? AND session_id<>?",
            params![provider.as_str(), pid, keep_session_id],
        )?)
    }

    pub(crate) fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        let db = self.tx;
        db.execute(
            "INSERT INTO meta(key,value) VALUES (?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub(crate) fn delete_meta(&self, key: &str) -> Result<bool> {
        let db = self.tx;
        Ok(db.execute("DELETE FROM meta WHERE key=?", [key])? == 1)
    }

    pub(crate) fn record_attach(&self, provider: Provider, session_id: &str) -> Result<()> {
        let tx = self.tx;
        let current = serde_json::to_string(&(provider.as_str(), session_id))?;
        let previous: Option<String> = tx
            .query_row(
                "SELECT value FROM meta WHERE key='last_attached'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(previous) = previous.filter(|value| value != &current) {
            set_meta_tx(tx, "previous_attached", &previous)?;
        }
        set_meta_tx(tx, "last_attached", &current)?;
        Ok(())
    }

    pub(crate) fn record_hook_observation(&self, observation: &HookObservation) -> Result<()> {
        let db = self.tx;
        db.execute(
            r#"INSERT INTO hook_observations(provider,fingerprint,event_name,session_id,observed_at,source,managed)
            VALUES (?,?,?,?,?,?,?) ON CONFLICT(provider) DO UPDATE SET
            fingerprint=excluded.fingerprint,event_name=excluded.event_name,
            session_id=excluded.session_id,observed_at=excluded.observed_at,
            source=excluded.source,managed=excluded.managed
            WHERE excluded.observed_at>=hook_observations.observed_at"#,
            params![observation.provider.as_str(), observation.fingerprint, observation.event_name, observation.session_id, observation.observed_at, observation.source, bool_i64(observation.managed)],
        )?;
        Ok(())
    }

    pub(crate) fn get_session_by_thread(
        &self,
        provider: Provider,
        thread_id: &str,
    ) -> Result<Option<Session>> {
        let db = self.tx;
        db.query_row(
            "SELECT * FROM sessions WHERE provider=? AND (session_id=? OR active_thread_id=?) ORDER BY CASE WHEN session_id=? THEN 0 ELSE 1 END LIMIT 1",
            params![provider.as_str(), thread_id, thread_id, thread_id],
            session_from_row,
        )
        .optional()
        .map_err(Into::into)
    }

    pub(crate) fn get_pending(&self, launch_token: &str) -> Result<Option<PendingLaunch>> {
        let db = self.tx;
        db.query_row(
            "SELECT * FROM pending_launches WHERE launch_token=?",
            [launch_token],
            pending_from_row,
        )
        .optional()
        .map_err(Into::into)
    }

    pub(crate) fn find_pending_for_pane(&self, pane: &str) -> Result<Option<PendingLaunch>> {
        let db = self.tx;
        db.query_row(
            "SELECT * FROM pending_launches WHERE tmux_pane=? ORDER BY created_at DESC LIMIT 1",
            [pane],
            pending_from_row,
        )
        .optional()
        .map_err(Into::into)
    }

    pub(crate) fn get_meta(&self, key: &str) -> Result<Option<String>> {
        let db = self.tx;
        db.query_row("SELECT value FROM meta WHERE key=?", [key], |row| {
            row.get(0)
        })
        .optional()
        .map_err(Into::into)
    }

    pub(crate) fn get_session(
        &self,
        provider: Provider,
        session_id: &str,
    ) -> Result<Option<Session>> {
        self.tx
            .query_row(
                "SELECT * FROM sessions WHERE provider=? AND session_id=?",
                params![provider.as_str(), session_id],
                session_from_row,
            )
            .optional()
            .map_err(Into::into)
    }

    pub(crate) fn is_untracked(&self, provider: Provider, session_id: &str) -> Result<bool> {
        is_untracked_tx(self.tx, provider, session_id).map_err(Into::into)
    }

    pub(crate) fn upsert_session(&self, session: &Session, preserve_name: bool) -> Result<bool> {
        upsert_session_tx(self.tx, session, preserve_name)
    }

    pub(crate) fn clear_session_runtime(
        &self,
        provider: Provider,
        session_id: &str,
        observed_at: f64,
    ) -> Result<bool> {
        Ok(self.tx.execute(
            "UPDATE sessions SET root_pid=NULL,updated_at=? WHERE provider=? AND session_id=?",
            params![observed_at, provider.as_str(), session_id],
        )? == 1)
    }

    pub(crate) fn clear_session_home(
        &self,
        provider: Provider,
        session_id: &str,
        observed_at: f64,
    ) -> Result<bool> {
        Ok(self.tx.execute(
            "UPDATE sessions SET tmux_session=NULL,tmux_pane=NULL,root_pid=NULL,updated_at=? WHERE provider=? AND session_id=?",
            params![observed_at, provider.as_str(), session_id],
        )? == 1)
    }

    pub(crate) fn status_observations(
        &self,
        provider: Provider,
        session_id: &str,
    ) -> Result<Vec<StatusObservation>> {
        let mut statement = self.tx.prepare(
            "SELECT kind,status,unread,attention_reason,error,observed_at,source FROM session_status_observations WHERE provider=? AND session_id=? ORDER BY kind",
        )?;
        Ok(statement
            .query_map(params![provider.as_str(), session_id], |row| {
                Ok(StatusObservation {
                    kind: parse_observation_kind(row.get_ref(0)?.as_str()?, 0)?,
                    status: parse_status(row.get_ref(1)?.as_str()?, 1)?,
                    unread: row.get::<_, i64>(2)? != 0,
                    attention_reason: row.get(3)?,
                    error: row.get(4)?,
                    observed_at: row.get(5)?,
                    source: row.get(6)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub(crate) fn record_status_observation(
        &self,
        provider: Provider,
        session_id: &str,
        observation: &StatusObservation,
    ) -> Result<bool> {
        Ok(self.tx.execute(
            r#"INSERT INTO session_status_observations(provider,session_id,kind,status,unread,attention_reason,error,observed_at,source)
            VALUES (?,?,?,?,?,?,?,?,?) ON CONFLICT(provider,session_id,kind) DO UPDATE SET
            status=excluded.status,unread=excluded.unread,attention_reason=excluded.attention_reason,
            error=excluded.error,observed_at=excluded.observed_at,source=excluded.source
            WHERE excluded.observed_at>=session_status_observations.observed_at"#,
            params![provider.as_str(), session_id, observation_kind_str(observation.kind), observation.status.as_str(), bool_i64(observation.unread), observation.attention_reason, observation.error, observation.observed_at, observation.source],
        )? == 1)
    }

    pub(crate) fn clear_status_observation(
        &self,
        provider: Provider,
        session_id: &str,
        kind: ObservationKind,
    ) -> Result<bool> {
        Ok(self.tx.execute(
            "DELETE FROM session_status_observations WHERE provider=? AND session_id=? AND kind=?",
            params![provider.as_str(), session_id, observation_kind_str(kind)],
        )? == 1)
    }

    pub(crate) fn clear_status_observation_observed_through(
        &self,
        provider: Provider,
        session_id: &str,
        kind: ObservationKind,
        observed_at: f64,
    ) -> Result<bool> {
        Ok(self.tx.execute(
            "DELETE FROM session_status_observations WHERE provider=? AND session_id=? AND kind=? AND observed_at<=?",
            params![
                provider.as_str(),
                session_id,
                observation_kind_str(kind),
                observed_at
            ],
        )? == 1)
    }

    pub(crate) fn clear_identity_interruption(
        &self,
        provider: Provider,
        session_id: &str,
    ) -> Result<bool> {
        Ok(self.tx.execute(
            "DELETE FROM identity_interruptions WHERE provider=? AND session_id=?",
            params![provider.as_str(), session_id],
        )? == 1)
    }

    pub(crate) fn live_owners(
        &self,
        provider: Provider,
        session_id: &str,
    ) -> Result<Vec<LiveOwner>> {
        let mut statement = self.tx.prepare(
            "SELECT provider,session_id,pid,start_time,owner_token,last_seen FROM live_owners WHERE provider=? AND session_id=? ORDER BY pid,owner_token",
        )?;
        Ok(statement
            .query_map(params![provider.as_str(), session_id], live_owner_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub(crate) fn delete_live_owner(
        &self,
        provider: Provider,
        session_id: &str,
        pid: i64,
        owner_token: &str,
    ) -> Result<bool> {
        Ok(self.tx.execute(
            "DELETE FROM live_owners WHERE provider=? AND session_id=? AND pid=? AND owner_token=?",
            params![provider.as_str(), session_id, pid, owner_token],
        )? == 1)
    }

    pub(crate) fn delete_live_owner_generation(&self, owner: &LiveOwner) -> Result<bool> {
        Ok(self.tx.execute(
            "DELETE FROM live_owners WHERE provider=? AND session_id=? AND pid=? AND start_time IS ? AND owner_token=?",
            params![
                owner.provider.as_str(),
                owner.session_id,
                owner.pid,
                owner.start_time,
                owner.owner_token
            ],
        )? == 1)
    }

    pub(crate) fn get_recovery_owner(
        &self,
        provider: Provider,
        session_id: &str,
    ) -> Result<Option<RecoveryOwner>> {
        recovery_owner_connection(self.tx, provider, session_id)
    }

    pub(crate) fn delete_recovery_owner(
        &self,
        provider: Provider,
        session_id: &str,
    ) -> Result<bool> {
        Ok(self.tx.execute(
            "DELETE FROM recovery_owners WHERE provider=? AND session_id=?",
            params![provider.as_str(), session_id],
        )? == 1)
    }

    pub(crate) fn get_launch_binding(
        &self,
        launch_token: &str,
    ) -> Result<Option<(Provider, String)>> {
        launch_binding_connection(self.tx, launch_token)
    }

    pub(crate) fn delete_launch_binding_if(
        &self,
        launch_token: &str,
        provider: Provider,
        session_id: &str,
    ) -> Result<bool> {
        Ok(self.tx.execute(
            "DELETE FROM launch_bindings WHERE launch_token=? AND provider=? AND session_id=?",
            params![launch_token, provider.as_str(), session_id],
        )? == 1)
    }
}

fn upsert_session_tx(tx: &Transaction<'_>, session: &Session, preserve_name: bool) -> Result<bool> {
    if is_untracked_tx(tx, session.provider, &session.session_id)? {
        return Ok(false);
    }
    let existing = session_attention_row(tx, session.provider, &session.session_id)?;
    let timestamp = now();
    let created_at = nonzero_or(session.created_at, timestamp);
    let last_activity_at = nonzero_or(
        session.last_activity_at,
        nonzero_or(session.updated_at, timestamp),
    );
    let last_event_at = nonzero_or(session.last_event_at, timestamp);
    let name = if preserve_name {
        existing
            .as_ref()
            .and_then(|row| row.name.as_deref())
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .or_else(|| session.name.clone())
    } else {
        session.name.clone()
    };
    if !(matches!(session.status, Status::Error | Status::OpenTwice)
        && session.attention_reason.as_deref() == Some("identity"))
    {
        tx.execute(
            "UPDATE identity_interruptions SET status=?,unread=?,attention_reason=?,error=?,last_event_at=? WHERE provider=? AND session_id=?",
            params![session.status.as_str(), bool_i64(session.unread), session.attention_reason, session.error, last_event_at, session.provider.as_str(), session.session_id],
        )?;
    }
    tx.execute(
        r#"INSERT INTO sessions(
            provider,session_id,active_thread_id,name,cwd,branch,transcript_path,
            tmux_session,tmux_pane,root_pid,status,unread,model,source,managed,error,
            attention_reason,created_at,updated_at,last_event_at,last_activity_at
        ) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)
        ON CONFLICT(provider,session_id) DO UPDATE SET
            active_thread_id=COALESCE(excluded.active_thread_id,sessions.active_thread_id),
            name=COALESCE(excluded.name,sessions.name),
            cwd=COALESCE(excluded.cwd,sessions.cwd),
            branch=COALESCE(excluded.branch,sessions.branch),
            transcript_path=COALESCE(excluded.transcript_path,sessions.transcript_path),
            tmux_session=COALESCE(excluded.tmux_session,sessions.tmux_session),
            tmux_pane=COALESCE(excluded.tmux_pane,sessions.tmux_pane),
            root_pid=COALESCE(excluded.root_pid,sessions.root_pid),
            status=excluded.status,unread=excluded.unread,
            model=COALESCE(excluded.model,sessions.model),source=excluded.source,
            managed=MAX(sessions.managed,excluded.managed),error=excluded.error,
            attention_reason=excluded.attention_reason,updated_at=excluded.updated_at,
            last_event_at=MAX(sessions.last_event_at,excluded.last_event_at),
            last_activity_at=MAX(sessions.last_activity_at,excluded.last_activity_at)"#,
        params![
            session.provider.as_str(),
            session.session_id,
            session.active_thread_id,
            name,
            session.cwd,
            session.branch,
            session.transcript_path,
            session.tmux_session,
            session.tmux_pane,
            session.root_pid,
            session.status.as_str(),
            bool_i64(session.unread),
            session.model,
            session.source,
            bool_i64(session.managed),
            session.error,
            session.attention_reason,
            created_at,
            timestamp,
            last_event_at,
            last_activity_at
        ],
    )?;
    let kind = initial_observation_kind(session);
    tx.execute(
        "INSERT OR IGNORE INTO session_status_observations(provider,session_id,kind,status,unread,attention_reason,error,observed_at,source) VALUES (?,?,?,?,?,?,?,?,?)",
        params![session.provider.as_str(), session.session_id, observation_kind_str(kind), session.status.as_str(), bool_i64(session.unread), session.attention_reason, session.error, last_event_at, "initial"],
    )?;
    if became_actionable(existing.as_ref(), session) {
        insert_session_event(tx, session, last_event_at)?;
    }
    Ok(true)
}

fn bool_i64(value: bool) -> i64 {
    i64::from(value)
}

fn set_mode(path: &Path, mode: u32) -> Result<()> {
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .with_context(|| format!("cannot set private permissions on {}", path.display()))?;
    #[cfg(not(unix))]
    let _ = (path, mode);
    Ok(())
}

fn conversion_error(column: usize, message: impl Into<String>) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        column,
        rusqlite::types::Type::Text,
        Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            message.into(),
        )),
    )
}

fn parse_provider(value: &str, column: usize) -> rusqlite::Result<Provider> {
    value
        .parse()
        .map_err(|message: String| conversion_error(column, message))
}

fn parse_status(value: &str, column: usize) -> rusqlite::Result<Status> {
    value
        .parse()
        .map_err(|message: String| conversion_error(column, message))
}

fn parse_observation_kind(value: &str, column: usize) -> rusqlite::Result<ObservationKind> {
    match value {
        "lifecycle" => Ok(ObservationKind::Lifecycle),
        "runtime" => Ok(ObservationKind::Runtime),
        "safety" => Ok(ObservationKind::Safety),
        _ => Err(conversion_error(
            column,
            format!("unknown observation kind: {value}"),
        )),
    }
}

fn observation_kind_str(kind: ObservationKind) -> &'static str {
    match kind {
        ObservationKind::Lifecycle => "lifecycle",
        ObservationKind::Runtime => "runtime",
        ObservationKind::Safety => "safety",
    }
}

fn initial_observation_kind(session: &Session) -> ObservationKind {
    if matches!(session.status, Status::Error | Status::OpenTwice)
        && session.attention_reason.as_deref() == Some("identity")
    {
        ObservationKind::Safety
    } else if session.status == Status::Error
        && session.attention_reason.as_deref() == Some("exited")
    {
        ObservationKind::Runtime
    } else {
        ObservationKind::Lifecycle
    }
}

fn session_from_row(row: &Row<'_>) -> rusqlite::Result<Session> {
    Ok(Session {
        provider: parse_provider(row.get_ref("provider")?.as_str()?, 0)?,
        session_id: row.get("session_id")?,
        active_thread_id: row.get("active_thread_id")?,
        name: row.get("name")?,
        cwd: row.get("cwd")?,
        branch: row.get("branch")?,
        transcript_path: row.get("transcript_path")?,
        tmux_session: row.get("tmux_session")?,
        tmux_pane: row.get("tmux_pane")?,
        root_pid: row.get("root_pid")?,
        status: parse_status(row.get_ref("status")?.as_str()?, 9)?,
        unread: row.get::<_, i64>("unread")? != 0,
        model: row.get("model")?,
        source: row.get("source")?,
        managed: row.get::<_, i64>("managed")? != 0,
        error: row.get("error")?,
        attention_reason: row.get("attention_reason")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
        last_event_at: row.get("last_event_at")?,
        last_activity_at: row.get("last_activity_at")?,
        live: false,
        attached: false,
        home_state: "unknown".into(),
        cpu_percent: None,
        rss_kb: None,
        input_tokens: None,
        output_tokens: None,
        cached_input_tokens: None,
        cache_write_tokens: None,
        total_tokens: None,
        estimated_cost_usd: None,
    })
}

fn pending_from_row(row: &Row<'_>) -> rusqlite::Result<PendingLaunch> {
    let encoded: Option<String> = row.get("preexisting_session_ids_json")?;
    Ok(PendingLaunch {
        launch_token: row.get("launch_token")?,
        provider: parse_provider(row.get_ref("provider")?.as_str()?, 1)?,
        name: row.get("name")?,
        cwd: row.get("cwd")?,
        tmux_session: row.get("tmux_session")?,
        tmux_pane: row.get("tmux_pane")?,
        expected_session_id: row.get("expected_session_id")?,
        root_pid: row.get("root_pid")?,
        root_pid_start: row.get("root_pid_start")?,
        preexisting_session_ids: encoded
            .map(|value| parse_string_vec(&value, 9))
            .transpose()?,
        candidate_session_id: row.get("candidate_session_id")?,
        candidate_observed_at: row.get("candidate_observed_at")?,
        created_at: row.get("created_at")?,
    })
}

fn live_owner_from_row(row: &Row<'_>) -> rusqlite::Result<LiveOwner> {
    Ok(LiveOwner {
        provider: parse_provider(row.get_ref(0)?.as_str()?, 0)?,
        session_id: row.get(1)?,
        pid: row.get(2)?,
        start_time: row.get(3)?,
        owner_token: row.get(4)?,
        last_seen: row.get(5)?,
    })
}

fn fleet_refresh_generation_key(node_id: &str) -> String {
    format!("fleet:refresh-generation:{node_id}")
}

fn remote_board_cache_prefix(node_id: &str) -> String {
    format!("fleet:board-cache:{node_id}:")
}

fn remote_board_cache_pattern(node_id: &str) -> String {
    format!("{}%", remote_board_cache_prefix(node_id))
}

fn remote_board_cache_header_key(node_id: &str) -> String {
    format!("{}header", remote_board_cache_prefix(node_id))
}

fn remote_board_cache_chunk_key(node_id: &str, index: usize) -> String {
    format!("{}chunk:{index:04}", remote_board_cache_prefix(node_id))
}

fn remote_expert_cache_prefix(node_id: &str) -> String {
    format!("fleet:expert-cache:{node_id}:")
}

fn remote_expert_cache_pattern(node_id: &str) -> String {
    format!("{}%", remote_expert_cache_prefix(node_id))
}

fn remote_cache_revision_key(node_id: &str) -> String {
    format!("fleet:cache-revision:{node_id}")
}

fn fts_identity(prefix: &str, value: &str) -> String {
    format!("{prefix}{}", value.replace('-', ""))
}

fn prepare_remote_snapshot(
    node_id: &str,
    payload: &Value,
    captured_at: f64,
) -> Result<PreparedRemoteSnapshot> {
    let object = payload
        .as_object()
        .context("remote snapshot must be a JSON object")?;
    let encoded = serde_json::to_string(payload)?;
    if encoded.len() > MAX_REMOTE_SNAPSHOT_BYTES {
        bail!("remote snapshot exceeds the 4 MiB safety limit");
    }
    let board_cache = prepare_remote_board_cache(node_id, object, captured_at, encoded.len())?;
    let expert_cache = prepare_remote_expert_cache(node_id, object, captured_at, encoded.len())?;
    Ok(PreparedRemoteSnapshot {
        revision: Uuid::new_v4().to_string(),
        node_id: node_id.to_owned(),
        encoded,
        board_cache,
        expert_cache,
    })
}

fn prepare_remote_board_cache(
    node_id: &str,
    object: &serde_json::Map<String, Value>,
    captured_at: f64,
    source_encoded_bytes: usize,
) -> Result<Option<PreparedRemoteBoardCache>> {
    let Some(source_node_id) = object.get("node_id").and_then(Value::as_str) else {
        return Ok(None);
    };
    let Some(protocol) = object.get("protocol").and_then(Value::as_str) else {
        return Ok(None);
    };
    let Some(version) = object.get("version").and_then(Value::as_i64) else {
        return Ok(None);
    };
    let Some(remote_captured_at) = object.get("captured_at").and_then(Value::as_f64) else {
        return Ok(None);
    };
    let Some(sessions) = object.get("sessions").and_then(Value::as_array) else {
        return Ok(None);
    };
    if source_node_id != node_id || sessions.len() > 2_000 {
        return Ok(None);
    }
    let keyed = |field: &str| {
        object
            .get(field)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|value| {
                let row = value.as_object()?;
                Some((
                    (
                        row.get("provider")?.as_str()?.to_owned(),
                        row.get("session_id")?.as_str()?.to_owned(),
                    ),
                    value,
                ))
            })
            .collect::<BTreeMap<_, _>>()
    };
    let profiles = keyed("profiles");
    let cards = keyed("cards");
    let mut encoded_rows = Vec::with_capacity(sessions.len());
    for session in sessions {
        let Some(session_object) = session.as_object() else {
            return Ok(None);
        };
        let Some(key) = session_object
            .get("provider")
            .and_then(Value::as_str)
            .zip(session_object.get("session_id").and_then(Value::as_str))
            .map(|(provider, session_id)| (provider.to_owned(), session_id.to_owned()))
        else {
            return Ok(None);
        };
        let row = serde_json::json!({
            "session": session,
            "profile": profiles.get(&key).map(|value| (*value).clone()).unwrap_or(Value::Null),
            "card": cards.get(&key).map(|value| (*value).clone()).unwrap_or(Value::Null),
        });
        encoded_rows.push(serde_json::to_string(&row)?);
    }
    let (chunks, chunk_metadata) = chunk_cache_rows(encoded_rows)?;
    if chunk_metadata.len() > MAX_REMOTE_CACHE_CHUNKS
        || chunks.iter().map(String::len).sum::<usize>() > MAX_REMOTE_BOARD_CACHE_BYTES
    {
        return Ok(None);
    }
    let directory_notices = object
        .get("directory_notices")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let header = serde_json::to_string(&RemoteBoardCacheHeader {
        schema: REMOTE_BOARD_CACHE_SCHEMA,
        node_id: node_id.to_owned(),
        protocol: protocol.to_owned(),
        version,
        source_captured_at: captured_at,
        remote_captured_at,
        source_encoded_bytes,
        source_sessions: sessions.len(),
        chunks: chunk_metadata,
        directory_notices,
    })?;
    if header.len() > MAX_REMOTE_CACHE_HEADER_BYTES
        || header.len() + chunks.iter().map(String::len).sum::<usize>()
            > MAX_REMOTE_BOARD_CACHE_BYTES
    {
        return Ok(None);
    }
    Ok(Some(PreparedRemoteBoardCache { header, chunks }))
}

fn prepare_remote_expert_cache(
    node_id: &str,
    object: &serde_json::Map<String, Value>,
    captured_at: f64,
    source_encoded_bytes: usize,
) -> Result<Option<PreparedRemoteExpertCache>> {
    let Some(source_node_id) = object.get("node_id").and_then(Value::as_str) else {
        return Ok(None);
    };
    let Some(protocol) = object.get("protocol").and_then(Value::as_str) else {
        return Ok(None);
    };
    let Some(version) = object.get("version").and_then(Value::as_i64) else {
        return Ok(None);
    };
    let Some(remote_captured_at) = object.get("captured_at").and_then(Value::as_f64) else {
        return Ok(None);
    };
    if source_node_id != node_id {
        return Ok(None);
    }
    let keyed = |field: &str| {
        object
            .get(field)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|value| {
                let row = value.as_object()?;
                Some((
                    (
                        row.get("provider")?.as_str()?.to_owned(),
                        row.get("session_id")?.as_str()?.to_owned(),
                    ),
                    value,
                ))
            })
            .collect::<BTreeMap<_, _>>()
    };
    let profiles = keyed("profiles");
    let cards = keyed("cards");
    let mut seen = BTreeSet::new();
    let mut rows = Vec::new();
    let mut indexed_bytes = 0_usize;
    for session in object
        .get("expert_sessions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .chain(
            object
                .get("sessions")
                .and_then(Value::as_array)
                .into_iter()
                .flatten(),
        )
    {
        let Some(row) = session.as_object() else {
            return Ok(None);
        };
        let Some(key) = row
            .get("provider")
            .and_then(Value::as_str)
            .zip(row.get("session_id").and_then(Value::as_str))
            .map(|(provider, session_id)| (provider.to_owned(), session_id.to_owned()))
        else {
            return Ok(None);
        };
        let Some(profile) = profiles.get(&key) else {
            continue;
        };
        if !seen.insert(key.clone()) {
            continue;
        }
        let Some(profile) = profile.as_object() else {
            return Ok(None);
        };
        let Some(session_id) = row.get("session_id").and_then(Value::as_str) else {
            return Ok(None);
        };
        let Some(topics) = joined_search_items(profile.get("topics")) else {
            return Ok(None);
        };
        let Some(artifacts) = joined_search_items(profile.get("artifacts")) else {
            return Ok(None);
        };
        let Some(scope) = profile.get("scope").and_then(Value::as_str) else {
            return Ok(None);
        };
        let Some(current) = profile.get("current_state").and_then(Value::as_str) else {
            return Ok(None);
        };
        let Some(name) = optional_search_text(row.get("name")) else {
            return Ok(None);
        };
        let Some(cwd) = optional_search_text(row.get("cwd")) else {
            return Ok(None);
        };
        let Some(branch) = optional_search_text(row.get("branch")) else {
            return Ok(None);
        };
        let live = match row.get("live") {
            None => false,
            Some(Value::Bool(value)) => *value,
            _ => return Ok(None),
        };
        let Some(profile_updated_at) = profile
            .get("updated_at")
            .and_then(Value::as_f64)
            .filter(|value| value.is_finite() && *value >= 0.0)
        else {
            return Ok(None);
        };
        let row_json = serde_json::to_string(&serde_json::json!({
            "session": session,
            "profile": Value::Object(profile.clone()),
            "card": cards.get(&key).map(|value| (*value).clone()).unwrap_or(Value::Null),
        }))?;
        let search_row = PreparedRemoteExpertRow {
            ordinal: rows.len(),
            row_json,
            live,
            profile_updated_at,
            session_id: session_id.to_owned(),
            topic_tokens: token_sequence(&topics),
            scope_tokens: token_sequence(scope),
            current_tokens: token_sequence(current),
            name_tokens: token_sequence(name),
            project_tokens: token_sequence(&[cwd, branch].join(" ")),
            artifact_tokens: token_sequence(&artifacts),
        };
        indexed_bytes = [
            search_row.row_json.len(),
            search_row.session_id.len(),
            search_row.topic_tokens.len(),
            search_row.scope_tokens.len(),
            search_row.current_tokens.len(),
            search_row.name_tokens.len(),
            search_row.project_tokens.len(),
            search_row.artifact_tokens.len(),
        ]
        .into_iter()
        .try_fold(indexed_bytes, |total, bytes| total.checked_add(bytes))
        .context("remote expert search index size overflowed")?;
        if indexed_bytes > MAX_REMOTE_EXPERT_INDEX_BYTES {
            return Ok(None);
        }
        rows.push(search_row);
    }
    if rows.len() > 2_000 {
        return Ok(None);
    }
    let source_experts = rows.len();
    let directory_notices = object
        .get("directory_notices")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let header = serde_json::to_string(&RemoteExpertCacheHeader {
        schema: REMOTE_EXPERT_CACHE_SCHEMA,
        node_id: node_id.to_owned(),
        protocol: protocol.to_owned(),
        version,
        source_captured_at: captured_at,
        remote_captured_at,
        source_encoded_bytes,
        source_experts,
        indexed_bytes,
        directory_notices,
    })?;
    if header.len() > MAX_REMOTE_CACHE_HEADER_BYTES {
        return Ok(None);
    }
    Ok(Some(PreparedRemoteExpertCache { header, rows }))
}

fn joined_search_items(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_array)?
        .iter()
        .map(Value::as_str)
        .collect::<Option<Vec<_>>>()
        .map(|items| items.join(" "))
}

fn optional_search_text(value: Option<&Value>) -> Option<&str> {
    match value {
        None | Some(Value::Null) => Some(""),
        Some(Value::String(value)) => Some(value),
        _ => None,
    }
}

fn chunk_cache_rows(
    encoded_rows: Vec<String>,
) -> Result<(Vec<String>, Vec<RemoteBoardCacheChunk>)> {
    let mut pending = Vec::<String>::new();
    let mut content_bytes = 0_usize;
    let mut chunks = Vec::<String>::new();
    let mut metadata = Vec::<RemoteBoardCacheChunk>::new();
    let flush = |rows: &mut Vec<String>,
                 chunks: &mut Vec<String>,
                 metadata: &mut Vec<RemoteBoardCacheChunk>| {
        if rows.is_empty() {
            return;
        }
        let encoded = format!("[{}]", rows.join(","));
        metadata.push(RemoteBoardCacheChunk {
            bytes: encoded.len(),
            rows: rows.len(),
        });
        chunks.push(encoded);
        rows.clear();
    };
    for row in encoded_rows {
        let next_bytes = 2 + content_bytes + pending.len() + row.len();
        if !pending.is_empty() && next_bytes > REMOTE_CACHE_CHUNK_BYTES {
            flush(&mut pending, &mut chunks, &mut metadata);
            content_bytes = 0;
        }
        content_bytes = content_bytes
            .checked_add(row.len())
            .context("remote cache size overflowed")?;
        pending.push(row);
    }
    flush(&mut pending, &mut chunks, &mut metadata);
    Ok((chunks, metadata))
}

fn write_remote_snapshot_tx(
    tx: &Transaction<'_>,
    node_id: &str,
    prepared: &PreparedRemoteSnapshot,
    captured_at: f64,
) -> Result<Option<String>> {
    let previous_revision = tx
        .query_row(
            "SELECT value FROM meta WHERE key=?",
            [remote_cache_revision_key(node_id)],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    tx.execute(
        "INSERT INTO remote_snapshots(node_id,payload_json,captured_at) VALUES (?,?,?) ON CONFLICT(node_id) DO UPDATE SET payload_json=excluded.payload_json,captured_at=excluded.captured_at",
        params![node_id, &prepared.encoded, captured_at],
    )?;
    // Large projection pages were committed to the independent cache WAL
    // before this transaction. The lifecycle writer performs only the
    // authoritative snapshot replacement and one small manifest swap.
    tx.execute(
        "INSERT INTO meta(key,value) VALUES (?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        params![remote_cache_revision_key(node_id), prepared.revision],
    )?;
    Ok(previous_revision)
}

fn fleet_refresh_is_current(tx: &Transaction<'_>, node_id: &str, generation: u64) -> Result<bool> {
    let key = fleet_refresh_generation_key(node_id);
    let current = tx
        .query_row("SELECT value FROM meta WHERE key=?", [&key], |row| {
            row.get::<_, String>(0)
        })
        .optional()?
        .and_then(|value| value.parse::<u64>().ok());
    Ok(current == Some(generation))
}

fn recovery_owner_from_row(row: &Row<'_>) -> rusqlite::Result<RecoveryOwner> {
    Ok(RecoveryOwner {
        provider: parse_provider(row.get_ref(0)?.as_str()?, 0)?,
        session_id: row.get(1)?,
        pid: row.get(2)?,
        start_time: row.get(3)?,
        launch_token: row.get(4)?,
        created_at: row.get(5)?,
    })
}

fn expert_profile_from_row(row: &Row<'_>) -> rusqlite::Result<StoredExpertProfile> {
    let topics: String = row.get("topics_json")?;
    let artifacts: String = row.get("artifacts_json")?;
    Ok(StoredExpertProfile {
        profile: ExpertProfile {
            provider: parse_provider(row.get_ref("provider")?.as_str()?, 0)?,
            session_id: row.get("session_id")?,
            summary: row.get("summary")?,
            current_state: row.get("current_state")?,
            topics: parse_string_vec(&topics, 4)?,
            artifacts: parse_string_vec(&artifacts, 5)?,
            source: row.get("source")?,
            updated_at: row.get("updated_at")?,
            scope_updated_at: row.get("scope_updated_at")?,
            current_state_updated_at: row.get("current_state_updated_at")?,
        },
        transcript_mtime_ns: row.get("transcript_mtime_ns")?,
        transcript_size: row.get("transcript_size")?,
        current_state_mtime_ns: row.get("current_state_mtime_ns")?,
        current_state_size: row.get("current_state_size")?,
    })
}

fn fleet_node_from_row(row: &Row<'_>) -> rusqlite::Result<FleetNode> {
    let sources: String = row.get("sources_json")?;
    let capabilities: String = row.get("capabilities_json")?;
    Ok(FleetNode {
        node_id: row.get("node_id")?,
        alias: row.get("alias")?,
        ssh_target: row.get("ssh_target")?,
        sources: parse_string_vec(&sources, 3)?,
        status: row.get("status")?,
        protocol_version: row.get("protocol_version")?,
        package_version: row.get("package_version")?,
        capabilities: parse_string_vec(&capabilities, 7)?,
        last_seen: row.get("last_seen")?,
        last_attempt_at: row.get("last_attempt_at")?,
        last_error: row.get("last_error")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
    })
}

fn usage_from_row(row: &Row<'_>) -> rusqlite::Result<UsageCacheRecord> {
    Ok(UsageCacheRecord {
        provider: parse_provider(row.get_ref(0)?.as_str()?, 0)?,
        session_id: row.get(1)?,
        source_path: row.get(2)?,
        source_mtime_ns: row.get(3)?,
        source_size: row.get(4)?,
        model: row.get(5)?,
        input_tokens: row.get(6)?,
        output_tokens: row.get(7)?,
        cached_input_tokens: row.get(8)?,
        cache_write_tokens: row.get(9)?,
        total_tokens: row.get(10)?,
        estimated_cost_usd: row.get(11)?,
        updated_at: row.get(12)?,
    })
}

fn parse_json_value(encoded: &str, column: usize) -> rusqlite::Result<Value> {
    serde_json::from_str(encoded)
        .map_err(|error| conversion_error(column, format!("invalid JSON: {error}")))
}

fn parse_string_vec(encoded: &str, column: usize) -> rusqlite::Result<Vec<String>> {
    serde_json::from_str(encoded)
        .map_err(|error| conversion_error(column, format!("invalid string array JSON: {error}")))
}

fn encode_optional_strings(value: &Option<Vec<String>>) -> Result<Option<String>> {
    value
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(Into::into)
}

fn session_attention_row(
    tx: &Transaction<'_>,
    provider: Provider,
    session_id: &str,
) -> rusqlite::Result<Option<AttentionRow>> {
    tx.query_row(
        "SELECT name,status,unread,attention_reason,error FROM sessions WHERE provider=? AND session_id=?",
        params![provider.as_str(), session_id],
        |row| {
            Ok(AttentionRow {
                name: row.get(0)?,
                status: parse_status(row.get_ref(1)?.as_str()?, 1)?,
                unread: row.get::<_, i64>(2)? != 0,
                attention_reason: row.get(3)?,
                error: row.get(4)?,
            })
        },
    )
    .optional()
}

fn became_actionable(existing: Option<&AttentionRow>, session: &Session) -> bool {
    if !session.unread
        || !matches!(
            session.status,
            Status::NeedsYou | Status::Ready | Status::Error | Status::OpenTwice
        )
    {
        return false;
    }
    existing.is_none_or(|row| {
        !(row.unread
            && row.status == session.status
            && row.attention_reason == session.attention_reason
            && row.error == session.error)
    })
}

fn insert_session_event(
    tx: &Transaction<'_>,
    session: &Session,
    event_at: f64,
) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT OR IGNORE INTO session_events(provider,session_id,event_at,status,attention_reason,error) VALUES (?,?,?,?,?,?)",
        params![session.provider.as_str(), session.session_id, event_at, session.status.as_str(), session.attention_reason, session.error],
    )?;
    Ok(())
}

fn is_untracked_tx(
    tx: &Transaction<'_>,
    provider: Provider,
    session_id: &str,
) -> rusqlite::Result<bool> {
    Ok(tx
        .query_row(
            "SELECT 1 FROM untracked_sessions WHERE provider=? AND session_id=?",
            params![provider.as_str(), session_id],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

fn is_untracked_connection(
    db: &Connection,
    provider: Provider,
    session_id: &str,
) -> rusqlite::Result<bool> {
    Ok(db
        .query_row(
            "SELECT 1 FROM untracked_sessions WHERE provider=? AND session_id=?",
            params![provider.as_str(), session_id],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

fn launch_binding_tx(
    tx: &Transaction<'_>,
    launch_token: &str,
) -> Result<Option<(Provider, String)>> {
    tx.query_row(
        "SELECT provider,session_id FROM launch_bindings WHERE launch_token=?",
        [launch_token],
        |row| Ok((parse_provider(row.get_ref(0)?.as_str()?, 0)?, row.get(1)?)),
    )
    .optional()
    .map_err(Into::into)
}

fn launch_binding_connection(
    db: &Connection,
    launch_token: &str,
) -> Result<Option<(Provider, String)>> {
    db.query_row(
        "SELECT provider,session_id FROM launch_bindings WHERE launch_token=?",
        [launch_token],
        |row| Ok((parse_provider(row.get_ref(0)?.as_str()?, 0)?, row.get(1)?)),
    )
    .optional()
    .map_err(Into::into)
}

#[allow(clippy::too_many_arguments)]
fn put_recovery_owner_tx(
    tx: &Transaction<'_>,
    provider: Provider,
    session_id: &str,
    pid: i64,
    start_time: i64,
    launch_token: &str,
    created_at: f64,
) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT INTO recovery_owners(provider,session_id,pid,start_time,launch_token,created_at) VALUES (?,?,?,?,?,?) ON CONFLICT(provider,session_id) DO UPDATE SET pid=excluded.pid,start_time=excluded.start_time,launch_token=excluded.launch_token,created_at=excluded.created_at",
        params![provider.as_str(), session_id, pid, start_time, launch_token, created_at],
    )?;
    Ok(())
}

fn put_recovery_owner_connection(db: &Connection, owner: &RecoveryOwner) -> rusqlite::Result<()> {
    db.execute(
        "INSERT INTO recovery_owners(provider,session_id,pid,start_time,launch_token,created_at) VALUES (?,?,?,?,?,?) ON CONFLICT(provider,session_id) DO UPDATE SET pid=excluded.pid,start_time=excluded.start_time,launch_token=excluded.launch_token,created_at=excluded.created_at",
        params![owner.provider.as_str(), owner.session_id, owner.pid, owner.start_time, owner.launch_token, owner.created_at],
    )?;
    Ok(())
}

fn recovery_owner_tx(
    tx: &Transaction<'_>,
    provider: Provider,
    session_id: &str,
) -> Result<Option<RecoveryOwner>> {
    tx.query_row(
        "SELECT provider,session_id,pid,start_time,launch_token,created_at FROM recovery_owners WHERE provider=? AND session_id=?",
        params![provider.as_str(), session_id],
        recovery_owner_from_row,
    )
    .optional()
    .map_err(Into::into)
}

fn recovery_owner_connection(
    db: &Connection,
    provider: Provider,
    session_id: &str,
) -> Result<Option<RecoveryOwner>> {
    db.query_row(
        "SELECT provider,session_id,pid,start_time,launch_token,created_at FROM recovery_owners WHERE provider=? AND session_id=?",
        params![provider.as_str(), session_id],
        recovery_owner_from_row,
    )
    .optional()
    .map_err(Into::into)
}

fn expert_profile_tx(
    tx: &Transaction<'_>,
    provider: Provider,
    session_id: &str,
) -> Result<Option<StoredExpertProfile>> {
    tx.query_row(
        "SELECT * FROM expert_profiles WHERE provider=? AND session_id=?",
        params![provider.as_str(), session_id],
        expert_profile_from_row,
    )
    .optional()
    .map_err(Into::into)
}

fn expert_profile_connection(
    db: &Connection,
    provider: Provider,
    session_id: &str,
) -> Result<Option<StoredExpertProfile>> {
    db.query_row(
        "SELECT * FROM expert_profiles WHERE provider=? AND session_id=?",
        params![provider.as_str(), session_id],
        expert_profile_from_row,
    )
    .optional()
    .map_err(Into::into)
}

fn set_meta_tx(tx: &Transaction<'_>, key: &str, value: &str) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT INTO meta(key,value) VALUES (?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        params![key, value],
    )?;
    Ok(())
}

fn validate_schema(db: &Connection) -> Result<()> {
    type TableContract<'a> = (&'a str, &'a [&'a str], &'a [&'a str]);
    const TABLES: &[TableContract<'_>] = &[
        (
            "sessions",
            &[
                "provider",
                "session_id",
                "active_thread_id",
                "name",
                "cwd",
                "branch",
                "transcript_path",
                "tmux_session",
                "tmux_pane",
                "root_pid",
                "status",
                "unread",
                "model",
                "source",
                "managed",
                "error",
                "attention_reason",
                "created_at",
                "updated_at",
                "last_event_at",
                "last_activity_at",
            ],
            &["provider", "session_id"],
        ),
        (
            "pending_launches",
            &[
                "launch_token",
                "provider",
                "name",
                "cwd",
                "tmux_session",
                "tmux_pane",
                "expected_session_id",
                "root_pid",
                "root_pid_start",
                "preexisting_session_ids_json",
                "candidate_session_id",
                "candidate_observed_at",
                "created_at",
            ],
            &["launch_token"],
        ),
        (
            "launch_reservations",
            &[
                "provider",
                "session_id",
                "token",
                "owner_pid",
                "owner_start_time",
                "created_at",
            ],
            &["provider", "session_id"],
        ),
        (
            "launch_bindings",
            &["launch_token", "provider", "session_id", "created_at"],
            &["launch_token"],
        ),
        (
            "live_owners",
            &[
                "provider",
                "session_id",
                "pid",
                "start_time",
                "owner_token",
                "last_seen",
            ],
            &["provider", "session_id", "pid", "owner_token"],
        ),
        (
            "recovery_owners",
            &[
                "provider",
                "session_id",
                "pid",
                "start_time",
                "launch_token",
                "created_at",
            ],
            &["provider", "session_id"],
        ),
        (
            "untracked_sessions",
            &["provider", "session_id", "untracked_at"],
            &["provider", "session_id"],
        ),
        (
            "usage_cache",
            &[
                "provider",
                "session_id",
                "source_path",
                "source_mtime_ns",
                "source_size",
                "model",
                "input_tokens",
                "output_tokens",
                "cached_input_tokens",
                "cache_write_tokens",
                "total_tokens",
                "estimated_cost_usd",
                "updated_at",
            ],
            &["provider", "session_id"],
        ),
        ("meta", &["key", "value"], &["key"]),
        (
            "hook_observations",
            &[
                "provider",
                "fingerprint",
                "event_name",
                "session_id",
                "observed_at",
                "source",
                "managed",
            ],
            &["provider"],
        ),
        (
            "session_events",
            &[
                "event_id",
                "provider",
                "session_id",
                "event_at",
                "status",
                "attention_reason",
                "error",
            ],
            &["event_id"],
        ),
        (
            "session_status_observations",
            &[
                "provider",
                "session_id",
                "kind",
                "status",
                "unread",
                "attention_reason",
                "error",
                "observed_at",
                "source",
            ],
            &["provider", "session_id", "kind"],
        ),
        (
            "identity_interruptions",
            &[
                "provider",
                "session_id",
                "status",
                "unread",
                "attention_reason",
                "error",
                "last_event_at",
            ],
            &["provider", "session_id"],
        ),
        (
            "expert_profiles",
            &[
                "provider",
                "session_id",
                "summary",
                "current_state",
                "topics_json",
                "artifacts_json",
                "source",
                "transcript_mtime_ns",
                "transcript_size",
                "updated_at",
                "scope_updated_at",
                "current_state_updated_at",
                "current_state_mtime_ns",
                "current_state_size",
            ],
            &["provider", "session_id"],
        ),
        (
            "expert_refresh_attempts",
            &[
                "provider",
                "session_id",
                "reset_at",
                "status",
                "detail",
                "attempted_at",
            ],
            &["provider", "session_id", "reset_at"],
        ),
        (
            "fleet_nodes",
            &[
                "node_id",
                "alias",
                "ssh_target",
                "sources_json",
                "status",
                "protocol_version",
                "package_version",
                "capabilities_json",
                "last_seen",
                "last_attempt_at",
                "last_error",
                "created_at",
                "updated_at",
            ],
            &["node_id"],
        ),
        (
            "remote_snapshots",
            &["node_id", "payload_json", "captured_at"],
            &["node_id"],
        ),
        (
            "ignored_node_candidates",
            &["candidate_key", "ignored_at"],
            &["candidate_key"],
        ),
    ];

    for &(table, columns, primary_key) in TABLES {
        let escaped = table.replace('\'', "''");
        let mut statement = db.prepare(&format!("PRAGMA table_info('{escaped}')"))?;
        let info = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(1)?, row.get::<_, i64>(5)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if info.is_empty() {
            bail!("incompatible Pika schema: missing table {table}");
        }
        let actual_columns: BTreeSet<_> = info.iter().map(|(name, _)| name.as_str()).collect();
        let missing: Vec<_> = columns
            .iter()
            .copied()
            .filter(|column| !actual_columns.contains(column))
            .collect();
        if !missing.is_empty() {
            bail!(
                "incompatible Pika schema: table {table} is missing column(s) {}",
                missing.join(", ")
            );
        }
        let mut actual_pk: Vec<_> = info
            .iter()
            .filter(|(_, position)| *position > 0)
            .map(|(name, position)| (*position, name.as_str()))
            .collect();
        actual_pk.sort_by_key(|(position, _)| *position);
        let actual_pk: Vec<_> = actual_pk.into_iter().map(|(_, name)| name).collect();
        if actual_pk != primary_key {
            bail!(
                "incompatible Pika schema: table {table} has primary key {actual_pk:?}, expected {primary_key:?}"
            );
        }
    }
    for index in [
        "sessions_name_idx",
        "sessions_status_idx",
        "sessions_active_thread_idx",
        "session_events_time_idx",
        "session_status_observations_time_idx",
        "expert_profiles_updated_idx",
    ] {
        let exists = db
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='index' AND name=?",
                [index],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if !exists {
            bail!("incompatible Pika schema: missing index {index}");
        }
    }
    Ok(())
}
