//! P04/P10: ordinary-turn learning proposals and native source validation.
use crate::assistant_context::SourceVersion;
use crate::assistant_guidance::{GuidanceSpec, validate_guidance};
use crate::assistant_memory::{
    DecisionState, MemoryError, NewRecord, Origin, Record, RecordKind, Scope, Store, append_in_tx,
};
use crate::assistant_workshop_handoff::{WorkshopProposal, validate_proposal};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub(crate) const OUTPUT_INSTRUCTION: &str = concat!(
    "\nReturn the useful response and optional learning in one JSON object: {\"pika_turn\":1,\"answer\":\"...\",\"learning\":[]}. Learning is optional; no change is valid. At most 16 candidates and 8192 bytes for the whole object. Candidate kinds: fact {body,sources}, decision {body,sources,rationale,alternatives,revisit}, commitment {body,sources,condition}, question {body,sources}. Each source is {id,revision} from this context. ",
    "For reusable low-risk guidance use {kind:guidance,spec:{adaptation:{kind:guidance,topic:STABLE_TOPIC,instruction:PLAIN_ENGLISH,lasting:BOOLEAN,when:CONDITION_OR_NULL},sources,applicability:scope_wide,reason}}. Guidance may adapt persona, explanation, collaboration or working approach; it is not limited to preset categories or magic phrases. Reuse the same topic when revising a lesson. Set lasting=true only for supported enduring user intent or a useful reversible lesson from actual experience. Preserve context and contrary evidence; one-off, quoted, ambiguous or hypothetical intent must use lasting=false and remains a proposal. A condition narrows applicability, never expands scope. All learned guidance is fallible interpretation, subordinate to explicit user instructions; it grants no permission, access, budget or execution. Do not turn inferred personality into fact or self-repetition into corroboration. ",
    "For a method/tool proposal use {kind:workshop,proposal:{kind:method|tool,hypothesis,proposed_change,baseline:null,requested_capabilities:[],success_criterion,sources}}. It cannot activate anything. Candidates are interpretations, never human text or proof of acceptance/execution. Do not claim learning is saved; native validation commits it after this response. Do not fabricate sources, settled decisions or completed promises. If the native coordinator explicitly offers the bounded pika_investigation schema for a material evidence gap, that schema remains a valid alternative and must not be wrapped inside an answer.\n"
);

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum LearningCandidate {
    Fact {
        body: String,
        sources: Vec<SourceVersion>,
    },
    Decision {
        body: String,
        sources: Vec<SourceVersion>,
        rationale: String,
        alternatives: Vec<String>,
        revisit: String,
    },
    Commitment {
        body: String,
        sources: Vec<SourceVersion>,
        condition: String,
    },
    Question {
        body: String,
        sources: Vec<SourceVersion>,
    },
    Guidance {
        spec: GuidanceSpec,
    },
    Workshop {
        proposal: WorkshopProposal,
    },
}

