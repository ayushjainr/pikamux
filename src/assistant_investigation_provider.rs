//! Production disposable Codex worker adapter. Construction is explicit and
//! isolated; no credentials or parent conversation state are discovered.

use crate::assistant_investigation::{DisposableWorker, WorkerFactory, WorkerPoll};
use crate::assistant_memory::Scope;
use crate::assistant_provider::{MainAssistant, MainProfile, ProviderError, TurnResult};
use crate::assistant_transport::{CodexTransport, TransportConfig};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

pub struct CodexWorkerFactory {
    executable: PathBuf,
    root: PathBuf,
    provider_home: PathBuf,
    next: u64,
}
impl CodexWorkerFactory {
    pub fn new(executable: impl Into<PathBuf>, root: impl Into<PathBuf>) -> Result<Self, String> {
        let executable = executable.into();
        let root = root.into();
        if !executable.is_absolute() || !root.is_absolute() {
            return Err("provider executable and worker root must be absolute".into());
        }
        crate::assistant_storage::directory(&root).map_err(|e| e.to_string())?;
        let provider_home = root.join("provider-home");
        crate::assistant_storage::directory(&provider_home).map_err(|e| e.to_string())?;
        Ok(Self {
            executable,
            root,
            provider_home,
            next: 0,
        })
    }
}
impl WorkerFactory for CodexWorkerFactory {
    fn create(&mut self, task_id: &str) -> Result<Box<dyn DisposableWorker>, String> {
        self.next = self.next.saturating_add(1);
        let dir = self
            .root
            .join(format!("worker-{}-{}", self.next, sanitize(task_id)));
        let scratch = dir.join("scratch");
        crate::assistant_storage::directory(&scratch).map_err(|e| e.to_string())?;
        let config = TransportConfig {
            executable: self.executable.clone(),
            codex_home: self.provider_home.clone(),
            scratch,
        };
        config.validate().map_err(|e| e.to_string())?;
        let transport = CodexTransport::spawn(config).map_err(|e| e.to_string())?;
        let provider = MainAssistant::new(
            transport,
            MainProfile {
                profile_id: format!("worker-{}", self.next),
                thread_id: None,
            },
        );
        Ok(Box::new(CodexWorker { provider }))
    }
}
fn sanitize(value: &str) -> String {
    let value = value
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(64)
        .collect::<String>();
    if value.is_empty() {
        "task".into()
    } else {
        value
    }
}

pub struct CodexWorker {
    provider: MainAssistant<CodexTransport>,
}
impl DisposableWorker for CodexWorker {
    fn start(&mut self, assignment: &str, scope: &Scope) -> Result<(), String> {
        if scope
            .project
            .as_deref()
            .is_some_and(|project| project != "personal")
            || scope.provider.is_some()
            || scope.conversation.is_some()
            || scope.node.is_some()
        {
            return Err("worker scope is not personal".into());
        }
        self.provider.start_or_resume().map_err(display)?;
        self.provider
            .begin_turn(assignment)
            .map(|_| ())
            .map_err(display)
    }
    fn poll(&mut self, cancel: &AtomicBool) -> Result<WorkerPoll, String> {
        if cancel.load(std::sync::atomic::Ordering::Acquire) {
            self.cancel()?;
            return Err("worker cancellation requested; delivery is unknown".into());
        }
        match self.provider.poll_turn().map_err(display)? {
            None => Ok(WorkerPoll::Pending),
            Some(TurnResult::Complete { text, .. }) => Ok(WorkerPoll::Complete(text)),
            Some(TurnResult::Failed { text, .. }) => Ok(WorkerPoll::Failed(text)),
        }
    }
    fn cancel(&mut self) -> Result<(), String> {
        self.provider.cancel().map_err(display)
    }
}
fn display(error: ProviderError) -> String {
    error.to_string()
}
