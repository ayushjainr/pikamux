//! Deterministic briefing and decision recall over explicitly supplied evidence.
//! No scanners, providers, authority changes, or cognitive scoring live here.
use crate::assistant_memory::{DecisionState, Record, RecordKind, Scope};
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
        ("Committed", decision.commitments),
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
}
impl From<&Record> for Entry {
    fn from(record: &Record) -> Self {
        Self {
            id: record.id.clone(),
            text: record.body.clone(),
            recorded_at: record.timestamp,
            scope: record.scope.clone(),
            decision_state: record.decision_state,
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
}

/// `allowed` is selected by the host/user, never derived from evidence text.
/// Historical decisions remain visible; recent findings are not called facts.
pub fn build(records: &[Record], allowed: &[Scope], since: i64) -> Brief {
    let mut brief = Brief {
        coverage: "Saved, dated evidence only; not a live verification of project state.".into(),
        ..Brief::default()
    };
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
            RecordKind::UserInstruction | RecordKind::Correction => brief.instructions.push(entry),
            RecordKind::Decision => {
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
                    if record.decision_state == Some(DecisionState::Accepted) {
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

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Recall {
    pub record_id: String,
    pub recorded_at: i64,
    pub original_words: String,
    pub structured: Option<Decision>,
    pub caveat: String,
}
pub fn recall(record: &Record, scope: &Scope) -> Result<Recall, &'static str> {
    if record.kind != RecordKind::Decision || !record.scope.permits(scope) {
        return Err("No decision in this scope");
    }
    Ok(Recall { record_id: record.id.clone(), recorded_at: record.timestamp, original_words: record.body.clone(), structured: serde_json::from_str(&record.body).ok(), caveat: "Historical rationale, not proof that the same choice remains right today. Missing ownership or alternatives are unknown, not reconstructed guesses.".into() })
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
}
