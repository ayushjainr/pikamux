//! Durable, bounded memory for the Pika assistant.
//!
//! This store intentionally knows nothing about provider transcripts or the
//! operational store.  Callers supply already-authorized, bounded records.

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use thiserror::Error;
use uuid::Uuid;

const SCHEMA_VERSION: i64 = 3;
const MEMORY_SEARCH_INDEX_VERSION: &str = "1";
const MAX_RECORD_BYTES: usize = 256 * 1024;
const MAX_RETRIEVAL: usize = 256;
// Search is deliberately lexical and bounded.  Terms are extracted locally,
// quoted, and joined with OR before being passed to SQLite FTS5 MATCH.
const MAX_SEARCH_QUERY_BYTES: usize = 4 * 1024;
const MAX_SEARCH_TERMS: usize = 32;
const MAX_SEARCH_TERM_BYTES: usize = 128;

#[derive(Debug, Error)]
pub enum MemoryError {
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("invalid memory record: {0}")]
    Invalid(String),
    #[error("worker origin cannot create user-owned or protected records")]
    WorkerCannotAssumeUserAuthority,
    #[error("record {0} was not found")]
    NotFound(String),
    #[error("filesystem error: {0}")]
    Filesystem(#[from] std::io::Error),
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scope {
    #[serde(default)]
    pub node: Option<String>,
    pub project: Option<String>,
    pub provider: Option<String>,
    pub conversation: Option<String>,
}

impl Scope {
    /// Material may be narrowed to a more specific scope, never broadened.
    pub fn permits(&self, destination: &Scope) -> bool {
        [
            (&self.node, &destination.node),
            (&self.project, &destination.project),
            (&self.provider, &destination.provider),
            (&self.conversation, &destination.conversation),
        ]
        .iter()
        .all(|(source, target)| source.is_none() || source == target)
    }
    pub(crate) fn validate(&self) -> Result<(), MemoryError> {
        for value in [
            &self.node,
            &self.project,
            &self.provider,
            &self.conversation,
        ] {
            if value
                .as_ref()
                .is_some_and(|v| v.is_empty() || v.len() > 512)
            {
                return Err(MemoryError::Invalid(
                    "scope values must be 1..512 bytes".into(),
                ));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Origin {
    Human,
    Worker,
    System,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RecordKind {
    Draft,
    UserInstruction,
    InferredPreference,
    Proposal,
    Decision,
    Finding,
    Briefing,
    GraspInteraction,
    Correction,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DecisionState {
    Proposed,
    Accepted,
    Rejected,
    Deferred,
    Unresolved,
    Superseded,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewRecord {
    pub kind: RecordKind,
    pub origin: Origin,
    pub scope: Scope,
    pub body: String,
    pub provenance: String,
    pub timestamp: i64,
    pub supersedes: Option<String>,
    pub dependencies: Vec<String>,
    pub decision_state: Option<DecisionState>,
    pub protected_policy: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub id: String,
    pub profile_id: String,
    pub kind: RecordKind,
    pub origin: Origin,
    pub scope: Scope,
    pub body: String,
    pub provenance: String,
    pub timestamp: i64,
    pub supersedes: Option<String>,
    pub dependencies: Vec<String>,
    pub decision_state: Option<DecisionState>,
    pub protected_policy: bool,
}

/// A bounded typed active page. `limited` means callers must not claim that
/// an empty/short projection proves no other matching records exist.
pub struct RecordPage {
    pub records: Vec<Record>,
    pub limited: bool,
    pub byte_limited: bool,
}

/// Stable insertion/update ordering independent of user-supplied timestamps.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct RevisionRecord {
    pub revision: u64,
    pub record: Record,
}
pub(crate) struct RevisionPage {
    pub records: Vec<RevisionRecord>,
    pub through: u64,
    pub next: u64,
    pub has_more: bool,
}

/// Minimal ancestry metadata: classification never needs historical bodies.
#[derive(Clone)]
pub struct RecordLineage {
    pub id: String,
    pub kind: RecordKind,
    pub origin: Origin,
    pub scope: Scope,
    pub supersedes: Option<String>,
    pub provenance: String,
}

pub struct Store {
    path: PathBuf,
    pub(crate) connection: Connection,
    pub(crate) profile_id: String,
}

fn create_memory_schema(connection: &Connection) -> Result<(), MemoryError> {
    connection.execute_batch("CREATE TABLE IF NOT EXISTS memory_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);\nCREATE TABLE IF NOT EXISTS memory_records (id TEXT PRIMARY KEY, profile_id TEXT NOT NULL, kind TEXT NOT NULL, origin TEXT NOT NULL, project TEXT, provider TEXT, conversation TEXT, body TEXT NOT NULL, provenance TEXT NOT NULL, timestamp INTEGER NOT NULL, supersedes TEXT, dependencies TEXT NOT NULL, decision_state TEXT, protected_policy INTEGER NOT NULL);\nCREATE TABLE IF NOT EXISTS memory_receipts (request_id TEXT PRIMARY KEY, payload_hash TEXT NOT NULL, record_id TEXT, timestamp INTEGER NOT NULL);\nCREATE INDEX IF NOT EXISTS memory_records_time ON memory_records(timestamp DESC);\nCREATE INDEX IF NOT EXISTS memory_records_profile ON memory_records(profile_id);\nCREATE INDEX IF NOT EXISTS memory_records_supersedes ON memory_records(profile_id,supersedes);")?;
    connection.execute_batch("CREATE INDEX IF NOT EXISTS memory_records_active_type ON memory_records(profile_id,kind,timestamp DESC,id DESC)")?;
    Ok(())
}

fn migrate_memory_schema(tx: &Transaction<'_>) -> Result<String, MemoryError> {
    let version: Option<String> = tx
        .query_row(
            "SELECT value FROM memory_meta WHERE key='schema_version'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    validate_schema_version(version.as_deref())?;
    add_node_column_if_missing(tx)?;
    update_schema_version(tx, version.as_deref())?;
    load_or_create_profile(tx)
}

fn validate_schema_version(version: Option<&str>) -> Result<(), MemoryError> {
    if version.is_some_and(|v| v != "1" && v != "2" && v != SCHEMA_VERSION.to_string().as_str()) {
        return Err(MemoryError::Invalid(
            "unsupported assistant memory schema".into(),
        ));
    }
    Ok(())
}

fn add_node_column_if_missing(tx: &Transaction<'_>) -> Result<(), MemoryError> {
    let has_node: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('memory_records') WHERE name='node')",
        [],
        |row| row.get(0),
    )?;
    if !has_node {
        tx.execute("ALTER TABLE memory_records ADD COLUMN node TEXT", [])?;
    }
    Ok(())
}

fn update_schema_version(tx: &Transaction<'_>, version: Option<&str>) -> Result<(), MemoryError> {
    if version.is_some_and(|v| v == "1" || v == "2") {
        tx.execute(
            "UPDATE memory_meta SET value=? WHERE key='schema_version'",
            [SCHEMA_VERSION.to_string()],
        )?;
    }
    Ok(())
}

fn load_or_create_profile(tx: &Transaction<'_>) -> Result<String, MemoryError> {
    let profile: Option<String> = tx
        .query_row(
            "SELECT value FROM memory_meta WHERE key='profile_id'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    match profile {
        Some(value) => {
            Uuid::parse_str(&value)
                .map_err(|_| MemoryError::Invalid("stored profile identity is invalid".into()))?;
            Ok(value)
        }
        None => {
            let value = Uuid::new_v4().to_string();
            tx.execute(
                "INSERT INTO memory_meta(key,value) VALUES ('schema_version',?), ('profile_id',?)",
                params![SCHEMA_VERSION, value],
            )?;
            Ok(value)
        }
    }
}

fn ensure_memory_search_index(tx: &Transaction<'_>) -> Result<(), MemoryError> {
    tx.execute_batch(
        "CREATE VIRTUAL TABLE IF NOT EXISTS memory_fts USING fts5(body, content='memory_records', content_rowid='rowid', tokenize='unicode61');
         CREATE TRIGGER IF NOT EXISTS memory_records_fts_ai AFTER INSERT ON memory_records BEGIN
             INSERT INTO memory_fts(rowid, body) VALUES (new.rowid, new.body);
         END;
         CREATE TRIGGER IF NOT EXISTS memory_records_fts_ad AFTER DELETE ON memory_records BEGIN
             INSERT INTO memory_fts(memory_fts, rowid, body) VALUES ('delete', old.rowid, old.body);
         END;
         CREATE TRIGGER IF NOT EXISTS memory_records_fts_au AFTER UPDATE ON memory_records BEGIN
             INSERT INTO memory_fts(memory_fts, rowid, body) VALUES ('delete', old.rowid, old.body);
             INSERT INTO memory_fts(rowid, body) VALUES (new.rowid, new.body);
         END;",
    )?;
    backfill_search_index_if_needed(tx)?;
    ensure_memory_revisions(tx)
}

fn ensure_memory_revisions(tx: &Transaction<'_>) -> Result<(), MemoryError> {
    tx.execute_batch("CREATE TABLE IF NOT EXISTS memory_revisions(revision INTEGER PRIMARY KEY AUTOINCREMENT,record_id TEXT NOT NULL UNIQUE);
        CREATE TRIGGER IF NOT EXISTS memory_revisions_insert AFTER INSERT ON memory_records BEGIN
            INSERT INTO memory_revisions(record_id) VALUES(new.id);
        END;
        CREATE TRIGGER IF NOT EXISTS memory_revisions_update AFTER UPDATE ON memory_records BEGIN
            DELETE FROM memory_revisions WHERE record_id=old.id;
            INSERT INTO memory_revisions(record_id) VALUES(new.id);
        END;
        CREATE TRIGGER IF NOT EXISTS memory_revisions_delete AFTER DELETE ON memory_records BEGIN
            DELETE FROM memory_revisions WHERE record_id=old.id;
        END;")?;
    let initialized: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM memory_meta WHERE key='memory_revision_index_version' AND value='1')",[],|r|r.get(0))?;
    if !initialized {
        tx.execute("INSERT OR IGNORE INTO memory_revisions(record_id) SELECT id FROM memory_records ORDER BY rowid",[])?;
        tx.execute("INSERT INTO memory_meta(key,value) VALUES('memory_revision_index_version','1') ON CONFLICT(key) DO UPDATE SET value=excluded.value",[])?;
    }
    Ok(())
}

fn backfill_search_index_if_needed(tx: &Transaction<'_>) -> Result<(), MemoryError> {
    let version: Option<String> = tx
        .query_row(
            "SELECT value FROM memory_meta WHERE key='memory_search_index_version'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if version.as_deref() == Some(MEMORY_SEARCH_INDEX_VERSION) {
        return Ok(());
    }
    // Rebuild and marker share the migration transaction; failures retry on reopen.
    tx.execute("INSERT INTO memory_fts(memory_fts) VALUES ('rebuild')", [])?;
    tx.execute(
        "INSERT INTO memory_meta(key,value) VALUES('memory_search_index_version',?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        [MEMORY_SEARCH_INDEX_VERSION],
    )?;
    Ok(())
}

fn validate_request_id(request_id: &str) -> Result<(), MemoryError> {
    if request_id.is_empty() || request_id.len() > 256 {
        return Err(MemoryError::Invalid("invalid request id".into()));
    }
    Ok(())
}

fn existing_idempotent_record(
    tx: &Transaction<'_>,
    request_id: &str,
    hash: &str,
    profile_id: &str,
) -> Result<Option<Record>, MemoryError> {
    let receipt = tx
        .query_row(
            "SELECT payload_hash,record_id FROM memory_receipts WHERE request_id=?",
            [request_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
        )
        .optional()?;
    let Some((old_hash, old_record)) = receipt else {
        return Ok(None);
    };
    if old_hash != hash {
        return Err(MemoryError::Invalid(
            "request id was reused with different content".into(),
        ));
    }
    let id = old_record.ok_or_else(|| MemoryError::NotFound(request_id.into()))?;
    let record = tx.query_row(
        "SELECT id,profile_id,kind,origin,project,provider,conversation,body,provenance,timestamp,supersedes,dependencies,decision_state,protected_policy,node FROM memory_records WHERE id=? AND profile_id=?",
        params![id, profile_id],
        row_to_record,
    ).optional()?.ok_or_else(|| MemoryError::NotFound(request_id.into()))?;
    Ok(Some(record))
}

impl Store {
    /// Bound this connection's lock wait without changing worker connections.
    pub(crate) fn set_busy_timeout(&mut self, millis: u64) -> Result<(), MemoryError> {
        self.connection
            .busy_timeout(std::time::Duration::from_millis(millis))?;
        Ok(())
    }

    /// Cache refreshes are optional work: they must never queue behind a
    /// foreground writer. Restore the normal timeout even when work fails.
    fn without_lock_wait<T>(
        &mut self,
        operation: impl FnOnce(&mut Self) -> Result<T, MemoryError>,
    ) -> Result<T, MemoryError> {
        let previous: u64 = self
            .connection
            .query_row("PRAGMA busy_timeout", [], |r| r.get(0))?;
        self.connection.busy_timeout(std::time::Duration::ZERO)?;
        let result = operation(self);
        self.connection
            .busy_timeout(std::time::Duration::from_millis(previous))?;
        result
    }

    pub(crate) fn try_read_snapshot<T>(
        &mut self,
        read: impl FnOnce(&Self) -> Result<T, MemoryError>,
    ) -> Result<T, MemoryError> {
        self.without_lock_wait(|store| store.read_snapshot(read))
    }

    pub(crate) fn try_publish_at_epoch<T>(
        &mut self,
        expected_epoch: u64,
        publish: impl FnOnce() -> Result<T, rusqlite::Error>,
    ) -> Result<T, MemoryError> {
        self.without_lock_wait(|store| store.publish_at_epoch(expected_epoch, publish))
    }

    /// Collect a bounded multi-query view without mixing record generations.
    pub(crate) fn read_snapshot<T>(
        &self,
        read: impl FnOnce(&Self) -> Result<T, MemoryError>,
    ) -> Result<T, MemoryError> {
        let tx = self.connection.unchecked_transaction()?;
        let result = read(self)?;
        tx.commit()?;
        Ok(result)
    }

    pub(crate) fn revision_page(
        &self,
        scope: &Scope,
        after: u64,
        through: Option<u64>,
        limit: usize,
        active_only: bool,
    ) -> Result<RevisionPage, MemoryError> {
        scope.validate()?;
        let maximum: u64 = self.connection.query_row(
            "SELECT COALESCE((SELECT seq FROM sqlite_sequence WHERE name='memory_revisions'),0)",
            [],
            |r| r.get(0),
        )?;
        let through = through.unwrap_or(maximum);
        if after > through || through > maximum || limit == 0 {
            return Err(MemoryError::Invalid("invalid memory revision page".into()));
        }
        let cap = limit.min(64);
        let mut query = self.connection.prepare("SELECT m.id,m.profile_id,m.kind,m.origin,m.project,m.provider,m.conversation,m.body,m.provenance,m.timestamp,m.supersedes,m.dependencies,m.decision_state,m.protected_policy,m.node,v.revision FROM memory_revisions v JOIN memory_records m ON m.id=v.record_id WHERE m.profile_id=? AND v.revision>? AND v.revision<=? AND (m.project IS NULL OR m.project=?) AND (m.provider IS NULL OR m.provider=?) AND (m.conversation IS NULL OR m.conversation=?) AND (m.node IS NULL OR m.node=?) AND (?=0 OR (m.kind!='\"Draft\"' AND (m.decision_state IS NULL OR m.decision_state!='\"Superseded\"') AND NOT EXISTS(SELECT 1 FROM memory_records n WHERE n.profile_id=m.profile_id AND n.supersedes=m.id))) ORDER BY v.revision ASC LIMIT ?")?;
        let records = query
            .query_map(
                params![
                    self.profile_id,
                    after,
                    through,
                    scope.project,
                    scope.provider,
                    scope.conversation,
                    scope.node,
                    active_only,
                    cap + 1
                ],
                |row| {
                    Ok(RevisionRecord {
                        revision: row.get(15)?,
                        record: row_to_record(row)?,
                    })
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(bound_revision_page(records, cap, through))
    }

    /// Open (or create) a private assistant database at an explicit path.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, MemoryError> {
        let path = path.as_ref().to_path_buf();
        crate::assistant_storage::database(&path)?;
        let mut connection = Connection::open(&path)?;
        connection.busy_timeout(std::time::Duration::from_millis(500))?;
        connection.execute_batch("PRAGMA foreign_keys=ON; PRAGMA journal_mode=WAL;")?;
        create_memory_schema(&connection)?;
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let profile_id = migrate_memory_schema(&transaction)?;
        ensure_memory_search_index(&transaction)?;
        transaction.commit()?;
        Ok(Self {
            path,
            connection,
            profile_id,
        })
    }

    /// Attach an existing authority without ever creating a replacement file.
    /// Verify identity under the migration transaction before any schema write.
    pub(crate) fn open_existing(
        path: impl AsRef<Path>,
        expected_profile: &str,
    ) -> Result<Self, MemoryError> {
        let path = path.as_ref().to_path_buf();
        crate::assistant_storage::existing_database(&path)?;
        let mut connection =
            Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        connection.busy_timeout(std::time::Duration::from_millis(500))?;
        connection.execute_batch("PRAGMA foreign_keys=ON;")?;
        let transaction =
            connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let actual: String = transaction.query_row(
            "SELECT value FROM memory_meta WHERE key='profile_id'",
            [],
            |row| row.get(0),
        )?;
        if actual != expected_profile {
            return Err(MemoryError::Invalid(
                "assistant profile changed; existing-authority attachment refused".into(),
            ));
        }
        create_memory_schema(&transaction)?;
        let profile_id = migrate_memory_schema(&transaction)?;
        ensure_memory_search_index(&transaction)?;
        transaction.commit()?;
        Ok(Self {
            path,
            connection,
            profile_id,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn profile_id(&self) -> &str {
        &self.profile_id
    }

    pub fn append(&mut self, input: NewRecord) -> Result<Record, MemoryError> {
        let id = Uuid::new_v4().to_string();
        let tx = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let record = append_in_tx(&tx, &self.profile_id, id, input)?;
        tx.commit()?;
        Ok(record)
    }

    /// Fence worker output against forgetting in the same SQLite transaction.
    pub fn append_at_epoch(
        &mut self,
        input: NewRecord,
        expected_epoch: u64,
    ) -> Result<Record, MemoryError> {
        let tx = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let epoch: Option<String> = tx
            .query_row(
                "SELECT value FROM memory_meta WHERE key='forget_epoch'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        let epoch = epoch
            .map(|value| {
                value
                    .parse::<u64>()
                    .map_err(|_| MemoryError::Invalid("invalid forget epoch".into()))
            })
            .transpose()?
            .unwrap_or(0);
        if epoch != expected_epoch {
            return Err(MemoryError::Invalid(
                "memory changed through forgetting; worker output discarded".into(),
            ));
        }
        let record = append_in_tx(&tx, &self.profile_id, Uuid::new_v4().to_string(), input)?;
        tx.commit()?;
        Ok(record)
    }

    /// Serialize a short derived-journal write against forgetting. Never run
    /// provider calls or other blocking external work inside this callback.
    /// A forget either precedes this fence (and rejects it), or follows the
    /// committed journal write (and its retention pass removes that copy).
    pub(crate) fn publish_at_epoch<T>(
        &mut self,
        expected_epoch: u64,
        publish: impl FnOnce() -> Result<T, rusqlite::Error>,
    ) -> Result<T, MemoryError> {
        let tx = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let epoch: Option<String> = tx
            .query_row(
                "SELECT value FROM memory_meta WHERE key='forget_epoch'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        let epoch = epoch
            .map(|value| {
                value
                    .parse::<u64>()
                    .map_err(|_| MemoryError::Invalid("invalid forget epoch".into()))
            })
            .transpose()?
            .unwrap_or(0);
        if epoch != expected_epoch {
            return Err(MemoryError::Invalid(
                "memory changed through forgetting; derived publication discarded".into(),
            ));
        }
        let result = publish()?;
        tx.commit()?;
        Ok(result)
    }

    /// Serialize a bounded provider dispatch against forgetting. A forget that
    /// wins the writer lock invalidates the callback; one arriving afterward
    /// cannot commit until the handshake ends. Callers must bound that handshake
    /// (the native transport uses 10 seconds), never wait for model generation,
    /// and must preserve uncertain-delivery accounting themselves.
    pub(crate) fn dispatch_at_epoch<T>(
        &mut self,
        expected_epoch: u64,
        dispatch: impl FnOnce() -> T,
    ) -> Result<T, MemoryError> {
        self.dispatch_at_epoch_checked(expected_epoch, |_| dispatch())
    }

    pub(crate) fn dispatch_at_epoch_checked<T>(
        &mut self,
        expected_epoch: u64,
        dispatch: impl FnOnce(&rusqlite::Transaction<'_>) -> T,
    ) -> Result<T, MemoryError> {
        let tx = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let stored: Option<String> = tx
            .query_row(
                "SELECT value FROM memory_meta WHERE key='forget_epoch'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        let epoch = stored
            .map(|value| {
                value
                    .parse::<u64>()
                    .map_err(|_| MemoryError::Invalid("invalid forget epoch".into()))
            })
            .transpose()?
            .unwrap_or(0);
        if epoch != expected_epoch {
            return Err(MemoryError::Invalid(
                "memory was forgotten before provider dispatch; context requires recovery".into(),
            ));
        }
        let result = dispatch(&tx);
        // No writes to commit after the external effect. Drop releases the
        // read-only writer reservation even on a callback error or unwind.
        drop(tx);
        Ok(result)
    }

    fn validate_input(input: &NewRecord) -> Result<(), MemoryError> {
        input.scope.validate()?;
        if input.dependencies.len() > 64
            || input
                .dependencies
                .iter()
                .any(|id| Uuid::parse_str(id).is_err())
            || input
                .supersedes
                .as_ref()
                .is_some_and(|id| Uuid::parse_str(id).is_err())
        {
            return Err(MemoryError::Invalid(
                "bounded exact dependencies required".into(),
            ));
        }
        if input.body.len() > MAX_RECORD_BYTES || input.provenance.len() > MAX_RECORD_BYTES {
            return Err(MemoryError::Invalid("record exceeds bounded size".into()));
        }
        if input.origin != Origin::Human
            && (input.kind == RecordKind::UserInstruction
                || input.protected_policy
                || input.supersedes.is_some()
                || input
                    .decision_state
                    .is_some_and(|state| state != DecisionState::Proposed))
        {
            return Err(MemoryError::WorkerCannotAssumeUserAuthority);
        }
        if input.kind == RecordKind::Correction && input.dependencies.is_empty() {
            return Err(MemoryError::Invalid(
                "corrections must name a dependency".into(),
            ));
        }
        Ok(())
    }

    /// Crash-retry safe append keyed by a caller-owned request id.
    pub fn append_idempotent(
        &mut self,
        request_id: &str,
        input: NewRecord,
    ) -> Result<Record, MemoryError> {
        self.append_idempotent_checked(request_id, input, false)
    }

    /// Trusted conversation adapter: preserve raw human words and, for the
    /// narrow recognized presentation grammar, atomically supersede the prior
    /// preference in exactly this scope. No model text reaches this boundary.
    pub(crate) fn append_conversation_input(
        &mut self,
        request_id: &str,
        mut input: NewRecord,
        preference: Option<crate::assistant_preferences::BriefingPreference>,
        expected_epoch: u64,
    ) -> Result<Record, MemoryError> {
        validate_request_id(request_id)?;
        validate_conversation_input(&input, preference)?;
        // Keep the historical raw-input receipt hash: retrying an older input
        // must return its existing classification, never reinterpret it.
        let hash = format!("{:x}", Sha256::digest(serde_json::to_vec(&input)?));
        let tx = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if read_forget_epoch(&tx)? != expected_epoch {
            return Err(MemoryError::Invalid(
                "memory changed through forgetting; input not saved".into(),
            ));
        }
        if let Some(record) = existing_idempotent_record(&tx, request_id, &hash, &self.profile_id)?
        {
            tx.commit()?;
            return Ok(record);
        }
        if let Some(preference) = preference {
            apply_presentation_revision(&tx, &self.profile_id, &mut input, preference)?;
        }
        let record = append_in_tx(&tx, &self.profile_id, Uuid::new_v4().to_string(), input)?;
        tx.execute("INSERT INTO memory_receipts(request_id,payload_hash,record_id,timestamp) VALUES(?,?,?,?)", params![request_id,hash,record.id,record.timestamp])?;
        tx.commit()?;
        Ok(record)
    }

    /// Append one immutable human revision only while its exact predecessor is
    /// current. The check shares the write transaction with the append, so two
    /// clients cannot fork the active decision. An identical retry still returns
    /// its original receipt, even after a later revision.
    pub fn append_revision_idempotent(
        &mut self,
        request_id: &str,
        input: NewRecord,
    ) -> Result<Record, MemoryError> {
        if input.origin != Origin::Human {
            return Err(MemoryError::WorkerCannotAssumeUserAuthority);
        }
        if !input
            .supersedes
            .as_ref()
            .is_some_and(|id| input.dependencies.contains(id))
        {
            return Err(MemoryError::Invalid(
                "revision must retain its exact predecessor as a dependency".into(),
            ));
        }
        self.append_idempotent_checked(request_id, input, true)
    }

    fn append_idempotent_checked(
        &mut self,
        request_id: &str,
        input: NewRecord,
        require_active_predecessor: bool,
    ) -> Result<Record, MemoryError> {
        validate_request_id(request_id)?;
        Self::validate_input(&input)?;
        let hash = format!("{:x}", Sha256::digest(serde_json::to_vec(&input)?));
        let tx = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if let Some(record) = existing_idempotent_record(&tx, request_id, &hash, &self.profile_id)?
        {
            tx.commit()?;
            return Ok(record);
        }
        if require_active_predecessor {
            let superseded: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM memory_records WHERE profile_id=? AND supersedes=?)",
                params![self.profile_id, input.supersedes],
                |row| row.get(0),
            )?;
            if superseded {
                return Err(MemoryError::Invalid(
                    "record has a newer revision; inspect and use its current record ID".into(),
                ));
            }
        }
        let id = Uuid::new_v4().to_string();
        let record = append_in_tx(&tx, &self.profile_id, id, input)?;
        tx.execute("INSERT INTO memory_receipts(request_id,payload_hash,record_id,timestamp) VALUES(?,?,?,?)", params![request_id, hash, record.id, record.timestamp])?;
        tx.commit()?;
        Ok(record)
    }
}

fn validate_conversation_input(
    input: &NewRecord,
    preference: Option<crate::assistant_preferences::BriefingPreference>,
) -> Result<(), MemoryError> {
    Store::validate_input(input)?;
    if input.origin != Origin::Human
        || input.kind != RecordKind::Finding
        || input.protected_policy
        || input.supersedes.is_some()
        || !input.dependencies.is_empty()
        || input.decision_state.is_some()
    {
        return Err(MemoryError::Invalid(
            "expected raw human conversation input".into(),
        ));
    }
    if preference != crate::assistant_preferences::recognize(&input.body) {
        return Err(MemoryError::Invalid(
            "presentation preference must match exact human words".into(),
        ));
    }
    Ok(())
}

fn apply_presentation_revision(
    tx: &Transaction<'_>,
    profile_id: &str,
    input: &mut NewRecord,
    preference: crate::assistant_preferences::BriefingPreference,
) -> Result<(), MemoryError> {
    let previous = tx.query_row(
        "SELECT id FROM memory_records m WHERE profile_id=? AND project IS ? AND provider IS ? AND conversation IS ? AND node IS ? AND kind='\"UserInstruction\"' AND origin='\"Human\"' AND protected_policy=0 AND CASE WHEN json_valid(provenance) THEN json_extract(provenance,'$.presentation_preference.key') END=? AND NOT EXISTS(SELECT 1 FROM memory_records n WHERE n.supersedes=m.id AND n.profile_id=m.profile_id) ORDER BY timestamp DESC,id DESC LIMIT 1",
        params![profile_id,input.scope.project,input.scope.provider,input.scope.conversation,input.scope.node,crate::assistant_preferences::KEY],
        |row| row.get::<_, String>(0),
    ).optional()?;
    input.kind = RecordKind::UserInstruction;
    let mut provenance: serde_json::Value = serde_json::from_str(&input.provenance)?;
    let fields = provenance
        .as_object_mut()
        .ok_or_else(|| MemoryError::Invalid("conversation provenance must be an object".into()))?;
    fields.insert("authority".into(), "presentation_preference_only".into());
    fields.insert(
        "presentation_preference".into(),
        serde_json::json!({"key":crate::assistant_preferences::KEY,"bullets":preference.bullets}),
    );
    input.provenance = provenance.to_string();
    input.dependencies = previous.iter().cloned().collect();
    input.supersedes = previous;
    Ok(())
}

pub(crate) fn append_in_tx(
    tx: &Transaction<'_>,
    profile_id: &str,
    id: String,
    input: NewRecord,
) -> Result<Record, MemoryError> {
    Store::validate_input(&input)?;
    validate_dependency_scopes(tx, &input)?;
    insert_record(tx, profile_id, id, input)
}

fn validate_dependency_scopes(tx: &Transaction<'_>, input: &NewRecord) -> Result<(), MemoryError> {
    for dependency in &input.dependencies {
        let existing =
            load_scope(tx, dependency)?.ok_or_else(|| MemoryError::NotFound(dependency.clone()))?;
        if !existing.permits(&input.scope) {
            return Err(MemoryError::Invalid(
                "derived records must retain every dependency scope".into(),
            ));
        }
    }
    if let Some(parent) = &input.supersedes {
        let existing =
            load_scope(tx, parent)?.ok_or_else(|| MemoryError::NotFound(parent.clone()))?;
        if existing != input.scope {
            return Err(MemoryError::Invalid(
                "supersession cannot broaden scope".into(),
            ));
        }
    }
    Ok(())
}

fn insert_record(
    tx: &Transaction<'_>,
    profile_id: &str,
    id: String,
    input: NewRecord,
) -> Result<Record, MemoryError> {
    let deps = serde_json::to_string(&input.dependencies)?;
    let kind = serde_json::to_string(&input.kind)?;
    let origin = serde_json::to_string(&input.origin)?;
    let state = input
        .decision_state
        .map(|s| serde_json::to_string(&s))
        .transpose()?;
    tx.execute("INSERT INTO memory_records (id,profile_id,kind,origin,project,provider,conversation,body,provenance,timestamp,supersedes,dependencies,decision_state,protected_policy,node) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)", params![id, profile_id, kind, origin, input.scope.project, input.scope.provider, input.scope.conversation, input.body, input.provenance, input.timestamp, input.supersedes, deps, state, input.protected_policy as i64,input.scope.node])?;
    Ok(Record {
        id,
        profile_id: profile_id.into(),
        kind: input.kind,
        origin: input.origin,
        scope: input.scope,
        body: input.body,
        provenance: input.provenance,
        timestamp: input.timestamp,
        supersedes: input.supersedes,
        dependencies: input.dependencies,
        decision_state: input.decision_state,
        protected_policy: input.protected_policy,
    })
}

impl Store {
    /// Trusted user-input adapter only; never expose this method to a worker.
    pub fn append_user(&mut self, input: NewRecord) -> Result<Record, MemoryError> {
        if input.origin != Origin::Human {
            return Err(MemoryError::WorkerCannotAssumeUserAuthority);
        }
        self.append(input)
    }

    pub fn retrieve(&self, scope: &Scope, limit: usize) -> Result<Vec<Record>, MemoryError> {
        self.select(scope, limit, false)
    }
    /// A bounded model working set: explicit instructions/corrections stay
    /// ahead of activity, unsent drafts and superseded records are excluded.
    pub fn working_set(&self, scope: &Scope, limit: usize) -> Result<Vec<Record>, MemoryError> {
        self.select(scope, limit, true)
    }

    /// Search the full eligible lexical index using SQLite FTS5's native BM25
    /// ranking.  Scope and active-record predicates are applied in SQL before
    /// the result limit; this must not become a recent-record shortlist.
    pub fn search_bm25(
        &self,
        scope: &Scope,
        query: &str,
        limit: usize,
    ) -> Result<Vec<Record>, MemoryError> {
        self.search_bm25_with_budget(scope, query, limit, MAX_RECORD_BYTES, &Default::default())
    }

    /// Internal bounded variant used when a caller has a smaller prompt
    /// budget.  The SQL candidate window remains bounded but larger than the
    /// requested output so an oversized high-ranked record cannot starve a
    /// smaller eligible match.
    pub(crate) fn search_bm25_with_budget(
        &self,
        scope: &Scope,
        query: &str,
        limit: usize,
        max_bytes: usize,
        excluded: &std::collections::HashSet<String>,
    ) -> Result<Vec<Record>, MemoryError> {
        self.search_bm25_filtered(scope, query, limit, max_bytes, excluded, false)
    }

    /// Filter standing guidance in SQL before the candidate limit. Otherwise
    /// hundreds of matching findings could hide an older applicable instruction.
    pub(crate) fn search_standing_bm25_with_budget(
        &self,
        scope: &Scope,
        query: &str,
        limit: usize,
        max_bytes: usize,
        excluded: &std::collections::HashSet<String>,
    ) -> Result<Vec<Record>, MemoryError> {
        self.search_bm25_filtered(scope, query, limit, max_bytes, excluded, true)
    }

    fn search_bm25_filtered(
        &self,
        scope: &Scope,
        query: &str,
        limit: usize,
        max_bytes: usize,
        excluded: &std::collections::HashSet<String>,
        standing_only: bool,
    ) -> Result<Vec<Record>, MemoryError> {
        scope.validate()?;
        let cap = limit.min(MAX_RETRIEVAL);
        if cap == 0 || max_bytes == 0 {
            return Ok(Vec::new());
        }
        let match_query = lexical_match_query(query);
        if match_query.is_empty() {
            return Ok(Vec::new());
        }
        let sql = r#"SELECT m.id,m.profile_id,m.kind,m.origin,m.project,m.provider,m.conversation,m.body,m.provenance,m.timestamp,m.supersedes,m.dependencies,m.decision_state,m.protected_policy,m.node
                    FROM memory_fts f
                    JOIN memory_records m ON m.rowid=f.rowid
                    WHERE memory_fts MATCH ?
                      AND m.profile_id=?
                      AND (m.project IS NULL OR m.project=?)
                      AND (m.provider IS NULL OR m.provider=?)
                      AND (m.conversation IS NULL OR m.conversation=?)
                      AND (m.node IS NULL OR m.node=?)
                      AND m.kind != '"Draft"'
                      AND (?=0 OR m.protected_policy=1 OR m.kind IN ('"UserInstruction"','"Correction"'))
                      AND (m.decision_state IS NULL OR m.decision_state != '"Superseded"')
                      AND NOT EXISTS(
                          SELECT 1 FROM memory_records n
                          WHERE n.supersedes=m.id AND n.profile_id=m.profile_id
                      )
                    ORDER BY bm25(memory_fts) ASC,m.timestamp DESC,m.id DESC
                    LIMIT ?"#;
        let mut stmt = self.connection.prepare(sql)?;
        let rows = stmt.query_map(
            params![
                match_query,
                self.profile_id,
                scope.project,
                scope.provider,
                scope.conversation,
                scope.node,
                standing_only,
                MAX_RETRIEVAL as i64,
            ],
            row_to_record,
        )?;
        let mut result = Vec::new();
        let mut bytes = 0usize;
        for row in rows {
            let record = row?;
            if excluded.contains(&record.id) {
                continue;
            }
            let cost = serde_json::to_vec(&record)?.len();
            if cost > max_bytes || bytes.saturating_add(cost) > max_bytes {
                continue;
            }
            bytes += cost;
            result.push(record);
            if result.len() >= cap {
                break;
            }
        }
        Ok(result)
    }

    fn select(
        &self,
        scope: &Scope,
        limit: usize,
        active: bool,
    ) -> Result<Vec<Record>, MemoryError> {
        scope.validate()?;
        let cap = limit.min(MAX_RETRIEVAL);
        let sql = if active {
            r#"SELECT id,profile_id,kind,origin,project,provider,conversation,body,provenance,timestamp,supersedes,dependencies,decision_state,protected_policy,node FROM memory_records m WHERE profile_id=? AND (project IS NULL OR project=?) AND (provider IS NULL OR provider=?) AND (conversation IS NULL OR conversation=?) AND (node IS NULL OR node=?) AND kind != '"Draft"' AND (decision_state IS NULL OR decision_state != '"Superseded"') AND NOT EXISTS(SELECT 1 FROM memory_records n WHERE n.supersedes=m.id AND n.profile_id=m.profile_id) ORDER BY protected_policy DESC,CASE WHEN kind IN ('"UserInstruction"','"Correction"') THEN 0 WHEN kind='"Decision"' THEN 1 ELSE 2 END,timestamp DESC,id DESC LIMIT ?"#
        } else {
            "SELECT id,profile_id,kind,origin,project,provider,conversation,body,provenance,timestamp,supersedes,dependencies,decision_state,protected_policy,node FROM memory_records WHERE profile_id=? AND (project IS NULL OR project=?) AND (provider IS NULL OR provider=?) AND (conversation IS NULL OR conversation=?) AND (node IS NULL OR node=?) ORDER BY timestamp DESC, id DESC LIMIT ?"
        };
        let mut stmt = self.connection.prepare(sql)?;
        let rows = stmt.query_map(
            params![
                self.profile_id,
                scope.project,
                scope.provider,
                scope.conversation,
                scope.node,
                cap as i64
            ],
            row_to_record,
        )?;
        let mut result = Vec::new();
        let mut bytes = 0usize;
        for row in rows {
            let record = row?;
            let cost = record.body.len().saturating_add(record.provenance.len());
            if bytes.saturating_add(cost) > MAX_RECORD_BYTES {
                continue;
            }
            bytes += cost;
            result.push(record);
        }
        Ok(result)
    }

    /// Exact dependency lookup for trusted coordinators; never a model-owned query.
    pub fn get(&self, id: &str) -> Result<Option<Record>, MemoryError> {
        Ok(self.connection.query_row("SELECT id,profile_id,kind,origin,project,provider,conversation,body,provenance,timestamp,supersedes,dependencies,decision_state,protected_policy,node FROM memory_records WHERE profile_id=? AND id=?", params![self.profile_id,id], row_to_record).optional()?)
    }

    /// A revision is immutable evidence identity, not model-authored freshness.
    pub(crate) fn source_version(&self, id: &str) -> Result<Option<u64>, MemoryError> {
        Ok(self.connection.query_row(
            "SELECT v.revision FROM memory_revisions v JOIN memory_records m ON m.id=v.record_id WHERE m.id=? AND m.profile_id=?",
            params![id, self.profile_id], |row| row.get(0),
        ).optional()?)
    }

    pub fn lineage(&self, id: &str) -> Result<Option<RecordLineage>, MemoryError> {
        Ok(self.connection.query_row(
            "SELECT id,kind,origin,node,project,provider,conversation,supersedes,substr(provenance,1,16384) FROM memory_records WHERE profile_id=? AND id=?",
            params![self.profile_id,id], |row| Ok(RecordLineage {
                id: row.get(0)?, kind: decode_json_column(row,1)?, origin: decode_json_column(row,2)?,
                scope: Scope { node: row.get(3)?, project: row.get(4)?, provider: row.get(5)?, conversation: row.get(6)? },
                supersedes: row.get(7)?, provenance: row.get(8)?,
            })
        ).optional()?)
    }

    /// Active typed records have their own quota independent of newer activity.
    /// Fetch one extra row to disclose overflow without loading/counting the
    /// whole archive. Scope filtering and supersession precede the bound.
    pub fn active_by_kind(
        &self,
        scope: &Scope,
        kind: RecordKind,
        since: Option<i64>,
        limit: usize,
    ) -> Result<RecordPage, MemoryError> {
        scope.validate()?;
        let cap = limit.min(MAX_RETRIEVAL);
        let mut stmt = self.connection.prepare(
            "SELECT id,profile_id,kind,origin,project,provider,conversation,body,provenance,timestamp,supersedes,dependencies,decision_state,protected_policy,node FROM memory_records m WHERE profile_id=? AND kind=? AND (project IS NULL OR project=?) AND (provider IS NULL OR provider=?) AND (conversation IS NULL OR conversation=?) AND (node IS NULL OR node=?) AND (? IS NULL OR timestamp>=?) AND (decision_state IS NULL OR decision_state != '\"Superseded\"') AND NOT EXISTS(SELECT 1 FROM memory_records n WHERE n.supersedes=m.id AND n.profile_id=m.profile_id) ORDER BY timestamp DESC,id DESC LIMIT ?"
        )?;
        let rows = stmt.query_map(
            params![
                self.profile_id,
                serde_json::to_string(&kind)?,
                scope.project,
                scope.provider,
                scope.conversation,
                scope.node,
                since,
                since,
                cap + 1
            ],
            row_to_record,
        )?;
        let mut page = RecordPage {
            records: vec![],
            limited: false,
            byte_limited: false,
        };
        let mut bytes = 0usize;
        for row in rows {
            let record = row?;
            let cost = record.body.len().saturating_add(record.provenance.len());
            if bytes.saturating_add(cost) > MAX_RECORD_BYTES {
                page.byte_limited = true;
                page.limited = true;
                continue;
            }
            if page.records.len() == cap {
                page.limited = true;
                continue;
            }
            bytes += cost;
            page.records.push(record);
        }
        Ok(page)
    }

    /// Exact active lookup; historical corrections remain retrievable with
    /// `get`, but cannot initiate a new experiment after being superseded.
    pub fn get_active(&self, id: &str) -> Result<Option<Record>, MemoryError> {
        let superseded: bool = self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM memory_records WHERE profile_id=? AND supersedes=?)",
            params![self.profile_id, id],
            |row| row.get(0),
        )?;
        if superseded { Ok(None) } else { self.get(id) }
    }

    /// A monotone fence for cached prompts and provider context. A forget must
    /// invalidate every prior context even if a caller omitted provenance IDs.
    pub fn forget_epoch(&self) -> Result<u64, MemoryError> {
        let value: Option<String> = self
            .connection
            .query_row(
                "SELECT value FROM memory_meta WHERE key='forget_epoch'",
                [],
                |row| row.get(0),
            )
            .optional()?;
        value
            .map(|v| {
                v.parse::<u64>()
                    .map_err(|_| MemoryError::Invalid("invalid forgetting generation".into()))
            })
            .unwrap_or(Ok(0))
    }

    pub fn recent(&self, scope: &Scope, limit: usize) -> Result<Vec<Record>, MemoryError> {
        self.retrieve(scope, limit)
    }

    /// Delete a record and every record that depends on it, transitively.
    /// Recognized presentation preferences include their same-key predecessor
    /// chain, so forgetting a correction does not revive the obsolete setting.
    pub fn forget(&mut self, id: &str) -> Result<usize, MemoryError> {
        let tx = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let epoch = next_forget_epoch(&tx)?;
        if tx
            .query_row(
                "SELECT 1 FROM memory_records WHERE id=? AND profile_id=?",
                params![id, self.profile_id],
                |_| Ok(()),
            )
            .optional()?
            .is_none()
        {
            return Err(MemoryError::NotFound(id.into()));
        }
        let mut pending = vec![presentation_preference_root(&tx, &self.profile_id, id)?];
        let mut removed = 0;
        while let Some(target) = pending.pop() {
            let mut stmt = tx.prepare("SELECT id FROM memory_records WHERE profile_id=? AND (supersedes=? OR dependencies LIKE '%' || ? || '%')")?;
            let children: Vec<String> = stmt
                .query_map(params![self.profile_id, target, target], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            pending.extend(children);
            removed += tx.execute(
                "DELETE FROM memory_records WHERE id=? AND profile_id=?",
                params![target, self.profile_id],
            )?;
        }
        tx.execute("INSERT INTO memory_meta(key,value) VALUES('forget_epoch',?) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [epoch.to_string()])?;
        tx.commit()?;
        Ok(removed)
    }

    /// Revoke model-derived memory for an exact scope when an external input
    /// grant is withdrawn. Older findings may not carry source-grant lineage,
    /// so retain no worker assertion from that scope (or its descendants).
    /// The same transaction advances the context fence even with zero matches:
    /// an in-flight worker must not append a late finding after revocation.
    pub(crate) fn forget_worker_scope(&mut self, scope: &Scope) -> Result<usize, MemoryError> {
        if scope
            .project
            .as_ref()
            .is_none_or(|name| name.trim().is_empty())
        {
            return Err(MemoryError::Invalid(
                "exact project scope is required for derived-memory revocation".into(),
            ));
        }
        let tx = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let epoch = next_forget_epoch(&tx)?;
        let mut pending = {
            let mut query = tx.prepare("SELECT id FROM memory_records WHERE profile_id=? AND origin='\"Worker\"' AND project IS ? AND provider IS ? AND conversation IS ? AND node IS ?")?;
            query
                .query_map(
                    params![
                        self.profile_id,
                        scope.project,
                        scope.provider,
                        scope.conversation,
                        scope.node
                    ],
                    |row| row.get::<_, String>(0),
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut removed = 0;
        while let Some(id) = pending.pop() {
            let mut query = tx.prepare("SELECT id FROM memory_records WHERE profile_id=? AND (supersedes=? OR EXISTS(SELECT 1 FROM json_each(memory_records.dependencies) WHERE value=?))")?;
            let children = query
                .query_map(params![self.profile_id, id, id], |row| {
                    row.get::<_, String>(0)
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            pending.extend(children);
            removed += tx.execute(
                "DELETE FROM memory_records WHERE profile_id=? AND id=?",
                params![self.profile_id, id],
            )?;
        }
        tx.execute("INSERT INTO memory_meta(key,value) VALUES('forget_epoch',?) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [epoch.to_string()])?;
        tx.commit()?;
        Ok(removed)
    }
}

fn bound_revision_page(records: Vec<RevisionRecord>, cap: usize, through: u64) -> RevisionPage {
    let mut page = RevisionPage {
        records: Vec::new(),
        through,
        next: through,
        has_more: false,
    };
    let mut bytes = 0usize;
    for entry in records {
        let cost = entry
            .record
            .body
            .len()
            .saturating_add(entry.record.provenance.len());
        if page.records.len() == cap || bytes.saturating_add(cost) > 2 * MAX_RECORD_BYTES {
            page.has_more = true;
            page.next = page.records.last().map_or(0, |r| r.revision);
            break;
        }
        bytes += cost;
        page.records.push(entry);
    }
    page
}

fn next_forget_epoch(connection: &Connection) -> Result<u64, MemoryError> {
    read_forget_epoch(connection)?
        .checked_add(1)
        .ok_or_else(|| MemoryError::Invalid("exhausted forget generation".into()))
}

fn read_forget_epoch(connection: &Connection) -> Result<u64, MemoryError> {
    let old: Option<String> = connection
        .query_row(
            "SELECT value FROM memory_meta WHERE key='forget_epoch'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    old.unwrap_or_else(|| "0".into())
        .parse::<u64>()
        .map_err(|_| MemoryError::Invalid("invalid forget generation".into()))
}

fn presentation_preference_root(
    connection: &Connection,
    profile_id: &str,
    id: &str,
) -> Result<String, MemoryError> {
    // This exception is deliberately limited to our typed presentation key.
    // Generic findings/decisions retain their existing forgetting behavior.
    let mut current = id.to_owned();
    let mut visited = std::collections::HashSet::new();
    while visited.insert(current.clone()) {
        let parent: Option<String> = connection.query_row(
            "SELECT p.id FROM memory_records m JOIN memory_records p ON p.id=m.supersedes AND p.profile_id=m.profile_id AND p.project IS m.project AND p.provider IS m.provider AND p.conversation IS m.conversation AND p.node IS m.node WHERE m.id=? AND m.profile_id=? AND m.kind='\"UserInstruction\"' AND p.kind=m.kind AND m.origin='\"Human\"' AND p.origin=m.origin AND CASE WHEN json_valid(m.provenance) THEN json_extract(m.provenance,'$.presentation_preference.key') END=? AND CASE WHEN json_valid(p.provenance) THEN json_extract(p.provenance,'$.presentation_preference.key') END=?",
            params![current,profile_id,crate::assistant_preferences::KEY,crate::assistant_preferences::KEY],
            |row| row.get(0),
        ).optional()?;
        let Some(parent) = parent else {
            return Ok(current);
        };
        current = parent;
    }
    Err(MemoryError::Invalid("cyclic preference lineage".into()))
}

fn load_scope(tx: &Transaction<'_>, id: &str) -> Result<Option<Scope>, MemoryError> {
    Ok(tx
        .query_row(
            "SELECT project,provider,conversation,node FROM memory_records WHERE id=?",
            [id],
            |r| {
                Ok(Scope {
                    project: r.get(0)?,
                    provider: r.get(1)?,
                    conversation: r.get(2)?,
                    node: r.get(3)?,
                })
            },
        )
        .optional()?)
}

fn lexical_match_query(query: &str) -> String {
    // Scan a UTF-8-safe prefix.  Runtime prompts may be much larger than the
    // index query budget; truncation keeps lookup bounded without rejecting a
    // valid prompt or ever splitting a code point.
    let mut terms = Vec::new();
    let mut term = String::new();
    let flush = |term: &mut String, terms: &mut Vec<String>| {
        if term.is_empty() {
            return;
        }
        if term.len() <= MAX_SEARCH_TERM_BYTES
            && terms.len() < MAX_SEARCH_TERMS
            && !terms.iter().any(|existing| existing == term)
        {
            terms.push(std::mem::take(term));
        } else {
            term.clear();
        }
    };
    let mut scanned: usize = 0;
    for character in query.chars() {
        let character_bytes = character.len_utf8();
        if scanned.saturating_add(character_bytes) > MAX_SEARCH_QUERY_BYTES {
            break;
        }
        scanned += character_bytes;
        if character.is_alphanumeric() || character == '_' {
            term.push(character);
        } else {
            flush(&mut term, &mut terms);
        }
    }
    flush(&mut term, &mut terms);
    // Every token is quoted, so punctuation and FTS operators are inert.  The
    // query itself is still bound as a parameter rather than interpolated SQL.
    terms
        .into_iter()
        .map(|term| format!("\"{}\"", term.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" OR ")
}

fn row_to_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<Record> {
    let kind = decode_json_column(row, 2)?;
    let origin = decode_json_column(row, 3)?;
    let dependencies = decode_json_column(row, 11)?;
    let decision_state = decode_optional_json_column(row, 12)?;
    Ok(Record {
        id: row.get(0)?,
        profile_id: row.get(1)?,
        kind,
        origin,
        scope: row_to_scope(row)?,
        body: row.get(7)?,
        provenance: row.get(8)?,
        timestamp: row.get(9)?,
        supersedes: row.get(10)?,
        dependencies,
        decision_state,
        protected_policy: row.get::<_, i64>(13)? != 0,
    })
}

fn row_to_scope(row: &rusqlite::Row<'_>) -> rusqlite::Result<Scope> {
    Ok(Scope {
        node: row.get(14)?,
        project: row.get(4)?,
        provider: row.get(5)?,
        conversation: row.get(6)?,
    })
}

fn decode_json_column<T: DeserializeOwned>(
    row: &rusqlite::Row<'_>,
    column: usize,
) -> rusqlite::Result<T> {
    let value: String = row.get(column)?;
    serde_json::from_str(&value).map_err(|error| {
        rusqlite::Error::InvalidColumnType(column, error.to_string(), rusqlite::types::Type::Text)
    })
}

fn decode_optional_json_column<T: DeserializeOwned>(
    row: &rusqlite::Row<'_>,
    column: usize,
) -> rusqlite::Result<Option<T>> {
    row.get::<_, Option<String>>(column)?
        .map(|value| {
            serde_json::from_str(&value).map_err(|error| {
                rusqlite::Error::InvalidColumnType(
                    column,
                    error.to_string(),
                    rusqlite::types::Type::Text,
                )
            })
        })
        .transpose()
}

#[cfg(test)]
#[path = "assistant_preference_storage_tests.rs"]
mod preference_storage_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    fn input(kind: RecordKind, origin: Origin, scope: Scope, body: &str) -> NewRecord {
        NewRecord {
            kind,
            origin,
            scope,
            body: body.into(),
            provenance: "test".into(),
            timestamp: 1,
            supersedes: None,
            dependencies: vec![],
            decision_state: None,
            protected_policy: false,
        }
    }
    #[test]
    fn forget_between_recall_and_dispatch_prevents_callback() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("private/memory.sqlite");
        let mut store = Store::open(&path).unwrap();
        let record = store
            .append(input(
                RecordKind::Finding,
                Origin::Human,
                Scope::default(),
                "heliotrope",
            ))
            .unwrap();
        let epoch = store.forget_epoch().unwrap();
        assert_eq!(
            store
                .search_bm25(&Scope::default(), "heliotrope", 1)
                .unwrap()[0]
                .id,
            record.id
        );
        let mut other = Store::open(&path).unwrap();
        other.forget(&record.id).unwrap();
        let mut dispatched = false;
        assert!(
            store
                .dispatch_at_epoch(epoch, || {
                    dispatched = true;
                })
                .is_err()
        );
        assert!(!dispatched);
        // A failed callback releases the writer reservation as well.
        let current = store.forget_epoch().unwrap();
        assert_eq!(
            store
                .dispatch_at_epoch(current, || Err::<(), _>("uncertain"))
                .unwrap(),
            Err("uncertain")
        );
        other
            .append(input(
                RecordKind::Finding,
                Origin::Human,
                Scope::default(),
                "new evidence",
            ))
            .unwrap();
    }
    #[test]
    fn reopens_identity_and_records() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("private/memory.sqlite");
        let mut a = Store::open(&p).unwrap();
        let id = a.profile_id().to_string();
        a.append(input(
            RecordKind::Finding,
            Origin::Worker,
            Scope::default(),
            "x",
        ))
        .unwrap();
        drop(a);
        let b = Store::open(&p).unwrap();
        assert_eq!(id, b.profile_id());
        assert_eq!(b.retrieve(&Scope::default(), 10).unwrap().len(), 1);
    }
    #[test]
    fn scope_exclusion_and_worker_guard() {
        let dir = tempdir().unwrap();
        let mut s = Store::open(dir.path().join("private/m.sqlite")).unwrap();
        let one = Scope {
            project: Some("one".into()),
            ..Default::default()
        };
        s.append(input(RecordKind::Finding, Origin::Worker, one.clone(), "x"))
            .unwrap();
        assert!(
            s.retrieve(
                &Scope {
                    project: Some("two".into()),
                    ..Default::default()
                },
                10
            )
            .unwrap()
            .is_empty()
        );
        let bad = input(RecordKind::UserInstruction, Origin::Worker, one, "no");
        assert!(matches!(
            s.append(bad),
            Err(MemoryError::WorkerCannotAssumeUserAuthority)
        ));
    }
    #[test]
    fn correction_keeps_scope_and_forget_is_transitive() {
        let dir = tempdir().unwrap();
        let mut s = Store::open(dir.path().join("private/m.sqlite")).unwrap();
        let scope = Scope {
            project: Some("p".into()),
            ..Default::default()
        };
        let root = s
            .append(input(
                RecordKind::Finding,
                Origin::Human,
                scope.clone(),
                "old",
            ))
            .unwrap();
        let mut corr = input(RecordKind::Correction, Origin::Human, scope.clone(), "new");
        corr.dependencies = vec![root.id.clone()];
        let child = s.append(corr).unwrap();
        let mut derived = input(RecordKind::Briefing, Origin::System, scope, "brief");
        derived.dependencies = vec![child.id];
        s.append(derived).unwrap();
        assert_eq!(s.forget(&root.id).unwrap(), 3);
        assert!(
            s.retrieve(
                &Scope {
                    project: Some("p".into()),
                    ..Default::default()
                },
                10
            )
            .unwrap()
            .is_empty()
        );
        drop(s);
        let reopened = Store::open(dir.path().join("private/m.sqlite")).unwrap();
        assert!(
            reopened
                .search_bm25(
                    &Scope {
                        project: Some("p".into()),
                        ..Default::default()
                    },
                    "brief",
                    10,
                )
                .unwrap()
                .is_empty()
        );
    }
    #[test]
    fn idempotency_mismatch_and_forget_receipt() {
        let dir = tempdir().unwrap();
        let mut s = Store::open(dir.path().join("private/m.sqlite")).unwrap();
        let x = input(RecordKind::Finding, Origin::Human, Scope::default(), "x");
        let r = s.append_idempotent("request-1", x.clone()).unwrap();
        assert_eq!(
            s.append_idempotent("request-1", x.clone()).unwrap().id,
            r.id
        );
        let mut changed = x.clone();
        changed.body = "different".into();
        assert!(matches!(
            s.append_idempotent("request-1", changed),
            Err(MemoryError::Invalid(_))
        ));
        s.forget(&r.id).unwrap();
        assert!(matches!(
            s.append_idempotent("request-1", x),
            Err(MemoryError::NotFound(_))
        ));
    }
    #[test]
    fn derived_scope_cannot_launder_project() {
        let dir = tempdir().unwrap();
        let mut s = Store::open(dir.path().join("private/m.sqlite")).unwrap();
        let root = s
            .append(input(
                RecordKind::Finding,
                Origin::Human,
                Scope {
                    project: Some("secret".into()),
                    ..Default::default()
                },
                "x",
            ))
            .unwrap();
        let mut derived = input(
            RecordKind::Briefing,
            Origin::System,
            Scope {
                project: Some("other".into()),
                ..Default::default()
            },
            "y",
        );
        derived.dependencies = vec![root.id];
        assert!(matches!(s.append(derived), Err(MemoryError::Invalid(_))));
    }
    #[test]
    fn forgotten_epoch_prevents_late_journal_copy() {
        let dir = tempdir().unwrap();
        let mut memory = Store::open(dir.path().join("private/m.sqlite")).unwrap();
        let source = memory
            .append(input(
                RecordKind::Finding,
                Origin::Human,
                Scope::default(),
                "secret",
            ))
            .unwrap();
        let epoch = memory.forget_epoch().unwrap();
        let mut derived = input(
            RecordKind::Finding,
            Origin::Worker,
            Scope::default(),
            "derived secret",
        );
        derived.dependencies = vec![source.id.clone()];
        memory.append_at_epoch(derived, epoch).unwrap();
        // Reproduce the gap after memory publication but before journal copy.
        Store::open(memory.path())
            .unwrap()
            .forget(&source.id)
            .unwrap();
        let journal = Connection::open_in_memory().unwrap();
        journal
            .execute_batch("CREATE TABLE replies(body TEXT)")
            .unwrap();
        assert!(
            memory
                .publish_at_epoch(epoch, || journal
                    .execute("INSERT INTO replies VALUES('derived secret')", []))
                .is_err()
        );
        assert_eq!(
            journal
                .query_row::<i64, _, _>("SELECT COUNT(*) FROM replies", [], |r| r.get(0))
                .unwrap(),
            0
        );
    }

    #[test]
    fn publication_holds_forget_writer_fence() {
        let dir = tempdir().unwrap();
        let mut memory = Store::open(dir.path().join("private/m.sqlite")).unwrap();
        let other = Connection::open(memory.path()).unwrap();
        other.busy_timeout(std::time::Duration::ZERO).unwrap();
        memory
            .publish_at_epoch(0, || {
                assert!(other.execute_batch("BEGIN IMMEDIATE").is_err());
                Ok(())
            })
            .unwrap();
        other.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
    }

    #[test]
    fn optional_publication_restores_owner_timeout_and_does_not_change_workers() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("private/m.sqlite");
        let mut owner = Store::open(&path).unwrap();
        let worker = Store::open(&path).unwrap();
        owner.set_busy_timeout(25).unwrap();
        let blocker = Connection::open(&path).unwrap();
        blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
        let mut called = false;
        assert!(
            owner
                .try_publish_at_epoch(0, || {
                    called = true;
                    Ok(())
                })
                .is_err()
        );
        assert!(!called);
        assert_eq!(
            owner
                .connection
                .query_row::<u64, _, _>("PRAGMA busy_timeout", [], |r| r.get(0))
                .unwrap(),
            25
        );
        assert_eq!(
            worker
                .connection
                .query_row::<u64, _, _>("PRAGMA busy_timeout", [], |r| r.get(0))
                .unwrap(),
            500
        );
        blocker.execute_batch("ROLLBACK").unwrap();
        owner.try_publish_at_epoch(0, || Ok(())).unwrap();
        assert_eq!(
            owner
                .connection
                .query_row::<u64, _, _>("PRAGMA busy_timeout", [], |r| r.get(0))
                .unwrap(),
            25
        );
    }

    #[test]
    fn existing_authority_open_preserves_identity_and_never_creates_missing_state() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("private/m.sqlite");
        assert!(Store::open_existing(&path, "expected").is_err());
        assert!(!path.parent().unwrap().exists());
        let owner = Store::open(&path).unwrap();
        let profile = owner.profile_id().to_owned();
        drop(owner);
        assert!(Store::open_existing(&path, "different").is_err());
        let attached = Store::open_existing(&path, &profile).unwrap();
        assert_eq!(attached.profile_id(), profile);
        drop(attached);
        std::fs::remove_file(&path).unwrap();
        assert!(Store::open_existing(&path, &profile).is_err());
        assert!(!path.exists());
    }

    #[test]
    fn existing_authority_mismatch_does_not_migrate_schema() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("private/m.sqlite");
        crate::assistant_storage::database(&path).unwrap();
        let db = Connection::open(&path).unwrap();
        db.execute_batch("CREATE TABLE memory_meta(key TEXT PRIMARY KEY,value TEXT NOT NULL); INSERT INTO memory_meta VALUES('profile_id','different');").unwrap();
        assert!(Store::open_existing(&path, "expected").is_err());
        let count: u64 = db
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name!='memory_meta'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 0);
    }
    #[test]
    fn identical_conversation_on_other_node_cannot_read_memory() {
        let dir = tempdir().unwrap();
        let mut store = Store::open(dir.path().join("private/m.sqlite")).unwrap();
        let mut scope = Scope {
            node: Some("one".into()),
            project: Some("p".into()),
            provider: Some("codex".into()),
            conversation: Some("same-uuid".into()),
        };
        let record = store
            .append(input(
                RecordKind::Finding,
                Origin::Human,
                scope.clone(),
                "private-one",
            ))
            .unwrap();
        scope.node = Some("two".into());
        assert!(store.retrieve(&scope, 10).unwrap().is_empty());
        assert_eq!(store.forget_epoch().unwrap(), 0);
        store.forget(&record.id).unwrap();
        assert_eq!(store.forget_epoch().unwrap(), 1);
        assert!(store.get(&record.id).unwrap().is_none());
    }

    #[test]
    fn bm25_search_is_lexical_scoped_and_excludes_inactive_records() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("private/m.sqlite");
        let mut store = Store::open(&path).unwrap();
        let scope = Scope {
            node: Some("node-a".into()),
            project: Some("project-a".into()),
            provider: Some("codex".into()),
            conversation: Some("conversation-a".into()),
        };
        let old = store
            .append(input(
                RecordKind::Finding,
                Origin::Human,
                scope.clone(),
                "rare needle only in an old record",
            ))
            .unwrap();
        for index in 0..40 {
            let mut recent = input(
                RecordKind::Finding,
                Origin::Human,
                scope.clone(),
                "ordinary recent record",
            );
            recent.timestamp = index + 2;
            store.append(recent).unwrap();
        }
        let mut wrong_scope = input(
            RecordKind::Finding,
            Origin::Human,
            Scope {
                project: Some("other".into()),
                ..scope.clone()
            },
            "rare needle wrong project",
        );
        wrong_scope.timestamp = 1000;
        store.append(wrong_scope).unwrap();
        let mut draft = input(
            RecordKind::Draft,
            Origin::Human,
            scope.clone(),
            "rare needle draft",
        );
        draft.timestamp = 1001;
        store.append(draft).unwrap();
        let old_hit = store.search_bm25(&scope, "rare needle", 20).unwrap();
        assert_eq!(old_hit.len(), 1);
        assert_eq!(old_hit[0].id, old.id);
        let mut replacement = input(
            RecordKind::Finding,
            Origin::Human,
            scope.clone(),
            "replacement text",
        );
        replacement.supersedes = Some(old.id.clone());
        store.append(replacement).unwrap();

        let hits = store
            .search_bm25(&scope, "rare needle OR +draft", 20)
            .unwrap();
        assert!(hits.iter().all(|record| record.id != old.id));
        assert!(hits.iter().all(|record| record.kind != RecordKind::Draft));
        assert!(hits.iter().all(|record| record.scope == scope));
    }

    #[test]
    fn bm25_search_handles_special_queries_zero_limits_and_reopen() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("private/m.sqlite");
        let mut store = Store::open(&path).unwrap();
        let scope = Scope::default();
        let record = store
            .append(input(
                RecordKind::Finding,
                Origin::Human,
                scope.clone(),
                "quoted punctuation operators",
            ))
            .unwrap();
        assert!(
            store
                .search_bm25(&scope, "!!! + - OR AND ( )", 10)
                .unwrap()
                .is_empty()
        );
        assert!(
            store
                .search_bm25(&scope, "operators' OR \"quoted\"", 0)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            store.search_bm25(&scope, "operators", 10).unwrap()[0].id,
            record.id
        );
        drop(store);
        let reopened = Store::open(&path).unwrap();
        assert_eq!(
            reopened.search_bm25(&scope, "operators", 10).unwrap()[0].id,
            record.id
        );
    }

    #[test]
    fn failed_append_does_not_leave_an_index_hit() {
        let dir = tempdir().unwrap();
        let mut store = Store::open(dir.path().join("private/m.sqlite")).unwrap();
        let mut derived = input(
            RecordKind::Briefing,
            Origin::System,
            Scope::default(),
            "should not be indexed",
        );
        derived.dependencies = vec![Uuid::new_v4().to_string()];
        assert!(matches!(
            store.append(derived),
            Err(MemoryError::NotFound(_))
        ));
        assert!(
            store
                .search_bm25(&Scope::default(), "indexed", 10)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn body_update_and_cascade_forget_keep_index_consistent() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("private/m.sqlite");
        let mut store = Store::open(&path).unwrap();
        let root = store
            .append(input(
                RecordKind::Finding,
                Origin::Human,
                Scope::default(),
                "before update",
            ))
            .unwrap();
        store
            .connection
            .execute(
                "UPDATE memory_records SET body=? WHERE id=?",
                params!["after update", root.id],
            )
            .unwrap();
        assert!(
            store
                .search_bm25(&Scope::default(), "before", 10)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            store.search_bm25(&Scope::default(), "after", 10).unwrap()[0].body,
            "after update"
        );
        store.forget(&root.id).unwrap();
        drop(store);
        let reopened = Store::open(&path).unwrap();
        assert!(
            reopened
                .search_bm25(&Scope::default(), "after", 10)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn v2_database_backfills_fts_once_on_migration() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("private/m.sqlite");
        let mut store = Store::open(&path).unwrap();
        store
            .append(input(
                RecordKind::Finding,
                Origin::Human,
                Scope::default(),
                "legacy backfill needle",
            ))
            .unwrap();
        drop(store);

        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "DROP TRIGGER memory_records_fts_ai;
                 DROP TRIGGER memory_records_fts_ad;
                 DROP TRIGGER memory_records_fts_au;
                 DROP TABLE memory_fts;
                 DELETE FROM memory_meta WHERE key='memory_search_index_version';
                 UPDATE memory_meta SET value='2' WHERE key='schema_version';",
            )
            .unwrap();
        drop(connection);

        let migrated = Store::open(&path).unwrap();
        assert_eq!(
            migrated
                .search_bm25(&Scope::default(), "legacy backfill", 10)
                .unwrap()
                .len(),
            1
        );
        let marker: String = migrated
            .connection
            .query_row(
                "SELECT value FROM memory_meta WHERE key='memory_search_index_version'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(marker, MEMORY_SEARCH_INDEX_VERSION);
    }

    #[test]
    fn bm25_scope_and_profile_filter_before_limit() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("private/m.sqlite");
        let mut store = Store::open(&path).unwrap();
        let scope = Scope {
            node: Some("node-a".into()),
            project: Some("project-a".into()),
            provider: Some("provider-a".into()),
            conversation: Some("conversation-a".into()),
        };
        let allowed = store
            .append(input(
                RecordKind::Finding,
                Origin::Human,
                scope.clone(),
                "scope needle",
            ))
            .unwrap();
        for wrong in [
            Scope {
                node: Some("node-b".into()),
                ..scope.clone()
            },
            Scope {
                project: Some("project-b".into()),
                ..scope.clone()
            },
            Scope {
                provider: Some("provider-b".into()),
                ..scope.clone()
            },
            Scope {
                conversation: Some("conversation-b".into()),
                ..scope.clone()
            },
        ] {
            store
                .append(input(
                    RecordKind::Finding,
                    Origin::Human,
                    wrong,
                    "scope needle",
                ))
                .unwrap();
        }
        let other_profile = Uuid::new_v4().to_string();
        store
            .connection
            .execute(
                "INSERT INTO memory_records (id,profile_id,kind,origin,project,provider,conversation,body,provenance,timestamp,supersedes,dependencies,decision_state,protected_policy,node) VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
                params![
                    Uuid::new_v4().to_string(),
                    other_profile,
                    "\"Finding\"",
                    "\"Human\"",
                    scope.project,
                    scope.provider,
                    scope.conversation,
                    "scope needle",
                    "test",
                    99_i64,
                    Option::<String>::None,
                    "[]",
                    Option::<String>::None,
                    0_i64,
                    scope.node,
                ],
            )
            .unwrap();

        let hits = store.search_bm25(&scope, "scope needle", 1).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, allowed.id);
        assert_eq!(hits[0].profile_id, store.profile_id());
    }

    #[test]
    fn search_bounds_prompt_terms_count_and_bytes_without_rejecting() {
        let dir = tempdir().unwrap();
        let mut store = Store::open(dir.path().join("private/m.sqlite")).unwrap();
        let target = store
            .append(input(
                RecordKind::Finding,
                Origin::Human,
                Scope::default(),
                "target lexical term",
            ))
            .unwrap();
        let mut prompt = vec!["target".into()];
        prompt.extend((0..40).map(|index| format!("term{index}")));
        prompt.push("x".repeat(MAX_SEARCH_TERM_BYTES + 1));
        let hits = store
            .search_bm25(&Scope::default(), &prompt.join(" "), 10)
            .unwrap();
        assert_eq!(hits[0].id, target.id);

        for _ in 0..MAX_RETRIEVAL + 10 {
            store
                .append(input(
                    RecordKind::Finding,
                    Origin::Human,
                    Scope::default(),
                    "count bound",
                ))
                .unwrap();
        }
        assert_eq!(
            store
                .search_bm25(&Scope::default(), "count", usize::MAX)
                .unwrap()
                .len(),
            MAX_RETRIEVAL
        );
        let large = "byte bound ".to_string() + &"x".repeat(100_000);
        for _ in 0..3 {
            store
                .append(input(
                    RecordKind::Finding,
                    Origin::Human,
                    Scope::default(),
                    &large,
                ))
                .unwrap();
        }
        assert!(
            store
                .search_bm25(&Scope::default(), "byte", 10)
                .unwrap()
                .len()
                <= 2
        );
    }

    #[test]
    fn budgeted_search_skips_oversized_top_hit_for_smaller_match() {
        let dir = tempdir().unwrap();
        let mut store = Store::open(dir.path().join("private/m.sqlite")).unwrap();
        let large = store
            .append(input(
                RecordKind::Finding,
                Origin::Human,
                Scope::default(),
                &format!("budgetterm {}", "budgetterm ".repeat(2000)),
            ))
            .unwrap();
        let small = store
            .append(input(
                RecordKind::Finding,
                Origin::Human,
                Scope::default(),
                "budgetterm small eligible match",
            ))
            .unwrap();
        let hits = store
            .search_bm25_with_budget(
                &Scope::default(),
                "budgetterm",
                1,
                8 * 1024,
                &Default::default(),
            )
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, small.id);
        assert_ne!(hits[0].id, large.id);
    }

    #[test]
    fn stale_epoch_append_rolls_back_without_an_index_entry() {
        let dir = tempdir().unwrap();
        let mut store = Store::open(dir.path().join("private/m.sqlite")).unwrap();
        let source = store
            .append(input(
                RecordKind::Finding,
                Origin::Human,
                Scope::default(),
                "epoch source",
            ))
            .unwrap();
        let epoch = store.forget_epoch().unwrap();
        store.forget(&source.id).unwrap();
        let stale = input(
            RecordKind::Finding,
            Origin::Worker,
            Scope::default(),
            "stale epoch entry",
        );
        assert!(matches!(
            store.append_at_epoch(stale, epoch),
            Err(MemoryError::Invalid(_))
        ));
        assert!(
            store
                .search_bm25(&Scope::default(), "stale epoch", 10)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn working_set_and_search_exclude_explicit_superseded_state() {
        let dir = tempdir().unwrap();
        let mut store = Store::open(dir.path().join("private/m.sqlite")).unwrap();
        let mut record = input(
            RecordKind::Finding,
            Origin::Human,
            Scope::default(),
            "explicit superseded state",
        );
        record.decision_state = Some(DecisionState::Superseded);
        let saved = store.append(record).unwrap();
        assert_eq!(store.retrieve(&Scope::default(), 10).unwrap().len(), 1);
        assert!(store.working_set(&Scope::default(), 10).unwrap().is_empty());
        assert!(
            store
                .search_bm25(&Scope::default(), "explicit superseded", 10)
                .unwrap()
                .iter()
                .all(|record| record.id != saved.id)
        );
    }
}
