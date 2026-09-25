use pikamux::assistant_investigation::{
    DisposableWorker, Investigation, InvestigationPlan, InvestigationTask, WorkerFactory,
    WorkerPoll,
};
use pikamux::assistant_memory::{Scope, Store};
use pikamux::assistant_policy::{AssistantPolicy, PolicyConfig};
use pikamux::assistant_provider::{
    MainAssistant, MainProfile, ProviderError, RpcTransport, ServerEvent,
};
use pikamux::assistant_recovery::recover_after_services_dropped;
use pikamux::assistant_runtime::{AssistantRuntime, RuntimeConfig, RuntimeState};
use serde_json::{Value, json};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

struct NeverFactory;
struct NeverWorker;
impl DisposableWorker for NeverWorker {
    fn start(&mut self, _: &str, _: &Scope) -> Result<(), String> {
        Ok(())
    }
    fn poll(&mut self, _: &AtomicBool) -> Result<WorkerPoll, String> {
        Ok(WorkerPoll::Pending)
    }
    fn cancel(&mut self) -> Result<(), String> {
        Ok(())
    }
}
impl WorkerFactory for NeverFactory {
    fn create(&mut self, _: &str) -> Result<Box<dyn DisposableWorker>, String> {
        Ok(Box::new(NeverWorker))
    }
}

#[derive(Default)]
struct RpcFake {
    calls: Vec<String>,
}
impl RpcTransport for RpcFake {
    fn request(&mut self, method: &str, _: Value) -> Result<Value, ProviderError> {
        self.calls.push(method.into());
        Ok(match method {
            "thread/start" | "thread/resume" => json!({
                "thread":{"id":if method == "thread/start" { "fresh-thread" } else { "old-thread" }},
                "activePermissionProfile":{"id":"pika-assistant","extends":null},
                "sandbox":{"type":"readOnly","networkAccess":false},"approvalPolicy":"never","model":"gpt-5.6-luna"
            }),
            "turn/start" => json!({"turn":{"id":"turn-1"}}),
            _ => json!({}),
        })
    }
    fn notify(&mut self, _: &str, _: Value) -> Result<(), ProviderError> {
        Ok(())
    }
    fn notifications(&mut self) -> Result<Vec<ServerEvent>, ProviderError> {
        Ok(Vec::new())
    }
    fn interrupt(&mut self, _: &str, _: &str) -> Result<(), ProviderError> {
        Err(ProviderError::Transport("lost".into()))
    }
}

fn paths() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let private = dir.path().join("private");
    std::fs::create_dir(&private).unwrap();
    #[cfg(unix)]
    std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o700)).unwrap();
    (
        dir,
        private.clone(),
        private.join("memory.sqlite"),
        private.join("policy.sqlite"),
        private.join("investigation.sqlite"),
    )
}

