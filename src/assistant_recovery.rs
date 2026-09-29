//! Explicit offline recovery for an assistant whose provider context is lost.
//!
//! The caller must first stop and drop the assistant service, investigation
//! service, and every provider transport. This module never starts a provider,
//! contacts a network, refunds policy spending, or edits authentication state.
//! It preserves request/job IDs and charges while retiring unresolved work.

use rusqlite::{Connection, OptionalExtension};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryReceipt {
    pub receipt_id: String,
    pub epoch: u64,
    pub databases: Vec<PathBuf>,
}

#[derive(Debug, thiserror::Error)]
pub enum RecoveryError {
    #[error("recovery root must be absolute")]
    RelativeRoot,
    #[error("recovery path is unsafe: {0}")]
    UnsafePath(String),
    #[error("recovery database: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("recovery policy: {0}")]
    Policy(#[from] crate::assistant_policy::PolicyError),
    #[error("recovery filesystem: {0}")]
    Filesystem(#[from] std::io::Error),
    #[error("recovery request id must be a UUID")]
    InvalidRequestId,
    #[error("recovery epoch exceeds SQLite integer range")]
    EpochOverflow,
}

fn open_known(path: &Path) -> Result<Option<Connection>, RecoveryError> {
    if !path.exists() {
        return Ok(None);
    }
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(RecoveryError::UnsafePath(path.display().to_string()));
    }
    crate::assistant_storage::database(path).map_err(RecoveryError::Filesystem)?;
    Ok(Some(Connection::open(path)?))
}

fn table_exists(conn: &Connection, table: &str) -> Result<bool, RecoveryError> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?)",
        [table],
        |r| r.get(0),
    )?)
}

/// Check before stopping services, so replaying a completed recovery request
/// cannot interrupt newer owned work.
pub fn existing_receipt(
    root: &Path,
    request_id: &str,
) -> Result<Option<RecoveryReceipt>, RecoveryError> {
    if !root.is_absolute() {
        return Err(RecoveryError::RelativeRoot);
    }
    if uuid::Uuid::parse_str(request_id).is_err() {
        return Err(RecoveryError::InvalidRequestId);
    }
    let Some(conn) = open_known(&root.join("recovery.sqlite"))? else {
        return Ok(None);
    };
    if !table_exists(&conn, "recovery_receipts")? {
        return Ok(None);
    }
    let epoch: Option<u64> = conn
        .query_row(
            "SELECT epoch FROM recovery_receipts WHERE id=?",
            [request_id],
            |r| r.get(0),
        )
        .optional()?;
    Ok(epoch.map(|epoch| RecoveryReceipt {
        receipt_id: request_id.into(),
        epoch,
        databases: Vec::new(),
    }))
}

fn column_exists(conn: &Connection, table: &str, column: &str) -> Result<bool, RecoveryError> {
    if !table_exists(conn, table)? {
        return Ok(false);
    }
    Ok(conn
        .prepare(&format!("PRAGMA table_info({table})"))?
        .query_map([], |r| r.get::<_, String>(1))?
        .filter_map(Result::ok)
        .any(|name| name == column))
}

