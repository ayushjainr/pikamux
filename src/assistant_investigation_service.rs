//! Off-UI bounded investigation runtime for the persistent assistant.
//!
//! The service owns no provider process until `begin_turn`.  Each request is
//! converted into two disposable child assignments (options and challenge),
//! with only a small, explicitly scoped memory dependency set.

use crate::assistant_investigation::{
    Investigation, InvestigationPlan, InvestigationState, InvestigationTask, WorkerFactory,
};
use crate::assistant_investigation_provider::CodexWorkerFactory;
use crate::assistant_memory::{Scope, Store};
use crate::assistant_policy::AssistantPolicy;
use crate::assistant_provider::{TurnResult, Usage};
use crate::assistant_service::LiveTurnRuntime;
use sha2::{Digest, Sha256};
use std::path::PathBuf;

const MAX_DEPENDENCIES: usize = 8;
const MAX_DEPENDENCY_BYTES: usize = 8 * 1024;
const MAX_QUESTION_BYTES: usize = 4 * 1024;

#[derive(Clone, Debug)]
pub struct InvestigationServiceConfig {
    pub root: PathBuf,
    pub executable: PathBuf,
    pub scope: Scope,
}

pub struct InvestigationService<F = CodexWorkerFactory> {
    config: InvestigationServiceConfig,
    engine: Option<Investigation>,
    factory: F,
    root_id: Option<String>,
    partial: String,
}

impl InvestigationService<CodexWorkerFactory> {
    pub fn new(config: InvestigationServiceConfig) -> Result<Self, String> {
        if !config.root.is_absolute() || !config.executable.is_absolute() {
            return Err("investigation root and provider executable must be absolute".into());
        }
        if config.scope.project.as_deref() != Some("personal")
            || config.scope.provider.is_some()
            || config.scope.conversation.is_some()
            || config.scope.node.is_some()
        {
            return Err("investigation scope must be canonical personal scope".into());
        }
        let factory = CodexWorkerFactory::new(&config.executable, &config.root)?;
        Ok(Self {
            config,
            engine: None,
            factory,
            root_id: None,
            partial: String::new(),
        })
    }
}

impl<F: WorkerFactory> InvestigationService<F> {
    pub fn with_factory(config: InvestigationServiceConfig, factory: F) -> Result<Self, String> {
        if !config.root.is_absolute() || !config.executable.is_absolute() {
            return Err("investigation root and provider executable must be absolute".into());
        }
        if config.scope.project.as_deref() != Some("personal")
            || config.scope.provider.is_some()
            || config.scope.conversation.is_some()
            || config.scope.node.is_some()
        {
            return Err("investigation scope must be canonical personal scope".into());
        }
        Ok(Self {
            config,
            engine: None,
            factory,
            root_id: None,
            partial: String::new(),
        })
    }

    fn root_id(request_id: &str) -> String {
        let mut hash = Sha256::new();
        hash.update(request_id.as_bytes());
        format!("investigation-{:x}", hash.finalize())
    }

    fn open_engine(&mut self, request_id: &str, prompt: &str, now: i64) -> Result<(), String> {
        let memory_path = self.config.root.join("memory.sqlite");
        let policy_path = self.config.root.join("policy.sqlite");
        let journal_path = self.config.root.join("investigation.sqlite");
        let memory = Store::open(memory_path).map_err(|e| e.to_string())?;
        let policy = AssistantPolicy::open(policy_path).map_err(|e| e.to_string())?;
        let mut dependencies = Vec::new();
        let mut dependency_bytes = 0usize;
        for record in memory
            .working_set(&self.config.scope, MAX_DEPENDENCIES)
            .map_err(|e| e.to_string())?
        {
            let cost = serde_json::to_vec(&record)
                .map_err(|e| e.to_string())?
                .len();
            if dependency_bytes.saturating_add(cost) > MAX_DEPENDENCY_BYTES {
                break;
            }
            dependency_bytes += cost;
            dependencies.push(record.id);
        }
        let root_id = Self::root_id(request_id);
        let plan = InvestigationPlan {
            scope: self.config.scope.clone(),
            tasks: vec![
                InvestigationTask {
                    id: "options".into(),
                    assignment: format!(
                        "Explore bounded options for this user question:\n{prompt}"
                    ),
                    dependencies: dependencies.clone(),
                },
                InvestigationTask {
                    id: "challenge".into(),
                    assignment: format!(
                        "Challenge assumptions and risks for this user question:\n{prompt}"
                    ),
                    dependencies,
                },
            ],
        };
        let engine = Investigation::open(memory, policy, journal_path, plan, &root_id, now)
            .map_err(|e| e.to_string())?;
        self.engine = Some(engine);
        self.root_id = Some(root_id);
        self.partial = "investigation started".into();
        Ok(())
    }
}

