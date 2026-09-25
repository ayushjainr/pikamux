//! Durable, fail-closed policy primitives for the persistent assistant.
//!
//! This module deliberately has no provider or scheduler knowledge.  An adapter
//! reserves work here before dispatch and records the receipt afterwards.  The
//! database is separate from Pika's operational store so a policy failure
//! cannot make an agent appear to have stopped (or vice versa).

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use thiserror::Error;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS policy_config (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    background_calls INTEGER NOT NULL DEFAULT 0,
    max_concurrent INTEGER NOT NULL DEFAULT 2,
    max_total_calls INTEGER NOT NULL DEFAULT 0,
    default_deadline_seconds INTEGER NOT NULL DEFAULT 120
);
INSERT OR IGNORE INTO policy_config(id) VALUES (1);
CREATE TABLE IF NOT EXISTS assistant_reservations (
    id TEXT PRIMARY KEY,
    root_id TEXT NOT NULL,
    parent_id TEXT,
    calls INTEGER NOT NULL CHECK (calls > 0),
    deadline_at INTEGER NOT NULL,
    background INTEGER NOT NULL CHECK (background IN (0,1)),
    state TEXT NOT NULL CHECK (state IN ('reserved','dispatched','completed','failed','unknown','released')),
    created_at INTEGER NOT NULL,
    dispatched_at INTEGER,
    completed_at INTEGER,
    outcome TEXT
);
CREATE INDEX IF NOT EXISTS reservations_root ON assistant_reservations(root_id);
CREATE TABLE IF NOT EXISTS assistant_grants (
    id TEXT PRIMARY KEY,
    provider TEXT NOT NULL,
    scope TEXT NOT NULL,
    capability TEXT NOT NULL,
    expires_at INTEGER NOT NULL,
    revoked_at INTEGER,
    created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS assistant_approvals (
    id TEXT PRIMARY KEY,
    grant_id TEXT NOT NULL REFERENCES assistant_grants(id),
    provider TEXT NOT NULL,
    scope TEXT NOT NULL,
    capability TEXT NOT NULL,
    target TEXT NOT NULL,
    payload_hash TEXT NOT NULL,
    version TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    used_at INTEGER
);
CREATE TABLE IF NOT EXISTS assistant_receipts (
    reservation_id TEXT PRIMARY KEY REFERENCES assistant_reservations(id),
    outcome TEXT NOT NULL,
    recorded_at INTEGER NOT NULL
);
"#;

#[derive(Debug, Error)]
pub enum PolicyError {
    #[error("policy database: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("policy filesystem: {0}")]
    Filesystem(#[from] std::io::Error),
    #[error("policy denied: {0}")]
    Denied(String),
    #[error("unknown reservation {0}")]
    UnknownReservation(String),
    #[error("unknown grant {0}")]
    UnknownGrant(String),
    #[error("unknown approval {0}")]
    UnknownApproval(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReservationState {
    Reserved,
    Dispatched,
    Completed,
    Failed,
    Unknown,
    Released,
}

impl ReservationState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Reserved => "reserved",
            Self::Dispatched => "dispatched",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Unknown => "unknown",
            Self::Released => "released",
        }
    }
    fn parse(value: &str) -> Self {
        match value {
            "dispatched" => Self::Dispatched,
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            "unknown" => Self::Unknown,
            "released" => Self::Released,
            _ => Self::Reserved,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reservation {
    pub id: String,
    pub root_id: String,
    pub parent_id: Option<String>,
    pub calls: u64,
    pub deadline_at: i64,
    pub background: bool,
    pub state: ReservationState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryOutcome {
    Completed,
    Failed,
    Unknown,
}
impl DeliveryOutcome {
    fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyConfig {
    pub background_calls: u64,
    pub max_concurrent: u64,
    pub max_total_calls: u64,
    pub default_deadline_seconds: u64,
}
impl Default for PolicyConfig {
    fn default() -> Self {
        Self {
            background_calls: 0,
            max_concurrent: 2,
            max_total_calls: 0,
            default_deadline_seconds: 120,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    pub id: String,
    pub provider: String,
    pub scope: String,
    pub capability: String,
    pub expires_at: i64,
    pub revoked_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionApproval {
    pub id: String,
    pub grant_id: String,
    pub provider: String,
    pub scope: String,
    pub capability: String,
    pub target: String,
    pub payload_hash: String,
    pub version: String,
}

#[derive(Debug)]
pub struct AssistantPolicy {
    path: PathBuf,
    conn: Connection,
}

impl AssistantPolicy {
    /// Open (or create) the policy store at exactly `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, PolicyError> {
        let path = path.as_ref().to_path_buf();
        crate::assistant_storage::database(&path)?;
        let conn = Connection::open(&path)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch("PRAGMA foreign_keys=ON; PRAGMA journal_mode=WAL;")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { path, conn })
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn configure(&mut self, config: &PolicyConfig) -> Result<(), PolicyError> {
        for value in [
            config.background_calls,
            config.max_concurrent,
            config.max_total_calls,
            config.default_deadline_seconds,
        ] {
            if value > i64::MAX as u64 {
                return Err(PolicyError::Denied(
                    "policy limit exceeds SQLite integer range".into(),
                ));
            }
        }
        if config.max_concurrent == 0 || config.default_deadline_seconds == 0 {
            return Err(PolicyError::Denied(
                "concurrency and deadline limits must be positive".into(),
            ));
        }
        self.conn.execute("UPDATE policy_config SET background_calls=?, max_concurrent=?, max_total_calls=?, default_deadline_seconds=? WHERE id=1", params![config.background_calls as i64, config.max_concurrent as i64, config.max_total_calls as i64, config.default_deadline_seconds as i64])?;
        Ok(())
    }
    pub fn config(&self) -> Result<PolicyConfig, PolicyError> {
        Ok(self.conn.query_row("SELECT background_calls,max_concurrent,max_total_calls,default_deadline_seconds FROM policy_config WHERE id=1", [], |r| Ok(PolicyConfig { background_calls: r.get::<_, i64>(0)? as u64, max_concurrent: r.get::<_, i64>(1)? as u64, max_total_calls: r.get::<_, i64>(2)? as u64, default_deadline_seconds: r.get::<_, i64>(3)? as u64 }))?)
    }
    /// Reserve a root before dispatch. `now` is supplied by the caller for deterministic tests.
    pub fn reserve_root(
        &mut self,
        id: &str,
        calls: u64,
        background: bool,
        now: i64,
        deadline_at: Option<i64>,
    ) -> Result<Reservation, PolicyError> {
        self.reserve(id, None, id, calls, background, now, deadline_at)
    }
    /// Reserve a child from an existing root. Children cannot themselves delegate.
    pub fn reserve_child(
        &mut self,
        id: &str,
        parent_id: &str,
        calls: u64,
        now: i64,
        deadline_at: Option<i64>,
    ) -> Result<Reservation, PolicyError> {
        let parent = self
            .reservation(parent_id)?
            .ok_or_else(|| PolicyError::UnknownReservation(parent_id.into()))?;
        if parent.parent_id.is_some() {
            return Err(PolicyError::Denied("nested delegation is disabled".into()));
        }
        self.reserve(
            id,
            Some(parent_id),
            &parent.root_id,
            calls,
            parent.background,
            now,
            Some(deadline_at.unwrap_or(parent.deadline_at)),
        )
    }
    #[allow(clippy::too_many_arguments)]
    fn reserve(
        &mut self,
        id: &str,
        parent_id: Option<&str>,
        root_id: &str,
        calls: u64,
        background: bool,
        now: i64,
        deadline_at: Option<i64>,
    ) -> Result<Reservation, PolicyError> {
        if calls == 0 {
            return Err(PolicyError::Denied(
                "reservation must contain at least one call".into(),
            ));
        }
        if calls > i64::MAX as u64 {
            return Err(PolicyError::Denied(
                "reservation exceeds SQLite integer range".into(),
            ));
        }
        let cfg = self.config()?;
        let deadline =
            deadline_at.unwrap_or(now.saturating_add(cfg.default_deadline_seconds as i64));
        if deadline <= now {
            return Err(PolicyError::Denied("deadline is expired".into()));
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let duplicate: Option<String> = tx
            .query_row(
                "SELECT id FROM assistant_reservations WHERE id=?",
                [id],
                |r| r.get(0),
            )
            .optional()?;
        if duplicate.is_some() {
            return Err(PolicyError::Denied("reservation id already exists".into()));
        }
        if let Some(parent) = parent_id {
            let (parent_root, parent_deadline, parent_state, parent_calls): (String, i64, String, i64) = tx.query_row(
                "SELECT root_id,deadline_at,state,calls FROM assistant_reservations WHERE id=? AND parent_id IS NULL",
                [parent], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
                .optional()?.ok_or_else(|| PolicyError::UnknownReservation(parent.into()))?;
            if parent_root != root_id
                || parent_state != "reserved"
                || parent_deadline <= now
                || deadline > parent_deadline
            {
                return Err(PolicyError::Denied(
                    "parent reservation is expired or no longer active".into(),
                ));
            }
            let children: i64 = tx.query_row("SELECT COALESCE(SUM(calls),0) FROM assistant_reservations WHERE root_id=? AND parent_id IS NOT NULL AND state <> 'released'", [root_id], |r| r.get(0))?;
            if children.saturating_add(calls as i64) > parent_calls {
                return Err(PolicyError::Denied(
                    "child reservations exceed the root budget".into(),
                ));
            }
        }
        if background && parent_id.is_none() {
            let used: i64 = tx.query_row("SELECT COALESCE(SUM(calls),0) FROM assistant_reservations WHERE background=1 AND parent_id IS NULL AND state <> 'released'", [], |r| r.get(0))?;
            if used.saturating_add(calls as i64) > cfg.background_calls as i64 {
                return Err(PolicyError::Denied(
                    "background allowance exhausted (background is disabled by default)".into(),
                ));
            }
        }
        if parent_id.is_none() {
            let total: i64 = tx.query_row("SELECT COALESCE(SUM(calls),0) FROM assistant_reservations WHERE parent_id IS NULL AND state <> 'released'", [], |r| r.get(0))?;
            if cfg.max_total_calls == 0
                || total.saturating_add(calls as i64) > cfg.max_total_calls as i64
            {
                return Err(PolicyError::Denied("total call allowance exhausted".into()));
            }
        }
        let active: i64 = tx.query_row(
            "SELECT COUNT(*) FROM assistant_reservations r WHERE state IN ('reserved','dispatched') AND (? IS NULL OR r.id <> ?) AND NOT EXISTS (SELECT 1 FROM assistant_reservations child WHERE child.parent_id=r.id)",
            params![parent_id, parent_id],
            |r| r.get(0),
        )?;
        if active >= cfg.max_concurrent as i64 {
            return Err(PolicyError::Denied(
                "concurrent reservation limit reached".into(),
            ));
        }
        tx.execute("INSERT INTO assistant_reservations(id,root_id,parent_id,calls,deadline_at,background,state,created_at) VALUES (?,?,?,?,?,?,?,?)", params![id, root_id, parent_id, calls as i64, deadline, background as i64, ReservationState::Reserved.as_str(), now])?;
        tx.commit()?;
        Ok(Reservation {
            id: id.into(),
            root_id: root_id.into(),
            parent_id: parent_id.map(str::to_owned),
            calls,
            deadline_at: deadline,
            background,
            state: ReservationState::Reserved,
        })
    }
    pub fn mark_dispatched(&mut self, id: &str, now: i64) -> Result<(), PolicyError> {
        self.transition(id, ReservationState::Dispatched, now, None, false)
    }
    pub fn record_outcome(
        &mut self,
        id: &str,
        outcome: DeliveryOutcome,
        now: i64,
    ) -> Result<(), PolicyError> {
        self.transition(
            id,
            ReservationState::parse(outcome.as_str()),
            now,
            Some(outcome.as_str()),
            true,
        )
    }
    /// Release is valid only before dispatch; an unknown provider delivery is never refunded.
    pub fn release_before_dispatch(&mut self, id: &str, now: i64) -> Result<(), PolicyError> {
        self.transition(id, ReservationState::Released, now, None, false)
    }
    fn transition(
        &mut self,
        id: &str,
        state: ReservationState,
        now: i64,
        outcome: Option<&str>,
        receipt: bool,
    ) -> Result<(), PolicyError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let old: Option<(String, i64)> = tx
            .query_row(
                "SELECT state,deadline_at FROM assistant_reservations WHERE id=?",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let (old, deadline) = old.ok_or_else(|| PolicyError::UnknownReservation(id.into()))?;
        // A root is either a direct call allocation or a non-billable envelope,
        // never both. Once it delegates, only children may dispatch or refund.
        let children: i64 = tx.query_row(
            "SELECT COUNT(*) FROM assistant_reservations WHERE parent_id=?",
            [id],
            |r| r.get(0),
        )?;
        if children > 0
            && matches!(
                state,
                ReservationState::Released | ReservationState::Dispatched
            )
        {
            return Err(PolicyError::Denied(
                "a delegated root cannot dispatch or refund its children's allowance".into(),
            ));
        }
        if state == ReservationState::Dispatched {
            let invalid_parent: i64 = tx.query_row("SELECT COUNT(*) FROM assistant_reservations child JOIN assistant_reservations parent ON parent.id=child.parent_id WHERE child.id=? AND (parent.state <> 'reserved' OR parent.deadline_at <= ?)", params![id, now], |r| r.get(0))?;
            if invalid_parent > 0 {
                return Err(PolicyError::Denied("root is no longer active".into()));
            }
            let allowed: i64 = tx.query_row(
                "SELECT max_total_calls FROM policy_config WHERE id=1",
                [],
                |r| r.get(0),
            )?;
            let total: i64 = tx.query_row("SELECT COALESCE(SUM(calls),0) FROM assistant_reservations WHERE parent_id IS NULL AND state <> 'released'", [], |r| r.get(0))?;
            if allowed == 0 || total > allowed {
                return Err(PolicyError::Denied(
                    "call allowance revoked or reduced".into(),
                ));
            }
            let background: bool = tx.query_row(
                "SELECT background FROM assistant_reservations WHERE id=?",
                [id],
                |r| r.get(0),
            )?;
            if background {
                let allowed: i64 = tx.query_row(
                    "SELECT background_calls FROM policy_config WHERE id=1",
                    [],
                    |r| r.get(0),
                )?;
                let charged: i64 = tx.query_row("SELECT COALESCE(SUM(calls),0) FROM assistant_reservations WHERE background=1 AND parent_id IS NULL AND state <> 'released'", [], |r| r.get(0))?;
                if allowed <= 0 || charged > allowed {
                    return Err(PolicyError::Denied(
                        "background allowance revoked or reduced".into(),
                    ));
                }
            }
        }
        if state == ReservationState::Released && old != "reserved" {
            return Err(PolicyError::Denied(
                "only an undispatched reservation can be released".into(),
            ));
        }
        if state == ReservationState::Dispatched && old != "reserved" {
            return Err(PolicyError::Denied(
                "reservation was already dispatched or finalized".into(),
            ));
        }
        if state == ReservationState::Dispatched && deadline <= now {
            return Err(PolicyError::Denied(
                "reservation deadline has expired".into(),
            ));
        }
        if receipt && !matches!(old.as_str(), "reserved" | "dispatched") {
            return Err(PolicyError::Denied(
                "reservation is already finalized".into(),
            ));
        }
        tx.execute("UPDATE assistant_reservations SET state=?, dispatched_at=CASE WHEN ?='dispatched' THEN ? ELSE dispatched_at END, completed_at=CASE WHEN ? IN ('completed','failed','unknown','released') THEN ? ELSE completed_at END, outcome=COALESCE(?,outcome) WHERE id=?", params![state.as_str(), state.as_str(), now, state.as_str(), now, outcome, id])?;
        if let Some(outcome) = outcome {
            tx.execute(
                "INSERT INTO assistant_receipts(reservation_id,outcome,recorded_at) VALUES (?,?,?)",
                params![id, outcome, now],
            )?;
        }
        tx.commit()?;
        Ok(())
    }
    pub fn reservation(&self, id: &str) -> Result<Option<Reservation>, PolicyError> {
        self.conn.query_row("SELECT id,root_id,parent_id,calls,deadline_at,background,state FROM assistant_reservations WHERE id=?", [id], |r| Ok(Reservation { id:r.get(0)?, root_id:r.get(1)?, parent_id:r.get(2)?, calls:r.get::<_,i64>(3)? as u64, deadline_at:r.get(4)?, background:r.get::<_,i64>(5)? != 0, state:ReservationState::parse(&r.get::<_,String>(6)?) })).optional().map_err(Into::into)
    }
    pub fn retry_allowed(&self, id: &str) -> Result<bool, PolicyError> {
        Ok(self
            .reservation(id)?
            .is_some_and(|r| matches!(r.state, ReservationState::Reserved)))
    }

    pub fn grant(&mut self, grant: &Grant) -> Result<(), PolicyError> {
        self.conn.execute("INSERT INTO assistant_grants(id,provider,scope,capability,expires_at,revoked_at,created_at) VALUES (?,?,?,?,?,?,?)", params![grant.id,grant.provider,grant.scope,grant.capability,grant.expires_at,grant.revoked_at,grant.expires_at])?;
        Ok(())
    }
    pub fn revoke_grant(&mut self, id: &str, now: i64) -> Result<(), PolicyError> {
        let n = self.conn.execute(
            "UPDATE assistant_grants SET revoked_at=? WHERE id=? AND revoked_at IS NULL",
            params![now, id],
        )?;
        if n == 0 {
            Err(PolicyError::UnknownGrant(id.into()))
        } else {
            Ok(())
        }
    }
    pub fn approve_action(
        &mut self,
        approval: &ActionApproval,
        now: i64,
    ) -> Result<(), PolicyError> {
        let g = self
            .grant_row(&approval.grant_id)?
            .ok_or_else(|| PolicyError::UnknownGrant(approval.grant_id.clone()))?;
        if !grant_matches(
            &g,
            &approval.provider,
            &approval.scope,
            &approval.capability,
            now,
        ) {
            return Err(PolicyError::Denied(
                "grant is expired, revoked, or scope/provider/capability mismatches".into(),
            ));
        }
        self.conn.execute("INSERT INTO assistant_approvals(id,grant_id,provider,scope,capability,target,payload_hash,version,created_at) VALUES (?,?,?,?,?,?,?,?,?)", params![approval.id,approval.grant_id,approval.provider,approval.scope,approval.capability,approval.target,approval.payload_hash,approval.version,now])?;
        Ok(())
    }
    #[allow(clippy::too_many_arguments)]
    pub fn validate_action(
        &mut self,
        approval_id: &str,
        provider: &str,
        scope: &str,
        capability: &str,
        target: &str,
        payload: &str,
        version: &str,
        now: i64,
    ) -> Result<(), PolicyError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let row: ActionApproval = tx.query_row("SELECT id,grant_id,provider,scope,capability,target,payload_hash,version FROM assistant_approvals WHERE id=? AND used_at IS NULL", [approval_id], |r| Ok(ActionApproval { id:r.get(0)?, grant_id:r.get(1)?, provider:r.get(2)?, scope:r.get(3)?, capability:r.get(4)?, target:r.get(5)?, payload_hash:r.get(6)?, version:r.get(7)? })).optional()?.ok_or_else(|| PolicyError::UnknownApproval(approval_id.into()))?;
        let grant: Grant = tx.query_row("SELECT id,provider,scope,capability,expires_at,revoked_at FROM assistant_grants WHERE id=?", [&row.grant_id], |r| Ok(Grant { id:r.get(0)?, provider:r.get(1)?, scope:r.get(2)?, capability:r.get(3)?, expires_at:r.get(4)?, revoked_at:r.get(5)? })).optional()?.ok_or_else(|| PolicyError::UnknownGrant(row.grant_id.clone()))?;
        if !grant_matches(&grant, provider, scope, capability, now)
            || row.provider != provider
            || row.scope != scope
            || row.capability != capability
            || row.target != target
            || row.version != version
            || row.payload_hash != payload_hash(payload)
        {
            return Err(PolicyError::Denied(
                "approval does not exactly match the current action".into(),
            ));
        }
        let changed = tx.execute(
            "UPDATE assistant_approvals SET used_at=? WHERE id=? AND used_at IS NULL",
            params![now, approval_id],
        )?;
        if changed != 1 {
            return Err(PolicyError::Denied(
                "approval was concurrently consumed".into(),
            ));
        }
        tx.commit()?;
        Ok(())
    }
    fn grant_row(&self, id: &str) -> Result<Option<Grant>, PolicyError> {
        self.conn.query_row("SELECT id,provider,scope,capability,expires_at,revoked_at FROM assistant_grants WHERE id=?",[id],|r|Ok(Grant{id:r.get(0)?,provider:r.get(1)?,scope:r.get(2)?,capability:r.get(3)?,expires_at:r.get(4)?,revoked_at:r.get(5)?})).optional().map_err(Into::into)
    }
}

fn grant_matches(g: &Grant, provider: &str, scope: &str, capability: &str, now: i64) -> bool {
    g.provider == provider
        && g.scope == scope
        && g.capability == capability
        && g.revoked_at.is_none()
        && now < g.expires_at
}
pub fn payload_hash(payload: &str) -> String {
    let mut h = Sha256::new();
    h.update(payload.as_bytes());
    format!("{:x}", h.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    fn db() -> AssistantPolicy {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        AssistantPolicy {
            path: PathBuf::from("in-memory-fixture"),
            conn,
        }
    }
    #[test]
    fn zero_background_default_and_nested_rejected() {
        let mut p = db();
        assert_eq!(p.config().unwrap().background_calls, 0);
        assert!(p.reserve_root("b", 1, true, 1, None).is_err());
        p.configure(&PolicyConfig {
            background_calls: 3,
            max_concurrent: 2,
            max_total_calls: 5,
            default_deadline_seconds: 10,
        })
        .unwrap();
        p.reserve_root("r", 1, true, 1, None).unwrap();
        assert!(p.reserve_child("c", "c", 1, 1, None).is_err());
        assert!(p.reserve_child("nested", "c", 1, 1, None).is_err());
    }
    #[test]
    fn unknown_survives_restart_without_retry() {
        let d = tempdir().unwrap();
        let path = d.path().join("private/p.sqlite");
        {
            let mut p = AssistantPolicy::open(&path).unwrap();
            p.configure(&PolicyConfig {
                background_calls: 3,
                max_concurrent: 2,
                max_total_calls: 5,
                default_deadline_seconds: 10,
            })
            .unwrap();
            p.reserve_root("r", 1, true, 1, None).unwrap();
            p.mark_dispatched("r", 2).unwrap();
            p.record_outcome("r", DeliveryOutcome::Unknown, 3).unwrap();
        }
        let p = AssistantPolicy::open(&path).unwrap();
        assert_eq!(
            p.reservation("r").unwrap().unwrap().state,
            ReservationState::Unknown
        );
        assert!(!p.retry_allowed("r").unwrap());
    }
    #[test]
    fn grant_revocation_scope_and_payload_are_exact() {
        let mut p = db();
        p.grant(&Grant {
            id: "g".into(),
            provider: "codex".into(),
            scope: "project:a".into(),
            capability: "read".into(),
            expires_at: 20,
            revoked_at: None,
        })
        .unwrap();
        let a = ActionApproval {
            id: "a".into(),
            grant_id: "g".into(),
            provider: "codex".into(),
            scope: "project:a".into(),
            capability: "read".into(),
            target: "file".into(),
            payload_hash: payload_hash("x"),
            version: "v1".into(),
        };
        p.approve_action(&a, 1).unwrap();
        assert!(
            p.validate_action("a", "codex", "project:b", "read", "file", "x", "v1", 2)
                .is_err()
        );
        p.revoke_grant("g", 3).unwrap();
        assert!(
            p.validate_action("a", "codex", "project:a", "read", "file", "x", "v1", 4)
                .is_err()
        );
    }
    #[test]
    fn concurrent_and_total_limits() {
        let mut p = db();
        p.configure(&PolicyConfig {
            background_calls: 0,
            max_concurrent: 1,
            max_total_calls: 1,
            default_deadline_seconds: 10,
        })
        .unwrap();
        p.reserve_root("a", 1, false, 1, None).unwrap();
        assert!(p.reserve_root("b", 1, false, 1, None).is_err());
        p.record_outcome("a", DeliveryOutcome::Completed, 2)
            .unwrap();
        assert!(p.reserve_root("b", 1, false, 3, None).is_err());
    }
    #[test]
    fn simultaneous_processes_cannot_overreserve() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("private/concurrent.sqlite");
        {
            let mut policy = AssistantPolicy::open(&path).unwrap();
            policy
                .configure(&PolicyConfig {
                    max_concurrent: 1,
                    max_total_calls: 2,
                    ..PolicyConfig::default()
                })
                .unwrap();
        }
        let left = path.clone();
        let right = path.clone();
        let first = std::thread::spawn(move || {
            AssistantPolicy::open(left)
                .unwrap()
                .reserve_root("left", 1, false, 1, Some(100))
                .is_ok()
        });
        let second = std::thread::spawn(move || {
            AssistantPolicy::open(right)
                .unwrap()
                .reserve_root("right", 1, false, 1, Some(100))
                .is_ok()
        });
        assert_eq!(
            u8::from(first.join().unwrap()) + u8::from(second.join().unwrap()),
            1
        );
    }
    #[test]
    fn children_share_root_budget_without_double_counting() {
        let mut policy = db();
        policy
            .configure(&PolicyConfig {
                max_total_calls: 2,
                max_concurrent: 3,
                ..PolicyConfig::default()
            })
            .unwrap();
        policy.reserve_root("root", 2, false, 1, Some(100)).unwrap();
        policy.reserve_child("one", "root", 1, 2, Some(90)).unwrap();
        policy.reserve_child("two", "root", 1, 2, Some(90)).unwrap();
        assert!(
            policy
                .reserve_child("three", "root", 1, 2, Some(90))
                .is_err()
        );
    }
    #[test]
    fn delegated_root_cannot_refund_or_spend_child_allowance() {
        let mut policy = db();
        policy
            .configure(&PolicyConfig {
                max_total_calls: 1,
                max_concurrent: 3,
                ..PolicyConfig::default()
            })
            .unwrap();
        policy.reserve_root("root", 1, false, 1, Some(100)).unwrap();
        policy
            .reserve_child("child", "root", 1, 2, Some(90))
            .unwrap();
        assert!(policy.mark_dispatched("root", 3).is_err());
        policy.mark_dispatched("child", 3).unwrap();
        assert!(policy.release_before_dispatch("root", 4).is_err());
        policy
            .record_outcome("child", DeliveryOutcome::Failed, 5)
            .unwrap();
        assert!(policy.reserve_root("another", 1, false, 6, None).is_err());
    }
    #[test]
    fn background_revocation_blocks_reserved_root_and_child() {
        for delegated in [false, true] {
            let mut policy = db();
            let mut config = PolicyConfig {
                background_calls: 1,
                max_total_calls: 1,
                ..Default::default()
            };
            policy.configure(&config).unwrap();
            policy.reserve_root("root", 1, true, 1, Some(100)).unwrap();
            let target = if delegated {
                policy.reserve_child("child", "root", 1, 2, None).unwrap();
                "child"
            } else {
                "root"
            };
            config.background_calls = 0;
            policy.configure(&config).unwrap();
            assert!(policy.mark_dispatched(target, 3).is_err());
            assert_eq!(
                policy.reservation(target).unwrap().unwrap().state,
                ReservationState::Reserved
            );
        }
    }
}
