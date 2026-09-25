//! Explicit, epoch-fenced cleanup of Pika-owned assistant state.
//!
//! This module deliberately names every database it may touch. It never scans
//! directories, opens provider logs, or removes credentials/authentication.

use rusqlite::{Connection, OptionalExtension};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanupReport {
    pub epoch: u64,
    pub databases: Vec<PathBuf>,
    pub changed: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum RetentionError {
    #[error("retention root must be absolute")]
    RelativeRoot,
    #[error("retention path is unsafe: {0}")]
    UnsafePath(String),
    #[error("retention database: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("retention filesystem: {0}")]
    Filesystem(#[from] std::io::Error),
}

fn table_exists(conn: &Connection, table: &str) -> Result<bool, RetentionError> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?)",
        [table],
        |row| row.get(0),
    )?)
}

fn execute_if_present(conn: &Connection, table: &str, sql: &str) -> Result<usize, RetentionError> {
    if table_exists(conn, table)? {
        Ok(conn.execute(sql, [])?)
    } else {
        Ok(0)
    }
}

fn open_known(path: &Path) -> Result<Option<Connection>, RetentionError> {
    if !path.exists() {
        return Ok(None);
    }
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(RetentionError::UnsafePath(path.display().to_string()));
    }
    crate::assistant_storage::database(path).map_err(RetentionError::Filesystem)?;
    Ok(Some(Connection::open(path)?))
}

/// Scrub all assistant-derived caches after a memory forget epoch advances.
/// Policy databases and provider-owned state are intentionally not included.
pub fn cleanup(root: impl AsRef<Path>, epoch: u64) -> Result<CleanupReport, RetentionError> {
    let root = root.as_ref();
    if !root.is_absolute() {
        return Err(RetentionError::RelativeRoot);
    }
    let known = [
        root.join("runtime.sqlite"),
        root.join("author-runtime.sqlite"),
        root.join("learning.sqlite"),
        root.join("investigation.sqlite"),
        root.join("workshop.sqlite"),
    ];
    let marker = root.join("retention.sqlite");
    if epoch == 0 {
        return Ok(CleanupReport {
            epoch,
            databases: Vec::new(),
            changed: 0,
        });
    }
    let previous = previous_epoch(&marker)?;
    if previous.is_some_and(|value| value >= epoch) {
        return Ok(CleanupReport {
            epoch,
            databases: Vec::new(),
            changed: 0,
        });
    }
    let mut databases = Vec::new();
    let mut changed = 0;
    for path in known {
        if let Some(count) = scrub_database(&path, epoch)? {
            changed += count;
            databases.push(path);
        }
    }
    // Marker is written last. A crash before this point safely causes an
    // idempotent repeat on the next startup.
    write_epoch(&marker, epoch)?;
    Ok(CleanupReport {
        epoch,
        databases,
        changed,
    })
}

fn previous_epoch(marker: &Path) -> Result<Option<u64>, RetentionError> {
    let Some(conn) = open_known(marker)? else {
        return Ok(None);
    };
    if !table_exists(&conn, "retention_meta")? {
        return Ok(None);
    }
    let value = conn
        .query_row("SELECT epoch FROM retention_meta WHERE id=1", [], |row| {
            row.get::<_, i64>(0)
        })
        .optional()?
        .unwrap_or(0);
    if value < 0 {
        return Err(RetentionError::UnsafePath("invalid retention epoch".into()));
    }
    Ok(Some(value as u64))
}

fn scrub_database(path: &Path, epoch: u64) -> Result<Option<usize>, RetentionError> {
    let Some(conn) = open_known(path)? else {
        return Ok(None);
    };
    conn.busy_timeout(std::time::Duration::from_millis(500))?;
    conn.execute_batch("PRAGMA foreign_keys=ON;")?;
    let tx = conn.unchecked_transaction()?;
    let mut changed = scrub_derived_tables(&tx, epoch)?;
    for table in [
        "assistant_evolution_pending",
        "learning_uses",
        "learning_candidates",
        "learning_hypotheses",
        "tool_comparisons",
        "tool_activations",
        "tool_grants",
        "tool_evaluations",
        "candidate_required_suites",
        "tool_candidates",
        "protected_suites",
    ] {
        changed += execute_if_present(&tx, table, &format!("DELETE FROM {table}"))?;
    }
    tx.commit()?;
    Ok(Some(changed))
}

