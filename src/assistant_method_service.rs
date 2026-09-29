//! Single-flight native workshop controls. No provider, model, or allowance.
use crate::assistant_memory::{Scope, Store};
use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::{
    Arc, RwLock,
    atomic::{AtomicBool, Ordering},
};
use std::thread::JoinHandle;

pub(crate) struct MethodService {
    root: PathBuf,
    state: Arc<RwLock<State>>,
    cancel: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
    retiring: bool,
}
struct State {
    scope: Option<Scope>,
    epoch: u64,
    value: Value,
}
struct Job {
    scope: Scope,
    operation: String,
    input: String,
    timestamp: i64,
    epoch: u64,
}
type Operation = fn(&mut Store, &Scope, &str, &str, i64, Option<&AtomicBool>) -> Result<Value>;

impl MethodService {
    pub(crate) fn new(root: PathBuf) -> Result<Self> {
        if !root.is_absolute() {
            bail!("Method service requires an absolute authority root");
        }
        Ok(Self {
            root,
            state: Arc::new(RwLock::new(State {
                scope: None,
                epoch: 0,
                value: json!({"state":"idle"}),
            })),
            cancel: Arc::new(AtomicBool::new(false)),
            join: None,
            retiring: false,
        })
    }
    pub(crate) fn busy(&self) -> bool {
        self.join
            .as_ref()
            .is_some_and(|thread| !thread.is_finished())
    }
    pub(crate) fn begin(
        &mut self,
        scope: Scope,
        operation: String,
        input: String,
        timestamp: i64,
        epoch: u64,
    ) -> Result<String> {
        self.start(
            scope,
            operation,
            input,
            timestamp,
            epoch,
            crate::assistant_method_controls::handle_cancellable,
        )
    }
    fn start(
        &mut self,
        scope: Scope,
        operation: String,
        input: String,
        timestamp: i64,
        epoch: u64,
        execute: Operation,
    ) -> Result<String> {
        if self.retiring || self.busy() {
            bail!("A native method job is already running or stopping");
        }
        if input.len() > 32 * 1024
            || !matches!(
                operation.as_str(),
                "pending" | "test" | "approve" | "assess"
            )
        {
            bail!("Invalid bounded native method request");
        }
        if let Some(thread) = self.join.take() {
            let _ = thread.join();
        }
        let id = uuid::Uuid::new_v4().to_string();
        self.cancel = Arc::new(AtomicBool::new(false));
        let cancel = self.cancel.clone();
        let state = self.state.clone();
        let root = self.root.clone();
        *state.write().expect("method state") = State {
            scope: Some(scope.clone()),
            epoch,
            value: json!({"state":"running","job":id,"notice":"Native evaluation/control only; no model call."}),
        };
        let job_id = id.clone();
        let job = Job {
            scope,
            operation,
            input,
            timestamp,
            epoch,
        };
        let spawned = std::thread::Builder::new().name("pika-method".into()).spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run(root, &job, &cancel, execute)
            })).unwrap_or_else(|_| Err(anyhow::anyhow!("Native method worker failed")));
            let mut state = state.write().expect("method state");
            if cancel.load(Ordering::Acquire) {
                state.value = json!({"state":"cancelled","job":job_id,"notice":"Stopped; a previously committed native control is not rolled back."});
            } else {
                state.value = match result {
                    Ok(output) => json!({"state":"completed","job":job_id,"result":output}),
                    Err(error) => json!({"state":"failed","job":job_id,"error":error.to_string()}),
                };
            }
        });
        match spawned {
            Ok(thread) => self.join = Some(thread),
            Err(error) => {
                self.state.write().expect("method state").value =
                    json!({"state":"failed","job":id,"error":"Native worker could not start"});
                return Err(error.into());
            }
        }
        Ok(id)
    }
    /// The host supplies its current epoch; cached content never survives forget.
    pub(crate) fn snapshot(&self, scope: &Scope, epoch: u64) -> Value {
        let mut state = self.state.write().expect("method state");
        if state.epoch != epoch {
            self.cancel.store(true, Ordering::Release);
            state.value = json!({"state":"invalidated","notice":"Saved context changed; native result is hidden."});
        }
        if state
            .scope
            .as_ref()
            .is_some_and(|selected| selected != scope)
        {
            return json!({"state":"different_scope"});
        }
        state.value.clone()
    }
    pub(crate) fn cancel(&self) {
        self.cancel.store(true, Ordering::Release);
    }
    /// Nonblocking recovery fence; recreate the service after recovery completes.
    pub(crate) fn quiesce(&mut self) -> bool {
        self.retiring = true;
        self.cancel();
        self.is_quiescent()
    }
    pub(crate) fn is_quiescent(&mut self) -> bool {
        if self.busy() {
            return false;
        }
        if let Some(thread) = self.join.take() {
            let _ = thread.join();
        }
        true
    }
}
impl Drop for MethodService {
    fn drop(&mut self) {
        self.cancel();
        if let Some(thread) = self.join.take() {
            let _ = thread.join();
        }
    }
}
fn run(root: PathBuf, job: &Job, cancel: &AtomicBool, execute: Operation) -> Result<Value> {
    let mut memory = Store::open(root.join("memory.sqlite"))?;
    memory.set_busy_timeout(25)?;
    if cancel.load(Ordering::Acquire) || memory.forget_epoch()? != job.epoch {
        bail!("Native method request invalidated before execution");
    }
    let output = execute(
        &mut memory,
        &job.scope,
        &job.operation,
        &job.input,
        job.timestamp,
        Some(cancel),
    )?;
    if cancel.load(Ordering::Acquire) || memory.forget_epoch()? != job.epoch {
        bail!("Native method result invalidated; no result published");
    }
    if serde_json::to_vec(&output)?.len() > 256 * 1024 {
        bail!("Native method result exceeds display bound");
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};
    fn held(
        _: &mut Store,
        _: &Scope,
        _: &str,
        _: &str,
        _: i64,
        cancel: Option<&AtomicBool>,
    ) -> Result<Value> {
        while !cancel.unwrap().load(Ordering::Acquire) {
            std::thread::sleep(Duration::from_millis(1));
        }
        Ok(json!({"private":"must not publish"}))
    }
    #[test]
    fn single_flight_cached_status_cancel_and_recovery_are_nonblocking() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("private");
        Store::open(root.join("memory.sqlite")).unwrap();
        let mut service = MethodService::new(root).unwrap();
        let scope = Scope::default();
        service
            .start(scope.clone(), "test".into(), "{}".into(), 1, 0, held)
            .unwrap();
        assert!(
            service
                .start(scope.clone(), "test".into(), "{}".into(), 1, 0, held)
                .is_err()
        );
        let started = Instant::now();
        assert_eq!(service.snapshot(&scope, 0)["state"], "running");
        service.quiesce();
        assert!(started.elapsed() < Duration::from_millis(100));
        while !service.is_quiescent() {
            assert!(started.elapsed() < Duration::from_secs(3));
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(service.snapshot(&scope, 0)["state"], "cancelled");
        assert!(!service.snapshot(&scope, 0).to_string().contains("private"));
    }
    #[test]
    fn cached_results_are_scope_and_epoch_fenced() {
        let temp = tempfile::tempdir().unwrap();
        let service = MethodService::new(temp.path().join("private")).unwrap();
        let scope = Scope {
            project: Some("one".into()),
            ..Default::default()
        };
        *service.state.write().unwrap() = State {
            scope: Some(scope.clone()),
            epoch: 0,
            value: json!({"state":"completed","result":"private"}),
        };
        assert_eq!(
            service.snapshot(&Scope::default(), 0)["state"],
            "different_scope"
        );
        assert_eq!(service.snapshot(&scope, 1)["state"], "invalidated");
        assert!(!service.snapshot(&scope, 1).to_string().contains("private"));
    }
    #[test]
    fn obsolete_queued_epoch_never_enters_native_operation() {
        fn forbidden(
            _: &mut Store,
            _: &Scope,
            _: &str,
            _: &str,
            _: i64,
            _: Option<&AtomicBool>,
        ) -> Result<Value> {
            panic!("obsolete request reached native operation")
        }
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("private");
        Store::open(root.join("memory.sqlite")).unwrap();
        let job = Job {
            scope: Scope::default(),
            operation: "test".into(),
            input: "{}".into(),
            timestamp: 1,
            epoch: 1,
        };
        assert!(
            run(root, &job, &AtomicBool::new(false), forbidden)
                .unwrap_err()
                .to_string()
                .contains("invalidated before execution")
        );
    }
}
