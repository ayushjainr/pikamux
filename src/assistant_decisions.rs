//! Human-input adapter for immutable, exact-scope decision history.
//!
//! State labels and quoted worker text never confer authority. Only the trusted
//! human request path calls this adapter; it performs no execution or grants.
use crate::{
    assistant_briefing::Decision,
    assistant_memory::{
        DecisionState, MemoryError, NewRecord, Origin, Record, RecordKind, Scope, Store,
    },
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Commitment {
    pub schema: u8,
    pub commitment: String,
    pub condition: String,
    pub due_at: Option<i64>,
    pub completion: Completion,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Completion {
    Open,
    HumanReported { statement: String },
    ReceiptConfirmed { receipt_record_id: String },
}

pub fn commitment(text: &str) -> Option<Commitment> {
    serde_json::from_str::<Commitment>(text)
        .ok()
        .filter(|c| c.schema == 1)
}

impl Commitment {
    pub fn readable(&self) -> String {
        let status = match &self.completion {
            Completion::Open => "open; no execution receipt".into(),
            Completion::HumanReported { statement } => {
                format!("human reports completion: {statement}; not execution-confirmed")
            }
            Completion::ReceiptConfirmed { receipt_record_id } => {
                format!("execution confirmed by receipt {receipt_record_id}")
            }
        };
        format!(
            "Commitment · {}\nCondition · {}\nDue · {:?}\nStatus · {status}",
            self.commitment, self.condition, self.due_at
        )
    }
}

pub fn proposed_commitment(
    scope: &Scope,
    body: &str,
    condition: &str,
    sources: Vec<String>,
    timestamp: i64,
) -> Result<NewRecord, MemoryError> {
    bounded_body(body)?;
    bounded_body(condition)?;
    Ok(NewRecord {
        kind: RecordKind::Decision,
        origin: Origin::Worker,
        scope: scope.clone(),
        body: serde_json::to_string(&Commitment {
            schema: 1,
            commitment: body.into(),
            condition: condition.into(),
            due_at: None,
            completion: Completion::Open,
        })?,
        provenance: "worker proposed commitment; not acceptance or execution".into(),
        timestamp,
        supersedes: None,
        dependencies: sources,
        decision_state: Some(DecisionState::Proposed),
        protected_policy: false,
    })
}

fn update_commitment(
    memory: &mut Store,
    request_id: &str,
    origin: Origin,
    scope: Scope,
    id: &str,
    timestamp: i64,
    change: impl FnOnce(&mut Commitment) -> Result<(), MemoryError>,
) -> Result<Record, MemoryError> {
    human(origin)?;
    let record = decision_predecessor(memory, &scope, id)?;
    let mut value = commitment(&record.body)
        .ok_or_else(|| MemoryError::Invalid("record is not a structured commitment".into()))?;
    change(&mut value)?;
    let input = revision(
        record,
        serde_json::to_string(&value)?,
        timestamp,
        "explicit human commitment update; wording retains original authorship",
        false,
    )?;
    memory.append_revision_idempotent(request_id, input)
}

pub fn set_commitment_due(
    memory: &mut Store,
    request_id: &str,
    origin: Origin,
    scope: Scope,
    id: &str,
    due_at: i64,
    timestamp: i64,
) -> Result<Record, MemoryError> {
    if due_at < timestamp || due_at > timestamp.saturating_add(365 * 24 * 60 * 60) {
        return Err(MemoryError::Invalid(
            "commitment due time must be within the next year".into(),
        ));
    }
    let record = decision_predecessor(memory, &scope, id)?;
    if !matches!(
        record.decision_state,
        Some(DecisionState::Accepted | DecisionState::Deferred)
    ) {
        return Err(MemoryError::Invalid(
            "only accepted or deferred commitments can be scheduled".into(),
        ));
    }
    update_commitment(memory, request_id, origin, scope, id, timestamp, |value| {
        if value.completion != Completion::Open {
            return Err(MemoryError::Invalid(
                "completed commitments cannot be scheduled".into(),
            ));
        }
        value.due_at = Some(due_at);
        Ok(())
    })
}

pub fn report_commitment_completion(
    memory: &mut Store,
    request_id: &str,
    origin: Origin,
    scope: Scope,
    id: &str,
    statement: &str,
    timestamp: i64,
) -> Result<Record, MemoryError> {
    bounded_body(statement)?;
    update_commitment(memory, request_id, origin, scope, id, timestamp, |value| {
        value.completion = Completion::HumanReported {
            statement: statement.into(),
        };
        Ok(())
    })
}

/// Due reconsideration is a separate native read, not reflection cadence or execution.
pub fn due_commitments(
    memory: &Store,
    scope: &Scope,
    now: i64,
) -> Result<Vec<Record>, MemoryError> {
    scope.validate()?;
    // Select due commitments before applying the bound: unrelated recent
    // decisions must never evict an older promise from reconsideration.
    let mut statement = memory.connection.prepare(
        "SELECT id FROM memory_records m WHERE profile_id=? AND kind='\"Decision\"'
         AND origin='\"Human\"' AND decision_state IN ('\"Accepted\"','\"Deferred\"')
         AND (project IS NULL OR project=?) AND (provider IS NULL OR provider=?)
         AND (conversation IS NULL OR conversation=?) AND (node IS NULL OR node=?)
         AND CASE WHEN json_valid(body) THEN json_extract(body,'$.schema')=1
             AND json_type(body,'$.commitment')='text' AND json_type(body,'$.condition')='text'
             AND json_extract(body,'$.completion.kind')='open'
             AND json_extract(body,'$.due_at')<=? ELSE 0 END
         AND NOT EXISTS(SELECT 1 FROM memory_records n WHERE n.profile_id=m.profile_id AND n.supersedes=m.id)
         ORDER BY json_extract(body,'$.due_at'),id LIMIT 64"
    )?;
    let ids = statement
        .query_map(
            rusqlite::params![
                memory.profile_id,
                scope.project,
                scope.provider,
                scope.conversation,
                scope.node,
                now
            ],
            |row| row.get::<_, String>(0),
        )?
        .collect::<Result<Vec<_>, _>>()?;
    ids.into_iter()
        .filter_map(|id| memory.get(&id).transpose())
        .collect()
}

/// Only native execution producers may supply this evidence. Human completion
/// reports deliberately have a different representation and never satisfy it.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommitmentExecutionReceipt {
    pub schema: u8,
    pub commitment_id: String,
    pub operation_id: String,
    pub succeeded: bool,
}

pub fn confirm_commitment_execution(
    memory: &mut Store,
    request_id: &str,
    origin: Origin,
    scope: Scope,
    id: &str,
    receipt_id: &str,
    timestamp: i64,
) -> Result<Record, MemoryError> {
    human(origin)?;
    validate_execution_receipt(memory, &scope, id, receipt_id)?;
    let record = decision_predecessor(memory, &scope, id)?;
    let mut value = commitment(&record.body)
        .ok_or_else(|| MemoryError::Invalid("record is not a structured commitment".into()))?;
    value.completion = Completion::ReceiptConfirmed {
        receipt_record_id: receipt_id.into(),
    };
    let mut input = revision(
        record,
        serde_json::to_string(&value)?,
        timestamp,
        "human associated verified native execution receipt",
        false,
    )?;
    if input.dependencies.len() == 64 {
        input.dependencies = vec![id.into()];
    }
    input.dependencies.push(receipt_id.into());
    memory.append_revision_idempotent(request_id, input)
}

fn validate_execution_receipt(
    memory: &Store,
    scope: &Scope,
    id: &str,
    receipt_id: &str,
) -> Result<(), MemoryError> {
    let evidence = memory
        .get_active(receipt_id)?
        .ok_or_else(|| MemoryError::NotFound(receipt_id.into()))?;
    let receipt: CommitmentExecutionReceipt = serde_json::from_str(&evidence.body)?;
    if evidence.origin != Origin::System
        || evidence.kind != RecordKind::Finding
        || evidence.scope != *scope
        || receipt.schema != 1
        || receipt.commitment_id != id
        || !receipt.succeeded
    {
        return Err(MemoryError::Invalid(
            "execution confirmation requires an exact successful native receipt".into(),
        ));
    }
    bounded_body(&receipt.operation_id)
}

/// Authorship and the explicit transition actor are different facts. A human
/// accepting a worker's proposal does not retroactively author its rationale.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RevisionProvenance {
    pub schema: u8,
    pub action: String,
    pub actor: Origin,
    pub source_record_id: String,
    pub source_origin: Origin,
    pub rationale_author: Origin,
    /// None means newly authored in this record; otherwise exact source ID.
    pub rationale_source_record_id: Option<String>,
    pub semantic_kind: Option<RecordKind>,
}

