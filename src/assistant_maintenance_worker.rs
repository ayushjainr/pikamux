//! One disposable P05/P07 assignment, using the existing service and dispatch fence.
//! The planner owns root accounting and atomic result validation/commit.

use crate::assistant_memory::{Scope, Store};
use crate::assistant_policy::{AssistantPolicy, ReservationState};
use crate::assistant_provider::{
    MainAssistant, MainProfile, ProviderConfig, RpcTransport, TurnResult,
};
use crate::assistant_runtime::AssistantRuntime;
use crate::assistant_service::{AssistantService, DispatchCancellation, LiveTurnRuntime};
use crate::assistant_transport::{CodexTransport, TransportConfig};
use std::path::PathBuf;

#[derive(Clone)]
pub(crate) struct MaintenanceWorkerConfig {
    pub root: PathBuf,
    pub executable: PathBuf,
    pub scope: Scope,
    /// Already reserved by the planner; this adapter cannot create allowance.
    pub root_id: String,
    pub dependencies: Vec<String>,
    pub deadline_at: i64,
    pub admission: DispatchCancellation,
    pub assignment: crate::assistant_maintenance::Assignment,
}

pub(crate) fn spawn(config: MaintenanceWorkerConfig) -> Result<AssistantService, String> {
    validate_config(&config)?;
    Ok(AssistantService::spawn(move || {
        let memory = Store::open(config.root.join("memory.sqlite")).map_err(display)?;
        let policy = AssistantPolicy::open(config.root.join("policy.sqlite")).map_err(display)?;
        let current = now();
        validate_reservation(&policy, &config, current)?;
        let worker_dir = config
            .root
            .join("maintenance-worker")
            .join(uuid::Uuid::new_v4().to_string());
        let cleanup = WorkerDirectory(worker_dir.clone());
        // Reuse only the assistant destination explicitly provisioned by its
        // human owner. Never discover/copy ambient provider credentials.
        let provider_home = config.root.join("provider-home");
        let scratch = worker_dir.join("scratch");
        crate::assistant_storage::directory(&provider_home).map_err(display)?;
        crate::assistant_storage::directory(&scratch).map_err(display)?;
        let provider = MainAssistant::with_config(
            CodexTransport::spawn(TransportConfig {
                executable: config.executable,
                codex_home: provider_home,
                scratch,
            })
            .map_err(display)?,
            MainProfile {
                profile_id: memory.profile_id().into(),
                thread_id: None,
            },
            ProviderConfig {
                max_prompt_bytes: 32 * 1024,
                max_response_bytes: 8 * 1024,
                ..Default::default()
            },
        )
        .map_err(display)?;
        let mut runtime = AssistantRuntime::open_assignment(
            provider,
            memory,
            policy,
            config.root.join("maintenance-runtime.sqlite"),
            config.scope,
        )
        .map_err(display)?;
        runtime.set_maintenance_assignment(config.assignment);
        runtime.start_fresh_disposable(current).map_err(display)?;
        Ok(MaintenanceRuntime {
            runtime,
            root_id: config.root_id,
            dependencies: config.dependencies,
            deadline_at: config.deadline_at,
            used: false,
            _cleanup: Some(cleanup),
            admission: config.admission,
        })
    }))
}

fn validate_config(config: &MaintenanceWorkerConfig) -> Result<(), String> {
    if !config.root.is_absolute()
        || !config.executable.is_absolute()
        || config.root_id.is_empty()
        || config.root_id.len() > 256
        || config.dependencies.len() > 64
        || config
            .dependencies
            .iter()
            .any(|id| id.is_empty() || id.len() > 256)
    {
        return Err("maintenance requires bounded exact identities and absolute paths".into());
    }
    Ok(())
}
fn validate_reservation(
    policy: &AssistantPolicy,
    config: &MaintenanceWorkerConfig,
    current: i64,
) -> Result<(), String> {
    let reserved = policy
        .reservation(&config.root_id)
        .map_err(display)?
        .ok_or("maintenance root reservation missing")?;
    if reserved.parent_id.is_some()
        || !reserved.background
        || reserved.state != ReservationState::Reserved
        || reserved.calls != 1
        || reserved.deadline_at != config.deadline_at
        || config.deadline_at <= current
        || config.deadline_at > current.saturating_add(120)
    {
        return Err("maintenance requires one current reserved background call".into());
    }
    Ok(())
}