#[test]
fn dispatched_investigation_is_quarantined_on_reopen_without_replay() {
    let (_dir, private, memory_path, policy_path, journal_path) = paths();
    let memory = Store::open(&memory_path).unwrap();
    let mut policy = AssistantPolicy::open(&policy_path).unwrap();
    policy
        .configure(&PolicyConfig {
            max_total_calls: 10,
            max_concurrent: 4,
            ..Default::default()
        })
        .unwrap();
    let plan = InvestigationPlan {
        scope: Scope {
            project: Some("personal".into()),
            ..Default::default()
        },
        tasks: vec![InvestigationTask {
            id: "options".into(),
            assignment: "bounded".into(),
            dependencies: vec![],
        }],
    };
    let mut investigation = Investigation::open(
        memory,
        policy,
        &journal_path,
        plan.clone(),
        "recovery-root-1",
        1,
    )
    .unwrap();
    investigation.poll(&mut NeverFactory, 2).unwrap();
    drop(investigation);
    let policy = AssistantPolicy::open(&policy_path).unwrap();
    let root_before = policy.reservation("recovery-root-1").unwrap().unwrap();
    let child_before = policy
        .reservation("recovery-root-1:options")
        .unwrap()
        .unwrap();
    assert_eq!(
        child_before.state,
        pikamux::assistant_policy::ReservationState::Dispatched
    );
    recover_after_services_dropped(&private, "11111111-1111-4111-8111-111111111111", 0).unwrap();
    let recovered_policy = AssistantPolicy::open(&policy_path).unwrap();
    let root_after = recovered_policy
        .reservation("recovery-root-1")
        .unwrap()
        .unwrap();
    assert_eq!(root_after.calls, root_before.calls);
    assert_eq!(
        recovered_policy
            .reservation("recovery-root-1:options")
            .unwrap()
            .unwrap()
            .calls,
        child_before.calls
    );
    let journal = rusqlite::Connection::open(&journal_path).unwrap();
    assert_eq!(
        journal
            .query_row::<String, _, _>(
                "SELECT state FROM investigation_roots WHERE id='recovery-root-1'",
                [],
                |r| r.get(0)
            )
            .unwrap(),
        "abandoned"
    );
    let memory = Store::open(&memory_path).unwrap();
    let policy = AssistantPolicy::open(&policy_path).unwrap();
    let mut fresh = Investigation::open(
        memory,
        policy,
        &journal_path,
        plan.clone(),
        "recovery-root-2",
        3,
    )
    .unwrap();
    fresh.poll(&mut NeverFactory, 4).unwrap();
    let memory = Store::open(&memory_path).unwrap();
    let policy = AssistantPolicy::open(&policy_path).unwrap();
    assert!(
        Investigation::open(memory, policy, &journal_path, plan, "recovery-root-1", 5).is_err()
    );
}

#[test]
fn runtime_recovery_requires_fresh_thread_and_preserves_old_request_charge() {
    let (_dir, private, memory_path, policy_path, _investigation_path) = paths();
    let journal_path = private.join("runtime.sqlite");
    let memory = Store::open(&memory_path).unwrap();
    let profile = memory.profile_id().to_owned();
    let policy = AssistantPolicy::open(&policy_path).unwrap();
    let mut runtime = AssistantRuntime::open(
        MainAssistant::new(
            RpcFake::default(),
            MainProfile {
                profile_id: profile.clone(),
                thread_id: None,
            },
        ),
        memory,
        policy,
        &journal_path,
        Scope {
            project: Some("personal".into()),
            ..Default::default()
        },
    )
    .unwrap();
    runtime
        .configure_explicit(RuntimeConfig {
            max_calls: 5,
            ..Default::default()
        })
        .unwrap();
    runtime.start_or_resume(1).unwrap();
    runtime
        .begin_user_turn("old-request", "hello", &[], 2)
        .unwrap();
    assert!(runtime.cancel(3).is_err());
    assert!(matches!(
        runtime.state(),
        RuntimeState::UnknownDelivery { .. }
    ));
    drop(runtime);
    recover_after_services_dropped(&private, "33333333-3333-4333-8333-333333333333", 0).unwrap();
    let memory = Store::open(&memory_path).unwrap();
    let policy = AssistantPolicy::open(&policy_path).unwrap();
    let mut fresh = AssistantRuntime::open(
        MainAssistant::new(
            RpcFake::default(),
            MainProfile {
                profile_id: profile,
                thread_id: None,
            },
        ),
        memory,
        policy,
        &journal_path,
        Scope {
            project: Some("personal".into()),
            ..Default::default()
        },
    )
    .unwrap();
    fresh
        .configure_explicit(RuntimeConfig {
            max_calls: 5,
            ..Default::default()
        })
        .unwrap();
    fresh.start_or_resume(4).unwrap();
    assert!(
        fresh
            .begin_user_turn("old-request", "must reject", &[], 5)
            .is_err()
    );
    assert_eq!(
        fresh
            .provider()
            .transport()
            .calls
            .iter()
            .filter(|call| *call == "thread/start")
            .count(),
        1
    );
    assert_eq!(
        fresh
            .provider()
            .transport()
            .calls
            .iter()
            .filter(|call| *call == "thread/resume")
            .count(),
        0
    );
}
