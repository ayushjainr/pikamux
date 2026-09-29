//! Public-API acceptance contracts for the first persistent-assistant slice.
//! All providers are deterministic fakes or pure native AST execution.

use pikamux::assistant_coordinator::{
    AssistantCoordinator, CancellableWorker, ObservationStatus, OperationalObservation,
    WorkerRequest, WorkerResult,
};
use pikamux::assistant_evolution::{
    Expr, Scope as ToolScope, ScopedInput, ToolDefinition, execute, validate_definition,
};
use pikamux::assistant_memory::{NewRecord, Origin, RecordKind, Scope, Store};
use pikamux::assistant_policy::{AssistantPolicy, PolicyConfig};
use serde_json::json;
use std::sync::atomic::AtomicBool;
use tempfile::tempdir;

fn paths() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let d = tempdir().unwrap();
    (
        d,
        std::path::PathBuf::from("private/memory.sqlite"),
        std::path::PathBuf::from("private/policy.sqlite"),
    )
}
fn observation(time: i64, revision: u64, summary: &str) -> OperationalObservation {
    OperationalObservation {
        node_id: "node-a".into(),
        provider: "codex".into(),
        conversation_id: "thread-1".into(),
        project: Some("project-a".into()),
        status: ObservationStatus::Ready,
        summary: summary.into(),
        observed_at: time,
        material_revision: revision,
    }
}
struct FakeWorker {
    calls: usize,
}

#[test]
fn tenfold_timestamp_replay_and_four_consumers_do_not_multiply_work() {
    for events in [100, 1000] {
        let (d, memory_path, policy_path) = paths();
        let memory = Store::open(d.path().join(memory_path)).unwrap();
        let policy = AssistantPolicy::open(d.path().join(policy_path)).unwrap();
        let mut coordinator = AssistantCoordinator::new(memory, policy).unwrap();
        let scope = Scope {
            node: Some("node-a".into()),
            project: Some("project-a".into()),
            provider: Some("codex".into()),
            conversation: Some("thread-1".into()),
        };
        let mut worker = FakeWorker { calls: 0 };
        let mut changes = 0;
        let mut samples = Vec::new();
        for time in 1..=events {
            let start = std::time::Instant::now();
            changes += coordinator
                .observe(vec![observation(time, 1, "same result")])
                .unwrap()
                .len();
            for _ in 0..4 {
                let view = coordinator.snapshot(&scope, 10).unwrap();
                assert_eq!(view.observations.len(), 1);
                assert_eq!(view.memory.len(), 1);
            }
            assert!(coordinator.run_one(&mut worker, time).unwrap().is_none());
            samples.push(start.elapsed().as_micros());
        }
        samples.sort_unstable();
        assert_eq!(changes, 1);
        assert_eq!(worker.calls, 0);
        assert_eq!(coordinator.queued_jobs(), 0);
        eprintln!(
            "replay events={events} consumers=4 material_changes=1 model_calls=0 operation_us_p50={} operation_us_p95={}",
            samples[samples.len() / 2],
            samples[samples.len() * 95 / 100]
        );
    }
}
impl CancellableWorker for FakeWorker {
    fn execute(&mut self, _: &WorkerRequest, _: &AtomicBool) -> WorkerResult {
        self.calls += 1;
        WorkerResult::Completed {
            text: "bounded finding".into(),
        }
    }
}