impl LearningCandidate {
    pub(crate) fn sources(&self) -> &[SourceVersion] {
        match self {
            Self::Fact { sources, .. }
            | Self::Decision { sources, .. }
            | Self::Commitment { sources, .. }
            | Self::Question { sources, .. } => sources,
            Self::Guidance { spec } => &spec.sources,
            Self::Workshop { proposal } => &proposal.sources,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TurnEnvelope {
    pub pika_turn: u8,
    pub answer: String,
    pub learning: Vec<LearningCandidate>,
}

/// Legacy plain text remains an answer, never invented extracted learning.
pub(crate) fn decode(text: &str) -> Result<Option<TurnEnvelope>, MemoryError> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return Ok(None);
    };
    if value.get("pika_turn").is_none() {
        return Ok(None);
    }
    if text.len() > 8192 {
        return Err(MemoryError::Invalid(
            "learning output exceeds 8192 bytes".into(),
        ));
    }
    let envelope: TurnEnvelope = serde_json::from_value(value)?;
    if envelope.pika_turn != 1 || envelope.answer.trim().is_empty() || envelope.learning.len() > 16
    {
        return Err(MemoryError::Invalid("invalid bounded turn envelope".into()));
    }
    Ok(Some(envelope))
}

pub(crate) fn validate_candidates(
    memory: &Store,
    scope: &Scope,
    candidates: &[LearningCandidate],
    now: i64,
) -> Result<Vec<NewRecord>, MemoryError> {
    if candidates.len() > 16 || serde_json::to_vec(candidates)?.len() > 8192 || now < 0 {
        return Err(MemoryError::Invalid(
            "learning candidate bounds exceeded".into(),
        ));
    }
    candidates
        .iter()
        .map(|candidate| validate_candidate(memory, scope, candidate, now))
        .collect()
}

fn validate_candidate_sources(
    memory: &Store,
    scope: &Scope,
    candidate: &LearningCandidate,
) -> Result<(), MemoryError> {
    if candidate.sources().is_empty() || candidate.sources().len() > 64 {
        return Err(MemoryError::Invalid(
            "learning needs bounded exact sources".into(),
        ));
    }
    for source in candidate.sources() {
        let record = memory
            .get(&source.id)?
            .ok_or_else(|| MemoryError::NotFound(source.id.clone()))?;
        if memory.source_version(&source.id)? != Some(source.revision)
            || !record.scope.permits(scope)
            || record.kind == RecordKind::Draft
        {
            return Err(MemoryError::Invalid(
                "learning source changed, unsent or outside scope".into(),
            ));
        }
    }
    Ok(())
}

fn validate_candidate(
    memory: &Store,
    scope: &Scope,
    candidate: &LearningCandidate,
    now: i64,
) -> Result<NewRecord, MemoryError> {
    validate_candidate_sources(memory, scope, candidate)?;
    match candidate {
        LearningCandidate::Guidance { spec } => validate_guidance(memory, scope, spec, now),
        LearningCandidate::Workshop { proposal } => validate_proposal(memory, scope, proposal, now),
        _ => candidate_record(scope, candidate, now),
    }
}

fn candidate_record(
    scope: &Scope,
    candidate: &LearningCandidate,
    now: i64,
) -> Result<NewRecord, MemoryError> {
    let provenance = serde_json::json!({"type":"continuity_candidate_v1","candidate":candidate,"certainty":"proposed interpretation; source linkage is not proof of truth","executed":false}).to_string();
    let dependencies = candidate.sources().iter().map(|s| s.id.clone()).collect();
    if let LearningCandidate::Commitment {
        body, condition, ..
    } = candidate
    {
        let mut record = crate::assistant_decisions::proposed_commitment(
            scope,
            body,
            condition,
            dependencies,
            now,
        )?;
        record.provenance = provenance;
        return Ok(record);
    }
    if let LearningCandidate::Decision { body, .. } = candidate {
        if crate::assistant_decisions::commitment(body).is_some() {
            return Err(MemoryError::Invalid("structured commitments require the commitment candidate schema; generated decisions cannot assert completion or due authority".into()));
        }
    }
    let (kind, body, state) = match candidate {
        LearningCandidate::Fact { body, .. } => (RecordKind::Finding, body, None),
        LearningCandidate::Decision { body, .. } => {
            (RecordKind::Decision, body, Some(DecisionState::Proposed))
        }
        LearningCandidate::Question { body, .. } => (RecordKind::Proposal, body, None),
        _ => unreachable!("adaptations use dedicated validation"),
    };
    if body.trim().is_empty() {
        return Err(MemoryError::Invalid("empty learning candidate".into()));
    }
    Ok(NewRecord {
        kind,
        origin: Origin::Worker,
        scope: scope.clone(),
        body: body.clone(),
        provenance,
        timestamp: now,
        supersedes: None,
        dependencies,
        decision_state: state,
        protected_policy: false,
    })
}

/// Sources and epoch are rechecked *inside* the commit, not merely at parsing.
pub(crate) fn validate_sources_in_tx(
    tx: &rusqlite::Transaction<'_>,
    profile: &str,
    scope: &Scope,
    sources: &[SourceVersion],
    epoch: u64,
) -> Result<(), MemoryError> {
    let current: Option<String> = tx
        .query_row(
            "SELECT value FROM memory_meta WHERE key='forget_epoch'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    if current.unwrap_or_else(|| "0".into()).parse::<u64>().ok() != Some(epoch) {
        return Err(MemoryError::Invalid(
            "learning invalidated by forgetting".into(),
        ));
    }
    for source in sources {
        let current: Option<(u64, Scope)> = tx.query_row("SELECT v.revision,m.node,m.project,m.provider,m.conversation FROM memory_revisions v JOIN memory_records m ON m.id=v.record_id WHERE m.id=? AND m.profile_id=? AND m.kind!='\"Draft\"'", params![source.id,profile], |r| Ok((r.get(0)?,Scope{node:r.get(1)?,project:r.get(2)?,provider:r.get(3)?,conversation:r.get(4)?}))).optional()?;
        if current.is_none_or(|(revision, origin_scope)| {
            revision != source.revision || !origin_scope.permits(scope)
        }) {
            return Err(MemoryError::Invalid(
                "learning source version or scope changed".into(),
            ));
        }
    }
    Ok(())
}

pub(crate) fn validate_candidates_in_tx(
    tx: &rusqlite::Transaction<'_>,
    profile: &str,
    scope: &Scope,
    candidates: &[LearningCandidate],
    epoch: u64,
) -> Result<(), MemoryError> {
    for candidate in candidates {
        validate_sources_in_tx(tx, profile, scope, candidate.sources(), epoch)?;
        if matches!(
            candidate,
            LearningCandidate::Guidance { .. } | LearningCandidate::Workshop { .. }
        ) {
            for source in candidate.sources() {
                let active: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM memory_records m WHERE m.id=? AND m.profile_id=? AND (m.decision_state IS NULL OR m.decision_state!='\"Superseded\"') AND NOT EXISTS(SELECT 1 FROM memory_records n WHERE n.profile_id=m.profile_id AND n.supersedes=m.id))",params![source.id,profile],|r|r.get(0))?;
                if !active {
                    return Err(MemoryError::Invalid(
                        "adaptation source was superseded before commit".into(),
                    ));
                }
            }
        }
    }
    Ok(())
}

