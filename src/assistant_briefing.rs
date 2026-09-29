//! Deterministic briefing and decision recall over explicitly supplied evidence.
//! No scanners, providers, authority changes, or cognitive scoring live here.
use crate::assistant_decisions::{RevisionProvenance, record_provenance, revision_provenance};
use crate::assistant_memory::{
    DecisionState, MemoryError, Origin, Record, RecordKind, RecordLineage, Scope, Store,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// Optional structure supplied by the person, never inferred as an approval.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Decision {
    pub chosen: String,
    pub rationale: String,
    #[serde(default)]
    pub rejected: Vec<String>,
    pub owner: String,
    #[serde(default)]
    pub open_questions: Vec<String>,
    #[serde(default)]
    pub commitments: Vec<String>,
}

/// Present saved words without inventing or grading the user's rationale.
pub fn readable_decision(text: &str) -> String {
    if let Some(value) = crate::assistant_decisions::commitment(text) {
        return value.readable();
    }
    let Ok(decision) = serde_json::from_str::<Decision>(text) else {
        return text.to_owned();
    };
    let mut lines = vec![
        format!("Choice · {}", decision.chosen),
        format!("Why · {}", decision.rationale),
        format!("Owner · {}", decision.owner),
    ];
    for (label, items) in [
        ("Rejected", decision.rejected),
        ("Still uncertain", decision.open_questions),
        ("Commitments if accepted", decision.commitments),
    ] {
        if !items.is_empty() {
            lines.push(format!("{label} · {}", items.join("; ")));
        }
    }
    lines.join("\n")
}
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Entry {
    pub id: String,
    pub text: String,
    pub recorded_at: i64,
    pub scope: Scope,
    pub decision_state: Option<DecisionState>,
    pub actor: Origin,
    pub source_lineage: Option<RevisionProvenance>,
}
impl From<&Record> for Entry {
    fn from(record: &Record) -> Self {
        Self {
            id: record.id.clone(),
            text: record.body.clone(),
            recorded_at: record.timestamp,
            scope: record.scope.clone(),
            decision_state: record.decision_state,
            actor: record.origin,
            source_lineage: record_provenance(record),
        }
    }
}
#[derive(Clone, Debug, Default, Serialize, PartialEq, Eq)]
pub struct Brief {
    pub changes: Vec<Entry>,
    pub decisions: Vec<Entry>,
    pub commitments: Vec<Entry>,
    pub uncertainty: Vec<Entry>,
    pub instructions: Vec<Entry>,
    pub ignored_drafts: usize,
    pub coverage: String,
    pub coverage_details: Coverage,
}

#[derive(Clone, Debug, Default, Serialize, PartialEq, Eq)]
pub struct Coverage {
    pub active_typed_query: bool,
    pub limited_kinds: Vec<RecordKind>,
    pub byte_limited: bool,
    pub input_limited: bool,
    pub unresolved_correction_ids: Vec<String>,
}

/// Current decisions/directives each have a separate bounded retrieval quota;
/// newer findings cannot evict them. Only changes honor `since`. Seven small
/// typed pages total at most 256 records and 256 KiB of body/provenance; legacy
/// correction classification gets at most 128 body-free ancestry lookups.
pub fn load(memory: &Store, scope: &Scope, since: i64) -> Result<Brief, MemoryError> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .min(i64::MAX as u64) as i64;
    load_at(memory, scope, since, now)
}

