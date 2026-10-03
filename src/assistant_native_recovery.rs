//! Context-generation fence for the real native provider conversation.
//! Memory remains authoritative; retired provider UUIDs are never reusable.
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::path::Path;

fn journal(root: &Path) -> Result<Connection> {
    let path = root.join("runtime.sqlite");
    crate::assistant_storage::database(&path)?;
    let db = Connection::open(path)?;
    db.busy_timeout(std::time::Duration::from_secs(5))?;
    db.execute_batch("CREATE TABLE IF NOT EXISTS assistant_native_contexts(
        thread_id TEXT PRIMARY KEY,profile_id TEXT NOT NULL,scope TEXT NOT NULL,
        memory_epoch INTEGER NOT NULL,state TEXT NOT NULL CHECK(state IN ('active','retiring','retired')),
        retirement_id TEXT);
        CREATE UNIQUE INDEX IF NOT EXISTS assistant_native_one_context ON assistant_native_contexts(profile_id) WHERE state='active';")?;
    Ok(db)
}

fn epoch(root: &Path, profile: &str, scope: &str) -> Result<u64> {
    crate::assistant_host::verify_existing_profile(root, profile)?;
    crate::assistant::scope(scope)?;
    Ok(crate::assistant_memory::Store::open(root.join("memory.sqlite"))?.forget_epoch()?)
}

/// Call on reopen, callback and memory-bearing native tool admission. A missing
/// thread is only a first launch, never permission to bypass an older context.
pub(crate) fn require_context(
    root: &Path,
    profile: &str,
    scope: &str,
    thread: Option<&str>,
) -> Result<()> {
    let current = epoch(root, profile, scope)?;
    let db = journal(root)?;
    if let Some(thread) = thread {
        uuid::Uuid::parse_str(thread)?;
        let other: bool=db.query_row("SELECT EXISTS(SELECT 1 FROM assistant_native_contexts WHERE profile_id=? AND state<>'retired' AND thread_id<>?)",params![profile,thread],|r|r.get(0))?;
        if other {
            bail!("Another native context generation is active or awaiting verified retirement");
        }
    }
    let row = saved_context(&db, profile, thread)?;
    require_saved_context(row, profile, scope, current)?;
    require_owner_context(root)
}