fn scrub_derived_tables(tx: &Connection, epoch: u64) -> Result<usize, RetentionError> {
    let mut changed = 0;
    // Runtime: preserve rows and receipts, but make replay impossible.
    changed += execute_if_present(
        tx,
        "assistant_runtime_turns",
        "UPDATE assistant_runtime_turns SET prompt='',reply='',dependencies='[]',state='unknown'",
    )?;
    changed += execute_if_present(
        tx,
        "assistant_runtime_guard",
        "UPDATE assistant_runtime_guard SET blocked=1 WHERE id=1",
    )?;
    changed += execute_if_present(
        tx,
        "assistant_runtime_epoch",
        &format!("UPDATE assistant_runtime_epoch SET epoch=MAX(epoch, {epoch}) WHERE id=1"),
    )?;
    // Investigation: retain reservation/job identities but remove assignments/evidence.
    changed += execute_if_present(
        tx,
        "investigation_jobs",
        &format!(
            "UPDATE investigation_jobs SET task_json='',finding=NULL,state='unknown',forget_epoch=MAX(forget_epoch, {epoch})"
        ),
    )?;
    changed += execute_if_present(
        tx,
        "investigation_roots",
        "UPDATE investigation_roots SET state='unknown'",
    )?;
    Ok(changed)
}

fn write_epoch(marker: &Path, epoch: u64) -> Result<(), RetentionError> {
    let marker_conn = crate::assistant_storage::database(marker)
        .map_err(RetentionError::Filesystem)
        .and_then(|_| Ok(Connection::open(marker)?))?;
    marker_conn.execute_batch("CREATE TABLE IF NOT EXISTS retention_meta(id INTEGER PRIMARY KEY CHECK(id=1), epoch INTEGER NOT NULL)")?;
    marker_conn.execute("INSERT INTO retention_meta(id,epoch) VALUES(1,?) ON CONFLICT(id) DO UPDATE SET epoch=MAX(epoch,excluded.epoch)", [epoch as i64])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    #[test]
    fn cleanup_is_explicit_idempotent_and_preserves_policy_outside_scope() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("private");
        fs::create_dir(&root).unwrap();
        #[cfg(unix)]
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let runtime = root.join("runtime.sqlite");
        let conn = Connection::open(&runtime).unwrap();
        conn.execute_batch("CREATE TABLE assistant_runtime_turns(prompt TEXT,reply TEXT,dependencies TEXT,state TEXT); CREATE TABLE assistant_runtime_guard(id INTEGER PRIMARY KEY,blocked INTEGER); INSERT INTO assistant_runtime_turns VALUES('secret','reply','[1]','in_flight'); INSERT INTO assistant_runtime_guard VALUES(1,0);").unwrap();
        drop(conn);
        #[cfg(unix)]
        fs::set_permissions(&runtime, fs::Permissions::from_mode(0o600)).unwrap();
        let author = root.join("author-runtime.sqlite");
        let author_conn = Connection::open(&author).unwrap();
        author_conn.execute_batch("CREATE TABLE assistant_runtime_turns(prompt TEXT,reply TEXT,dependencies TEXT,state TEXT); CREATE TABLE assistant_runtime_guard(id INTEGER PRIMARY KEY,blocked INTEGER); INSERT INTO assistant_runtime_turns VALUES('author secret','author answer','[1]','in_flight'); INSERT INTO assistant_runtime_guard VALUES(1,0);").unwrap();
        drop(author_conn);
        #[cfg(unix)]
        fs::set_permissions(&author, fs::Permissions::from_mode(0o600)).unwrap();
        let untouched = cleanup(&root, 0).unwrap();
        assert_eq!(untouched.changed, 0);
        let report = cleanup(&root, 1).unwrap();
        assert_eq!(report.epoch, 1);
        cleanup(&root, 1).unwrap();
        let conn = Connection::open(runtime).unwrap();
        let row: (String, String, String, String) = conn
            .query_row(
                "SELECT prompt,reply,dependencies,state FROM assistant_runtime_turns",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(
            row,
            (String::new(), String::new(), "[]".into(), "unknown".into())
        );
        let author_conn = Connection::open(author).unwrap();
        let author_row: (String, String, String, String) = author_conn
            .query_row(
                "SELECT prompt,reply,dependencies,state FROM assistant_runtime_turns",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(
            author_row,
            (String::new(), String::new(), "[]".into(), "unknown".into())
        );
    }
}
