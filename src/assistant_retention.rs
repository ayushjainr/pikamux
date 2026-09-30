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
    cleanup_inner(root.as_ref(), epoch, true)
}

/// Scope revocation already removed its own grant and fenced derived memory.
/// Scrub old provider/tool caches without revoking unrelated board scopes.
pub(crate) fn cleanup_revoked_context(
    root: &Path,
    epoch: u64,
) -> Result<CleanupReport, RetentionError> {
    cleanup_inner(root, epoch, false)
}

fn cleanup_inner(
    root: &Path,
    epoch: u64,
    scrub_permissions: bool,
) -> Result<CleanupReport, RetentionError> {
    if !root.is_absolute() {
        return Err(RetentionError::RelativeRoot);
    }
    let known = [
        root.join("runtime.sqlite"),
        root.join("author-runtime.sqlite"),
        root.join("maintenance-runtime.sqlite"),
        root.join("learning.sqlite"),
        root.join("investigation.sqlite"),
        root.join("workshop.sqlite"),
        root.join("consultation-permissions.sqlite"),
        root.join("owner.sqlite"),
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
        if let Some(count) = scrub_known(root, &path, epoch, scrub_permissions)? {
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

fn scrub_known(
    root: &Path,
    path: &Path,
    epoch: u64,
    scrub_permissions: bool,
) -> Result<Option<usize>, RetentionError> {
    if !scrub_permissions && path == root.join("owner.sqlite") {
        return scrub_presentation_only(path);
    }
    if !scrub_permissions && path == root.join("workshop.sqlite") {
        return scrub_observations(path);
    }
    scrub_database(path, epoch, scrub_permissions)
}

fn previous_epoch(marker: &Path) -> Result<Option<u64>, RetentionError> {
    let Some(conn) = open_known(marker)? else {
        return Ok(None);
    };
    if !table_exists(&conn, "retention_meta")? {
        return Ok(None);
    }
    if !table_exists(&conn, "retention_format")? {
        return Ok(None);
    }
    let format: Option<u64> = conn
        .query_row("SELECT version FROM retention_format WHERE id=1", [], |r| {
            r.get(0)
        })
        .optional()?;
    if format != Some(2) {
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

fn scrub_database(
    path: &Path,
    epoch: u64,
    scrub_permissions: bool,
) -> Result<Option<usize>, RetentionError> {
    let Some(conn) = open_known(path)? else {
        return Ok(None);
    };
    conn.busy_timeout(std::time::Duration::from_millis(500))?;
    conn.execute_batch("PRAGMA foreign_keys=ON;")?;
    let tx = conn.unchecked_transaction()?;
    let mut changed = scrub_derived_tables(&tx, epoch, scrub_permissions)?;
    for table in [
        "assistant_evolution_pending",
        "consultation_permissions",
        "assistant_board_shares",
        "assistant_lifetime",
        "tool_assessments",
        "tool_observations",
        "tool_retirements",
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
        if !scrub_permissions && table != "assistant_evolution_pending" {
            continue;
        }
        changed += execute_if_present(&tx, table, &format!("DELETE FROM {table}"))?;
    }
    tx.commit()?;
    Ok(Some(changed))
}

fn scrub_derived_tables(
    tx: &Connection,
    epoch: u64,
    scrub_permissions: bool,
) -> Result<usize, RetentionError> {
    let mut changed = scrub_presentation_tables(tx)?;
    // Full forget also revokes saved board grants. Their derived memory must
    // be invalidated before fresh-context recovery can authorize another send.
    // Persist the intent first; Controller retries it before recovery begins.
    if scrub_permissions && table_exists(tx, "assistant_board_shares")? {
        tx.execute_batch("CREATE TABLE IF NOT EXISTS assistant_board_revocations(scope TEXT PRIMARY KEY,epoch INTEGER)")?;
        changed += tx.execute("INSERT OR IGNORE INTO assistant_board_revocations(scope,epoch) SELECT scope,NULL FROM assistant_board_shares",[])?;
    }
    // Forgetting revokes metadata grants and saved background activation, but
    // never erases charged work or recovery acknowledgements. Pending board
    // revocations survive until the controller verifies their cleanup. Keep
    // the user's pause preference; fresh-context recovery is independently
    // required before a provider can reason again.
    changed += execute_if_present(
        tx,
        "assistant_control",
        "UPDATE assistant_control SET context_blocked=1 WHERE id=1",
    )?;
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

fn scrub_presentation_tables(connection: &Connection) -> Result<usize, RetentionError> {
    let mut changed = 0;
    for table in [
        "assistant_briefing_cache",
        "assistant_briefing_cursors",
        "assistant_briefing_pages",
    ] {
        changed += execute_if_present(connection, table, &format!("DELETE FROM {table}"))?;
    }
    Ok(changed)
}

fn scrub_presentation_only(path: &Path) -> Result<Option<usize>, RetentionError> {
    let Some(connection) = open_known(path)? else {
        return Ok(None);
    };
    let tx = connection.unchecked_transaction()?;
    let changed = scrub_presentation_tables(&tx)?;
    tx.commit()?;
    Ok(Some(changed))
}

fn scrub_observations(path: &Path) -> Result<Option<usize>, RetentionError> {
    let Some(connection) = open_known(path)? else {
        return Ok(None);
    };
    Ok(Some(execute_if_present(
        &connection,
        "tool_observations",
        "DELETE FROM tool_observations",
    )?))
}

fn write_epoch(marker: &Path, epoch: u64) -> Result<(), RetentionError> {
    let marker_conn = crate::assistant_storage::database(marker)
        .map_err(RetentionError::Filesystem)
        .and_then(|_| Ok(Connection::open(marker)?))?;
    marker_conn.execute_batch("CREATE TABLE IF NOT EXISTS retention_meta(id INTEGER PRIMARY KEY CHECK(id=1), epoch INTEGER NOT NULL)")?;
    marker_conn.execute("INSERT INTO retention_meta(id,epoch) VALUES(1,?) ON CONFLICT(id) DO UPDATE SET epoch=MAX(epoch,excluded.epoch)", [epoch as i64])?;
    marker_conn.execute_batch("CREATE TABLE IF NOT EXISTS retention_format(id INTEGER PRIMARY KEY,version INTEGER NOT NULL); INSERT INTO retention_format VALUES(1,2) ON CONFLICT(id) DO UPDATE SET version=2")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    #[test]
    fn scoped_revocation_preserves_unrelated_standing_permissions_and_tools() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("private");
        let grants = root.join("consultation-permissions.sqlite");
        crate::assistant_storage::database(&grants).unwrap();
        let conn = Connection::open(&grants).unwrap();
        conn.execute_batch("CREATE TABLE consultation_permissions(id TEXT); INSERT INTO consultation_permissions VALUES('unrelated');").unwrap();
        let workshop = root.join("workshop.sqlite");
        crate::assistant_storage::database(&workshop).unwrap();
        let tools = Connection::open(&workshop).unwrap();
        tools.execute_batch("CREATE TABLE tool_grants(id TEXT); INSERT INTO tool_grants VALUES('standing'); CREATE TABLE tool_candidates(id TEXT); INSERT INTO tool_candidates VALUES('unrelated-tool');").unwrap();
        let runtime = root.join("runtime.sqlite");
        crate::assistant_storage::database(&runtime).unwrap();
        let cache = Connection::open(&runtime).unwrap();
        cache.execute_batch("CREATE TABLE assistant_runtime_turns(prompt TEXT,reply TEXT,dependencies TEXT,state TEXT); INSERT INTO assistant_runtime_turns VALUES('old context','old result','[]','completed');").unwrap();
        cleanup_revoked_context(&root, 1).unwrap();
        assert_eq!(
            conn.query_row("SELECT id FROM consultation_permissions", [], |r| r
                .get::<_, String>(0))
                .unwrap(),
            "unrelated"
        );
        assert_eq!(
            tools
                .query_row("SELECT id FROM tool_grants", [], |r| r.get::<_, String>(0))
                .unwrap(),
            "standing"
        );
        assert_eq!(
            tools
                .query_row("SELECT id FROM tool_candidates", [], |r| r
                    .get::<_, String>(0))
                .unwrap(),
            "unrelated-tool"
        );
        assert_eq!(
            cache
                .query_row("SELECT prompt FROM assistant_runtime_turns", [], |r| r
                    .get::<_, String>(
                    0
                ))
                .unwrap(),
            ""
        );
        cleanup(&root, 2).unwrap();
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM consultation_permissions", [], |r| r
                .get::<_, u64>(
                0
            ))
            .unwrap(),
            0
        );
    }

    #[test]
    fn forget_scrubs_owner_permissions_but_preserves_charges_and_pending_cleanup() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("private");
        let path = root.join("owner.sqlite");
        crate::assistant_storage::database(&path).unwrap();
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE assistant_board_shares(scope TEXT,body TEXT);
            INSERT INTO assistant_board_shares VALUES('private-project','exact-private-task-ids');
            CREATE TABLE assistant_lifetime(id INTEGER,body TEXT);
            INSERT INTO assistant_lifetime VALUES(1,'saved executable and scope');
            CREATE TABLE assistant_control(id INTEGER,paused INTEGER,context_blocked INTEGER);
            INSERT INTO assistant_control VALUES(1,1,0);
            CREATE TABLE assistant_background_roots(id TEXT,calls INTEGER,state TEXT);
            INSERT INTO assistant_background_roots VALUES('charged-root',4,'unknown');
            CREATE TABLE assistant_control_recovered(kind TEXT,id TEXT);
            INSERT INTO assistant_control_recovered VALUES('policy','acknowledged-root');
            CREATE TABLE assistant_board_revocations(scope TEXT PRIMARY KEY,epoch INTEGER);
            INSERT INTO assistant_board_revocations VALUES('pending-project',1);",
            )
            .unwrap();
        cleanup(&root, 1).unwrap();
        for table in ["assistant_board_shares", "assistant_lifetime"] {
            let count: u64 = connection
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
                .unwrap();
            assert_eq!(count, 0);
        }
        assert_eq!(
            connection
                .query_row(
                    "SELECT paused,context_blocked FROM assistant_control WHERE id=1",
                    [],
                    |r| Ok((r.get::<_, bool>(0)?, r.get::<_, bool>(1)?))
                )
                .unwrap(),
            (true, true)
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT calls FROM assistant_background_roots WHERE id='charged-root'",
                    [],
                    |r| r.get::<_, u64>(0)
                )
                .unwrap(),
            4
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT COUNT(*) FROM assistant_control_recovered",
                    [],
                    |r| r.get::<_, u64>(0)
                )
                .unwrap(),
            1
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT COUNT(*) FROM assistant_board_revocations",
                    [],
                    |r| r.get::<_, u64>(0)
                )
                .unwrap(),
            2
        );
        cleanup(&root, 1).unwrap();
    }

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
        let maintenance = root.join("maintenance-runtime.sqlite");
        let maintenance_conn = Connection::open(&maintenance).unwrap();
        maintenance_conn.execute_batch("CREATE TABLE assistant_runtime_turns(prompt TEXT,reply TEXT,dependencies TEXT,state TEXT); CREATE TABLE assistant_runtime_guard(id INTEGER PRIMARY KEY,blocked INTEGER); INSERT INTO assistant_runtime_turns VALUES('maintenance sentinel','private response','[1]','completed'); INSERT INTO assistant_runtime_guard VALUES(1,0);").unwrap();
        drop(maintenance_conn);
        #[cfg(unix)]
        fs::set_permissions(&maintenance, fs::Permissions::from_mode(0o600)).unwrap();
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
        let maintenance_conn = Connection::open(maintenance).unwrap();
        let row: (String, String, String) = maintenance_conn
            .query_row(
                "SELECT prompt,reply,dependencies FROM assistant_runtime_turns",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(row, (String::new(), String::new(), "[]".into()));
    }
}
