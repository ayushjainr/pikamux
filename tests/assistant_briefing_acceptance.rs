use pikamux::assistant_briefing::{Decision, ExplainBack, build, explanation_gap, recall};
use pikamux::assistant_memory::{
    DecisionState, NewRecord, Origin, Record, RecordKind, Scope, Store,
};

fn put(store: &mut Store, project: &str, kind: RecordKind, body: String, timestamp: i64) -> Record {
    store
        .append(NewRecord {
            kind,
            origin: Origin::Human,
            scope: Scope {
                project: Some(project.into()),
                ..Scope::default()
            },
            body,
            provenance: "acceptance fixture".into(),
            timestamp,
            supersedes: None,
            dependencies: Vec::new(),
            decision_state: (kind == RecordKind::Decision).then_some(DecisionState::Accepted),
            protected_policy: false,
        })
        .unwrap()
}

#[test]
fn replayed_multi_project_records_have_explicit_briefing_taxonomy() {
    let temp = tempfile::tempdir().unwrap();
    let mut store = Store::open(temp.path().join("private/memory.sqlite")).unwrap();
    put(
        &mut store,
        "alpha",
        RecordKind::Finding,
        "build completed; not live-verified".into(),
        10,
    );
    put(
        &mut store,
        "alpha",
        RecordKind::Decision,
        serde_json::to_string(&Decision {
            chosen: "cache snapshots".into(),
            rationale: "avoid repeated polling".into(),
            rejected: vec!["poll every view".into()],
            owner: "user".into(),
            open_questions: vec!["freshness during outage".into()],
            commitments: vec!["measure stale reads".into()],
        })
        .unwrap(),
        11,
    );
    put(
        &mut store,
        "alpha",
        RecordKind::Draft,
        "unsubmitted draft".into(),
        12,
    );
    put(
        &mut store,
        "beta",
        RecordKind::Finding,
        "unrelated beta change".into(),
        10,
    );
    let alpha = Scope {
        project: Some("alpha".into()),
        ..Scope::default()
    };
    let mut replay = store.recent(&alpha, 256).unwrap();
    let beta = Scope {
        project: Some("beta".into()),
        ..Scope::default()
    };
    replay.extend(store.recent(&beta, 256).unwrap());
    let brief = build(&replay, &[alpha.clone(), beta.clone()], 0);
    assert_eq!(brief.changes.len(), 2);
    assert_eq!(brief.decisions.len(), 1);
    assert_eq!(brief.commitments.len(), 1);
    assert_eq!(brief.uncertainty.len(), 1);
    assert_eq!(brief.ignored_drafts, 1);
    assert!(brief.coverage.contains("dated evidence"));
    let alpha_only = build(&replay, std::slice::from_ref(&alpha), 0);
    assert_eq!(alpha_only.changes.len(), 1);
}

#[test]
fn simulated_months_later_decision_recall_preserves_human_ownership() {
    let temp = tempfile::tempdir().unwrap();
    let mut store = Store::open(temp.path().join("private/memory.sqlite")).unwrap();
    let decision = Decision {
        chosen: "bounded workers".into(),
        rationale: "limit fanout".into(),
        rejected: vec!["unbounded spawn".into()],
        owner: "Ayush".into(),
        open_questions: vec!["provider outage".into()],
        commitments: vec!["measure cleanup".into()],
    };
    let record = put(
        &mut store,
        "alpha",
        RecordKind::Decision,
        serde_json::to_string(&decision).unwrap(),
        1,
    );
    drop(store);
    let reopened = Store::open(temp.path().join("private/memory.sqlite")).unwrap();
    let recalled = recall(
        &reopened.get(&record.id).unwrap().unwrap(),
        &Scope {
            project: Some("alpha".into()),
            ..Scope::default()
        },
    )
    .unwrap();
    assert_eq!(recalled.structured, Some(decision));
    assert!(recalled.caveat.contains("Historical rationale"));
    assert!(
        recall(
            &reopened.get(&record.id).unwrap().unwrap(),
            &Scope {
                project: Some("beta".into()),
                ..Scope::default()
            }
        )
        .is_err()
    );
}

#[test]
fn correction_changes_next_scoped_brief_without_cross_project_leak() {
    let temp = tempfile::tempdir().unwrap();
    let mut store = Store::open(temp.path().join("private/memory.sqlite")).unwrap();
    let prior = put(
        &mut store,
        "alpha",
        RecordKind::UserInstruction,
        "be terse".into(),
        1,
    );
    store
        .append(NewRecord {
            kind: RecordKind::Correction,
            origin: Origin::Human,
            scope: Scope {
                project: Some("alpha".into()),
                ..Scope::default()
            },
            body: "explain trade-offs".into(),
            provenance: "acceptance fixture".into(),
            timestamp: 2,
            supersedes: Some(prior.id.clone()),
            dependencies: vec![prior.id],
            decision_state: None,
            protected_policy: false,
        })
        .unwrap();
    put(
        &mut store,
        "beta",
        RecordKind::UserInstruction,
        "keep beta terse".into(),
        2,
    );
    let alpha = Scope {
        project: Some("alpha".into()),
        ..Scope::default()
    };
    let beta = Scope {
        project: Some("beta".into()),
        ..Scope::default()
    };
    let alpha_brief = build(
        &store.recent(&alpha, 256).unwrap(),
        std::slice::from_ref(&alpha),
        0,
    );
    let beta_brief = build(
        &store.recent(&beta, 256).unwrap(),
        std::slice::from_ref(&beta),
        0,
    );
    assert_eq!(alpha_brief.instructions[0].text, "explain trade-offs");
    assert_eq!(beta_brief.instructions[0].text, "keep beta terse");
    assert!(
        !beta_brief
            .instructions
            .iter()
            .any(|entry| entry.text == "explain trade-offs")
    );
}

#[test]
fn grasp_gap_and_skip_are_transparent_without_a_score() {
    let decision = Decision {
        chosen: "cache".into(),
        rationale: "less polling".into(),
        rejected: vec!["polling".into()],
        owner: "user".into(),
        ..Decision::default()
    };
    let explanation = ExplainBack {
        choice: "cache".into(),
        why: "less polling".into(),
        ..ExplainBack::default()
    };
    assert_eq!(
        explanation_gap(&decision, &explanation),
        Some("Which alternative did you reject, and what trade-off ruled it out?")
    );
    let complete = ExplainBack {
        alternative: "polling costs more".into(),
        ..explanation
    };
    assert_eq!(explanation_gap(&decision, &complete), None);
    let temp = tempfile::tempdir().unwrap();
    let mut store = Store::open(temp.path().join("private/memory.sqlite")).unwrap();
    put(
        &mut store,
        "alpha",
        RecordKind::GraspInteraction,
        "{\"action\":\"skip\",\"reason\":\"not useful now\"}".into(),
        3,
    );
    let scope = Scope {
        project: Some("alpha".into()),
        ..Scope::default()
    };
    let brief = build(
        &store.recent(&scope, 256).unwrap(),
        std::slice::from_ref(&scope),
        0,
    );
    assert!(brief.decisions.is_empty());
    assert!(brief.changes.is_empty());
}