/// Retire unresolved work after all live assistant services/transports have
/// been dropped. This is safe to repeat after a crash or partial completion.
pub fn recover_after_services_dropped(
    root: impl AsRef<Path>,
    request_id: &str,
    memory_epoch: u64,
) -> Result<RecoveryReceipt, RecoveryError> {
    let root = root.as_ref();
    validate_request(root, request_id, memory_epoch)?;
    crate::assistant_storage::directory(root).map_err(RecoveryError::Filesystem)?;
    let receipt_path = root.join("recovery.sqlite");
    let receipt_conn = crate::assistant_storage::database(&receipt_path)
        .map_err(RecoveryError::Filesystem)
        .and_then(|_| Ok(Connection::open(&receipt_path)?))?;
    receipt_conn.execute_batch("CREATE TABLE IF NOT EXISTS recovery_receipts(id TEXT PRIMARY KEY, epoch INTEGER NOT NULL, completed_at INTEGER NOT NULL)")?;
    let receipt_id = request_id.to_owned();
    if receipt_conn.query_row::<i64, _, _>(
        "SELECT COUNT(*) FROM recovery_receipts WHERE id=?",
        [&receipt_id],
        |r| r.get(0),
    )? > 0
    {
        return Ok(RecoveryReceipt {
            receipt_id,
            epoch: memory_epoch,
            databases: Vec::new(),
        });
    }
    // Quarantine first. Then retire concurrency claims as unknown, retaining
    // their full call charges and immutable IDs in the policy receipt ledger.
    let reservation_ids = quarantine_and_collect_reservations(root)?;
    settle_unknown_reservations(root, reservation_ids)?;
    let databases = scrub_owned_databases(root, memory_epoch)?;
    receipt_conn.execute("INSERT OR IGNORE INTO recovery_receipts(id,epoch,completed_at) VALUES(?,?,strftime('%s','now'))", rusqlite::params![receipt_id, memory_epoch as i64])?;
    // This is intentionally the final mutation. If anything above crashes,
    // the guard may remain blocked. A new explicit recovery request is then
    // required; replaying an old receipt must never unblock later unknown work.
    unblock_runtime_guards(root)?;
    Ok(RecoveryReceipt {
        receipt_id,
        epoch: memory_epoch,
        databases,
    })
}

fn validate_request(root: &Path, request_id: &str, memory_epoch: u64) -> Result<(), RecoveryError> {
    if !root.is_absolute() {
        return Err(RecoveryError::RelativeRoot);
    }
    if uuid::Uuid::parse_str(request_id).is_err() {
        return Err(RecoveryError::InvalidRequestId);
    }
    if memory_epoch > i64::MAX as u64 {
        return Err(RecoveryError::EpochOverflow);
    }
    Ok(())
}

fn quarantine_and_collect_reservations(
    root: &Path,
) -> Result<std::collections::BTreeSet<String>, RecoveryError> {
    let mut reservation_ids = std::collections::BTreeSet::new();
    for name in [
        "runtime.sqlite",
        "author-runtime.sqlite",
        "maintenance-runtime.sqlite",
    ] {
        collect_runtime_reservations(root, name, &mut reservation_ids)?;
    }
    collect_investigation_reservations(root, &mut reservation_ids)?;
    Ok(reservation_ids)
}

fn collect_runtime_reservations(
    root: &Path,
    name: &str,
    reservation_ids: &mut std::collections::BTreeSet<String>,
) -> Result<(), RecoveryError> {
    let Some(conn) = open_known(&root.join(name))? else {
        return Ok(());
    };
    if table_exists(&conn, "assistant_runtime_guard")? {
        conn.execute(
            "UPDATE assistant_runtime_guard SET blocked=1 WHERE id=1",
            [],
        )?;
    }
    if column_exists(&conn, "assistant_runtime_turns", "reservation_id")? {
        let mut stmt=conn.prepare("SELECT reservation_id FROM assistant_runtime_turns WHERE state IN ('reserved','dispatch_intent','in_flight','unknown')")?;
        for id in stmt.query_map([], |r| r.get::<_, String>(0))? {
            reservation_ids.insert(id?);
        }
    }
    Ok(())
}

fn collect_investigation_reservations(
    root: &Path,
    reservation_ids: &mut std::collections::BTreeSet<String>,
) -> Result<(), RecoveryError> {
    let Some(conn) = open_known(&root.join("investigation.sqlite"))? else {
        return Ok(());
    };
    if table_exists(&conn, "investigation_jobs")? {
        let mut stmt=conn.prepare("SELECT reservation_id FROM investigation_jobs WHERE state IN ('queued','intent','dispatched','running','unknown') UNION SELECT root_id FROM investigation_jobs WHERE state IN ('queued','intent','dispatched','running','unknown')")?;
        for id in stmt.query_map([], |r| r.get::<_, String>(0))? {
            reservation_ids.insert(id?);
        }
    }
    if table_exists(&conn, "investigation_roots")? {
        let mut stmt=conn.prepare("SELECT id FROM investigation_roots WHERE state IN ('planning','queued','intent','active','running','unknown')")?;
        for id in stmt.query_map([], |r| r.get::<_, String>(0))? {
            reservation_ids.insert(id?);
        }
    }
    Ok(())
}

