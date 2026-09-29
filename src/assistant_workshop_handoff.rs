//! Durable local handoff of validated proposals; ingestion never authorizes
//! paid evaluation, activates a tool, or turns a method into standing guidance.
use crate::assistant_context::SourceVersion;
use crate::assistant_guidance::validate_sources;
use crate::assistant_memory::{
    DecisionState, MemoryError, NewRecord, Origin, RecordKind, Scope, Store,
};
use rusqlite::params;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProposalKind {
    Method,
    Tool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkshopProposal {
    pub kind: ProposalKind,
    pub hypothesis: String,
    pub proposed_change: String,
    pub baseline: Option<String>,
    pub requested_capabilities: Vec<String>,
    pub success_criterion: String,
    pub sources: Vec<SourceVersion>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct HandoffReceipt {
    pub record_id: String,
    pub state: String,
}

pub(crate) fn validate_proposal(
    store: &Store,
    scope: &Scope,
    proposal: &WorkshopProposal,
    timestamp: i64,
) -> Result<NewRecord, MemoryError> {
    validate_sources(store, scope, &proposal.sources)?;
    if [
        &proposal.hypothesis,
        &proposal.proposed_change,
        &proposal.success_criterion,
    ]
    .iter()
    .any(|s| s.trim().is_empty() || s.len() > 2048)
        || proposal.baseline.as_ref().is_some_and(|s| s.len() > 2048)
        || proposal.requested_capabilities.len() > 16
        || proposal
            .requested_capabilities
            .iter()
            .any(|s| s.is_empty() || s.len() > 128)
    {
        return Err(MemoryError::Invalid(
            "workshop proposal fields exceed bounds".into(),
        ));
    }
    Ok(NewRecord {
        kind: RecordKind::Proposal,
        origin: Origin::Worker,
        scope: scope.clone(),
        body: serde_json::to_string(proposal)?,
        provenance: "validated_workshop_proposal_v1".into(),
        timestamp,
        supersedes: None,
        dependencies: proposal.sources.iter().map(|s| s.id.clone()).collect(),
        decision_state: Some(DecisionState::Proposed),
        protected_policy: false,
    })
}

pub(crate) fn proposal(store: &Store, id: &str) -> Result<(Scope, WorkshopProposal), MemoryError> {
    let record = store
        .get_active(id)?
        .ok_or_else(|| MemoryError::Invalid("workshop proposal is unavailable".into()))?;
    if record.kind != RecordKind::Proposal
        || record.origin != Origin::Worker
        || record.provenance != "validated_workshop_proposal_v1"
        || record.decision_state != Some(DecisionState::Proposed)
    {
        return Err(MemoryError::Invalid(
            "not a validated workshop proposal".into(),
        ));
    }
    let proposal: WorkshopProposal = serde_json::from_str(&record.body)?;
    validate_proposal(store, &record.scope, &proposal, record.timestamp)?;
    if record.dependencies
        != proposal
            .sources
            .iter()
            .map(|s| s.id.clone())
            .collect::<Vec<_>>()
    {
        return Err(MemoryError::Invalid("workshop dependency mismatch".into()));
    }
    Ok((record.scope, proposal))
}

fn schema(store: &Store) -> Result<(), MemoryError> {
    store.connection.execute_batch("CREATE TABLE IF NOT EXISTS workshop_handoffs(record_id TEXT PRIMARY KEY,state TEXT NOT NULL CHECK(state IN ('pending','bound')),candidate_hash TEXT)")?;
    Ok(())
}

pub(crate) fn ingest(store: &mut Store, id: &str) -> Result<HandoffReceipt, MemoryError> {
    let (_, proposal) = proposal(store, id)?;
    schema(store)?;
    let tx = store
        .connection
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let live:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM memory_records m WHERE id=? AND NOT EXISTS(SELECT 1 FROM memory_records n WHERE n.supersedes=m.id))",[id],|r|r.get(0))?;
    if !live {
        return Err(MemoryError::Invalid("workshop proposal changed".into()));
    }
    crate::assistant_guidance::validate_sources_in_tx(&tx, &store.profile_id, &proposal.sources)?;
    tx.execute(
        "INSERT OR IGNORE INTO workshop_handoffs(record_id,state) VALUES(?,'pending')",
        [id],
    )?;
    let state = tx.query_row(
        "SELECT state FROM workshop_handoffs WHERE record_id=?",
        [id],
        |r| r.get(0),
    )?;
    tx.commit()?;
    Ok(HandoffReceipt {
        record_id: id.into(),
        state,
    })
}

/// Bind only after the separately authorized author/evaluator has produced an
/// exact version. A retry cannot substitute different candidate bytes.
pub(crate) fn bind_candidate(store: &mut Store, id: &str, hash: &str) -> Result<(), MemoryError> {
    let (_, proposal) = proposal(store, id)?;
    if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(MemoryError::Invalid("exact candidate hash required".into()));
    }
    ingest(store, id)?;
    let tx = store
        .connection
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    crate::assistant_guidance::validate_sources_in_tx(&tx, &store.profile_id, &proposal.sources)?;
    let live: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM memory_records m WHERE m.id=? AND NOT EXISTS(SELECT 1 FROM memory_records n WHERE n.supersedes=m.id))",[id],|r|r.get(0))?;
    if !live {
        return Err(MemoryError::Invalid(
            "workshop proposal is no longer active".into(),
        ));
    }
    let existing: Option<String> = tx.query_row(
        "SELECT candidate_hash FROM workshop_handoffs WHERE record_id=?",
        [id],
        |r| r.get(0),
    )?;
    if existing.as_deref().is_some_and(|value| value != hash) {
        return Err(MemoryError::Invalid(
            "workshop candidate binding is immutable".into(),
        ));
    }
    tx.execute(
        "UPDATE workshop_handoffs SET state='bound',candidate_hash=? WHERE record_id=?",
        params![hash, id],
    )?;
    tx.commit()?;
    Ok(())
}