pub fn load_at(memory: &Store, scope: &Scope, since: i64, now: i64) -> Result<Brief, MemoryError> {
    let due = crate::assistant_decisions::due_commitments(memory, scope, now)?;
    let due_ids: BTreeSet<_> = due.iter().map(|record| record.id.clone()).collect();
    let mut records = Vec::new();
    let mut coverage = Coverage {
        active_typed_query: true,
        ..Coverage::default()
    };
    let mut bytes = 0usize;
    if due.len() == 64 {
        coverage.limited_kinds.push(RecordKind::Decision);
    }
    append_bounded(&mut records, due, &mut bytes, &mut coverage);
    for (kind, limit) in [
        (RecordKind::Decision, 64usize.saturating_sub(due_ids.len())),
        (RecordKind::UserInstruction, 32),
        (RecordKind::Correction, 32),
        (RecordKind::Proposal, 24),
        (RecordKind::InferredPreference, 24),
        (RecordKind::Finding, 48),
        (RecordKind::Briefing, 32),
    ] {
        let recent_since =
            matches!(kind, RecordKind::Finding | RecordKind::Briefing).then_some(since);
        let page = memory.active_by_kind(scope, kind, recent_since, limit)?;
        coverage.byte_limited |= page.byte_limited;
        if page.limited {
            coverage.limited_kinds.push(kind);
        }
        append_bounded(
            &mut records,
            page.records
                .into_iter()
                .filter(|record| !due_ids.contains(&record.id)),
            &mut bytes,
            &mut coverage,
        );
    }
    let mut instructions = BTreeSet::new();
    let mut ancestry_budget = 128;
    for record in records.iter().filter(|r| r.kind == RecordKind::Correction) {
        match semantic_kind(memory, record, &mut ancestry_budget)? {
            Some(RecordKind::UserInstruction) => {
                instructions.insert(record.id.clone());
            }
            None => coverage.unresolved_correction_ids.push(record.id.clone()),
            Some(_) => {}
        }
    }
    let mut brief = build_classified(&records, std::slice::from_ref(scope), since, &instructions);
    let partial = coverage.byte_limited
        || !coverage.limited_kinds.is_empty()
        || !coverage.unresolved_correction_ids.is_empty();
    brief.coverage_details = coverage;
    if partial {
        brief.coverage.push_str(" Partial bounded coverage: some active records or correction ancestry were omitted. Empty sections do not establish absence; inspect exact record IDs for historical recall.");
    } else {
        brief.coverage.push_str(
            " Active decisions and instructions are retrieved independently of recent activity.",
        );
    }
    Ok(brief)
}

fn append_bounded(
    records: &mut Vec<Record>,
    incoming: impl IntoIterator<Item = Record>,
    bytes: &mut usize,
    coverage: &mut Coverage,
) {
    for record in incoming {
        let cost = record.body.len().saturating_add(record.provenance.len());
        if bytes.saturating_add(cost) > 256 * 1024 {
            coverage.byte_limited = true;
        } else {
            *bytes += cost;
            records.push(record);
        }
    }
}

pub(crate) fn semantic_kind(
    memory: &Store,
    record: &Record,
    budget: &mut usize,
) -> Result<Option<RecordKind>, MemoryError> {
    let mut current = RecordLineage {
        id: record.id.clone(),
        kind: record.kind,
        origin: record.origin,
        scope: record.scope.clone(),
        supersedes: record.supersedes.clone(),
        provenance: record.provenance.clone(),
    };
    for _ in 0..=128 {
        if current.scope != record.scope {
            return Ok(None);
        }
        if current.kind != RecordKind::Correction {
            return Ok((current.kind != RecordKind::UserInstruction
                || current.origin == Origin::Human)
                .then_some(current.kind));
        }
        if current.origin != Origin::Human {
            return Ok(None);
        }
        if let Some(kind) = revision_provenance(&current.provenance)
            .filter(|p| {
                p.actor == Origin::Human
                    && current.supersedes.as_deref() == Some(&p.source_record_id)
            })
            .and_then(|p| p.semantic_kind)
        {
            return Ok(Some(kind));
        }
        let Some(id) = current.supersedes.as_deref() else {
            return Ok(None);
        };
        if *budget == 0 {
            return Ok(None);
        }
        *budget -= 1;
        let Some(parent) = memory.lineage(id)? else {
            return Ok(None);
        };
        current = parent;
    }
    Ok(None)
}

/// `allowed` is selected by the host/user, never derived from evidence text.
/// Historical decisions remain visible; recent findings are not called facts.
pub fn build(records: &[Record], allowed: &[Scope], since: i64) -> Brief {
    build_classified(records, allowed, since, &BTreeSet::new())
}