fn settle_unknown_reservations(
    root: &Path,
    reservation_ids: std::collections::BTreeSet<String>,
) -> Result<(), RecoveryError> {
    if !reservation_ids.is_empty() && root.join("policy.sqlite").exists() {
        let mut policy =
            crate::assistant_policy::AssistantPolicy::open(root.join("policy.sqlite"))?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            .min(i64::MAX as u64) as i64;
        for id in reservation_ids {
            if policy.reservation(&id)?.is_some_and(|r| {
                matches!(
                    r.state,
                    crate::assistant_policy::ReservationState::Reserved
                        | crate::assistant_policy::ReservationState::Dispatched
                )
            }) {
                policy.record_outcome(
                    &id,
                    crate::assistant_policy::DeliveryOutcome::Unknown,
                    now,
                )?;
            }
        }
    }
    Ok(())
}

fn scrub_owned_databases(root: &Path, memory_epoch: u64) -> Result<Vec<PathBuf>, RecoveryError> {
    let mut databases = Vec::new();
    for path in [
        root.join("runtime.sqlite"),
        root.join("author-runtime.sqlite"),
        root.join("maintenance-runtime.sqlite"),
        root.join("investigation.sqlite"),
        root.join("workshop.sqlite"),
    ] {
        if scrub_database(&path, memory_epoch)? {
            databases.push(path);
        }
    }
    Ok(databases)
}

fn scrub_database(path: &Path, memory_epoch: u64) -> Result<bool, RecoveryError> {
    let Some(conn) = open_known(path)? else {
        return Ok(false);
    };
    let tx = conn.unchecked_transaction()?;
    scrub_runtime_tables(&tx, memory_epoch)?;
    scrub_investigation_tables(&tx)?;
    scrub_evolution_tables(&tx)?;
    tx.commit()?;
    Ok(true)
}

fn scrub_runtime_tables(conn: &Connection, memory_epoch: u64) -> Result<(), RecoveryError> {
    if table_exists(conn, "assistant_runtime_guard")? {
        conn.execute(
            "UPDATE assistant_runtime_guard SET blocked=1 WHERE id=1",
            [],
        )?;
    }
    if table_exists(conn, "assistant_runtime_turns")? {
        conn.execute(
            "UPDATE assistant_runtime_turns SET prompt='',reply='',dependencies='[]'",
            [],
        )?;
        conn.execute("UPDATE assistant_runtime_turns SET state='abandoned' WHERE state IN ('reserved','dispatch_intent','in_flight','unknown')", [])?;
    }
    if table_exists(conn, "assistant_runtime_profile")? {
        conn.execute("UPDATE assistant_runtime_profile SET thread_id=NULL", [])?;
    }
    if table_exists(conn, "assistant_runtime_epoch")? {
        conn.execute(
            "UPDATE assistant_runtime_epoch SET epoch=? WHERE id=1",
            [memory_epoch as i64],
        )?;
    }
    Ok(())
}

fn scrub_investigation_tables(conn: &Connection) -> Result<(), RecoveryError> {
    if table_exists(conn, "investigation_jobs")? {
        conn.execute(
            "UPDATE investigation_jobs SET task_json='',finding=NULL",
            [],
        )?;
        conn.execute("UPDATE investigation_jobs SET state='abandoned' WHERE state IN ('queued','intent','dispatched','running','unknown')", [])?;
    }
    if table_exists(conn, "investigation_roots")? {
        conn.execute("UPDATE investigation_roots SET state='abandoned' WHERE state IN ('planning','queued','active','running','unknown','intent')", [])?;
    }
    Ok(())
}

fn scrub_evolution_tables(conn: &Connection) -> Result<(), RecoveryError> {
    if column_exists(conn, "assistant_evolution_pending", "state")? {
        conn.execute("UPDATE assistant_evolution_pending SET state='abandoned' WHERE state IN ('pending','candidate','submitted','queued','running')", [])?;
        if column_exists(conn, "assistant_evolution_pending", "candidate_json")? {
            conn.execute("UPDATE assistant_evolution_pending SET candidate_json=NULL WHERE state='abandoned'", [])?;
        }
    }
    Ok(())
}