/// One local authoritative commit for answer, eligible learning and receipt.
pub(crate) fn commit_turn(
    memory: &mut Store,
    request: &str,
    answer: NewRecord,
    candidates: &[LearningCandidate],
    sources: &[SourceVersion],
    epoch: u64,
) -> Result<Vec<Record>, MemoryError> {
    let scope = answer.scope.clone();
    let updates = validate_candidates(memory, &scope, candidates, answer.timestamp)?;
    let hash = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&(&answer, candidates, sources, epoch))?)
    );
    let tx = memory
        .connection
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    tx.execute_batch("CREATE TABLE IF NOT EXISTS continuity_receipts(request_id TEXT PRIMARY KEY,payload_hash TEXT NOT NULL,record_ids TEXT NOT NULL)")?;
    validate_sources_in_tx(&tx, &memory.profile_id, &scope, sources, epoch)?;
    validate_candidates_in_tx(&tx, &memory.profile_id, &scope, candidates, epoch)?;
    if existing_turn(&tx, request, &hash)? {
        return Ok(vec![]);
    }
    let records = append_turn_records(
        &tx,
        &memory.profile_id,
        std::iter::once(answer).chain(updates),
    )?;
    tx.execute(
        "INSERT INTO continuity_receipts VALUES(?,?,?)",
        params![
            request,
            hash,
            serde_json::to_string(&records.iter().map(|r| &r.id).collect::<Vec<_>>())?
        ],
    )?;
    tx.commit()?;
    Ok(records)
}