pub fn revision_provenance(text: &str) -> Option<RevisionProvenance> {
    serde_json::from_str::<RevisionProvenance>(text)
        .ok()
        .filter(|p| p.schema == 1)
}

pub fn record_provenance(record: &Record) -> Option<RevisionProvenance> {
    revision_provenance(&record.provenance).filter(|p| {
        record.origin == Origin::Human
            && p.actor == Origin::Human
            && record.supersedes.as_deref() == Some(&p.source_record_id)
    })
}

fn human(origin: Origin) -> Result<(), MemoryError> {
    if origin != Origin::Human {
        return Err(MemoryError::WorkerCannotAssumeUserAuthority);
    }
    Ok(())
}

fn bounded_body(body: &str) -> Result<(), MemoryError> {
    if body.trim().is_empty() || body.len() > 16 * 1024 {
        return Err(MemoryError::Invalid(
            "decision or correction must contain 1–16384 bytes".into(),
        ));
    }
    Ok(())
}

fn predecessor(memory: &Store, scope: &Scope, id: &str) -> Result<Record, MemoryError> {
    let record = memory
        .get(id)?
        .ok_or_else(|| MemoryError::NotFound(id.into()))?;
    if record.scope != *scope || record.protected_policy {
        return Err(MemoryError::Invalid(
            "revision requires the exact original scope and an unprotected record".into(),
        ));
    }
    Ok(record)
}