struct MaintenanceRuntime<T: RpcTransport> {
    runtime: AssistantRuntime<T>,
    root_id: String,
    dependencies: Vec<String>,
    deadline_at: i64,
    used: bool,
    // Declared after runtime: kill/reap the provider before removing its files.
    _cleanup: Option<WorkerDirectory>,
    admission: DispatchCancellation,
}

struct WorkerDirectory(PathBuf);
impl Drop for WorkerDirectory {
    fn drop(&mut self) {
        // Only this constructor's unique disposable directory; never shared
        // memory, policy, journal, provider credentials, or installed settings.
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl<T: RpcTransport + Send + 'static> LiveTurnRuntime for MaintenanceRuntime<T> {
    fn set_dispatch_cancellation(&mut self, cancellation: DispatchCancellation) {
        self.runtime.set_cancellation(cancellation);
    }
    fn begin_turn(&mut self, _: &str, _: &str, _: i64) -> Result<(), String> {
        Err("maintenance worker accepts only its reserved background assignment".into())
    }
    fn begin_background_turn(
        &mut self,
        request_id: &str,
        prompt: &str,
        now: i64,
    ) -> Result<(), String> {
        if self.used || now >= self.deadline_at || prompt.len() > 32 * 1024 {
            return Err("maintenance assignment already used, expired or oversized".into());
        }
        self.used = true;
        let _admission = self.admission.enter()?;
        self.runtime
            .begin_child_turn(request_id, prompt, &self.dependencies, &self.root_id, now)
            .map(|_| ())
            .map_err(display)
    }
    fn poll_turn(&mut self, now: i64) -> Result<Option<TurnResult>, String> {
        if now >= self.deadline_at {
            let _ = self.runtime.cancel(now);
            return Err("maintenance deadline expired; delivery remains unresolved".into());
        }
        self.runtime.poll_turn(now).map_err(display)
    }
    fn cancel(&mut self, now: i64) -> Result<(), String> {
        self.runtime.cancel(now).map_err(display)
    }
}

fn display(error: impl std::fmt::Display) -> String {
    error.to_string()
}

#[cfg(test)]
pub(crate) fn fake_service<T: RpcTransport + Send + 'static>(
    config: MaintenanceWorkerConfig,
    transport: T,
) -> Result<AssistantService, String> {
    Ok(AssistantService::spawn(move || {
        let memory = Store::open(config.root.join("memory.sqlite")).map_err(display)?;
        let policy = AssistantPolicy::open(config.root.join("policy.sqlite")).map_err(display)?;
        let provider = MainAssistant::new(
            transport,
            MainProfile {
                profile_id: memory.profile_id().into(),
                thread_id: None,
            },
        );
        let mut runtime = AssistantRuntime::open_assignment(
            provider,
            memory,
            policy,
            config.root.join("maintenance-runtime.sqlite"),
            config.scope,
        )
        .map_err(display)?;
        runtime.set_maintenance_assignment(config.assignment);
        runtime.start_fresh_disposable(now()).map_err(display)?;
        Ok(MaintenanceRuntime {
            runtime,
            root_id: config.root_id,
            dependencies: config.dependencies,
            deadline_at: config.deadline_at,
            used: false,
            _cleanup: None,
            admission: config.admission,
        })
    }))
}
fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .min(i64::MAX as u64) as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assistant_provider::{ProviderError, ServerEvent};
    use serde_json::{Value, json};

    struct Fake;
    impl RpcTransport for Fake {
        fn request(&mut self, method: &str, _: Value) -> Result<Value, ProviderError> {
            Ok(match method {
                "thread/start" => json!({"thread":{"id":"fixture-thread"},
                    "activePermissionProfile":{"id":"pika-assistant","extends":null},
                    "sandbox":{"type":"readOnly","networkAccess":false},
                    "approvalPolicy":"never","model":crate::assistant_provider::DEFAULT_MODEL}),
                "turn/start" => json!({"turn":{"id":"fixture-turn"}}),
                _ => json!({}),
            })
        }
        fn notify(&mut self, _: &str, _: Value) -> Result<(), ProviderError> {
            Ok(())
        }
        fn notifications(&mut self) -> Result<Vec<ServerEvent>, ProviderError> {
            Ok(vec![])
        }
        fn interrupt(&mut self, _: &str, _: &str) -> Result<(), ProviderError> {
            Ok(())
        }
    }

    #[test]
    fn unknown_maintenance_requires_recovery_and_never_replays_old_request() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("private");
        let open = || {
            let memory = Store::open(root.join("memory.sqlite")).unwrap();
            let policy = AssistantPolicy::open(root.join("policy.sqlite")).unwrap();
            let provider = MainAssistant::new(
                Fake,
                MainProfile {
                    profile_id: memory.profile_id().into(),
                    thread_id: None,
                },
            );
            AssistantRuntime::open_assignment(
                provider,
                memory,
                policy,
                root.join("maintenance-runtime.sqlite"),
                Scope::default(),
            )
        };
        let mut runtime = open().unwrap();
        let mut policy = AssistantPolicy::open(root.join("policy.sqlite")).unwrap();
        policy
            .configure(&crate::assistant_policy::PolicyConfig {
                background_calls: 3,
                max_total_calls: 3,
                ..Default::default()
            })
            .unwrap();
        policy
            .reserve_root("old-root", 1, true, 1, Some(100))
            .unwrap();
        runtime.start_fresh_disposable(1).unwrap();
        runtime
            .begin_child_turn("old-request", "old source", &[], "old-root", 2)
            .unwrap();
        assert!(runtime.cancel(3).is_err());
        drop(runtime);
        assert!(open().is_err());
        let memory = Store::open(root.join("memory.sqlite")).unwrap();
        crate::assistant_recovery::recover_after_services_dropped(
            &root,
            &uuid::Uuid::new_v4().to_string(),
            memory.forget_epoch().unwrap(),
        )
        .unwrap();
        let mut runtime = open().unwrap();
        runtime.start_fresh_disposable(4).unwrap();
        policy
            .reserve_root("new-root", 1, true, 4, Some(110))
            .unwrap();
        assert!(
            runtime
                .begin_child_turn("old-request", "old source", &[], "new-root", 5)
                .is_err()
        );
        runtime
            .begin_child_turn("new-request", "new source", &[], "new-root", 5)
            .unwrap();
        assert_eq!(
            policy
                .reservation("assistant:old-request")
                .unwrap()
                .unwrap()
                .state,
            ReservationState::Unknown
        );
        let journal = rusqlite::Connection::open(root.join("maintenance-runtime.sqlite")).unwrap();
        let old: (String, String) = journal
            .query_row(
                "SELECT state,prompt FROM assistant_runtime_turns WHERE request_id='old-request'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(old, ("abandoned".into(), String::new()));
    }

    #[test]
    fn worker_never_reserves_a_new_root_or_retries_failed_assignment() {
        let dir = tempfile::tempdir().unwrap();
        let memory = Store::open(dir.path().join("private/memory.sqlite")).unwrap();
        let policy = AssistantPolicy::open(dir.path().join("private/policy.sqlite")).unwrap();
        let provider = MainAssistant::new(
            Fake,
            MainProfile {
                profile_id: memory.profile_id().into(),
                thread_id: None,
            },
        );
        let mut runtime = AssistantRuntime::open(
            provider,
            memory,
            policy,
            dir.path().join("private/maintenance-runtime.sqlite"),
            Scope::default(),
        )
        .unwrap();
        runtime.set_assignment_mode();
        runtime.start_fresh_disposable(1).unwrap();
        let mut worker = MaintenanceRuntime {
            runtime,
            root_id: "not-reserved".into(),
            dependencies: vec![],
            deadline_at: 100,
            used: false,
            _cleanup: None,
            admission: Default::default(),
        };
        assert!(worker.begin_turn("ordinary", "input", 2).is_err());
        assert!(
            worker
                .begin_background_turn("assignment", "input", 2)
                .is_err()
        );
        assert!(
            worker
                .begin_background_turn("retry", "input", 3)
                .unwrap_err()
                .contains("already used")
        );
        let policy = AssistantPolicy::open(dir.path().join("private/policy.sqlite")).unwrap();
        assert!(policy.reservation("not-reserved").unwrap().is_none());
    }
}