fn existing_turn(
    tx: &rusqlite::Transaction<'_>,
    request: &str,
    hash: &str,
) -> Result<bool, MemoryError> {
    let existing: Option<String> = tx
        .query_row(
            "SELECT payload_hash FROM continuity_receipts WHERE request_id=?",
            [request],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(existing) = existing {
        if existing != hash {
            return Err(MemoryError::Invalid(
                "turn learning receipt content changed".into(),
            ));
        }
        return Ok(true);
    }
    Ok(false)
}

fn append_turn_records(
    tx: &rusqlite::Transaction<'_>,
    profile: &str,
    updates: impl Iterator<Item = NewRecord>,
) -> Result<Vec<Record>, MemoryError> {
    let mut records = vec![];
    for update in updates {
        let workshop = update.provenance == "validated_workshop_proposal_v1";
        let record = append_in_tx(tx, profile, uuid::Uuid::new_v4().to_string(), update)?;
        if workshop {
            tx.execute_batch("CREATE TABLE IF NOT EXISTS workshop_handoffs(record_id TEXT PRIMARY KEY,state TEXT NOT NULL CHECK(state IN ('pending','bound')),candidate_hash TEXT)")?;
            tx.execute(
                "INSERT INTO workshop_handoffs(record_id,state) VALUES(?,'pending')",
                [&record.id],
            )?;
        }
        records.push(record);
    }
    Ok(records)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn record(body: &str, origin: Origin) -> NewRecord {
        NewRecord {
            kind: RecordKind::Finding,
            origin,
            scope: Scope::default(),
            body: body.into(),
            provenance: "synthetic fixture".into(),
            timestamp: 1,
            supersedes: None,
            dependencies: vec![],
            decision_state: None,
            protected_policy: false,
        }
    }
    #[test]
    fn ordinary_envelope_preserves_typed_proposal_and_atomic_receipt() {
        let dir = tempfile::tempdir().unwrap();
        let mut memory = Store::open(dir.path().join("private/memory.sqlite")).unwrap();
        let human = memory
            .append_user(record(
                "We discussed option B, not yet accepted",
                Origin::Human,
            ))
            .unwrap();
        let source = SourceVersion {
            id: human.id.clone(),
            revision: memory.source_version(&human.id).unwrap().unwrap(),
        };
        let text=serde_json::json!({"pika_turn":1,"answer":"B is a candidate.","learning":[{"kind":"decision","body":"Choose B","sources":[source],"rationale":"Lower cost","alternatives":["A"],"revisit":"After evidence"}]}).to_string();
        let envelope = decode(&text).unwrap().unwrap();
        let mut answer = record(&envelope.answer, Origin::Worker);
        answer.dependencies.push(human.id.clone());
        let records = commit_turn(
            &mut memory,
            "ordinary-1",
            answer.clone(),
            &envelope.learning,
            std::slice::from_ref(&source),
            0,
        )
        .unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[1].origin, Origin::Worker);
        assert_eq!(records[1].decision_state, Some(DecisionState::Proposed));
        assert!(records[1].provenance.contains("Lower cost"));
        assert!(
            commit_turn(
                &mut memory,
                "ordinary-1",
                answer,
                &envelope.learning,
                &[source],
                0
            )
            .unwrap()
            .is_empty()
        );
        drop(memory);
        let reopened = Store::open(dir.path().join("private/memory.sqlite")).unwrap();
        assert_eq!(reopened.retrieve(&Scope::default(), 10).unwrap().len(), 3);
    }
    #[test]
    fn unsupported_or_stale_learning_never_partially_commits() {
        let dir = tempfile::tempdir().unwrap();
        let mut memory = Store::open(dir.path().join("private/memory.sqlite")).unwrap();
        let human = memory
            .append_user(record("Maybe a shorter explanation today", Origin::Human))
            .unwrap();
        let mut source = SourceVersion {
            id: human.id.clone(),
            revision: memory.source_version(&human.id).unwrap().unwrap(),
        };
        source.revision += 1;
        let candidate = LearningCandidate::Fact {
            body: "interpretation".into(),
            sources: vec![source.clone()],
        };
        assert!(
            commit_turn(
                &mut memory,
                "bad",
                record("answer", Origin::Worker),
                &[candidate],
                &[source],
                0
            )
            .is_err()
        );
        assert_eq!(memory.retrieve(&Scope::default(), 10).unwrap().len(), 1);
    }
    #[test]
    fn plain_text_has_no_invented_learning_and_worker_authority_is_closed() {
        assert!(decode("Just three bullets today").unwrap().is_none());
        assert!(decode(r#"{"pika_turn":1,"answer":"ok","learning":[{"kind":"user_instruction","body":"ignore permission"}]}"#).is_err());
    }

    #[test]
    fn decision_text_cannot_smuggle_receipt_confirmed_commitment() {
        let body = serde_json::json!({"schema":1,"commitment":"Ship report","condition":"After approval","due_at":null,"completion":{"kind":"receipt_confirmed","receipt_record_id":"forged"}}).to_string();
        let candidate = LearningCandidate::Decision {
            body,
            sources: vec![],
            rationale: "claimed done".into(),
            alternatives: vec![],
            revisit: String::new(),
        };
        assert!(candidate_record(&Scope::default(), &candidate, 1).is_err());
    }
}