fn saved_context(
    db: &Connection,
    profile: &str,
    thread: Option<&str>,
) -> Result<Option<(String, String, u64, String)>> {
    Ok(if let Some(thread) = thread {
        db.query_row("SELECT profile_id,scope,memory_epoch,state FROM assistant_native_contexts WHERE thread_id=?", [thread], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?
    } else {
        db.query_row("SELECT profile_id,scope,memory_epoch,state FROM assistant_native_contexts WHERE profile_id=? AND state<>'retired'", [profile], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?
    })
}

fn require_saved_context(
    row: Option<(String, String, u64, String)>,
    profile: &str,
    scope: &str,
    current: u64,
) -> Result<()> {
    if let Some((actual, stored_scope, stored_epoch, state)) = row {
        if actual != profile
            || stored_scope != scope
            || stored_epoch != current
            || state != "active"
        {
            bail!(
                "Native Pika context is invalidated or retired; explicitly recover fresh context before reuse"
            );
        }
    }
    Ok(())
}

fn require_owner_context(root: &Path) -> Result<()> {
    // Shared board revocation may invalidate context even without a new epoch.
    let owner = root.join("owner.sqlite");
    if owner.exists() {
        crate::assistant_storage::existing_database(&owner)?;
        let control =
            Connection::open_with_flags(owner, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let exists: bool = control.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='assistant_control')",
            [],
            |r| r.get(0),
        )?;
        if exists
            && control.query_row::<bool, _, _>(
                "SELECT context_blocked FROM assistant_control WHERE id=1",
                [],
                |r| r.get(0),
            )?
        {
            bail!(
                "Native Pika context is blocked by the existing authority; explicit fresh-context recovery is required"
            );
        }
    }
    Ok(())
}

pub(crate) fn record_context(root: &Path, profile: &str, scope: &str, thread: &str) -> Result<u64> {
    uuid::Uuid::parse_str(thread)?;
    require_context(root, profile, scope, Some(thread))?;
    let current = epoch(root, profile, scope)?;
    journal(root)?.execute("INSERT INTO assistant_native_contexts VALUES(?,?,?,?,'active',NULL) ON CONFLICT(thread_id) DO NOTHING",params![thread,profile,scope,current])?;
    Ok(current)
}

/// Old completed requests are receipt reads, never authority over a new binding.
pub(crate) fn completed_receipt(
    root: &Path,
    profile: &str,
    scope: &str,
    request: &str,
) -> Result<Option<Value>> {
    uuid::Uuid::parse_str(request)?;
    let current = epoch(root, profile, scope)?;
    let row: Option<(String,String,String)>=journal(root)?.query_row("SELECT thread_id,profile_id,scope FROM assistant_native_contexts WHERE retirement_id=? AND state='retired'",[request],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
    let Some((thread, actual, stored_scope)) = row else {
        return Ok(None);
    };
    if actual != profile || stored_scope != scope {
        bail!("Native recovery receipt belongs to another profile or scope");
    }
    Ok(Some(
        json!({"recovered":true,"already_recorded":true,"retired_thread":thread,"memory_epoch":current}),
    ))
}

/// An explicit trusted Human request only. The cleanup boundary must validate
/// the exact owned pane generation and refuse busy/ambiguous ownership.
fn fresh_context_with_cleanup<F>(
    root: &Path,
    profile: &str,
    scope: &str,
    request: &str,
    thread: Option<&str>,
    mut cleanup: F,
) -> Result<Value>
where
    F: FnMut(&str) -> Result<()>,
{
    uuid::Uuid::parse_str(request)?;
    if let Some(receipt) = completed_receipt(root, profile, scope, request)? {
        return Ok(receipt);
    }
    let current = epoch(root, profile, scope)?;
    let mut db = journal(root)?;
    let prior: Option<(String, String)> = db
        .query_row(
            "SELECT thread_id,state FROM assistant_native_contexts WHERE retirement_id=?",
            [request],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let thread =
        thread.context("Fresh native context requires the exact recorded provider UUID")?;
    uuid::Uuid::parse_str(thread)?;
    stage_retirement(&mut db, profile, scope, request, thread, current, prior)?;
    // Failure leaves a durable fence. Do not start another context or clear
    // uncertainty merely because cleanup was attempted.
    cleanup(thread)?;
    complete_retirement(&db, root, request, thread)?;
    Ok(
        json!({"recovered":true,"already_recorded":false,"retired_thread":thread,"memory_epoch":current,"model_calls":null,"notice":"Old native context retired; no paid turn was replayed or refunded. Provider history is external and was not deleted."}),
    )
}

#[allow(clippy::too_many_arguments)]
fn stage_retirement(
    db: &mut Connection,
    profile: &str,
    scope: &str,
    request: &str,
    thread: &str,
    current: u64,
    prior: Option<(String, String)>,
) -> Result<()> {
    if prior.as_ref().is_some_and(|(old, _)| old != thread) {
        bail!("Recovery request belongs to another native provider UUID");
    }
    // Do not kill a busy or uncertain provider. A lost Stop can be recovered
    // only after the cleanup boundary proves the exact provider is absent.
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let actual: Option<(String, String, String)> = tx
        .query_row(
            "SELECT profile_id,scope,state FROM assistant_native_contexts WHERE thread_id=?",
            [thread],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    if actual
        .as_ref()
        .is_some_and(|(p, s, state)| p != profile || s != scope || state == "retired")
    {
        bail!("Native context retirement profile/scope mismatch");
    }
    let another: bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM assistant_native_contexts WHERE profile_id=? AND thread_id<>? AND state<>'retired')",params![profile,thread],|r|r.get(0))?;
    if another {
        bail!("Fresh-context request does not own the current native provider generation");
    }
    tx.execute("INSERT INTO assistant_native_contexts VALUES(?,?,?,?,'retiring',?) ON CONFLICT(thread_id) DO UPDATE SET state='retiring',retirement_id=excluded.retirement_id WHERE assistant_native_contexts.state<>'retired'",params![thread,profile,scope,current,request])?;
    tx.commit()?;
    Ok(())
}

fn complete_retirement(db: &Connection, root: &Path, request: &str, thread: &str) -> Result<()> {
    db.execute("UPDATE assistant_native_contexts SET state='retired' WHERE thread_id=? AND retirement_id=? AND state='retiring'",params![thread,request])?;
    crate::assistant_native_turns::recovered(root)?;
    // Prevent legacy adoption from resurrecting the retired backing UUID.
    let legacy: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='assistant_runtime_profile')",
        [],
        |r| r.get(0),
    )?;
    if legacy {
        db.execute(
            "UPDATE assistant_runtime_profile SET thread_id=NULL WHERE thread_id=?",
            [thread],
        )?;
    }
    Ok(())
}

/// Recovery after the user exits the native CLI. Detach is not provider exit.
/// Never kill an uncertain or live foreground conversation from a hook.
pub(crate) fn fresh_context(
    root: &Path,
    profile: &str,
    scope: &str,
    request: &str,
    thread: Option<&str>,
) -> Result<Value> {
    fresh_context_with_cleanup(root, profile, scope, request, thread, |thread| {
        let registry_path = root.join("native-registry/pika.db");
        crate::assistant_storage::existing_database(&registry_path)?;
        let registry = crate::store::Store::at(registry_path);
        let observation = crate::process::observe();
        let processes = observation
            .require_complete("retire native Pika context")
            .map_err(anyhow::Error::msg)?;
        let session = registry.get_session(crate::model::Provider::Codex, thread)?;
        let panes = crate::tmux::Tmux::default().list_panes()?;
        let owners = registry.live_owners(crate::model::Provider::Codex, thread)?;
        require_provider_absent(thread, session.as_ref(), &panes, &owners, processes)?;
        Ok(())
    })
}

fn require_provider_absent(
    thread: &str,
    session: Option<&crate::model::Session>,
    panes: &[crate::model::Pane],
    owners: &[crate::store::LiveOwner],
    processes: &std::collections::BTreeMap<i64, crate::process::ProcessRecord>,
) -> Result<()> {
    let live_pane = panes.iter().any(|pane| {
        !pane.dead
            && (pane.pika_provider == Some(crate::model::Provider::Codex)
                && pane.pika_session_id.as_deref() == Some(thread)
                || session.is_some_and(|session| {
                    session.tmux_pane.as_deref() == Some(pane.pane_id.as_str())
                }))
    });
    let live_process = owners
        .iter()
        .any(|owner| processes.contains_key(&owner.pid))
        || session
            .and_then(|session| session.root_pid)
            .is_some_and(|pid| processes.contains_key(&pid))
        || !crate::process::find_session_processes(
            thread,
            crate::model::Provider::Codex,
            processes,
        )
        .is_empty();
    if live_pane || live_process {
        bail!(
            "Pika is still open, or its earlier conversation cannot yet be verified as closed. Exit that assistant with /quit before continuing. Your saved memory is unchanged; no conversation was stopped."
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exact_absence_check_refuses_live_pane_and_resume_without_touching_others() {
        let thread = uuid::Uuid::new_v4().to_string();
        let mut pane = crate::model::Pane {
            session_name: "pika-codex-synthetic".into(),
            pane_id: "%91".into(),
            pane_pid: 901,
            cwd: "/synthetic".into(),
            current_command: "codex".into(),
            attached: false,
            dead: false,
            dead_status: None,
            activity: 1.,
            created: 1.,
            pika_provider: Some(crate::model::Provider::Codex),
            pika_session_id: Some(thread.clone()),
            pika_name: Some("Pika".into()),
            pika_launch_token: None,
        };
        let mut processes = std::collections::BTreeMap::new();
        assert!(require_provider_absent(&thread, None, &[pane.clone()], &[], &processes).is_err());
        pane.dead = true;
        assert!(require_provider_absent(&thread, None, &[pane.clone()], &[], &processes).is_ok());
        pane.dead = false;
        pane.pika_session_id = Some(uuid::Uuid::new_v4().to_string());
        assert!(require_provider_absent(&thread, None, &[pane], &[], &processes).is_ok());
        processes.insert(
            902,
            crate::process::ProcessRecord {
                pid: 902,
                parent_pid: None,
                start_time: 1,
                argv: vec!["codex".into(), "resume".into(), thread.clone()],
            },
        );
        assert!(require_provider_absent(&thread, None, &[], &[], &processes).is_err());
    }
    fn fixture() -> (tempfile::TempDir, std::path::PathBuf, String, String) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("profile");
        let memory = crate::assistant_memory::Store::open(root.join("memory.sqlite")).unwrap();
        let profile = memory.profile_id().to_owned();
        let thread = uuid::Uuid::new_v4().to_string();
        record_context(&root, &profile, "scope", &thread).unwrap();
        (temp, root, profile, thread)
    }
    #[test]
    fn forget_fences_old_context_and_retirement_never_re_adopts_it() {
        let (_temp, root, profile, thread) = fixture();
        let mut memory = crate::assistant_memory::Store::open(root.join("memory.sqlite")).unwrap();
        memory
            .forget_worker_scope(&crate::assistant::scope("scope").unwrap())
            .unwrap();
        assert!(require_context(&root, &profile, "scope", Some(&thread)).is_err());
        let request = uuid::Uuid::new_v4().to_string();
        fresh_context_with_cleanup(
            &root,
            &profile,
            "scope",
            &request,
            Some(&thread),
            |actual| {
                assert_eq!(actual, thread);
                Ok(())
            },
        )
        .unwrap();
        assert!(record_context(&root, &profile, "scope", &thread).is_err());
        assert!(
            record_context(&root, &profile, "scope", &uuid::Uuid::new_v4().to_string()).is_ok()
        );
        fresh_context_with_cleanup(&root, &profile, "scope", &request, Some(&thread), |_| {
            bail!("old receipt must not touch newer work")
        })
        .unwrap();
    }
    #[test]
    fn failed_cleanup_keeps_context_blocked_across_restart() {
        let (_temp, root, profile, thread) = fixture();
        let request = uuid::Uuid::new_v4().to_string();
        assert!(
            fresh_context_with_cleanup(
                &root,
                &profile,
                "scope",
                &request,
                Some(&thread),
                |_| bail!("ambiguous live provider")
            )
            .is_err()
        );
        assert!(require_context(&root, &profile, "scope", Some(&thread)).is_err());
        assert!(require_context(&root, &profile, "scope", None).is_err());
    }
}