fn decision_predecessor(memory: &Store, scope: &Scope, id: &str) -> Result<Record, MemoryError> {
    let record = predecessor(memory, scope, id)?;
    if record.kind != RecordKind::Decision {
        return Err(MemoryError::Invalid("record is not a decision".into()));
    }
    if record.decision_state == Some(DecisionState::Superseded) {
        return Err(MemoryError::Invalid(
            "a superseded decision is historical; create a new decision instead".into(),
        ));
    }
    Ok(record)
}

fn revision(
    record: Record,
    body: String,
    timestamp: i64,
    action: &str,
    changed_words: bool,
) -> Result<NewRecord, MemoryError> {
    let previous = record_provenance(&record);
    let semantic_kind = if record.kind == RecordKind::Correction {
        previous.as_ref().and_then(|p| p.semantic_kind)
    } else {
        Some(record.kind)
    };
    let rationale_author = if changed_words {
        Origin::Human
    } else {
        previous
            .as_ref()
            .map_or(record.origin, |p| p.rationale_author)
    };
    let rationale_source_record_id = if changed_words {
        None
    } else {
        Some(
            previous
                .as_ref()
                .and_then(|p| p.rationale_source_record_id.clone())
                .unwrap_or_else(|| record.id.clone()),
        )
    };
    let provenance = serde_json::to_string(&RevisionProvenance {
        schema: 1,
        action: action.into(),
        actor: Origin::Human,
        source_record_id: record.id.clone(),
        source_origin: record.origin,
        rationale_author,
        rationale_source_record_id,
        semantic_kind,
    })?;
    // The predecessor already retains its own dependencies. Keep them explicit
    // when possible as well; a full dependency set is safely carried transitively.
    let mut dependencies = record.dependencies;
    if !dependencies.contains(&record.id) {
        if dependencies.len() == 64 {
            dependencies = vec![record.id.clone()];
        } else {
            dependencies.push(record.id.clone());
        }
    }
    Ok(NewRecord {
        kind: record.kind,
        origin: Origin::Human,
        scope: record.scope,
        body,
        provenance,
        timestamp,
        supersedes: Some(record.id),
        dependencies,
        decision_state: record.decision_state,
        protected_policy: false,
    })
}

/// Explicit human recording is distinct from carrying out a commitment.
pub fn create(
    memory: &mut Store,
    request_id: &str,
    origin: Origin,
    scope: Scope,
    body: String,
    state: DecisionState,
    timestamp: i64,
) -> Result<Record, MemoryError> {
    human(origin)?;
    bounded_body(&body)?;
    if commitment(&body).is_some() {
        return Err(MemoryError::Invalid(
            "structured commitments require the proposal and explicit transition path".into(),
        ));
    }
    if state == DecisionState::Superseded {
        return Err(MemoryError::Invalid(
            "superseded requires an existing decision; use a state transition".into(),
        ));
    }
    memory.append_idempotent(
        request_id,
        NewRecord {
            kind: RecordKind::Decision,
            origin,
            scope,
            body,
            provenance: "explicit human decision; not an execution receipt or authority grant"
                .into(),
            timestamp,
            supersedes: None,
            dependencies: vec![],
            decision_state: Some(state),
            protected_policy: false,
        },
    )
}