fn build_classified(
    records: &[Record],
    allowed: &[Scope],
    since: i64,
    instructions: &BTreeSet<String>,
) -> Brief {
    let mut brief = Brief {
        coverage: "Saved, dated evidence only; not a live verification of project state.".into(),
        ..Brief::default()
    };
    brief.coverage_details.input_limited = records.len() > 256;
    let eligible: Vec<_> = records
        .iter()
        .filter(|r| allowed.iter().any(|s| r.scope.permits(s)))
        .take(256)
        .collect();
    let superseded: BTreeSet<_> = eligible
        .iter()
        .filter_map(|r| r.supersedes.as_deref())
        .collect();
    for record in eligible
        .iter()
        .filter(|r| !superseded.contains(r.id.as_str()))
    {
        let entry = Entry::from(*record);
        match record.kind {
            RecordKind::Draft => brief.ignored_drafts += 1,
            RecordKind::UserInstruction if record.origin == Origin::Human => {
                brief.instructions.push(entry)
            }
            RecordKind::Correction => {
                // Legacy corrections may revise a finding or a decision. The
                // label alone must not turn factual wording into instructions.
                if instructions.contains(&record.id) || corrects_instruction(record, &eligible) {
                    brief.instructions.push(entry);
                } else if record.timestamp >= since {
                    brief.changes.push(entry);
                }
            }
            RecordKind::Decision => {
                if project_commitment(&mut brief, record, &entry) {
                    continue;
                }
                if matches!(
                    record.decision_state,
                    Some(
                        DecisionState::Unresolved
                            | DecisionState::Proposed
                            | DecisionState::Deferred
                    )
                ) {
                    brief.uncertainty.push(entry.clone());
                }
                if let Ok(decision) = serde_json::from_str::<Decision>(&record.body) {
                    if record.decision_state == Some(DecisionState::Accepted)
                        && record.origin == Origin::Human
                    {
                        for text in decision.commitments {
                            brief.commitments.push(Entry {
                                text,
                                ..entry.clone()
                            });
                        }
                    }
                    for text in decision.open_questions {
                        brief.uncertainty.push(Entry {
                            text,
                            ..entry.clone()
                        });
                    }
                }
                brief.decisions.push(entry);
            }
            RecordKind::Proposal | RecordKind::InferredPreference => brief.uncertainty.push(entry),
            RecordKind::Finding | RecordKind::Briefing if record.timestamp >= since => {
                brief.changes.push(entry)
            }
            _ => {}
        }
    }
    brief
}

fn project_commitment(brief: &mut Brief, record: &Record, entry: &Entry) -> bool {
    let Some(value) = crate::assistant_decisions::commitment(&record.body) else {
        return false;
    };
    let item = Entry {
        text: value.readable(),
        ..entry.clone()
    };
    if record.decision_state == Some(DecisionState::Accepted) && record.origin == Origin::Human {
        brief.commitments.push(item.clone());
    } else {
        brief.uncertainty.push(item.clone());
    }
    brief.decisions.push(item);
    true
}

