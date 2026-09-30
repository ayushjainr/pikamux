//! Production disposable Codex worker adapter. Construction is explicit and
//! isolated; no credentials or parent conversation state are discovered.

use crate::assistant_investigation::{
    DisposableWorker, WorkerFactory, WorkerPoll, WorkerReceipt, validate_project_scope,
};
use crate::assistant_memory::Scope;
use crate::assistant_provider::{
    MainAssistant, MainProfile, ProviderError, RpcTransport, TurnResult,
};
use crate::assistant_transport::{CodexTransport, TransportConfig};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

#[path = "assistant_investigation_consultation.rs"]
mod consultation;
pub use consultation::{ConsultationAuthorization, ScopedWorkerFactory};

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
        Ok(Box::new(CodexWorker {
            provider,
            receipt: WorkerReceipt::default(),
            native_parent: None,
        }))
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

pub struct CodexWorker<T: RpcTransport = CodexTransport> {
    provider: MainAssistant<T>,
    receipt: WorkerReceipt,
    native_parent: Option<(PathBuf, String, String)>,
}
impl<T: RpcTransport + Send> DisposableWorker for CodexWorker<T> {
    fn bind_native_parent(
        &mut self,
        runtime: &std::path::Path,
        turn: &str,
        session: &str,
    ) -> Result<(), String> {
        self.native_parent = Some((runtime.to_owned(), turn.into(), session.into()));
        Ok(())
    }
    fn start(&mut self, assignment: &str, scope: &Scope) -> Result<(), String> {
        validate_project_scope(scope).map_err(|e| e.to_string())?;
        self.provider.start_or_resume().map_err(display)?;
        self.receipt.provider = Some("codex".into());
        self.receipt.thread_id = self.provider.profile().thread_id.clone();
        self.receipt.delivery = Some("unknown".into());
        self.receipt.cleanup = Some("provider_retention_unknown".into());
        let _parent = self
            .native_parent
            .as_ref()
            .map(|(path, turn, session)| {
                crate::assistant_native_helpers::parent_lease(path, turn, session)
            })
            .transpose()
            .map_err(|e| e.to_string())?;
        let turn_id = self.provider.begin_turn(assignment).map_err(display)?;
        self.receipt.turn_id = Some(turn_id);
        Ok(())
    }
    fn start_with_cancellation(
        &mut self,
        assignment: &str,
        scope: &Scope,
        cancellation: &crate::assistant_service::DispatchCancellation,
    ) -> Result<(), String> {
        validate_project_scope(scope).map_err(|e| e.to_string())?;
        self.provider.start_or_resume().map_err(display)?;
        self.receipt.provider = Some("codex".into());
        self.receipt.thread_id = self.provider.profile().thread_id.clone();
        self.receipt.delivery = Some("unknown".into());
        self.receipt.cleanup = Some("provider_retention_unknown".into());
        let _admission = cancellation.enter()?;
        let _parent = self
            .native_parent
            .as_ref()
            .map(|(path, turn, session)| {
                crate::assistant_native_helpers::parent_lease(path, turn, session)
            })
            .transpose()
            .map_err(|e| e.to_string())?;
        self.receipt.turn_id = Some(self.provider.begin_turn(assignment).map_err(display)?);
        Ok(())
    }
    fn poll(&mut self, cancel: &AtomicBool) -> Result<WorkerPoll, String> {
        if cancel.load(std::sync::atomic::Ordering::Acquire) {
            self.cancel()?;
            return Err("worker cancellation requested; delivery is unknown".into());
        }
        match self.provider.poll_turn().map_err(display)? {
            None => Ok(WorkerPoll::Pending),
            Some(TurnResult::Complete { text, usage, .. }) => {
                self.receipt.delivery = Some("completed".into());
                self.receipt.input_tokens = usage.as_ref().map(|u| u.input_tokens);
                self.receipt.output_tokens = usage.as_ref().map(|u| u.output_tokens);
                Ok(WorkerPoll::Complete(text))
            }
            Some(TurnResult::Failed { text, .. }) => Ok(WorkerPoll::Failed(text)),
        }
    }
    fn cancel(&mut self) -> Result<(), String> {
        self.provider.cancel().map_err(display)
    }
    fn receipt(&self) -> WorkerReceipt {
        self.receipt.clone()
    }
}
fn display(error: ProviderError) -> String {
    error.to_string()
}

#[cfg(test)]
mod cancellation_tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    };
    use std::time::Duration;
    struct SlowInitialization {
        entered: mpsc::SyncSender<()>,
        proceed: mpsc::Receiver<()>,
        sends: Arc<AtomicUsize>,
    }
    impl RpcTransport for SlowInitialization {
        fn request(
            &mut self,
            method: &str,
            _: serde_json::Value,
        ) -> Result<serde_json::Value, ProviderError> {
            match method {
                "initialize" => {
                    self.entered.send(()).unwrap();
                    self.proceed.recv_timeout(Duration::from_secs(2)).unwrap();
                    Ok(serde_json::json!({}))
                }
                "thread/start" => Ok(
                    serde_json::json!({"thread":{"id":"fake-worker"},"activePermissionProfile":{"id":"pika-assistant","extends":null},"sandbox":{"type":"readOnly","networkAccess":false},"approvalPolicy":"never","model":crate::assistant_provider::DEFAULT_MODEL}),
                ),
                "turn/start" => {
                    self.sends.fetch_add(1, Ordering::SeqCst);
                    Ok(serde_json::json!({"turn":{"id":"should-not-send"}}))
                }
                _ => Ok(serde_json::json!({})),
            }
        }
        fn notify(&mut self, _: &str, _: serde_json::Value) -> Result<(), ProviderError> {
            Ok(())
        }
        fn notifications(
            &mut self,
        ) -> Result<Vec<crate::assistant_provider::ServerEvent>, ProviderError> {
            Ok(vec![])
        }
        fn interrupt(&mut self, _: &str, _: &str) -> Result<(), ProviderError> {
            Ok(())
        }
    }
    #[test]
    fn cancel_during_native_worker_initialization_prevents_turn_start() {
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (proceed_tx, proceed_rx) = mpsc::sync_channel(1);
        let sends = Arc::new(AtomicUsize::new(0));
        let provider = MainAssistant::new(
            SlowInitialization {
                entered: entered_tx,
                proceed: proceed_rx,
                sends: sends.clone(),
            },
            MainProfile {
                profile_id: "fixture".into(),
                thread_id: None,
            },
        );
        let mut worker = CodexWorker {
            provider,
            receipt: Default::default(),
            native_parent: None,
        };
        let cancellation = crate::assistant_service::DispatchCancellation::default();
        let worker_gate = cancellation.clone();
        let join = std::thread::spawn(move || {
            worker.start_with_cancellation(
                "bounded",
                &Scope {
                    project: Some("fixture".into()),
                    ..Default::default()
                },
                &worker_gate,
            )
        });
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        cancellation.cancel();
        proceed_tx.send(()).unwrap();
        assert!(join.join().unwrap().unwrap_err().contains("cancelled"));
        assert_eq!(sends.load(Ordering::SeqCst), 0);
    }
}