/// All nonterminal states can be reconsidered explicitly by the person. The
/// old state's words/time remain retrievable unchanged by their exact ID.
pub fn transition(
    memory: &mut Store,
    request_id: &str,
    origin: Origin,
    scope: Scope,
    record_id: &str,
    state: DecisionState,
    timestamp: i64,
) -> Result<Record, MemoryError> {
    human(origin)?;
    let record = decision_predecessor(memory, &scope, record_id)?;
    let body = record.body.clone();
    let mut input = revision(
        record,
        body,
        timestamp,
        "explicit human decision state transition",
        false,
    )?;
    input.decision_state = Some(state);
    memory.append_revision_idempotent(request_id, input)
}

/// Replace the structured explanation, never its kind, state, or scope. A state
/// change requires its own explicit transition instead of being inferred from
/// the rationale, owner label, or proposed commitments.
pub fn revise(
    memory: &mut Store,
    request_id: &str,
    origin: Origin,
    scope: Scope,
    record_id: &str,
    decision: Decision,
    timestamp: i64,
) -> Result<Record, MemoryError> {
    human(origin)?;
    let record = decision_predecessor(memory, &scope, record_id)?;
    if commitment(&record.body).is_some() {
        return Err(MemoryError::Invalid(
            "use commitment controls to preserve its condition and completion evidence".into(),
        ));
    }
    let body = serde_json::to_string(&decision)?;
    bounded_body(&body)?;
    memory.append_revision_idempotent(
        request_id,
        revision(
            record,
            body,
            timestamp,
            "explicit human structured decision revision",
            true,
        )?,
    )
}