impl<F: WorkerFactory + Send + 'static> LiveTurnRuntime for InvestigationService<F> {
    fn begin_turn(&mut self, request_id: &str, prompt: &str, now: i64) -> Result<(), String> {
        if request_id.is_empty() || prompt.is_empty() || prompt.len() > MAX_QUESTION_BYTES {
            return Err("investigation request is busy or invalid".into());
        }
        if let Some(engine) = &self.engine {
            if !matches!(
                engine.state(),
                InvestigationState::Complete | InvestigationState::Cancelled
            ) {
                return Err("investigation is already active".into());
            }
            self.engine = None;
            self.root_id = None;
            self.partial.clear();
        }
        self.open_engine(request_id, prompt, now)
    }

    fn poll_turn(&mut self, now: i64) -> Result<Option<TurnResult>, String> {
        let engine = self
            .engine
            .as_mut()
            .ok_or_else(|| "investigation has not begun".to_string())?;
        match engine
            .poll(&mut self.factory, now)
            .map_err(|e| e.to_string())?
        {
            Some(text) => Ok(Some(TurnResult::Complete {
                turn_id: self.root_id.clone().unwrap_or_default(),
                text,
                usage: None::<Usage>,
            })),
            None => {
                self.partial = format!("investigation {:?}", engine.snapshot());
                Ok(None)
            }
        }
    }

    fn cancel(&mut self, now: i64) -> Result<(), String> {
        self.engine
            .as_mut()
            .ok_or_else(|| "investigation has not begun".to_string())?
            .cancel(now)
            .map_err(|e| e.to_string())
    }

    fn partial_output(&self) -> String {
        self.partial.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assistant_investigation::{DisposableWorker, WorkerPoll};
    use crate::assistant_policy::PolicyConfig;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use tempfile::tempdir;

    struct Worker {
        result: Option<String>,
        starts: Arc<AtomicUsize>,
    }
    impl DisposableWorker for Worker {
        fn start(&mut self, _: &str, _: &Scope) -> Result<(), String> {
            self.starts.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn poll(&mut self, _: &std::sync::atomic::AtomicBool) -> Result<WorkerPoll, String> {
            Ok(WorkerPoll::Complete(self.result.take().unwrap_or_default()))
        }
        fn cancel(&mut self) -> Result<(), String> {
            Ok(())
        }
    }
    struct Factory {
        starts: Arc<AtomicUsize>,
    }
    impl WorkerFactory for Factory {
        fn create(&mut self, task_id: &str) -> Result<Box<dyn DisposableWorker>, String> {
            Ok(Box::new(Worker {
                result: Some(format!("result-{task_id}")),
                starts: self.starts.clone(),
            }))
        }
    }

    fn service(dir: &std::path::Path, starts: Arc<AtomicUsize>) -> InvestigationService<Factory> {
        let mut policy = AssistantPolicy::open(dir.join("policy.sqlite")).unwrap();
        policy
            .configure(&PolicyConfig {
                max_total_calls: 10,
                max_concurrent: 2,
                ..Default::default()
            })
            .unwrap();
        InvestigationService::with_factory(
            InvestigationServiceConfig {
                root: dir.to_path_buf(),
                executable: dir.join("fake-provider"),
                scope: Scope {
                    project: Some("personal".into()),
                    ..Default::default()
                },
            },
            Factory { starts },
        )
        .unwrap()
    }

    #[test]
    fn bounded_two_workers_synthesis_and_reuse_without_provider() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("private");
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        let starts = Arc::new(AtomicUsize::new(0));
        let mut service = service(&root, starts.clone());
        service
            .begin_turn("question-one", "What should we choose?", 1)
            .unwrap();
        let mut output = None;
        for tick in 2..20 {
            output = service.poll_turn(tick).unwrap();
            if output.is_some() {
                break;
            }
        }
        assert!(matches!(output, Some(TurnResult::Complete { .. })));
        assert_eq!(starts.load(Ordering::SeqCst), 3);
        let before = starts.load(Ordering::SeqCst);
        assert!(service.poll_turn(21).is_err());
        assert_eq!(starts.load(Ordering::SeqCst), before);
        service
            .begin_turn("question-two", "Challenge it", 22)
            .unwrap();
        for tick in 23..40 {
            if service.poll_turn(tick).unwrap().is_some() {
                break;
            }
        }
        assert_eq!(starts.load(Ordering::SeqCst), 6);
    }

    #[test]
    fn rejects_nonpersonal_scope_and_oversized_question() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("private");
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        let starts = Arc::new(AtomicUsize::new(0));
        let bad = InvestigationService::with_factory(
            InvestigationServiceConfig {
                root: root.clone(),
                executable: root.join("fake"),
                scope: Scope {
                    project: Some("project-x".into()),
                    ..Default::default()
                },
            },
            Factory {
                starts: starts.clone(),
            },
        );
        assert!(bad.is_err());
        let mut service = service(&root, starts);
        assert!(
            service
                .begin_turn("too-big", &"x".repeat(MAX_QUESTION_BYTES + 1), 1)
                .is_err()
        );
    }
}
