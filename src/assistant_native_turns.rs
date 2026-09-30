//! Native CLI turn receipts, not a replacement inference loop.
//! A CLI turn may contain several model requests. Hooks cannot measure or bound
//! those requests; unknown counts must never be presented as paid-call counts.
use anyhow::{Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use std::{
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Debug, Serialize)]
pub(crate) struct Receipt {
    pub request_id: String,
    pub session_id: String,
    pub state: String,
    pub model_calls: Option<u64>,
}

fn journal(root: &Path) -> Result<Connection> {
    let path = root.join("runtime.sqlite");
    crate::assistant_storage::database(&path)?;
    let db = Connection::open(path)?;
    db.busy_timeout(std::time::Duration::from_secs(5))?;
    db.execute_batch("CREATE TABLE IF NOT EXISTS assistant_native_turns(
        request_id TEXT PRIMARY KEY, session_id TEXT NOT NULL, scope TEXT NOT NULL,
        state TEXT NOT NULL CHECK(state IN ('intent','dispatched','completed','unknown','acknowledged')),
        model_calls INTEGER, updated_at INTEGER NOT NULL);")?;
    Ok(db)
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .min(i64::MAX as u64) as i64
}

fn exact(request: &str, session: &str) -> Result<()> {
    if request.is_empty() || request.len() > 256 || request.chars().any(char::is_control) {
        bail!("Native hook requires the exact provider turn_id");
    }
    uuid::Uuid::parse_str(session)?;
    Ok(())
}

/// Authority-side call only, after authenticating a genuine provider hook and
/// its exact registered session. No model request is made by this function.
pub(crate) fn before_submit(
    root: &Path,
    scope: &str,
    request: &str,
    session: &str,
) -> Result<Receipt> {
    exact(request, session)?;
    let memory = crate::assistant_memory::Store::open(root.join("memory.sqlite"))?;
    crate::assistant_native_recovery::require_context(
        root,
        memory.profile_id(),
        scope,
        Some(session),
    )?;
    drop(memory);
    let control = crate::assistant_control::Controller::attach(root)?;
    control.require_foreground(scope)?;
    let policy = crate::assistant_policy::AssistantPolicy::open(root.join("policy.sqlite"))?;
    // Serialize with existing maintenance reservations without inventing a
    // positive model-call reservation for an unmeasured CLI turn.
    crate::assistant_storage::existing_database(policy.path())?;
    let mut shared = Connection::open(policy.path())?;
    shared.busy_timeout(std::time::Duration::from_secs(5))?;
    dispatch_under_policy(&mut shared, root, scope, request, session, &control)
}

fn dispatch_under_policy(
    shared: &mut Connection,
    root: &Path,
    scope: &str,
    request: &str,
    session: &str,
    control: &crate::assistant_control::Controller,
) -> Result<Receipt> {
    let _guard = shared.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let (max_calls, max_concurrent): (u64, u64) = _guard.query_row(
        "SELECT max_total_calls,max_concurrent FROM policy_config WHERE id=1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    if max_calls != crate::assistant_policy::NO_CALL_LIMIT {
        bail!(
            "Native Codex hooks cannot enforce a hard model-call limit: a user turn may contain multiple model requests. Native dispatch requires the explicitly selected unlimited policy; finite max_calls is unsupported"
        );
    }
    control.require_foreground(scope)?;
    require_capacity(&_guard, max_concurrent)?;
    let mut db = journal(root)?;
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    record_intent(&tx, request, session, scope)?;
    tx.commit()?;
    // A crash between durable intent and this transition remains unresolved.
    db.execute("UPDATE assistant_native_turns SET state='dispatched',updated_at=? WHERE request_id=? AND state='intent'", params![now(),request])?;
    Ok(Receipt {
        request_id: request.into(),
        session_id: session.into(),
        state: "dispatched".into(),
        model_calls: None,
    })
}

fn require_capacity(policy: &Connection, max_concurrent: u64) -> Result<()> {
    let occupied: u64 = policy.query_row(
        "SELECT COUNT(*) FROM assistant_reservations WHERE state IN ('reserved','dispatched')",
        [],
        |r| r.get(0),
    )?;
    if occupied >= max_concurrent {
        bail!("Shared assistant dispatch concurrency is occupied");
    }
    Ok(())
}