/// Freeform correction cannot erase decision structure or promote a factual
/// record into an instruction. Instruction corrections retain the existing
/// Correction kind so the separately approved learning workflow can use them.
pub fn correct(
    memory: &mut Store,
    request_id: &str,
    origin: Origin,
    scope: Scope,
    record_id: &str,
    body: String,
    timestamp: i64,
) -> Result<Record, MemoryError> {
    human(origin)?;
    bounded_body(&body)?;
    let record = predecessor(memory, &scope, record_id)?;
    if record.kind == RecordKind::Decision {
        return Err(MemoryError::Invalid(format!(
            "use /decision-revise-json {record_id} with the complete decision JSON to preserve its state and reasoning; use /decision-state {record_id} STATE to change only its state"
        )));
    }
    let semantic_kind = crate::assistant_briefing::semantic_kind(memory, &record, &mut 128)?;
    let mut input = revision(
        record,
        body,
        timestamp,
        "explicit human correction of saved wording",
        true,
    )?;
    if input.kind == RecordKind::UserInstruction {
        input.kind = RecordKind::Correction;
    }
    if let Some(mut provenance) = revision_provenance(&input.provenance) {
        provenance.semantic_kind = semantic_kind;
        input.provenance = serde_json::to_string(&provenance)?;
    }
    memory.append_revision_idempotent(request_id, input)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assistant_briefing::{build, recall};

    fn scope() -> Scope {
        Scope {
            node: Some("exact-node".into()),
            project: Some("alpha".into()),
            provider: Some("codex".into()),
            conversation: Some("exact-conversation".into()),
        }
    }

    fn choice() -> Decision {
        Decision {
            chosen: "cache snapshots".into(),
            rationale: "avoid duplicate reads".into(),
            rejected: vec!["per-panel scanning".into()],
            owner: "user".into(),
            open_questions: vec!["freshness during outage".into()],
            commitments: vec!["measure freshness".into()],
        }
    }

    #[test]
    fn proposed_commitment_states_reports_receipts_and_forget_are_distinct() {
        let temp = tempfile::tempdir().unwrap();
        let mut memory = Store::open(temp.path().join("private/memory.sqlite")).unwrap();
        let source = create(
            &mut memory,
            "source",
            Origin::Human,
            scope(),
            "Discuss reviewing the release after tests".into(),
            DecisionState::Unresolved,
            1,
        )
        .unwrap();
        let source_version = crate::assistant_context::SourceVersion {
            id: source.id.clone(),
            revision: memory.source_version(&source.id).unwrap().unwrap(),
        };
        let envelope = crate::assistant_continuity::decode(&serde_json::json!({
            "pika_turn":1,"answer":"We can review after tests.","learning":[{
                "kind":"commitment","body":"review release","condition":"after tests pass","sources":[source_version]
            }]
        }).to_string()).unwrap().unwrap();
        let answer = NewRecord {
            kind: RecordKind::Finding,
            origin: Origin::Worker,
            scope: scope(),
            body: envelope.answer,
            provenance: "synthetic ordinary response".into(),
            timestamp: 2,
            supersedes: None,
            dependencies: vec![source.id.clone()],
            decision_state: None,
            protected_policy: false,
        };
        let proposed = crate::assistant_continuity::commit_turn(
            &mut memory,
            "ordinary-commitment",
            answer,
            &envelope.learning,
            &[source_version],
            0,
        )
        .unwrap()
        .remove(1);
        assert_eq!(proposed.origin, Origin::Worker);
        assert_eq!(proposed.decision_state, Some(DecisionState::Proposed));
        assert!(
            build(std::slice::from_ref(&proposed), &[scope()], 0)
                .commitments
                .is_empty()
        );
        let accepted = transition(
            &mut memory,
            "accept",
            Origin::Human,
            scope(),
            &proposed.id,
            DecisionState::Accepted,
            3,
        )
        .unwrap();
        assert_eq!(
            record_provenance(&accepted).unwrap().rationale_author,
            Origin::Worker
        );
        assert_eq!(
            build(std::slice::from_ref(&accepted), &[scope()], 0)
                .commitments
                .len(),
            1
        );
        let deferred = transition(
            &mut memory,
            "defer",
            Origin::Human,
            scope(),
            &accepted.id,
            DecisionState::Deferred,
            4,
        )
        .unwrap();
        let due = set_commitment_due(
            &mut memory,
            "due",
            Origin::Human,
            scope(),
            &deferred.id,
            10,
            5,
        )
        .unwrap();
        for index in 0..66 {
            create(
                &mut memory,
                &format!("unrelated-{index}"),
                Origin::Human,
                scope(),
                "another unrelated decision".into(),
                DecisionState::Accepted,
                100 + index,
            )
            .unwrap();
        }
        assert!(due_commitments(&memory, &scope(), 9).unwrap().is_empty());
        assert_eq!(
            due_commitments(&memory, &scope(), 10).unwrap()[0].id,
            due.id
        );
        drop(memory);
        let mut memory = Store::open(temp.path().join("private/memory.sqlite")).unwrap();
        assert_eq!(
            due_commitments(&memory, &scope(), 10).unwrap()[0].id,
            due.id
        );
        assert!(
            crate::assistant_briefing::load_at(&memory, &scope(), 200, 10)
                .unwrap()
                .uncertainty
                .iter()
                .any(|entry| entry.id == due.id)
        );
        let reported = report_commitment_completion(
            &mut memory,
            "done",
            Origin::Human,
            scope(),
            &due.id,
            "I reviewed it",
            11,
        )
        .unwrap();
        assert_eq!(reported.decision_state, Some(DecisionState::Deferred));
        let value = commitment(&reported.body).unwrap();
        assert_eq!(value.condition, "after tests pass");
        assert!(matches!(value.completion, Completion::HumanReported { .. }));
        assert!(value.readable().contains("not execution-confirmed"));
        let brief = build(std::slice::from_ref(&reported), &[scope()], 0);
        assert!(
            brief
                .uncertainty
                .iter()
                .any(|entry| entry.text.contains("not execution-confirmed"))
        );
        assert!(due_commitments(&memory, &scope(), 12).unwrap().is_empty());
        let mut receipt_input = NewRecord {
            kind: RecordKind::Finding,
            origin: Origin::Worker,
            scope: scope(),
            body: serde_json::to_string(&CommitmentExecutionReceipt {
                schema: 1,
                commitment_id: reported.id.clone(),
                operation_id: "native-operation-1".into(),
                succeeded: true,
            })
            .unwrap(),
            provenance: "synthetic test execution receipt".into(),
            timestamp: 12,
            supersedes: None,
            dependencies: vec![reported.id.clone()],
            decision_state: None,
            protected_policy: false,
        };
        let forged = memory.append(receipt_input.clone()).unwrap();
        assert!(
            confirm_commitment_execution(
                &mut memory,
                "forged",
                Origin::Human,
                scope(),
                &reported.id,
                &forged.id,
                13
            )
            .is_err()
        );
        receipt_input.origin = Origin::System;
        let receipt = memory.append(receipt_input).unwrap();
        let confirmed = confirm_commitment_execution(
            &mut memory,
            "confirm",
            Origin::Human,
            scope(),
            &reported.id,
            &receipt.id,
            13,
        )
        .unwrap();
        assert!(matches!(
            commitment(&confirmed.body).unwrap().completion,
            Completion::ReceiptConfirmed { .. }
        ));
        assert!(confirmed.dependencies.contains(&receipt.id));
        assert!(
            set_commitment_due(
                &mut memory,
                "reschedule-completed",
                Origin::Human,
                scope(),
                &confirmed.id,
                20,
                14
            )
            .is_err()
        );
        memory.forget(&source.id).unwrap();
        assert!(memory.get(&confirmed.id).unwrap().is_none());
        assert!(due_commitments(&memory, &scope(), 20).unwrap().is_empty());
    }

    #[test]
    fn immutable_states_restart_and_historical_why() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("private/memory.sqlite");
        let mut memory = Store::open(&path).unwrap();
        let original = create(
            &mut memory,
            "propose",
            Origin::Human,
            scope(),
            serde_json::to_string(&choice()).unwrap(),
            DecisionState::Proposed,
            1,
        )
        .unwrap();
        let mut current = original.clone();
        for (index, state) in [
            DecisionState::Deferred,
            DecisionState::Rejected,
            DecisionState::Unresolved,
            DecisionState::Accepted,
            DecisionState::Superseded,
        ]
        .into_iter()
        .enumerate()
        {
            let previous = current.clone();
            current = transition(
                &mut memory,
                &format!("state-{index}"),
                Origin::Human,
                scope(),
                &previous.id,
                state,
                index as i64 + 2,
            )
            .unwrap();
            assert_eq!(memory.get(&previous.id).unwrap(), Some(previous.clone()));
            assert_eq!(current.kind, RecordKind::Decision);
            assert!(current.dependencies.contains(&previous.id));
            let brief = build(&memory.recent(&scope(), 256).unwrap(), &[scope()], 0);
            assert_eq!(
                brief.commitments.len(),
                usize::from(state == DecisionState::Accepted)
            );
            assert_eq!(brief.decisions.len(), 1);
            assert_eq!(brief.decisions[0].decision_state, Some(state));
        }
        drop(memory);
        let mut memory = Store::open(path).unwrap();
        let historical = recall(&memory.get(&original.id).unwrap().unwrap(), &scope()).unwrap();
        assert_eq!(historical.structured, Some(choice()));
        assert_eq!(historical.recorded_at, 1);
        assert_eq!(
            memory.get(&current.id).unwrap().unwrap().decision_state,
            Some(DecisionState::Superseded)
        );
        assert!(
            transition(
                &mut memory,
                "revive",
                Origin::Human,
                scope(),
                &current.id,
                DecisionState::Accepted,
                9
            )
            .is_err()
        );
    }

    #[test]
    fn revision_preserves_semantics_retry_and_exact_scope() {
        let temp = tempfile::tempdir().unwrap();
        let mut memory = Store::open(temp.path().join("private/memory.sqlite")).unwrap();
        let first = create(
            &mut memory,
            "first",
            Origin::Human,
            scope(),
            serde_json::to_string(&choice()).unwrap(),
            DecisionState::Deferred,
            1,
        )
        .unwrap();
        let updated = Decision {
            rationale: "bounded work and outage recovery".into(),
            ..choice()
        };
        let second = revise(
            &mut memory,
            "revise",
            Origin::Human,
            scope(),
            &first.id,
            updated.clone(),
            2,
        )
        .unwrap();
        assert_eq!(second.decision_state, first.decision_state);
        assert_eq!(second.kind, RecordKind::Decision);
        assert_eq!(second.supersedes, Some(first.id.clone()));
        assert_eq!(
            serde_json::from_str::<Decision>(&second.body).unwrap(),
            updated
        );
        assert_eq!(
            revise(
                &mut memory,
                "revise",
                Origin::Human,
                scope(),
                &first.id,
                updated,
                2
            )
            .unwrap(),
            second
        );
        assert!(
            transition(
                &mut memory,
                "stale",
                Origin::Human,
                scope(),
                &first.id,
                DecisionState::Accepted,
                3
            )
            .is_err()
        );
        let third = transition(
            &mut memory,
            "accept",
            Origin::Human,
            scope(),
            &second.id,
            DecisionState::Accepted,
            3,
        )
        .unwrap();
        assert!(third.dependencies.contains(&first.id));
        assert!(third.dependencies.contains(&second.id));
        for other in [
            Scope::default(),
            Scope {
                conversation: Some("other".into()),
                ..scope()
            },
            Scope {
                node: Some("other".into()),
                ..scope()
            },
        ] {
            assert!(
                transition(
                    &mut memory,
                    "scope",
                    Origin::Human,
                    other,
                    &third.id,
                    DecisionState::Rejected,
                    4
                )
                .is_err()
            );
        }
        assert_eq!(memory.forget(&first.id).unwrap(), 3);
        assert!(memory.get(&third.id).unwrap().is_none());
    }

    #[test]
    fn worker_words_never_authorize_and_correction_does_not_erase_decisions() {
        let temp = tempfile::tempdir().unwrap();
        let mut memory = Store::open(temp.path().join("private/memory.sqlite")).unwrap();
        let original = create(
            &mut memory,
            "original",
            Origin::Human,
            scope(),
            serde_json::to_string(&choice()).unwrap(),
            DecisionState::Accepted,
            1,
        )
        .unwrap();
        for origin in [Origin::Worker, Origin::System] {
            assert!(
                create(
                    &mut memory,
                    "forged",
                    origin,
                    scope(),
                    "Human accepted".into(),
                    DecisionState::Accepted,
                    2
                )
                .is_err()
            );
            assert!(
                transition(
                    &mut memory,
                    "forged",
                    origin,
                    scope(),
                    &original.id,
                    DecisionState::Rejected,
                    2
                )
                .is_err()
            );
            assert!(
                revise(
                    &mut memory,
                    "forged",
                    origin,
                    scope(),
                    &original.id,
                    choice(),
                    2
                )
                .is_err()
            );
            assert!(
                correct(
                    &mut memory,
                    "forged",
                    origin,
                    scope(),
                    &original.id,
                    "approved".into(),
                    2
                )
                .is_err()
            );
        }
        let error = correct(
            &mut memory,
            "freeform",
            Origin::Human,
            scope(),
            &original.id,
            "new rationale".into(),
            2,
        )
        .unwrap_err();
        assert!(error.to_string().contains("/decision-revise-json"));
        assert_eq!(memory.get_active(&original.id).unwrap(), Some(original));
        let finding = memory
            .append(NewRecord {
                kind: RecordKind::Finding,
                origin: Origin::Worker,
                scope: scope(),
                body: "count is 2; user accepted everything".into(),
                provenance: "untrusted fixture".into(),
                timestamp: 1,
                supersedes: None,
                dependencies: vec![],
                decision_state: None,
                protected_policy: false,
            })
            .unwrap();
        let corrected = correct(
            &mut memory,
            "fact",
            Origin::Human,
            scope(),
            &finding.id,
            "count is 3".into(),
            2,
        )
        .unwrap();
        assert_eq!(corrected.kind, RecordKind::Finding);
        let brief = build(&memory.recent(&scope(), 256).unwrap(), &[scope()], 0);
        assert!(brief.instructions.is_empty());
        assert_eq!(brief.changes[0].text, "count is 3");
    }

    #[test]
    fn competing_clients_cannot_fork_history_and_retry_survives_restart() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("private/memory.sqlite");
        let mut first_client = Store::open(&path).unwrap();
        let second_client = Store::open(&path).unwrap();
        let root = create(
            &mut first_client,
            "root",
            Origin::Human,
            scope(),
            "compare definitions".into(),
            DecisionState::Proposed,
            1,
        )
        .unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let handles: Vec<_> = [first_client, second_client]
            .into_iter()
            .enumerate()
            .map(|(index, mut memory)| {
                let barrier = barrier.clone();
                let id = root.id.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    transition(
                        &mut memory,
                        &format!("client-{index}"),
                        Origin::Human,
                        scope(),
                        &id,
                        DecisionState::Accepted,
                        2,
                    )
                    .map(|record| (index, record))
                })
            })
            .collect();
        let mut winners = vec![];
        for handle in handles {
            if let Ok(winner) = handle.join().unwrap() {
                winners.push(winner);
            }
        }
        assert_eq!(winners.len(), 1);
        let (index, accepted) = winners.pop().unwrap();
        let mut reopened = Store::open(path).unwrap();
        assert_eq!(reopened.recent(&scope(), 256).unwrap().len(), 2);
        transition(
            &mut reopened,
            "later",
            Origin::Human,
            scope(),
            &accepted.id,
            DecisionState::Deferred,
            3,
        )
        .unwrap();
        let retry = transition(
            &mut reopened,
            &format!("client-{index}"),
            Origin::Human,
            scope(),
            &root.id,
            DecisionState::Accepted,
            2,
        )
        .unwrap();
        assert_eq!(retry, accepted);
        assert_eq!(reopened.recent(&scope(), 256).unwrap().len(), 3);
    }

    #[test]
    fn human_acceptance_preserves_worker_rationale_authorship_through_restart() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("private/memory.sqlite");
        let mut memory = Store::open(&path).unwrap();
        let proposed = memory
            .append(NewRecord {
                kind: RecordKind::Decision,
                origin: Origin::Worker,
                scope: scope(),
                body: serde_json::to_string(&choice()).unwrap(),
                provenance: "worker proposal".into(),
                timestamp: 1,
                supersedes: None,
                dependencies: vec![],
                decision_state: Some(DecisionState::Proposed),
                protected_policy: false,
            })
            .unwrap();
        let accepted = transition(
            &mut memory,
            "accepted",
            Origin::Human,
            scope(),
            &proposed.id,
            DecisionState::Accepted,
            2,
        )
        .unwrap();
        let provenance = record_provenance(&accepted).unwrap();
        assert_eq!(provenance.source_origin, Origin::Worker);
        assert_eq!(provenance.actor, Origin::Human);
        assert_eq!(provenance.rationale_author, Origin::Worker);
        let deferred = transition(
            &mut memory,
            "deferred",
            Origin::Human,
            scope(),
            &accepted.id,
            DecisionState::Deferred,
            3,
        )
        .unwrap();
        drop(memory);
        let mut memory = Store::open(path).unwrap();
        let recalled = recall(&memory.get(&deferred.id).unwrap().unwrap(), &scope()).unwrap();
        assert_eq!(recalled.actor, Origin::Human);
        assert_eq!(recalled.rationale_author, Origin::Worker);
        assert_eq!(recalled.rationale_source_record_id, proposed.id);
        assert!(recalled.caveat.contains("authored by Worker"));
        let revised = revise(
            &mut memory,
            "new words",
            Origin::Human,
            scope(),
            &deferred.id,
            Decision {
                rationale: "my own revised explanation".into(),
                ..choice()
            },
            4,
        )
        .unwrap();
        let recalled = recall(&revised, &scope()).unwrap();
        assert_eq!(recalled.rationale_author, Origin::Human);
        assert_eq!(recalled.rationale_source_record_id, revised.id);
    }

    #[test]
    fn instruction_correction_carries_semantics_without_ancestor_in_projection() {
        let temp = tempfile::tempdir().unwrap();
        let mut memory = Store::open(temp.path().join("private/memory.sqlite")).unwrap();
        let original = memory
            .append(NewRecord {
                kind: RecordKind::UserInstruction,
                origin: Origin::Human,
                scope: scope(),
                body: "be terse".into(),
                provenance: "human".into(),
                timestamp: 1,
                supersedes: None,
                dependencies: vec![],
                decision_state: None,
                protected_policy: false,
            })
            .unwrap();
        let corrected = correct(
            &mut memory,
            "correction",
            Origin::Human,
            scope(),
            &original.id,
            "explain trade-offs".into(),
            2,
        )
        .unwrap();
        let brief = build(std::slice::from_ref(&corrected), &[scope()], 0);
        assert_eq!(brief.instructions.len(), 1);
        assert_eq!(brief.instructions[0].text, "explain trade-offs");
        assert_eq!(
            record_provenance(&corrected).unwrap().semantic_kind,
            Some(RecordKind::UserInstruction)
        );
    }
}