#[test]
fn metadata_replay_coalesces_timestamps_and_restarts_with_scope() {
    let (d, memory_path, policy_path) = paths();
    let mut policy = AssistantPolicy::open(d.path().join(&policy_path)).unwrap();
    policy
        .configure(&PolicyConfig {
            max_total_calls: 2,
            ..Default::default()
        })
        .unwrap();
    let memory = Store::open(d.path().join(memory_path.clone())).unwrap();
    let mut coordinator = AssistantCoordinator::new(memory, policy).unwrap();
    assert_eq!(
        coordinator
            .observe(vec![observation(1, 1, "done")])
            .unwrap()
            .len(),
        1
    );
    assert!(
        coordinator
            .observe(vec![observation(2, 1, "done")])
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        coordinator
            .observe(vec![observation(3, 2, "changed")])
            .unwrap()
            .len(),
        1
    );
    let snapshot = coordinator
        .snapshot(
            &Scope {
                node: Some("node-a".into()),
                project: Some("project-a".into()),
                provider: Some("codex".into()),
                conversation: Some("thread-1".into()),
            },
            10,
        )
        .unwrap();
    assert_eq!(snapshot.observations.len(), 1);
    assert_eq!(snapshot.memory.len(), 2);
    drop(coordinator);
    let policy = AssistantPolicy::open(d.path().join(policy_path)).unwrap();
    let mut restarted =
        AssistantCoordinator::new(Store::open(d.path().join(memory_path)).unwrap(), policy)
            .unwrap();
    assert!(
        restarted
            .observe(vec![observation(2, 1, "stale")])
            .unwrap()
            .is_empty()
    );
}

#[test]
fn forgetting_scrubs_all_queued_work_and_unknown_is_not_retried() {
    let (d, memory_path, policy_path) = paths();
    let mut memory = Store::open(d.path().join(memory_path)).unwrap();
    let scope = Scope {
        project: Some("project-a".into()),
        ..Default::default()
    };
    let source = memory
        .append(NewRecord {
            kind: RecordKind::Finding,
            origin: Origin::Human,
            scope: scope.clone(),
            body: "rationale".into(),
            provenance: "explicit".into(),
            timestamp: 1,
            supersedes: None,
            dependencies: vec![],
            decision_state: None,
            protected_policy: false,
        })
        .unwrap();
    let mut policy = AssistantPolicy::open(d.path().join(policy_path)).unwrap();
    policy
        .configure(&PolicyConfig {
            max_total_calls: 2,
            ..Default::default()
        })
        .unwrap();
    let mut coordinator = AssistantCoordinator::new(memory, policy).unwrap();
    coordinator.reserve_root_envelope("root", 1, 1).unwrap();
    coordinator
        .enqueue_child(
            "root",
            WorkerRequest {
                job_id: "job".into(),
                root_id: "root".into(),
                scope,
                prompt: "contains source".into(),
                dependency_ids: vec![source.id.clone()],
            },
            1,
        )
        .unwrap();
    assert_eq!(coordinator.queued_jobs(), 1);
    coordinator.forget(&source.id, 2).unwrap();
    assert_eq!(coordinator.queued_jobs(), 0);
    let mut worker = FakeWorker { calls: 0 };
    assert!(coordinator.run_one(&mut worker, 3).unwrap().is_none());
    assert_eq!(worker.calls, 0);
}

#[test]
fn native_ast_execution_is_bounded_and_rejects_bad_candidates() {
    let definition = ToolDefinition {
        name: "project-status".into(),
        version: 1,
        input_scope: ToolScope::new(["project-a"]),
        expression: Expr::Map {
            input: Box::new(Expr::Input),
            expr: Box::new(Expr::CurrentField {
                path: "status".into(),
            }),
        },
    };
    let hash = validate_definition(&definition).unwrap();
    let input = vec![ScopedInput {
        value: json!({"status":"ready","secret":"excluded"}),
        scope: ToolScope::new(["project-a"]),
    }];
    let first = execute(&definition, &input, None).unwrap();
    assert_eq!(first.tool_hash, hash);
    assert_eq!(first.value, json!(["ready"]));
    let bad = ToolDefinition {
        name: "bad".into(),
        version: 1,
        input_scope: ToolScope::default(),
        expression: Expr::Current,
    };
    assert!(execute(&bad, &input, None).is_err());
    assert!(execute(&definition, &[], None).is_ok());
}