pub(crate) fn pending(
    store: &Store,
    scope: &Scope,
    limit: usize,
) -> Result<Vec<HandoffReceipt>, MemoryError> {
    schema(store)?;
    let mut query=store.connection.prepare("SELECT h.record_id FROM workshop_handoffs h JOIN memory_records m ON m.id=h.record_id WHERE h.state='pending' AND m.profile_id=? AND (m.project IS NULL OR m.project=?) AND (m.provider IS NULL OR m.provider=?) AND (m.conversation IS NULL OR m.conversation=?) AND (m.node IS NULL OR m.node=?) ORDER BY m.timestamp,m.id LIMIT ?")?;
    let ids = query
        .query_map(
            params![
                store.profile_id,
                scope.project,
                scope.provider,
                scope.conversation,
                scope.node,
                limit.min(64)
            ],
            |r| r.get::<_, String>(0),
        )?
        .collect::<Result<Vec<_>, _>>()?;
    ids.into_iter()
        .filter_map(|id| match proposal(store, &id) {
            Ok(_) => Some(Ok(HandoffReceipt {
                record_id: id,
                state: "pending".into(),
            })),
            Err(MemoryError::Invalid(_)) => None,
            Err(e) => Some(Err(e)),
        })
        .collect()
}

pub(crate) fn validate_candidate_sources(store: &Store, hash: &str) -> Result<(), MemoryError> {
    let exists: bool = store.connection.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='workshop_handoffs')", [], |r|r.get(0))?;
    if !exists {
        return Ok(());
    }
    let mut query=store.connection.prepare("SELECT record_id FROM workshop_handoffs WHERE candidate_hash=? ORDER BY record_id LIMIT 65")?;
    let ids = query
        .query_map([hash], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    if ids.len() > 64 {
        return Err(MemoryError::Invalid(
            "too many candidate dependencies".into(),
        ));
    }
    for id in ids {
        proposal(store, &id)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pending_handoff_is_durable_idempotent_nonactivating_and_dependency_fenced() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("private/memory.sqlite");
        let mut store = Store::open(&path).unwrap();
        let scope = Scope {
            project: Some("fixture".into()),
            ..Scope::default()
        };
        let source = store
            .append(NewRecord {
                kind: RecordKind::Finding,
                origin: Origin::Human,
                scope: scope.clone(),
                body: "The last categorization omitted an open question".into(),
                provenance: "fixture".into(),
                timestamp: 1,
                supersedes: None,
                dependencies: vec![],
                decision_state: None,
                protected_policy: false,
            })
            .unwrap();
        let proposed=WorkshopProposal{kind:ProposalKind::Method,hypothesis:"A separate open-question pass may prevent omissions".into(),proposed_change:"Check unresolved questions before finalizing summaries".into(),baseline:Some("Current summary method".into()),requested_capabilities:vec![],success_criterion:"Recover the original omitted question while preserving a contrasting no-question case".into(),sources:vec![SourceVersion{id:source.id.clone(),revision:store.source_version(&source.id).unwrap().unwrap()}]};
        let input = validate_proposal(&store, &scope, &proposed, 2).unwrap();
        let record = store.append(input).unwrap();
        assert_eq!(ingest(&mut store, &record.id).unwrap().state, "pending");
        ingest(&mut store, &record.id).unwrap();
        drop(store);
        let mut store = Store::open(path).unwrap();
        assert_eq!(pending(&store, &scope, 64).unwrap().len(), 1);
        assert!(!temp.path().join("private/workshop.sqlite").exists());
        let hash = "a".repeat(64);
        bind_candidate(&mut store, &record.id, &hash).unwrap();
        bind_candidate(&mut store, &record.id, &hash).unwrap();
        assert!(bind_candidate(&mut store, &record.id, &"b".repeat(64)).is_err());
        validate_candidate_sources(&store, &hash).unwrap();
        store.forget(&source.id).unwrap();
        assert!(validate_candidate_sources(&store, &hash).is_err());
        assert!(ingest(&mut store, &record.id).is_err());
        assert!(pending(&store, &scope, 64).unwrap().is_empty());
    }

    #[test]
    fn evaluated_exact_tool_reuses_fresh_input_but_forget_blocks_use_and_rollback() {
        use crate::assistant_evolution::{
            CandidateManifest, EvaluationCase, Expr, Scope as ToolScope, ScopedInput,
            ToolDefinition,
        };
        use crate::assistant_workshop::Workshop;
        let temp = tempfile::tempdir().unwrap();
        let mut memory = Store::open(temp.path().join("private/memory.sqlite")).unwrap();
        let scope = Scope {
            project: Some("fixture".into()),
            ..Scope::default()
        };
        let source = memory
            .append(NewRecord {
                kind: RecordKind::Finding,
                origin: Origin::Human,
                scope: scope.clone(),
                body: "Preserve supplied values in their original order".into(),
                provenance: "synthetic".into(),
                timestamp: 1,
                supersedes: None,
                dependencies: vec![],
                decision_state: None,
                protected_policy: false,
            })
            .unwrap();
        let proposed = WorkshopProposal {
            kind: ProposalKind::Tool,
            hypothesis: "A pure identity transformation preserves order".into(),
            proposed_change: "Return supplied records unchanged".into(),
            baseline: None,
            requested_capabilities: vec!["supplied_json".into()],
            success_criterion: "Both original and contrasting cases retain values".into(),
            sources: vec![SourceVersion {
                id: source.id.clone(),
                revision: memory.source_version(&source.id).unwrap().unwrap(),
            }],
        };
        let input = validate_proposal(&memory, &scope, &proposed, 2).unwrap();
        let proposal = memory.append(input).unwrap();
        let workshop = Workshop::open(&temp.path().join("private/workshop.sqlite")).unwrap();
        let tool_scope = ToolScope::new(["fixture"]);
        let scoped = |value| ScopedInput {
            value,
            scope: tool_scope.clone(),
        };
        workshop
            .protect_cases(
                "preserve-order",
                &[
                    EvaluationCase {
                        inputs: vec![scoped(serde_json::json!(1))],
                        expected: serde_json::json!([1]),
                    },
                    EvaluationCase {
                        inputs: vec![scoped(serde_json::json!(false))],
                        expected: serde_json::json!([false]),
                    },
                ],
            )
            .unwrap();
        let candidate = CandidateManifest {
            definition: ToolDefinition {
                name: "identity-fixture".into(),
                version: 1,
                input_scope: tool_scope.clone(),
                expression: Expr::Input,
            },
            authoring_evidence: "Synthetic deterministic author; no provider calls".into(),
        };
        let report = workshop
            .submit_handoff_candidate(
                &mut memory,
                &proposal.id,
                "preserve-order",
                &candidate,
                None,
            )
            .unwrap();
        assert!(report.passed);
        assert!(
            workshop
                .invoke(
                    "identity-fixture",
                    &[scoped(serde_json::json!("fresh"))],
                    None
                )
                .is_err()
        );
        workshop
            .approve_exact(&report.tool_hash, tool_scope.clone())
            .unwrap();
        assert_eq!(
            workshop
                .invoke(
                    "identity-fixture",
                    &[scoped(serde_json::json!("fresh"))],
                    None
                )
                .unwrap()
                .value,
            serde_json::json!(["fresh"])
        );
        memory.forget(&source.id).unwrap();
        drop(workshop);
        let workshop = Workshop::open(&temp.path().join("private/workshop.sqlite")).unwrap();
        assert!(workshop.invoke("identity-fixture", &[], None).is_err());
        assert!(
            workshop
                .rollback_exact("identity-fixture", &report.tool_hash, tool_scope.clone())
                .is_err()
        );
        let catalog = workshop
            .catalog(&tool_scope, Some(&report.tool_hash))
            .unwrap();
        assert_eq!(catalog["tools"][0]["source_unavailable"], true);
        assert!(catalog["tools"][0].get("definition").is_none());
    }
}
