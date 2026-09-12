use crate::model::{
    ExpertProfile, FleetNode, ObservationKind, Provider, Session, Status, StatusObservation,
};
use crate::paths::Paths;
use anyhow::{Context, Result, anyhow, bail};
use fs2::FileExt;
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Row, Transaction, TransactionBehavior, params,
};
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
            "DELETE FROM meta WHERE key LIKE ? OR key LIKE ?",
            params![
                format!("fleet:pending-adopt:{node_id}:%"),
                format!("fleet:pending-untrack:{node_id}:%")
            ],
        )?;
        let deleted = tx.execute("DELETE FROM fleet_nodes WHERE node_id=?", [node_id])? == 1;
        tx.commit()?;
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

    pub fn put_remote_snapshot(
        &self,
        node_id: &str,
        payload: &Value,
        captured_at: f64,
    ) -> Result<()> {
        if !payload.is_object() {
            bail!("remote snapshot must be a JSON object");
        }
        let encoded = serde_json::to_string(payload)?;
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
            bail!("remote snapshot requires an adopted fleet node");
        }
        tx.execute(
            "INSERT INTO remote_snapshots(node_id,payload_json,captured_at) VALUES (?,?,?) ON CONFLICT(node_id) DO UPDATE SET payload_json=excluded.payload_json,captured_at=excluded.captured_at",
            params![node_id, encoded, captured_at],
        )?;
        tx.execute(
            "UPDATE fleet_nodes SET status='ready',last_seen=?,last_error=NULL,last_attempt_at=?,updated_at=? WHERE node_id=?",
            params![captured_at, captured_at, now(), node_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn get_remote_snapshot(&self, node_id: &str) -> Result<Option<RemoteSnapshot>> {
        if !self.exists() {
            return Ok(None);
        }
        let db = self.open_read()?;
        let snapshot = db
            .query_row(
                "SELECT node_id,payload_json,captured_at FROM remote_snapshots WHERE node_id=?",
                [node_id],
                |row| {
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
                    })
                },
            )
            .optional()?;
        Ok(snapshot)
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
        db.execute(
            r#"INSERT INTO live_owners(provider,session_id,pid,start_time,owner_token,last_seen)
            VALUES (?,?,?,?,?,?) ON CONFLICT(provider,session_id,pid,owner_token) DO UPDATE SET
            start_time=excluded.start_time,last_seen=excluded.last_seen"#,
            params![
                owner.provider.as_str(),
                owner.session_id,
                owner.pid,
                owner.start_time,
                owner.owner_token,
                owner.last_seen
            ],
        )?;
        Ok(true)
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
            source=excluded.source,managed=excluded.managed"#,
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