fn record_intent(tx: &Connection, request: &str, session: &str, scope: &str) -> Result<()> {
    let duplicate: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM assistant_native_turns WHERE request_id=?)",
        [request],
        |r| r.get(0),
    )?;
    if duplicate {
        bail!("Native turn already has a durable dispatch receipt; automatic replay is forbidden");
    }
    let unfinished: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM assistant_native_turns WHERE state IN ('intent','dispatched','unknown'))", [], |r| r.get(0))?;
    if unfinished {
        bail!("Native turn delivery is unfinished or unknown; fresh-context recovery is required");
    }
    tx.execute(
        "INSERT INTO assistant_native_turns VALUES(?,?,?,'intent',NULL,?)",
        params![request, session, scope, now()],
    )?;
    Ok(())
}

/// Stop confirms the CLI turn ended, not how many inference calls it made.
pub(crate) fn close_adapter(
    root: &Path,
    scope: &str,
    session: &str,
    request: &str,
) -> Result<serde_json::Value> {
    exact(request, session)?;
    let mut db = journal(root)?;
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let changed = tx.execute("UPDATE assistant_native_turns SET state='unknown',updated_at=? WHERE scope=? AND session_id=? AND request_id=? AND state IN ('intent','dispatched')", params![now(),scope,session,request])?;
    tx.commit()?;
    Ok(serde_json::json!({"closed_native_turns":changed,"model_calls":null,"replay_allowed":false}))
}

/// Stop confirms the CLI turn ended, not how many inference calls it made.
/// Interrupt is ambiguous delivery and is never a refund or replay allowance.
pub(crate) fn finish(
    root: &Path,
    scope: &str,
    request: &str,
    session: &str,
    interrupted: bool,
) -> Result<Receipt> {
    exact(request, session)?;
    let mut db = journal(root)?;
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let prior: Option<String> = tx.query_row("SELECT state FROM assistant_native_turns WHERE request_id=? AND session_id=? AND scope=?", params![request,session,scope], |r| r.get(0)).optional()?;
    let Some(prior) = prior else {
        bail!("Native outcome does not match an exact durable turn receipt");
    };
    // Unknown/acknowledged outcomes cannot be upgraded by a late Stop callback.
    let state = if prior == "unknown" || prior == "acknowledged" || prior == "completed" {
        prior
    } else if interrupted {
        "unknown".into()
    } else {
        "completed".into()
    };
    tx.execute(
        "UPDATE assistant_native_turns SET state=?,updated_at=? WHERE request_id=?",
        params![state, now(), request],
    )?;
    tx.commit()?;
    Ok(Receipt {
        request_id: request.into(),
        session_id: session.into(),
        state,
        model_calls: None,
    })
}

pub(crate) fn busy(root: &Path) -> Result<bool> {
    if !root.join("runtime.sqlite").exists() {
        return Ok(false);
    }
    let db = journal(root)?;
    Ok(db.query_row("SELECT EXISTS(SELECT 1 FROM assistant_native_turns WHERE state IN ('intent','dispatched','unknown'))", [], |r| r.get(0))?)
}

pub(crate) fn snapshot(root: &Path, scope: &str) -> Result<serde_json::Value> {
    let db = journal(root)?;
    let latest: Option<Receipt> = db.query_row("SELECT request_id,session_id,state FROM assistant_native_turns WHERE scope=? ORDER BY updated_at DESC,rowid DESC LIMIT 1", [scope], |r| Ok(Receipt { request_id:r.get(0)?,session_id:r.get(1)?,state:r.get(2)?,model_calls:None })).optional()?;
    Ok(
        serde_json::json!({"latest_turn":latest,"model_call_accounting":"unknown: native CLI hooks observe turns, not individual model requests","hard_model_call_limit_supported":false}),
    )
}