fn unblock_runtime_guards(root: &Path) -> Result<(), RecoveryError> {
    for name in [
        "runtime.sqlite",
        "author-runtime.sqlite",
        "maintenance-runtime.sqlite",
    ] {
        if let Some(conn) = open_known(&root.join(name))? {
            if table_exists(&conn, "assistant_runtime_guard")? {
                conn.execute(
                    "UPDATE assistant_runtime_guard SET blocked=0 WHERE id=1",
                    [],
                )?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    #[test]
    fn recovery_retires_coordinator_envelope_without_worker_rows() {
        use crate::assistant_policy::{AssistantPolicy, PolicyConfig, ReservationState};
        let dir = tempdir().unwrap();
        let root = dir.path().join("private");
        let mut policy = AssistantPolicy::open(root.join("policy.sqlite")).unwrap();
        policy
            .configure(&PolicyConfig {
                max_total_calls: 4,
                max_concurrent: 2,
                ..Default::default()
            })
            .unwrap();
        policy
            .reserve_root("coordinator-orphan", 1, false, 1, None)
            .unwrap();
        let path = root.join("investigation.sqlite");
        crate::assistant_storage::database(&path).unwrap();
        let db = Connection::open(&path).unwrap();
        db.execute_batch("CREATE TABLE investigation_roots(id TEXT PRIMARY KEY,state TEXT NOT NULL); INSERT INTO investigation_roots VALUES('coordinator-orphan','planning');").unwrap();
        drop(db);
        recover_after_services_dropped(&root, "aaaaaaaa-aaaa-4aaa-aaaa-aaaaaaaaaaaa", 0).unwrap();
        let reservation = policy.reservation("coordinator-orphan").unwrap().unwrap();
        assert_eq!(reservation.state, ReservationState::Unknown);
        assert_eq!(reservation.calls, 1);
        let db = Connection::open(path).unwrap();
        assert_eq!(
            db.query_row::<String, _, _>("SELECT state FROM investigation_roots", [], |r| r.get(0))
                .unwrap(),
            "abandoned"
        );
    }

    #[test]
    fn recovery_preserves_ids_and_is_repeatable_without_policy_or_provider() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("private");
        fs::create_dir(&root).unwrap();
        #[cfg(unix)]
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let runtime = root.join("runtime.sqlite");
        let conn = Connection::open(&runtime).unwrap();
        conn.execute_batch("CREATE TABLE assistant_runtime_guard(id INTEGER PRIMARY KEY,blocked INTEGER); INSERT INTO assistant_runtime_guard VALUES(1,1); CREATE TABLE assistant_runtime_profile(id INTEGER PRIMARY KEY,thread_id TEXT); INSERT INTO assistant_runtime_profile VALUES(1,'old-thread'); CREATE TABLE assistant_runtime_epoch(id INTEGER PRIMARY KEY,epoch INTEGER); INSERT INTO assistant_runtime_epoch VALUES(1,0); CREATE TABLE assistant_runtime_turns(request_id TEXT PRIMARY KEY,prompt TEXT,reply TEXT,dependencies TEXT,state TEXT); INSERT INTO assistant_runtime_turns VALUES('old-request','secret','answer','[1]','unknown');").unwrap();
        drop(conn);
        #[cfg(unix)]
        fs::set_permissions(&runtime, fs::Permissions::from_mode(0o600)).unwrap();
        // An author journal is independently quarantined.  Its unknown guard
        // must block a fresh author startup until this explicit recovery.
        let author = root.join("author-runtime.sqlite");
        let author_conn = Connection::open(&author).unwrap();
        author_conn.execute_batch("CREATE TABLE assistant_runtime_guard(id INTEGER PRIMARY KEY,blocked INTEGER); INSERT INTO assistant_runtime_guard VALUES(1,1); CREATE TABLE assistant_runtime_profile(id INTEGER PRIMARY KEY,thread_id TEXT); INSERT INTO assistant_runtime_profile VALUES(1,'author-thread'); CREATE TABLE assistant_runtime_turns(request_id TEXT PRIMARY KEY,reservation_id TEXT,prompt TEXT,reply TEXT,dependencies TEXT,state TEXT); INSERT INTO assistant_runtime_turns VALUES('author-request','author-call','author secret','author answer','[]','unknown');").unwrap();
        drop(author_conn);
        #[cfg(unix)]
        fs::set_permissions(&author, fs::Permissions::from_mode(0o600)).unwrap();
        let mut policy =
            crate::assistant_policy::AssistantPolicy::open(root.join("policy.sqlite")).unwrap();
        policy
            .configure(&crate::assistant_policy::PolicyConfig {
                background_calls: 0,
                max_concurrent: 2,
                max_total_calls: 2,
                default_deadline_seconds: 60,
            })
            .unwrap();
        policy
            .reserve_root("author-call", 1, false, 1, Some(60))
            .unwrap();
        policy.mark_dispatched("author-call", 2).unwrap();
        assert!(matches!(
            recover_after_services_dropped(&root, "not-a-uuid", 1),
            Err(RecoveryError::InvalidRequestId)
        ));
        let request_id = "11111111-1111-4111-8111-111111111111";
        let receipt = recover_after_services_dropped(&root, request_id, 1).unwrap();
        assert_eq!(receipt.epoch, 1);
        let conn = Connection::open(runtime).unwrap();
        let row: (String, String, String, String) = conn
            .query_row(
                "SELECT request_id,prompt,reply,state FROM assistant_runtime_turns",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(
            row,
            (
                "old-request".into(),
                "".into(),
                "".into(),
                "abandoned".into()
            )
        );
        assert_eq!(
            conn.query_row::<Option<String>, _, _>(
                "SELECT thread_id FROM assistant_runtime_profile WHERE id=1",
                [],
                |r| r.get(0)
            )
            .unwrap(),
            None
        );
        let author_conn = Connection::open(author).unwrap();
        assert_eq!(
            author_conn
                .query_row::<String, _, _>("SELECT state FROM assistant_runtime_turns", [], |r| r
                    .get(0))
                .unwrap(),
            "abandoned"
        );
        assert!(
            !author_conn
                .query_row::<bool, _, _>("SELECT blocked FROM assistant_runtime_guard", [], |r| r
                    .get(0))
                .unwrap()
        );
        let policy =
            crate::assistant_policy::AssistantPolicy::open(root.join("policy.sqlite")).unwrap();
        assert_eq!(
            policy.reservation("author-call").unwrap().unwrap().state,
            crate::assistant_policy::ReservationState::Unknown
        );
        let again = recover_after_services_dropped(&root, request_id, 1).unwrap();
        assert_eq!(again.receipt_id, receipt.receipt_id);
        conn.execute("INSERT INTO assistant_runtime_turns VALUES('new-request','new secret','new answer','[]','unknown')",[]).unwrap();
        conn.execute("UPDATE assistant_runtime_guard SET blocked=1", [])
            .unwrap();
        recover_after_services_dropped(&root, request_id, 1).unwrap();
        assert_eq!(
            conn.query_row::<String, _, _>(
                "SELECT state FROM assistant_runtime_turns WHERE request_id='new-request'",
                [],
                |r| r.get(0)
            )
            .unwrap(),
            "unknown"
        );
        assert!(
            conn.query_row::<bool, _, _>("SELECT blocked FROM assistant_runtime_guard", [], |r| r
                .get(0))
                .unwrap()
        );
        let second =
            recover_after_services_dropped(&root, "22222222-2222-4222-8222-222222222222", 1)
                .unwrap();
        assert_eq!(second.epoch, 1);
        assert_eq!(
            conn.query_row::<String, _, _>(
                "SELECT state FROM assistant_runtime_turns WHERE request_id='new-request'",
                [],
                |r| r.get(0)
            )
            .unwrap(),
            "abandoned"
        );
    }
}
