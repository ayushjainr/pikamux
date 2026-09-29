//! Human-approved exact private consultation scopes. Only registry metadata is
//! read here; approval/listing never opens providers or reads transcripts.
use crate::{
    assistant_investigation_provider::ConsultationAuthorization,
    assistant_memory::Scope,
    assistant_policy::{AssistantPolicy, Grant},
    core::Pika,
    model::Provider,
    store::Store,
};
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Saved {
    id: String,
    grant_id: String,
    node: String,
    provider: Provider,
    conversation: String,
    project: String,
    executable: PathBuf,
    registry: PathBuf,
    expires_at: i64,
}
fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .min(i64::MAX as u64) as i64
}
fn database(root: &Path) -> Result<Connection> {
    let path = root.join("consultation-permissions.sqlite");
    crate::assistant_storage::database(&path)?;
    let db = Connection::open(path)?;
    db.execute_batch("CREATE TABLE IF NOT EXISTS consultation_permissions(id TEXT PRIMARY KEY,project TEXT NOT NULL,body TEXT NOT NULL)")?;
    Ok(db)
}
fn configured_executable(pika: &Pika, provider: Provider) -> Result<PathBuf> {
    let configured = PathBuf::from(pika.config.executable(provider));
    let path = if configured.is_absolute() {
        configured
    } else {
        if configured.components().count() != 1 {
            bail!("Provider executable must be absolute or an exact configured command name");
        }
        let search = pika
            .config
            .provider_runtime_path
            .clone()
            .or_else(|| std::env::var("PATH").ok())
            .unwrap_or_default();
        std::env::split_paths(&search)
            .filter(|p| p.is_absolute())
            .map(|p| p.join(&configured))
            .find(|p| p.is_file())
            .context("Configured provider executable is unavailable; no approval was saved")?
    };
    let resolved =
        std::fs::canonicalize(path).context("Cannot bind the configured provider executable")?;
    if !resolved.is_file() {
        bail!("Configured provider executable is not a regular file");
    }
    Ok(resolved)
}
fn authorization(saved: &Saved) -> Result<ConsultationAuthorization> {
    let store = Store::at(&saved.registry);
    if store.local_node_id()?.as_deref() != Some(&saved.node) {
        bail!("Approved authority identity changed");
    }
    let parent = store
        .get_session(saved.provider, &saved.conversation)?
        .context("Approved exact conversation is unavailable")?;
    if parent.provider_thread_id() != saved.conversation
        || !store.is_watched(saved.provider, &saved.conversation)?
    {
        bail!("Approved conversation identity or watching scope changed");
    }
    Ok(ConsultationAuthorization {
        id: saved.id.clone(),
        grant_id: saved.grant_id.clone(),
        authority_node: saved.node.clone(),
        target_node: saved.node.clone(),
        parent,
        scope: Scope {
            project: Some(saved.project.clone()),
            ..Default::default()
        },
        destination: "codex".into(),
        executable: saved.executable.clone(),
    })
}
fn validate_approval_request(scope: &str, conversation: &str, expires_at: i64) -> Result<()> {
    if scope.is_empty()
        || scope.len() > 256
        || scope.chars().any(char::is_control)
        || uuid::Uuid::parse_str(conversation).is_err()
    {
        bail!("Exact project scope and conversation UUID required");
    }
    if expires_at <= now() {
        bail!("Consultation permission has already expired");
    }
    Ok(())
}
/// Call only for an explicit human scope command, not a model proposal.
pub(crate) fn approve(
    root: &Path,
    pika: &Pika,
    scope: &str,
    provider: Provider,
    conversation: &str,
    expires_at: i64,
) -> Result<Value> {
    validate_approval_request(scope, conversation, expires_at)?;
    crate::consult::consultation_policy(provider, false)?;
    let node = pika
        .store
        .local_node_id()?
        .context("Local authority identity is unavailable")?;
    let executable = configured_executable(pika, provider)?;
    let saved = Saved {
        id: uuid::Uuid::new_v4().to_string(),
        grant_id: uuid::Uuid::new_v4().to_string(),
        node,
        provider,
        conversation: conversation.into(),
        project: scope.into(),
        executable,
        registry: pika.store.path().into(),
        expires_at,
    };
    let a = authorization(&saved)?;
    let db = database(root)?;
    let count: i64 = db.query_row("SELECT COUNT(*) FROM consultation_permissions", [], |r| {
        r.get(0)
    })?;
    if count >= 32 {
        bail!("Remove an old consultation permission before adding another (maximum 32)");
    }
    let mut policy = AssistantPolicy::open(root.join("policy.sqlite"))?;
    policy.grant(&Grant {
        id: saved.grant_id.clone(),
        provider: "codex".into(),
        scope: a.grant_scope(),
        capability: "private-consultation".into(),
        expires_at,
        revoked_at: None,
    })?;
    db.execute(
        "INSERT INTO consultation_permissions(id,project,body) VALUES(?,?,?)",
        params![saved.id, saved.project, serde_json::to_string(&saved)?],
    )?;
    Ok(
        json!({"id":saved.id,"grant_id":saved.grant_id,"node":saved.node,"provider":provider.as_str(),"conversation":conversation,"scope":scope,"destination":"codex","executable":saved.executable,"expires_at":expires_at,"notice":"Approved bounded questions and scoped context to this exact source provider, and its dated replies to Codex. Shared call allowance still applies. Provider retention is unchanged; no consultation was started."}),
    )
}
fn rows(root: &Path, scope: Option<&str>) -> Result<Vec<Saved>> {
    if !root.join("consultation-permissions.sqlite").exists() {
        return Ok(vec![]);
    }
    let db = database(root)?;
    let mut stmt=db.prepare("SELECT body FROM consultation_permissions WHERE (?1 IS NULL OR project=?1) ORDER BY id LIMIT 33")?;
    let mut saved = Vec::new();
    for row in stmt.query_map([scope], |r| r.get::<_, String>(0))? {
        let body = row?;
        if body.len() > 16 * 1024 {
            bail!("Oversized permission record");
        }
        saved.push(serde_json::from_str(&body)?);
    }
    if saved.len() > 32 {
        bail!("Permission record bound exceeded");
    }
    Ok(saved)
}
pub(crate) fn load(
    root: &Path,
    scope: &Scope,
    time: i64,
) -> Result<Vec<ConsultationAuthorization>> {
    crate::assistant_investigation::validate_project_scope(scope)?;
    let policy = AssistantPolicy::open(root.join("policy.sqlite"))?;
    let mut allowed = Vec::new();
    for saved in rows(root, scope.project.as_deref())? {
        if saved.expires_at <= time {
            continue;
        }
        let Ok(a) = authorization(&saved) else {
            continue;
        };
        if policy
            .validate_grant(
                &a.grant_id,
                "codex",
                &a.grant_scope(),
                "private-consultation",
                time,
            )
            .is_ok()
        {
            allowed.push(a);
        }
    }
    Ok(allowed)
}
/// Serialize the exact registry binding against unwatch/identity changes at
/// the provider send, without reading the source conversation's transcript.
pub(crate) fn fence_binding(
    root: &Path,
    expected: &ConsultationAuthorization,
) -> Result<Connection> {
    let saved = rows(root, expected.scope.project.as_deref())?
        .into_iter()
        .find(|s| s.id == expected.id)
        .context("Consultation permission removed before dispatch")?;
    let db =
        Connection::open_with_flags(&saved.registry, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    db.busy_timeout(std::time::Duration::from_secs(5))?;
    db.execute_batch("BEGIN IMMEDIATE")?;
    let current = authorization(&saved)?;
    if current.grant_id != expected.grant_id
        || current.grant_scope() != expected.grant_scope()
        || current.executable != expected.executable
        || current.parent.session_id != expected.parent.session_id
        || current.parent.cwd != expected.parent.cwd
        || current.parent.model != expected.parent.model
    {
        bail!("Consultation exact source binding changed before dispatch");
    }
    Ok(db)
}
pub(crate) fn list(root: &Path, scope: &str) -> Result<Value> {
    let policy = AssistantPolicy::open(root.join("policy.sqlite"))?;
    let values=rows(root,Some(scope))?.into_iter().map(|saved|{
        let eligible=authorization(&saved).ok().is_some_and(|a|policy.validate_grant(&a.grant_id,"codex",&a.grant_scope(),"private-consultation",now()).is_ok());
        json!({"id":saved.id,"grant_id":saved.grant_id,"node":saved.node,"provider":saved.provider.as_str(),"conversation":saved.conversation,"scope":saved.project,"destination":"codex","expires_at":saved.expires_at,"eligible":eligible})
    }).collect::<Vec<_>>();
    Ok(
        json!({"permissions":values,"notice":"Private consultations remain charged to the shared allowance. Revocation blocks queued work; provider retention is unchanged."}),
    )
}
pub(crate) fn revoke(root: &Path, id: &str) -> Result<Value> {
    let db = database(root)?;
    let body: Option<String> = db
        .query_row(
            "SELECT body FROM consultation_permissions WHERE id=?",
            [id],
            |r| r.get(0),
        )
        .optional()?;
    let saved: Saved =
        serde_json::from_str(&body.context("Unknown exact consultation permission")?)?;
    let mut policy = AssistantPolicy::open(root.join("policy.sqlite"))?;
    // Idempotent public revocation; an already-revoked grant is already safe.
    match policy.revoke_grant(&saved.grant_id, now()) {
        Ok(()) | Err(crate::assistant_policy::PolicyError::UnknownGrant(_)) => {}
        Err(error) => return Err(error.into()),
    }
    Ok(
        json!({"id":id,"revoked":true,"notice":"Queued consultation use is blocked. Existing provider copies are not deleted."}),
    )
}
pub(crate) fn remove(root: &Path, id: &str) -> Result<Value> {
    revoke(root, id)?;
    database(root)?.execute("DELETE FROM consultation_permissions WHERE id=?", [id])?;
    Ok(
        json!({"id":id,"removed":true,"notice":"Permission removed after revocation; provider retention is unchanged."}),
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn standing_consultation_survives_time_but_not_revocation() {
        let (dir, pika, id) = fixture();
        let root = dir.path().join("assistant");
        let approval = approve(
            &root,
            &pika,
            "project-a",
            Provider::Codex,
            &id,
            crate::assistant_policy::UNTIL_REVOKED,
        )
        .unwrap();
        let scope = Scope {
            project: Some("project-a".into()),
            ..Scope::default()
        };
        assert_eq!(load(&root, &scope, now() + 30 * 86400).unwrap().len(), 1);
        revoke(&root, approval["id"].as_str().unwrap()).unwrap();
        assert!(load(&root, &scope, now() + 30 * 86400).unwrap().is_empty());
    }
    use super::*;
    fn fixture() -> (tempfile::TempDir, Pika, String) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let paths = crate::paths::Paths {
            config_dir: root.join("config"),
            state_dir: root.join("state"),
            config: root.join("config/pika.json"),
            database: root.join("state/pika.sqlite"),
            codex_home: root.join("codex"),
            claude_home: root.join("claude"),
            opencode_data_home: root.join("opencode-data"),
            opencode_config_home: root.join("opencode-config"),
            muse_data_home: root.join("muse-data"),
            muse_config_home: root.join("muse-config"),
        };
        let store = Store::from_paths(&paths);
        store.ensure_local_node_id().unwrap();
        let id = "aaaaaaaa-aaaa-4aaa-aaaa-aaaaaaaaaaaa";
        let parent:crate::model::Session=serde_json::from_value(json!({"provider":"codex","session_id":id,"name":"Synthetic expert","status":"READY","unread":true,"source":"fixture","managed":true,"created_at":1.0,"updated_at":1.0,"last_event_at":1.0,"last_activity_at":1.0,"live":false,"attached":false,"home_state":""})).unwrap();
        store.upsert_session(&parent, false).unwrap();
        store.restore_tracking(Provider::Codex, id).unwrap();
        let mut config = crate::config::Config::default();
        config.provider_executables.insert(
            "codex".into(),
            std::env::current_exe()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
        );
        let pika = Pika::with_components(paths, config, store, crate::tmux::Tmux::default());
        (dir, pika, id.into())
    }
    #[test]
    fn approval_reload_revoke_remove_are_exact_metadata_only_and_preserve_unread() {
        let (dir, pika, id) = fixture();
        let root = dir.path().join("assistant");
        let before = pika
            .store
            .get_session(Provider::Codex, &id)
            .unwrap()
            .unwrap();
        let approval = approve(
            &root,
            &pika,
            "project-a",
            Provider::Codex,
            &id,
            now() + 3600,
        )
        .unwrap();
        let scope = Scope {
            project: Some("project-a".into()),
            ..Default::default()
        };
        let loaded = load(&root, &scope, now()).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].parent.session_id, id);
        assert!(
            load(
                &root,
                &Scope {
                    project: Some("excluded".into()),
                    ..Default::default()
                },
                now()
            )
            .unwrap()
            .is_empty()
        );
        assert_eq!(
            pika.store
                .get_session(Provider::Codex, &id)
                .unwrap()
                .unwrap(),
            before
        );
        assert_eq!(
            list(&root, "project-a").unwrap()["permissions"][0]["eligible"],
            true
        );
        let permission = approval["id"].as_str().unwrap();
        revoke(&root, permission).unwrap();
        revoke(&root, permission).unwrap();
        assert!(load(&root, &scope, now()).unwrap().is_empty());
        remove(&root, permission).unwrap();
        assert_eq!(list(&root, "project-a").unwrap()["permissions"], json!([]));
    }
    #[test]
    fn unknown_continuation_changed_unwatched_and_expired_sources_never_load() {
        let (dir, pika, id) = fixture();
        let root = dir.path().join("assistant");
        let scope = Scope {
            project: Some("project-a".into()),
            ..Default::default()
        };
        assert!(
            approve(
                &root,
                &pika,
                "project-a",
                Provider::Codex,
                "bbbbbbbb-bbbb-4bbb-bbbb-bbbbbbbbbbbb",
                now() + 10
            )
            .is_err()
        );
        let approval =
            approve(&root, &pika, "project-a", Provider::Codex, &id, now() + 60).unwrap();
        assert!(load(&root, &scope, now() + 61).unwrap().is_empty());
        let mut parent = pika
            .store
            .get_session(Provider::Codex, &id)
            .unwrap()
            .unwrap();
        parent.active_thread_id = Some("cccccccc-cccc-4ccc-cccc-cccccccccccc".into());
        pika.store.upsert_session(&parent, false).unwrap();
        assert!(load(&root, &scope, now()).unwrap().is_empty());
        parent.active_thread_id = None;
        pika.store.upsert_session(&parent, false).unwrap();
        pika.store.untrack_session(Provider::Codex, &id).unwrap();
        assert!(load(&root, &scope, now()).unwrap().is_empty());
        revoke(&root, approval["id"].as_str().unwrap()).unwrap();
    }
    #[test]
    fn actual_dispatch_source_fence_serializes_unwatch_and_rejects_stale_binding() {
        use std::sync::mpsc;
        use std::time::Duration;
        let (dir, pika, id) = fixture();
        let root = dir.path().join("assistant");
        approve(&root, &pika, "project-a", Provider::Codex, &id, now() + 60).unwrap();
        let scope = Scope {
            project: Some("project-a".into()),
            ..Default::default()
        };
        let authorization = load(&root, &scope, now()).unwrap().remove(0);
        let guard = fence_binding(&root, &authorization).unwrap();
        let path = pika.store.path().to_path_buf();
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let (done_tx, done_rx) = mpsc::sync_channel(1);
        let join = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            Store::at(path)
                .untrack_session(Provider::Codex, &id)
                .unwrap();
            done_tx.send(()).unwrap();
        });
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(done_rx.recv_timeout(Duration::from_millis(50)).is_err());
        drop(guard);
        done_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        join.join().unwrap();
        assert!(fence_binding(&root, &authorization).is_err());
    }
}
