//! Durable, non-authoritative learning links for the authoring workflow.
//!
//! These records describe provenance and measured use only.  They never mark
//! a proposal as accepted, claim that a tool helped, or infer a human choice.

use crate::assistant_memory::{NewRecord, Origin, Record, RecordKind, Scope, Store};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum LearningError {
    #[error("learning root must be absolute")]
    RelativeRoot,
    #[error("learning scope mismatch")]
    ScopeMismatch,
    #[error("learning correction is not a live human correction")]
    InvalidCorrection,
    #[error("learning database: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("learning memory: {0}")]
    Memory(#[from] crate::assistant_memory::MemoryError),
    #[error("learning filesystem: {0}")]
    Filesystem(#[from] std::io::Error),
    #[error("learning input is too large")]
    TooLarge,
    #[error("learning link not found")]
    NotFound,
    #[error("learning candidate binding is immutable")]
    ImmutableBinding,
}

const MAX_TEXT: usize = 16 * 1024;

fn paths(root: &Path) -> Result<(PathBuf, PathBuf), LearningError> {
    if !root.is_absolute() {
        return Err(LearningError::RelativeRoot);
    }
    Ok((root.join("memory.sqlite"), root.join("learning.sqlite")))
}

fn metadata(path: &Path) -> Result<Connection, LearningError> {
    crate::assistant_storage::database(path)?;
    let conn = Connection::open(path)?;
    conn.execute_batch("PRAGMA foreign_keys=ON; CREATE TABLE IF NOT EXISTS learning_hypotheses(request_id TEXT PRIMARY KEY, correction_id TEXT NOT NULL, scope TEXT NOT NULL, need TEXT NOT NULL, record_id TEXT NOT NULL, epoch INTEGER NOT NULL); CREATE TABLE IF NOT EXISTS learning_candidates(request_id TEXT PRIMARY KEY, candidate_hash TEXT NOT NULL, linked_at INTEGER NOT NULL); CREATE TABLE IF NOT EXISTS learning_uses(request_id TEXT PRIMARY KEY, candidate_hash TEXT NOT NULL, scope TEXT NOT NULL, record_id TEXT NOT NULL, epoch INTEGER NOT NULL);")?;
    Ok(conn)
}

fn scope_for(value: &str) -> Scope {
    Scope {
        project: Some(value.to_owned()),
        ..Scope::default()
    }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .min(i64::MAX as u64) as i64
}

/// Prepare a bounded hypothesis from the exact human correction. This is a
/// deterministic proposal only: no provider call, protected-case authoring,
/// activation, or inferred human acceptance. Repeated requests are idempotent.
pub fn prepare_correction_proposal(
    root: impl AsRef<Path>,
    scope: &str,
    correction_id: &str,
) -> Result<Record, LearningError> {
    let (memory_path, _) = paths(root.as_ref())?;
    let memory = Store::open(memory_path)?;
    let correction = validated_correction(&memory, correction_id, &scope_for(scope))?;
    let excerpt: String = correction.body.chars().take(1024).collect();
    let need = format!(
        "Hypothesis: a scoped pure-data transformation or working-method revision may prevent recurrence of this correction (bounded excerpt): {excerpt}. First reproduce the failure and contrast it with an unaffected case. Benefit remains unassessed; existing access and spending limits remain unchanged."
    );
    register_hypothesis(
        root,
        scope,
        &format!("correction-proposal-{correction_id}"),
        correction_id,
        &need,
    )
}

/// Register a human correction as a worker proposal request, preserving the
/// correction dependency and exact scope.  The proposal is not a decision.
pub fn register_hypothesis(
    root: impl AsRef<Path>,
    scope: &str,
    request_id: &str,
    correction_id: &str,
    need: &str,
) -> Result<Record, LearningError> {
    if need.is_empty() || need.len() > MAX_TEXT || request_id.is_empty() {
        return Err(LearningError::TooLarge);
    }
    let root = root.as_ref();
    let (memory_path, learning_path) = paths(root)?;
    let mut memory = Store::open(memory_path)?;
    let expected_scope = scope_for(scope);
    let correction = validated_correction(&memory, correction_id, &expected_scope)?;
    let epoch = memory.forget_epoch()?;
    let record = memory.append_idempotent(
        request_id,
        NewRecord {
            kind: RecordKind::Proposal,
            origin: Origin::Worker,
            scope: expected_scope,
            body: need.to_owned(),
            provenance: "hypothesis derived from explicit human correction; outcome unassessed"
                .into(),
            timestamp: correction.timestamp,
            supersedes: None,
            dependencies: vec![correction_id.to_owned()],
            decision_state: Some(crate::assistant_memory::DecisionState::Proposed),
            protected_policy: false,
        },
    )?;
    let db = metadata(&learning_path)?;
    memory.publish_at_epoch(epoch, || {
        db.execute(
            "INSERT OR IGNORE INTO learning_hypotheses(request_id,correction_id,scope,need,record_id,epoch) VALUES(?,?,?,?,?,?)",
            params![request_id, correction_id, scope, need, record.id, epoch as i64],
        )?;
        Ok(())
    })?;
    Ok(record)
}

fn validated_correction(
    memory: &Store,
    correction_id: &str,
    scope: &Scope,
) -> Result<Record, LearningError> {
    let correction = memory
        .get(correction_id)?
        .ok_or(LearningError::InvalidCorrection)?;
    if correction.kind != RecordKind::Correction
        || correction.origin != Origin::Human
        || &correction.scope != scope
        || memory.get_active(correction_id)?.is_none()
    {
        return Err(LearningError::InvalidCorrection);
    }
    Ok(correction)
}

/// Bind an evaluated candidate hash to the exact hypothesis request.
pub fn bind_candidate(
    root: impl AsRef<Path>,
    request_id: &str,
    hash: &str,
) -> Result<(), LearningError> {
    if hash.is_empty() || hash.len() > 128 {
        return Err(LearningError::TooLarge);
    }
    let (memory_path, learning_path) = paths(root.as_ref())?;
    let mut memory = Store::open(memory_path)?;
    let epoch = memory.forget_epoch()?;
    let db = metadata(&learning_path)?;
    let hypothesis_epoch: Option<i64> = db
        .query_row(
            "SELECT epoch FROM learning_hypotheses WHERE request_id=?",
            [request_id],
            |r| r.get(0),
        )
        .optional()?;
    let Some(hypothesis_epoch) = hypothesis_epoch else {
        return Ok(());
    };
    if hypothesis_epoch as u64 != epoch {
        return Err(LearningError::ScopeMismatch);
    }
    memory.publish_at_epoch(epoch, || {
        db.execute(
            "INSERT OR IGNORE INTO learning_candidates(request_id,candidate_hash,linked_at) SELECT ?,?,strftime('%s','now') WHERE EXISTS(SELECT 1 FROM learning_hypotheses WHERE request_id=?)",
            params![request_id, hash, request_id],
        )?;
        let stored:String=db.query_row("SELECT candidate_hash FROM learning_candidates WHERE request_id=?",[request_id],|row|row.get(0))?;
        if stored!=hash {return Err(rusqlite::Error::InvalidParameterName("candidate binding is immutable".into()));}
        Ok(())
    })?;
    Ok(())
}

/// Record later pure-tool use without asserting benefit or human acceptance.
pub fn record_use(
    root: impl AsRef<Path>,
    scope: &str,
    hash: &str,
    summary: &Value,
) -> Result<(), LearningError> {
    validate_hash(hash)?;
    let (memory_path, learning_path) = paths(root.as_ref())?;
    let mut memory = Store::open(memory_path)?;
    let expected_scope = scope_for(scope);
    let db = metadata(&learning_path)?;
    let hypotheses = linked_hypotheses(&db, scope, hash)?;
    if hypotheses.is_empty() {
        return Ok(());
    }
    if hypotheses.len() > 32 {
        return Err(LearningError::TooLarge);
    }
    let output = bounded_summary(summary)?;
    let body = serde_json::json!({
        "candidate_hash": hash,
        "output": output,
        "human_benefit": "unknown",
    })
    .to_string();
    let use_id = format!("learning-use-{}", uuid::Uuid::new_v4());
    let epoch = memory.forget_epoch()?;
    let record = memory.append_at_epoch(
        NewRecord {
            kind: RecordKind::Finding,
            origin: Origin::Worker,
            scope: expected_scope,
            body,
            provenance: format!("measured pure tool execution candidate={hash}; benefit unknown until human assessment"),
            timestamp: now(),
            supersedes: None,
            dependencies: hypotheses,
            decision_state: None,
            protected_policy: false,
        },epoch,
    )?;
    let db = metadata(&learning_path)?;
    memory.publish_at_epoch(epoch, || {
        db.execute(
            "INSERT OR IGNORE INTO learning_uses(request_id,candidate_hash,scope,record_id,epoch) VALUES(?,?,?,?,?)",
            params![use_id, hash, scope, record.id, epoch as i64],
        )?;
        Ok(())
    })?;
    Ok(())
}

fn validate_hash(hash: &str) -> Result<(), LearningError> {
    if hash.is_empty() || hash.len() > 128 {
        return Err(LearningError::TooLarge);
    }
    Ok(())
}

fn linked_hypotheses(
    db: &Connection,
    scope: &str,
    hash: &str,
) -> Result<Vec<String>, LearningError> {
    let mut statement=db.prepare("SELECT DISTINCT h.record_id FROM learning_hypotheses h JOIN learning_candidates c ON c.request_id=h.request_id WHERE h.scope=? AND c.candidate_hash=? ORDER BY h.record_id LIMIT 33")?;
    Ok(statement
        .query_map(params![scope, hash], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?)
}

fn bounded_summary(summary: &Value) -> Result<String, LearningError> {
    let serialized = serde_json::to_string(summary).map_err(|_| LearningError::TooLarge)?;
    if serialized.len() <= MAX_TEXT {
        return Ok(serialized);
    }
    let mut end = MAX_TEXT;
    while !serialized.is_char_boundary(end) {
        end -= 1;
    }
    Ok(format!("{}…[truncated]", &serialized[..end]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assistant_memory::NewRecord;

    #[test]
    fn correction_proposal_is_nonexecuting_scoped_idempotent_and_forgettable() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("private");
        let mut memory = Store::open(root.join("memory.sqlite")).unwrap();
        let scope = scope_for("alpha");
        let source = memory
            .append(NewRecord {
                kind: RecordKind::Finding,
                origin: Origin::Worker,
                scope: scope.clone(),
                body: "old".into(),
                provenance: "fixture".into(),
                timestamp: 1,
                supersedes: None,
                dependencies: vec![],
                decision_state: None,
                protected_policy: false,
            })
            .unwrap();
        let correction = memory
            .append(NewRecord {
                kind: RecordKind::Correction,
                origin: Origin::Human,
                scope: scope.clone(),
                body: "Explain the changed assumptions".into(),
                provenance: "explicit correction".into(),
                timestamp: 2,
                supersedes: Some(source.id.clone()),
                dependencies: vec![source.id],
                decision_state: None,
                protected_policy: false,
            })
            .unwrap();
        let proposal = prepare_correction_proposal(&root, "alpha", &correction.id).unwrap();
        assert_eq!(proposal.kind, RecordKind::Proposal);
        assert_eq!(proposal.origin, Origin::Worker);
        assert_eq!(proposal.scope, scope);
        assert_eq!(proposal.dependencies, vec![correction.id.clone()]);
        assert_eq!(
            prepare_correction_proposal(&root, "alpha", &correction.id)
                .unwrap()
                .id,
            proposal.id
        );
        assert!(prepare_correction_proposal(&root, "beta", &correction.id).is_err());
        assert!(!root.join("workshop.sqlite").exists());
        assert!(!root.join("policy.sqlite").exists());
        assert!(!root.join("author-runtime.sqlite").exists());
        memory.forget(&correction.id).unwrap();
        assert!(memory.get(&proposal.id).unwrap().is_none());
        assert!(prepare_correction_proposal(&root, "alpha", &correction.id).is_err());
    }

    #[test]
    fn correction_hypothesis_candidate_use_survives_restart_and_scope_is_exact() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("private");
        std::fs::create_dir_all(&root).unwrap();
        #[cfg(unix)]
        std::fs::set_permissions(&root, std::os::unix::fs::PermissionsExt::from_mode(0o700))
            .unwrap();
        let mut store = Store::open(root.join("memory.sqlite")).unwrap();
        let source = store
            .append(NewRecord {
                kind: RecordKind::Finding,
                origin: Origin::Human,
                scope: scope_for("personal"),
                body: "prior title".into(),
                provenance: "user".into(),
                timestamp: 1,
                supersedes: None,
                dependencies: vec![],
                decision_state: None,
                protected_policy: false,
            })
            .unwrap();
        let correction = store
            .append(NewRecord {
                kind: RecordKind::Correction,
                origin: Origin::Human,
                scope: scope_for("personal"),
                body: "prefer concise titles".into(),
                provenance: "user".into(),
                timestamp: 2,
                supersedes: None,
                dependencies: vec![source.id],
                decision_state: None,
                protected_policy: false,
            })
            .unwrap();
        let hypothesis = register_hypothesis(
            &root,
            "personal",
            "hypothesis-1",
            &correction.id,
            "derive concise titles",
        )
        .unwrap();
        bind_candidate(&root, "hypothesis-1", "candidate-hash").unwrap();
        bind_candidate(&root, "hypothesis-1", "candidate-hash").unwrap();
        assert!(bind_candidate(&root, "hypothesis-1", "changed-hash").is_err());
        assert_eq!(
            register_hypothesis(
                &root,
                "personal",
                "hypothesis-1",
                &correction.id,
                "derive concise titles"
            )
            .unwrap()
            .id,
            hypothesis.id
        );
        let summary = serde_json::json!({"output":"measured pure tool execution output"});
        record_use(&root, "personal", "candidate-hash", &summary).unwrap();
        record_use(
            &root,
            "personal",
            "candidate-hash",
            &serde_json::json!("界".repeat(20_000)),
        )
        .unwrap();
        assert!(record_use(&root, "other", "candidate-hash", &summary).is_ok());
        let reopened = Store::open(root.join("memory.sqlite")).unwrap();
        let finding = reopened
            .recent(&scope_for("personal"), 16)
            .unwrap()
            .into_iter()
            .find(|r| r.kind == RecordKind::Finding && r.origin == Origin::Worker)
            .unwrap();
        assert!(finding.body.contains("candidate-hash"));
        assert!(finding.body.contains("unknown"));
        let correction2 = store
            .append(NewRecord {
                kind: RecordKind::Correction,
                origin: Origin::Human,
                scope: scope_for("personal"),
                body: "A second correction sharing the same transform".into(),
                provenance: "fixture".into(),
                timestamp: 3,
                supersedes: None,
                dependencies: vec![correction.id.clone()],
                decision_state: None,
                protected_policy: false,
            })
            .unwrap();
        let second = register_hypothesis(
            &root,
            "personal",
            "hypothesis-2",
            &correction2.id,
            "second hypothesis",
        )
        .unwrap();
        bind_candidate(&root, "hypothesis-2", "candidate-hash").unwrap();
        record_use(
            &root,
            "personal",
            "candidate-hash",
            &serde_json::json!("shared use"),
        )
        .unwrap();
        let shared = store
            .recent(&scope_for("personal"), 32)
            .unwrap()
            .into_iter()
            .find(|r| r.kind == RecordKind::Finding && r.body.contains("shared use"))
            .unwrap();
        assert!(shared.dependencies.contains(&hypothesis.id));
        assert!(shared.dependencies.contains(&second.id));
        store.forget(&correction2.id).unwrap();
        assert!(store.get(&shared.id).unwrap().is_none());
        assert!(store.get(&hypothesis.id).unwrap().is_some());
    }
}