/// Existing owner calls this only after verified fresh-context recovery.
/// Preserve every receipt and its unknown model count, never replay it.
pub(crate) fn recovered(root: &Path) -> Result<()> {
    if root.join("runtime.sqlite").exists() {
        journal(root)?.execute("UPDATE assistant_native_turns SET state='acknowledged',updated_at=? WHERE state IN ('intent','dispatched','unknown')", [now()])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn private_root() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        root
    }
    #[test]
    fn admission_honors_pause_finite_budget_and_no_replay() {
        let root = private_root();
        let mut control = crate::assistant_control::Controller::open(root.path()).unwrap();
        let mut policy =
            crate::assistant_policy::AssistantPolicy::open(root.path().join("policy.sqlite"))
                .unwrap();
        let mut config = policy.config().unwrap();
        config.max_total_calls = 3;
        policy.configure(&config).unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        assert!(
            before_submit(root.path(), "scope", "turn", &session)
                .unwrap_err()
                .to_string()
                .contains("hard model-call limit")
        );
        config.max_total_calls = crate::assistant_policy::NO_CALL_LIMIT;
        policy.configure(&config).unwrap();
        control.pause().unwrap();
        assert!(before_submit(root.path(), "scope", "turn", &session).is_err());
        control.resume().unwrap();
        let receipt = before_submit(root.path(), "scope", "turn", &session).unwrap();
        assert_eq!(receipt.model_calls, None);
        assert!(before_submit(root.path(), "scope", "next", &session).is_err());
        finish(root.path(), "scope", "turn", &session, false).unwrap();
        assert!(before_submit(root.path(), "scope", "turn", &session).is_err());
        assert!(before_submit(root.path(), "scope", "next", &session).is_ok());
    }
    #[test]
    fn forgotten_native_context_cannot_admit_new_turn_even_with_unlimited_policy() {
        let root = private_root();
        let _control = crate::assistant_control::Controller::open(root.path()).unwrap();
        let mut policy =
            crate::assistant_policy::AssistantPolicy::open(root.path().join("policy.sqlite"))
                .unwrap();
        let mut config = policy.config().unwrap();
        config.max_total_calls = crate::assistant_policy::NO_CALL_LIMIT;
        policy.configure(&config).unwrap();
        let mut memory =
            crate::assistant_memory::Store::open(root.path().join("memory.sqlite")).unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        crate::assistant_native_recovery::record_context(
            root.path(),
            memory.profile_id(),
            "scope",
            &session,
        )
        .unwrap();
        memory
            .forget_worker_scope(&crate::assistant::scope("scope").unwrap())
            .unwrap();
        assert!(
            before_submit(root.path(), "scope", "after-forget", &session)
                .unwrap_err()
                .to_string()
                .contains("invalidated")
        );
        assert!(snapshot(root.path(), "scope").unwrap()["latest_turn"].is_null());
    }
    fn fixture(state: &str) -> (tempfile::TempDir, String) {
        let root = private_root();
        let session = uuid::Uuid::new_v4().to_string();
        journal(root.path())
            .unwrap()
            .execute(
                "INSERT INTO assistant_native_turns VALUES('turn',?,'scope',?,NULL,1)",
                params![session, state],
            )
            .unwrap();
        (root, session)
    }
    #[test]
    fn delayed_old_adapter_eof_cannot_downgrade_a_newer_turn() {
        let (root, session) = fixture("dispatched");
        // Old adapter observed `turn`; provider Stop completes it and a new
        // user turn is admitted on the same persistent provider UUID.
        finish(root.path(), "scope", "turn", &session, false).unwrap();
        journal(root.path())
            .unwrap()
            .execute(
                "INSERT INTO assistant_native_turns VALUES('newer',?,'scope','dispatched',NULL,?)",
                params![session, now()],
            )
            .unwrap();
        assert_eq!(
            close_adapter(root.path(), "scope", &session, "turn").unwrap()["closed_native_turns"],
            0
        );
        assert_eq!(
            snapshot(root.path(), "scope").unwrap()["latest_turn"]["state"],
            "dispatched"
        );
        assert_eq!(
            close_adapter(root.path(), "scope", &session, "newer").unwrap()["closed_native_turns"],
            1
        );
        assert_eq!(
            snapshot(root.path(), "scope").unwrap()["latest_turn"]["state"],
            "unknown"
        );
    }
    #[test]
    fn stop_is_turn_completion_not_model_accounting() {
        let (root, session) = fixture("dispatched");
        let receipt = finish(root.path(), "scope", "turn", &session, false).unwrap();
        assert_eq!(receipt.state, "completed");
        assert_eq!(receipt.model_calls, None);
        assert!(!busy(root.path()).unwrap());
        assert!(finish(root.path(), "another-scope", "turn", &session, false).is_err());
    }
    #[test]
    fn interrupt_and_crash_are_never_upgraded_or_refunded() {
        let (root, session) = fixture("dispatched");
        assert_eq!(
            finish(root.path(), "scope", "turn", &session, true)
                .unwrap()
                .state,
            "unknown"
        );
        assert_eq!(
            finish(root.path(), "scope", "turn", &session, false)
                .unwrap()
                .state,
            "unknown"
        );
        assert!(busy(root.path()).unwrap());
        recovered(root.path()).unwrap();
        assert!(!busy(root.path()).unwrap());
        assert_eq!(
            finish(root.path(), "scope", "turn", &session, false)
                .unwrap()
                .state,
            "acknowledged"
        );
        let (crash, _) = fixture("intent");
        assert!(busy(crash.path()).unwrap());
    }
}
