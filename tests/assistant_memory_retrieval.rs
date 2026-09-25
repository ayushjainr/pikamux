//! Offline retrieval experiment: no provider, network, or real transcripts.
use pikamux::assistant_memory::{NewRecord, Origin, RecordKind, Scope, Store};
use std::time::Instant;

fn finding(body: &str, timestamp: i64, project: &str) -> NewRecord {
    NewRecord {
        kind: RecordKind::Finding,
        origin: Origin::Human,
        scope: Scope {
            project: Some(project.into()),
            ..Default::default()
        },
        body: body.into(),
        provenance: "synthetic retrieval fixture".into(),
        timestamp,
        supersedes: None,
        dependencies: vec![],
        decision_state: None,
        protected_policy: false,
    }
}

#[test]
fn old_anchored_memories_are_recalled_without_a_recent_shortlist() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path().join("private/memory.sqlite")).unwrap();
    let scope = Scope {
        project: Some("pika".into()),
        ..Default::default()
    };
    let cases = [
        (
            "Why was heliotrope rejected?",
            "Heliotrope was rejected because corrections were lost.",
        ),
        (
            "What about rs6?",
            "rs6 is the workhorse; validate previews on rs2a first.",
        ),
        (
            "F12 return behavior?",
            "F12 returns to the board without stopping an agent.",
        ),
        (
            "GRASP briefing style?",
            "GRASP: gather, recall, apply, summarize, present.",
        ),
        (
            "What is the consultation target?",
            "Consultation latency target is under thirty seconds.",
        ),
        (
            "Café decision?",
            "Café directory names must retain their Unicode spelling.",
        ),
    ];
    let ids: Vec<_> = cases
        .iter()
        .enumerate()
        .map(|(i, (_, body))| store.append(finding(body, i as i64, "pika")).unwrap().id)
        .collect();
    for index in 0..1000 {
        store
            .append(finding(
                &format!("Inventory item {index} was checked and archived."),
                index + 100,
                "pika",
            ))
            .unwrap();
    }
    // Higher-frequency exact matches in another project must not occupy the
    // top-k slots before scope filtering.
    for index in 0..100 {
        store
            .append(finding(
                "heliotrope rs6 F12 GRASP consultation café",
                index + 2000,
                "other",
            ))
            .unwrap();
    }
    let baseline = store.working_set(&scope, 32).unwrap();
    let baseline_hits = ids
        .iter()
        .filter(|id| baseline.iter().any(|r| &r.id == *id))
        .count();
    let mut retrieved = 0;
    let mut times = Vec::new();
    for _ in 0..10 {
        for ((query, _), expected) in cases.iter().zip(&ids) {
            let start = Instant::now();
            let hits = store.search_bm25(&scope, query, 5).unwrap();
            times.push(start.elapsed().as_micros());
            assert!(hits.iter().all(|r| r.scope.permits(&scope)));
            if hits.iter().any(|r| &r.id == expected) {
                retrieved += 1;
            }
        }
    }
    times.sort_unstable();
    assert_eq!(baseline_hits, 0);
    assert_eq!(retrieved, cases.len() * 10);
    eprintln!(
        "synthetic BM25: records=1106 queries=6 repetitions=10 recency_recall_at_32=0/6 bm25_recall_at_5=6/6 model_calls=0 lookup_us_p50={} lookup_us_p95={}",
        times[times.len() / 2],
        times[times.len() * 95 / 100]
    );
}

#[test]
fn lexical_recall_does_not_claim_to_solve_synonyms() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path().join("private/memory.sqlite")).unwrap();
    store
        .append(finding("Use inexpensive models.", 1, "pika"))
        .unwrap();
    let scope = Scope {
        project: Some("pika".into()),
        ..Default::default()
    };
    assert_eq!(
        store.search_bm25(&scope, "inexpensive", 5).unwrap().len(),
        1
    );
    // No shared tokens: BM25 cannot infer this semantic relationship.
    assert!(
        store
            .search_bm25(&scope, "economical assistants", 5)
            .unwrap()
            .is_empty()
    );
}