fn corrects_instruction(record: &Record, records: &[&Record]) -> bool {
    let mut current = record;
    for _ in 0..records.len() {
        if current.origin != Origin::Human || current.scope != record.scope {
            return false;
        }
        if current.kind == RecordKind::UserInstruction {
            return true;
        }
        if current.kind != RecordKind::Correction {
            return false;
        }
        if let Some(kind) = record_provenance(current).and_then(|p| p.semantic_kind) {
            return kind == RecordKind::UserInstruction;
        }
        let Some(parent) = current
            .supersedes
            .as_ref()
            .and_then(|id| records.iter().find(|r| r.id == *id))
        else {
            // Missing ancestry is unknown, never inferred instruction authority.
            return false;
        };
        current = parent;
    }
    false
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Recall {
    pub record_id: String,
    pub recorded_at: i64,
    pub original_words: String,
    pub structured: Option<Decision>,
    pub decision_state: Option<DecisionState>,
    pub supersedes: Option<String>,
    pub actor: Origin,
    pub source_lineage: Option<RevisionProvenance>,
    pub rationale_author: Origin,
    pub rationale_source_record_id: String,
    pub caveat: String,
}
pub fn recall(record: &Record, scope: &Scope) -> Result<Recall, &'static str> {
    if record.kind != RecordKind::Decision || !record.scope.permits(scope) {
        return Err("No decision in this scope");
    }
    let source_lineage = record_provenance(record);
    let rationale_author = source_lineage
        .as_ref()
        .map_or(record.origin, |p| p.rationale_author);
    let rationale_source_record_id = source_lineage
        .as_ref()
        .and_then(|p| p.rationale_source_record_id.clone())
        .unwrap_or_else(|| record.id.clone());
    Ok(Recall {
        record_id: record.id.clone(),
        recorded_at: record.timestamp,
        original_words: record.body.clone(),
        structured: serde_json::from_str(&record.body).ok(),
        decision_state: record.decision_state,
        supersedes: record.supersedes.clone(),
        actor: record.origin,
        source_lineage,
        rationale_author,
        rationale_source_record_id,
        caveat: format!(
            "Historical rationale authored by {rationale_author:?}; this record's action was by {:?}. Acceptance does not change rationale authorship or prove execution. Historical reasoning is not proof that the same choice remains right today. Missing ownership or alternatives are unknown, not reconstructed guesses.",
            record.origin
        ),
    })
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExplainBack {
    pub choice: String,
    pub why: String,
    #[serde(default)]
    pub alternative: String,
    #[serde(default)]
    pub remaining_question: String,
}
/// A transparent completeness prompt, not semantic grading or a learning score.
pub fn explanation_gap(decision: &Decision, explanation: &ExplainBack) -> Option<&'static str> {
    if explanation.choice.trim().is_empty() {
        Some("What did you choose?")
    } else if explanation.why.trim().is_empty() {
        Some("What made that choice preferable?")
    } else if !decision.rejected.is_empty() && explanation.alternative.trim().is_empty() {
        Some("Which alternative did you reject, and what trade-off ruled it out?")
    } else if !decision.open_questions.is_empty()
        && explanation.remaining_question.trim().is_empty()
    {
        Some("What remains uncertain or could change this decision?")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assistant_memory::{NewRecord, Origin, Store};
    fn put(
        store: &mut Store,
        project: &str,
        kind: RecordKind,
        body: String,
        time: i64,
        supersedes: Option<String>,
    ) -> Record {
        store
            .append(NewRecord {
                kind,
                body,
                scope: Scope {
                    project: Some(project.into()),
                    ..Scope::default()
                },
                origin: Origin::Human,
                timestamp: time,
                provenance: "explicit fixture".into(),
                dependencies: supersedes.iter().cloned().collect(),
                supersedes,
                decision_state: (kind == RecordKind::Decision).then_some(DecisionState::Accepted),
                protected_policy: false,
            })
            .unwrap()
    }
    #[test]
    fn replay_correction_restart_and_months_later_recall_are_scoped() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("private/memory.sqlite");
        let mut store = Store::open(&path).unwrap();
        let old = put(
            &mut store,
            "alpha",
            RecordKind::UserInstruction,
            "Be terse".into(),
            1,
            None,
        );
        put(
            &mut store,
            "alpha",
            RecordKind::Correction,
            "Explain the trade-off".into(),
            2,
            Some(old.id),
        );
        put(
            &mut store,
            "beta",
            RecordKind::UserInstruction,
            "Keep beta terse".into(),
            2,
            None,
        );
        put(
            &mut store,
            "alpha",
            RecordKind::Finding,
            "One comparison completed; not verified".into(),
            3,
            None,
        );
        put(
            &mut store,
            "alpha",
            RecordKind::Draft,
            "unsent confidential text".into(),
            4,
            None,
        );
        let decision = Decision {
            chosen: "Cache immutable snapshots".into(),
            rationale: "Avoid repeated work".into(),
            rejected: vec!["Poll all providers per view".into()],
            owner: "User".into(),
            open_questions: vec!["Freshness under outage".into()],
            commitments: vec!["Measure stale reads".into()],
        };
        let record = put(
            &mut store,
            "alpha",
            RecordKind::Decision,
            serde_json::to_string(&decision).unwrap(),
            5,
            None,
        );
        drop(store);
        let store = Store::open(path).unwrap();
        let scope = Scope {
            project: Some("alpha".into()),
            ..Scope::default()
        };
        let records = store.recent(&scope, 256).unwrap();
        let brief = build(&records, std::slice::from_ref(&scope), 2);
        assert_eq!(brief.instructions.len(), 1);
        assert_eq!(brief.instructions[0].text, "Explain the trade-off");
        assert_eq!(brief.changes.len(), 1);
        assert_eq!(brief.commitments.len(), 1);
        assert_eq!(brief.uncertainty.len(), 1);
        assert_eq!(brief.ignored_drafts, 1);
        let historical = recall(&store.get(&record.id).unwrap().unwrap(), &scope).unwrap();
        assert_eq!(historical.structured, Some(decision));
        assert_eq!(historical.recorded_at, 5);
        assert!(
            recall(
                &record,
                &Scope {
                    project: Some("beta".into()),
                    ..Scope::default()
                }
            )
            .is_err()
        );
        let beta = Scope {
            project: Some("beta".into()),
            ..Scope::default()
        };
        assert_eq!(
            build(&store.recent(&beta, 256).unwrap(), &[beta], 9_000_000).instructions[0].text,
            "Keep beta terse"
        );
    }
    #[test]
    fn proposed_or_rejected_choices_are_not_active_commitments() {
        let tmp = tempfile::tempdir().unwrap();
        let mut store = Store::open(tmp.path().join("private/memory.sqlite")).unwrap();
        let mut record = put(
            &mut store,
            "alpha",
            RecordKind::Decision,
            serde_json::to_string(&Decision {
                commitments: vec!["ship tomorrow".into()],
                ..Decision::default()
            })
            .unwrap(),
            1,
            None,
        );
        for state in [
            DecisionState::Proposed,
            DecisionState::Rejected,
            DecisionState::Deferred,
            DecisionState::Unresolved,
            DecisionState::Superseded,
        ] {
            record.decision_state = Some(state);
            let brief = build(
                std::slice::from_ref(&record),
                std::slice::from_ref(&record.scope),
                0,
            );
            assert!(brief.commitments.is_empty());
            assert_eq!(brief.decisions[0].decision_state, Some(state));
        }
        record.decision_state = Some(DecisionState::Accepted);
        assert_eq!(
            build(
                std::slice::from_ref(&record),
                std::slice::from_ref(&record.scope),
                0
            )
            .commitments
            .len(),
            1
        );
        record.origin = Origin::Worker;
        assert!(
            build(
                std::slice::from_ref(&record),
                std::slice::from_ref(&record.scope),
                0
            )
            .commitments
            .is_empty()
        );
    }

    #[test]
    fn factual_and_unproven_legacy_corrections_are_not_instructions() {
        let temp = tempfile::tempdir().unwrap();
        let mut store = Store::open(temp.path().join("private/memory.sqlite")).unwrap();
        let fact = put(
            &mut store,
            "alpha",
            RecordKind::Finding,
            "count: 2".into(),
            1,
            None,
        );
        let correction = put(
            &mut store,
            "alpha",
            RecordKind::Correction,
            "count: 3".into(),
            2,
            Some(fact.id),
        );
        let records = store.recent(&correction.scope, 256).unwrap();
        let brief = build(&records, std::slice::from_ref(&correction.scope), 0);
        assert!(brief.instructions.is_empty());
        assert_eq!(brief.changes.len(), 1);
        assert_eq!(brief.changes[0].text, "count: 3");
        // A bounded working set may omit the parent. Missing provenance cannot
        // turn the surviving correction into a standing instruction.
        assert!(
            build(
                std::slice::from_ref(&correction),
                &[correction.scope.clone()],
                0
            )
            .instructions
            .is_empty()
        );
    }

    #[test]
    fn explain_back_identifies_missing_alternative_without_scoring() {
        let decision = Decision {
            rejected: vec!["Polling".into()],
            ..Decision::default()
        };
        let mut reply = ExplainBack {
            choice: "Caching".into(),
            why: "Less repeated work".into(),
            ..ExplainBack::default()
        };
        assert_eq!(
            explanation_gap(&decision, &reply),
            Some("Which alternative did you reject, and what trade-off ruled it out?")
        );
        reply.alternative = "Polling costs more".into();
        assert_eq!(explanation_gap(&decision, &reply), None);
    }

    #[test]
    fn active_brief_retains_old_commitments_and_legacy_instruction_corrections() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("private/memory.sqlite");
        let mut memory = Store::open(&path).unwrap();
        let decision = put(
            &mut memory,
            "alpha",
            RecordKind::Decision,
            serde_json::to_string(&Decision {
                commitments: vec!["measure freshness".into()],
                ..Decision::default()
            })
            .unwrap(),
            1,
            None,
        );
        let instruction = put(
            &mut memory,
            "alpha",
            RecordKind::UserInstruction,
            "be terse".into(),
            2,
            None,
        );
        let corrected = put(
            &mut memory,
            "alpha",
            RecordKind::Correction,
            "explain trade-offs".into(),
            3,
            Some(instruction.id),
        );
        for i in 0..300 {
            put(
                &mut memory,
                "alpha",
                RecordKind::Finding,
                format!("finding {i}"),
                i + 4,
                None,
            );
        }
        put(
            &mut memory,
            "other",
            RecordKind::UserInstruction,
            "excluded".into(),
            400,
            None,
        );
        assert!(
            !memory
                .recent(&decision.scope, 256)
                .unwrap()
                .iter()
                .any(|r| r.id == decision.id)
        );
        drop(memory);
        let memory = Store::open(path).unwrap();
        let brief = load(&memory, &decision.scope, 0).unwrap();
        assert_eq!(brief.commitments.len(), 1);
        assert_eq!(brief.commitments[0].text, "measure freshness");
        assert_eq!(brief.instructions.len(), 1);
        assert_eq!(brief.instructions[0].id, corrected.id);
        assert_eq!(brief.changes.len(), 48);
        assert!(brief.coverage_details.active_typed_query);
        assert!(
            brief
                .coverage_details
                .limited_kinds
                .contains(&RecordKind::Finding)
        );
        assert!(brief.coverage_details.unresolved_correction_ids.is_empty());
        assert!(
            brief
                .coverage
                .contains("Empty sections do not establish absence")
        );
    }

    #[test]
    fn bounded_active_decision_overflow_is_disclosed() {
        let temp = tempfile::tempdir().unwrap();
        let mut memory = Store::open(temp.path().join("private/memory.sqlite")).unwrap();
        let mut selected = Scope::default();
        for i in 0..65 {
            selected = put(
                &mut memory,
                "alpha",
                RecordKind::Decision,
                format!("decision {i}"),
                i,
                None,
            )
            .scope;
        }
        let brief = load(&memory, &selected, 0).unwrap();
        assert_eq!(brief.decisions.len(), 64);
        assert!(
            brief
                .coverage_details
                .limited_kinds
                .contains(&RecordKind::Decision)
        );
        assert!(brief.coverage.contains("Partial bounded coverage"));
    }

    #[test]
    fn unavailable_legacy_ancestry_is_explicit_not_an_absence_claim() {
        let temp = tempfile::tempdir().unwrap();
        let mut memory = Store::open(temp.path().join("private/memory.sqlite")).unwrap();
        let mut previous = put(
            &mut memory,
            "alpha",
            RecordKind::UserInstruction,
            "instruction".into(),
            0,
            None,
        );
        for i in 1..=129 {
            previous = put(
                &mut memory,
                "alpha",
                RecordKind::Correction,
                format!("revision {i}"),
                i,
                Some(previous.id),
            );
        }
        let brief = load(&memory, &previous.scope, 0).unwrap();
        assert!(brief.instructions.is_empty());
        assert_eq!(
            brief.coverage_details.unresolved_correction_ids,
            vec![previous.id]
        );
        assert!(
            brief
                .coverage
                .contains("Empty sections do not establish absence")
        );
    }
}
