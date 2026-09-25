//! Asynchronous explicit recovery boundary.
//!
//! The caller supplies a closure that owns the already-stopped session and
//! investigation services. Recovery cannot report completion until that
//! closure returns and the durable recovery receipt is written.

use crate::assistant_recovery::recover_after_services_dropped;
use rusqlite::Connection;
use serde::Serialize;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::thread::{self, JoinHandle};

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RecoveryServiceSnapshot {
    pub state: String,
    pub receipt_id: String,
    pub error: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum RecoveryServiceError {
    #[error("recovery service root must be absolute")]
    RelativeRoot,
    #[error("recovery request id must be a UUID")]
    InvalidRequestId,
    #[error("recovery epoch exceeds SQLite integer range")]
    EpochOverflow,
    #[error("recovery intent database: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("recovery intent filesystem: {0}")]
    Filesystem(#[from] std::io::Error),
    #[error("recovery service is already busy")]
    Busy,
}

fn intent_db(root: &Path) -> Result<Connection, RecoveryServiceError> {
    let path = root.join("recovery-intents.sqlite");
    crate::assistant_storage::database(&path).map_err(RecoveryServiceError::Filesystem)?;
    let conn = Connection::open(path)?;
    conn.execute_batch("CREATE TABLE IF NOT EXISTS recovery_intents(id TEXT PRIMARY KEY, epoch INTEGER NOT NULL, state TEXT NOT NULL CHECK(state IN ('pending','completed','failed','abandoned')), error TEXT)")?;
    Ok(conn)
}

pub fn has_unfinished(root: impl AsRef<Path>) -> Result<bool, RecoveryServiceError> {
    let root = root.as_ref();
    if !root.is_absolute() {
        return Err(RecoveryServiceError::RelativeRoot);
    }
    if !root.exists() {
        return Ok(false);
    }
    let conn = intent_db(root)?;
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM recovery_intents WHERE state IN ('pending','failed'))",
        [],
        |r| r.get(0),
    )?)
}

pub struct RecoveryService {
    snapshot: Arc<RwLock<RecoveryServiceSnapshot>>,
    join: Option<JoinHandle<()>>,
}

impl RecoveryService {
    pub fn start<F>(
        root: PathBuf,
        request_id: String,
        epoch: u64,
        cleanup: F,
    ) -> Result<Self, RecoveryServiceError>
    where
        F: FnOnce() -> Result<(), String> + Send + 'static,
    {
        if !root.is_absolute() {
            return Err(RecoveryServiceError::RelativeRoot);
        }
        if uuid::Uuid::parse_str(&request_id).is_err() {
            return Err(RecoveryServiceError::InvalidRequestId);
        }
        if epoch > i64::MAX as u64 {
            return Err(RecoveryServiceError::EpochOverflow);
        }
        crate::assistant_storage::directory(&root).map_err(RecoveryServiceError::Filesystem)?;
        let conn = intent_db(&root)?;
        let tx = conn.unchecked_transaction()?;
        tx.execute(
            "UPDATE recovery_intents SET state='abandoned' WHERE state IN ('pending','failed')",
            [],
        )?;
        tx.execute(
            "INSERT INTO recovery_intents(id,epoch,state,error) VALUES(?,?, 'pending',NULL)",
            rusqlite::params![request_id, epoch as i64],
        )?;
        tx.commit()?;
        let snapshot = Arc::new(RwLock::new(RecoveryServiceSnapshot {
            state: "pending".into(),
            receipt_id: String::new(),
            error: None,
        }));
        let published = snapshot.clone();
        let thread_root = root.clone();
        let thread_id = request_id.clone();
        let join = thread::Builder::new().name("pika-recovery".into()).spawn(move || {
            let result = catch_unwind(AssertUnwindSafe(cleanup))
                .map_err(|_| "recovery cleanup panicked".to_owned())
                .and_then(|result| result)
                .and_then(|_| recover_after_services_dropped(&thread_root, &thread_id, epoch).map(|receipt| receipt.receipt_id).map_err(|e| e.to_string()));
            let conn = intent_db(&thread_root);
            let mut state = published.write().expect("recovery snapshot lock");
            match result {
                Ok(receipt_id) => {
                    let durable = conn.and_then(|conn| conn.execute("UPDATE recovery_intents SET state='completed',error=NULL WHERE id=?", [&thread_id]).map(|_| ()).map_err(Into::into));
                    if let Err(error) = durable {
                        state.state = "failed".into();
                        state.error = Some(format!("completion receipt update failed: {error}"));
                        return;
                    }
                    state.state = "completed".into();
                    state.receipt_id = receipt_id;
                }
                Err(error) => {
                    let _ = conn.and_then(|conn| conn.execute("UPDATE recovery_intents SET state='failed',error=? WHERE id=?", rusqlite::params![error, thread_id]).map(|_| ()).map_err(Into::into));
                    state.state = "failed".into();
                    state.error = Some(error);
                }
            }
        }).map_err(RecoveryServiceError::Filesystem)?;
        Ok(Self {
            snapshot,
            join: Some(join),
        })
    }

    pub fn snapshot(&self) -> RecoveryServiceSnapshot {
        self.snapshot
            .read()
            .expect("recovery snapshot lock")
            .clone()
    }
    pub fn busy(&self) -> bool {
        self.snapshot().state == "pending"
    }
}

impl Drop for RecoveryService {
    fn drop(&mut self) {
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};
    use tempfile::tempdir;

    fn fixture() -> (tempfile::TempDir, PathBuf) {
        let dir = tempdir().unwrap();
        let root = dir.path().join("private");
        std::fs::create_dir(&root).unwrap();
        #[cfg(unix)]
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        (dir, root)
    }

    #[test]
    fn pending_success_duplicate_failure_and_panic_paths() {
        let (_dir, root) = fixture();
        let (release_tx, release_rx) = mpsc::channel();
        let service = RecoveryService::start(
            root.clone(),
            "44444444-4444-4444-8444-444444444444".into(),
            0,
            move || {
                release_rx
                    .recv_timeout(Duration::from_secs(3))
                    .map_err(|e| e.to_string())
                    .map(|_| ())
            },
        )
        .unwrap();
        assert!(service.busy());
        assert!(!root.join("recovery.sqlite").exists());
        assert!(
            RecoveryService::start(
                root.clone(),
                "44444444-4444-4444-8444-444444444444".into(),
                0,
                || Ok(())
            )
            .is_err()
        );
        let state: String = intent_db(&root)
            .unwrap()
            .query_row("SELECT state FROM recovery_intents", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            state, "pending",
            "duplicate rejection must not abandon the existing intent"
        );
        release_tx.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while service.busy() && Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert_eq!(service.snapshot().state, "completed");
        assert!(!has_unfinished(&root).unwrap());
        let (_dir, root) = fixture();
        let service = RecoveryService::start(
            root.clone(),
            "55555555-5555-4555-8555-555555555555".into(),
            0,
            || Err("closed".into()),
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while service.busy() && Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert_eq!(service.snapshot().state, "failed");
        assert!(has_unfinished(&root).unwrap());
        let (_dir, root) = fixture();
        let service = RecoveryService::start(
            root.clone(),
            "66666666-6666-4666-8666-666666666666".into(),
            0,
            || panic!("boom"),
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while service.busy() && Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert_eq!(service.snapshot().state, "failed");
        assert!(has_unfinished(&root).unwrap());
        let (_dir, root) = fixture();
        let (release_tx, release_rx) = mpsc::channel();
        let service = RecoveryService::start(
            root.clone(),
            "77777777-7777-4777-8777-777777777777".into(),
            0,
            move || {
                release_rx
                    .recv_timeout(Duration::from_secs(3))
                    .map_err(|e| e.to_string())
                    .map(|_| ())
            },
        )
        .unwrap();
        let conn = Connection::open(root.join("recovery-intents.sqlite")).unwrap();
        conn.execute_batch("CREATE TRIGGER fail_completion BEFORE UPDATE OF state ON recovery_intents WHEN NEW.state='completed' BEGIN SELECT RAISE(FAIL, 'blocked completion'); END").unwrap();
        release_tx.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while service.busy() && Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert_eq!(service.snapshot().state, "failed");
        assert!(has_unfinished(&root).unwrap());
    }
}
