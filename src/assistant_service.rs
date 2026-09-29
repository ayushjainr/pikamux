//! One bounded foreground assistant worker shared by all views.

use crate::assistant_provider::{RpcTransport, TurnResult};
use crate::assistant_runtime::AssistantRuntime;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Trusted human-input adapter payload. Raw words are durable conversation;
/// host-added evidence/investigation instructions belong only in `prompt`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserTurnInput {
    pub raw_body: String,
    pub prompt: String,
    pub scope: crate::assistant_memory::Scope,
    /// Serialized caller timestamp, never recomputed by queue/retry dispatch.
    pub timestamp: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceState {
    Starting,
    Idle,
    Running,
    Cancelling,
    Completed,
    Failed,
    Stopped,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceSnapshot {
    pub state: ServiceState,
    pub request_id: Option<String>,
    pub user_record_id: Option<String>,
    pub partial: String,
    pub result: Option<TurnResult>,
    pub error: Option<String>,
}
impl Default for ServiceSnapshot {
    fn default() -> Self {
        Self {
            state: ServiceState::Starting,
            request_id: None,
            user_record_id: None,
            partial: String::new(),
            result: None,
            error: None,
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceError {
    Busy,
    Closed,
    InvalidRequest(String),
}

/// Linearizable, nonblocking send admission for one request. Cancellation
/// winning before admission prevents dispatch; admission winning first is an
/// already-in-flight call, never permission for a later call or retry.
#[derive(Clone, Default)]
pub struct DispatchCancellation(Arc<AtomicU64>);
pub struct DispatchAdmission(DispatchCancellation);
impl DispatchCancellation {
    pub fn cancel(&self) {
        // Low bit is sticky cancellation; upper bits count admitted sends.
        self.0.fetch_or(1, Ordering::AcqRel);
    }
    pub fn enter(&self) -> Result<DispatchAdmission, String> {
        self.0
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |state| {
                if state & 1 == 0 {
                    state.checked_add(2)
                } else {
                    None
                }
            })
            .map(|_| DispatchAdmission(self.clone()))
            .map_err(|_| "Request cancelled; no new dispatch admitted".into())
    }
}
impl Drop for DispatchAdmission {
    fn drop(&mut self) {
        self.0.0.fetch_sub(2, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod dispatch_gate_tests {
    use super::*;
    #[test]
    fn cancelling_an_admitted_send_never_reopens_gate_or_affects_a_new_request() {
        let old = DispatchCancellation::default();
        let admission = old.enter().unwrap();
        let other_admission = old.enter().unwrap();
        old.cancel();
        drop(admission);
        drop(other_admission);
        assert!(old.enter().is_err());
        let new = DispatchCancellation::default();
        assert!(new.enter().is_ok());
        assert!(old.enter().is_err());
    }
}

pub trait LiveTurnRuntime: Send + 'static {
    fn set_dispatch_cancellation(&mut self, _cancellation: DispatchCancellation) {}
    fn begin_turn(&mut self, request_id: &str, prompt: &str, now: i64) -> Result<(), String>;
    fn begin_conversation_turn(
        &mut self,
        _request_id: &str,
        _input: &UserTurnInput,
        _now: i64,
    ) -> Result<(), String> {
        Err("Runtime does not support durable human conversation input".into())
    }
    fn user_input_record_id(&self, _request_id: &str) -> Option<String> {
        None
    }
    fn begin_background_turn(
        &mut self,
        _request_id: &str,
        _prompt: &str,
        _now: i64,
    ) -> Result<(), String> {
        Err("Background execution has no approved runtime allowance".into())
    }
    fn poll_turn(&mut self, now: i64) -> Result<Option<TurnResult>, String>;
    fn cancel(&mut self, now: i64) -> Result<(), String>;
    fn partial_output(&self) -> String {
        String::new()
    }
}
pub trait RuntimeFactory: Send + 'static {
    type Runtime: LiveTurnRuntime;
    fn create(self) -> Result<Self::Runtime, String>;
}
impl<F, R> RuntimeFactory for F
where
    F: FnOnce() -> Result<R, String> + Send + 'static,
    R: LiveTurnRuntime,
{
    type Runtime = R;
    fn create(self) -> Result<R, String> {
        self()
    }
}
enum Command {
    Begin {
        request_id: String,
        prompt: String,
        background: bool,
        user_input: Option<UserTurnInput>,
        dispatch: DispatchCancellation,
    },
}

pub struct AssistantService {
    command: SyncSender<Command>,
    snapshot: Arc<RwLock<ServiceSnapshot>>,
    join: Arc<Mutex<Option<JoinHandle<()>>>>,
    stop: Arc<AtomicBool>,
    cancel: Arc<AtomicBool>,
    busy: Arc<AtomicBool>,
    requests: Arc<Mutex<std::collections::BTreeSet<String>>>,
    gate: Arc<Mutex<()>>,
    dispatch: Arc<Mutex<DispatchCancellation>>,
}
impl Clone for AssistantService {
    fn clone(&self) -> Self {
        Self {
            command: self.command.clone(),
            snapshot: self.snapshot.clone(),
            join: self.join.clone(),
            stop: self.stop.clone(),
            cancel: self.cancel.clone(),
            busy: self.busy.clone(),
            requests: self.requests.clone(),
            gate: self.gate.clone(),
            dispatch: self.dispatch.clone(),
        }
    }
}
impl AssistantService {
    pub fn spawn<F: RuntimeFactory>(factory: F) -> Self {
        let (command, commands) = mpsc::sync_channel(1);
        let snapshot = Arc::new(RwLock::new(ServiceSnapshot::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let cancel = Arc::new(AtomicBool::new(false));
        let busy = Arc::new(AtomicBool::new(false));
        let gate = Arc::new(Mutex::new(()));
        let worker_gate = gate.clone();
        let published = snapshot.clone();
        let worker_stop = stop.clone();
        let worker_cancel = cancel.clone();
        let worker_busy = busy.clone();
        let join = thread::Builder::new()
            .name("pika-assistant".into())
            .spawn(move || {
                worker(
                    factory,
                    commands,
                    published,
                    worker_stop,
                    worker_cancel,
                    worker_busy,
                    worker_gate,
                )
            })
            .expect("assistant worker thread");
        Self {
            command,
            snapshot,
            join: Arc::new(Mutex::new(Some(join))),
            stop,
            cancel,
            busy,
            requests: Arc::new(Mutex::new(std::collections::BTreeSet::new())),
            gate,
            dispatch: Arc::new(Mutex::new(DispatchCancellation::default())),
        }
    }
    pub fn snapshot(&self) -> ServiceSnapshot {
        self.snapshot
            .read()
            .expect("assistant snapshot lock")
            .clone()
    }
    pub fn busy(&self) -> bool {
        self.busy.load(Ordering::Acquire)
    }
    pub fn begin(
        &self,
        request_id: impl Into<String>,
        prompt: impl Into<String>,
    ) -> Result<(), ServiceError> {
        self.begin_kind(request_id.into(), prompt.into(), false, None)
    }
    pub fn begin_user(
        &self,
        request_id: impl Into<String>,
        input: UserTurnInput,
    ) -> Result<(), ServiceError> {
        if input.raw_body.trim().is_empty()
            || input.raw_body.len() > 16 * 1024
            || input.timestamp < 0
        {
            return Err(ServiceError::InvalidRequest(
                "bounded raw user message and nonnegative timestamp required".into(),
            ));
        }
        self.begin_kind(request_id.into(), input.prompt.clone(), false, Some(input))
    }
    /// Native lifecycle dispatch only. Ordinary runtimes deny it by default;
    /// the coordinator must independently enforce the approved background cap.
    pub fn begin_background(
        &self,
        request_id: impl Into<String>,
        prompt: impl Into<String>,
    ) -> Result<(), ServiceError> {
        self.begin_kind(request_id.into(), prompt.into(), true, None)
    }
    fn begin_kind(
        &self,
        request_id: String,
        prompt: String,
        background: bool,
        user_input: Option<UserTurnInput>,
    ) -> Result<(), ServiceError> {
        let _gate = self.gate.lock().expect("assistant command gate");
        if request_id.is_empty()
            || request_id.len() > 256
            || prompt.is_empty()
            || prompt.len() > if background { 32 * 1024 } else { 16 * 1024 }
        {
            return Err(ServiceError::InvalidRequest(
                "bounded request id and prompt are required".into(),
            ));
        }
        if self.stop.load(Ordering::Acquire) {
            return Err(ServiceError::Closed);
        }
        {
            let mut requests = self.requests.lock().expect("assistant request lock");
            if requests.contains(&request_id) {
                return Err(ServiceError::Busy);
            }
            // A bounded convenience cache, not the durable replay authority.
            // The runtime journal rejects old IDs even after eviction.
            if requests.len() >= 128 {
                requests.pop_first();
            }
            requests.insert(request_id.clone());
        }
        if self
            .busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            self.requests
                .lock()
                .expect("assistant request lock")
                .remove(&request_id);
            return Err(ServiceError::Busy);
        }
        let dispatch = DispatchCancellation::default();
        *self.dispatch.lock().expect("assistant dispatch gate") = dispatch.clone();
        if let Err(error) = self.command.try_send(Command::Begin {
            request_id: request_id.clone(),
            prompt,
            background,
            user_input,
            dispatch,
        }) {
            self.requests
                .lock()
                .expect("assistant request lock")
                .remove(&request_id);
            self.busy.store(false, Ordering::Release);
            return Err(match error {
                TrySendError::Full(_) => ServiceError::Busy,
                TrySendError::Disconnected(_) => ServiceError::Closed,
            });
        }
        Ok(())
    }
    pub fn cancel(&self) -> Result<(), ServiceError> {
        let _gate = self.gate.lock().expect("assistant command gate");
        if self.stop.load(Ordering::Acquire) {
            Err(ServiceError::Closed)
        } else {
            if self.busy.load(Ordering::Acquire) {
                self.cancel.store(true, Ordering::Release);
                self.dispatch
                    .lock()
                    .expect("assistant dispatch gate")
                    .cancel();
            }
            Ok(())
        }
    }
    pub fn shutdown(&self) -> Result<(), ServiceError> {
        self.stop.store(true, Ordering::Release);
        self.cancel.store(true, Ordering::Release);
        self.dispatch
            .lock()
            .expect("assistant dispatch gate")
            .cancel();
        Ok(())
    }
    pub fn join(&self) {
        if let Some(join) = self.join.lock().expect("assistant join lock").take() {
            let _ = join.join();
        }
    }
}
impl Drop for AssistantService {
    fn drop(&mut self) {
        if Arc::strong_count(&self.join) == 1 {
            let _ = self.shutdown();
            self.join();
        }
    }
}
fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .min(i64::MAX as u64) as i64
}
fn publish(cell: &Arc<RwLock<ServiceSnapshot>>, mut value: ServiceSnapshot) {
    let mut previous = cell.write().expect("assistant snapshot lock");
    if value.user_record_id.is_none()
        && value.request_id.is_some()
        && value.request_id == previous.request_id
    {
        value.user_record_id = previous.user_record_id.clone();
    }
    *previous = value;
}

fn release_busy(busy: &AtomicBool, cancel: &AtomicBool, gate: &Mutex<()>) {
    let _guard = gate.lock().expect("assistant command gate");
    cancel.store(false, Ordering::Release);
    busy.store(false, Ordering::Release);
}

struct WorkerControls<'a> {
    snapshot: &'a Arc<RwLock<ServiceSnapshot>>,
    stop: &'a AtomicBool,
    cancel: &'a AtomicBool,
    busy: &'a AtomicBool,
    gate: &'a Mutex<()>,
}

// True means dispatch was cancelled and this worker iteration is finished.
fn start_command<R: LiveTurnRuntime>(
    runtime: &mut R,
    command: Command,
    active: &mut Option<String>,
    controls: &WorkerControls<'_>,
) -> bool {
    let Command::Begin {
        request_id,
        prompt,
        background,
        user_input,
        dispatch,
    } = command;
    runtime.set_dispatch_cancellation(dispatch);
    if controls.cancel.swap(false, Ordering::AcqRel) || controls.stop.load(Ordering::Acquire) {
        release_busy(controls.busy, controls.cancel, controls.gate);
        publish(
            controls.snapshot,
            ServiceSnapshot {
                state: ServiceState::Failed,
                request_id: Some(request_id),
                error: Some("cancelled before dispatch".into()),
                ..Default::default()
            },
        );
        return true;
    }
    let result = if let Some(input) = user_input {
        runtime.begin_conversation_turn(&request_id, &input, now())
    } else if background {
        runtime.begin_background_turn(&request_id, &prompt, now())
    } else {
        runtime.begin_turn(&request_id, &prompt, now())
    };
    let user_record_id = runtime.user_input_record_id(&request_id);
    match result {
        Ok(()) => {
            *active = Some(request_id.clone());
            publish(
                controls.snapshot,
                ServiceSnapshot {
                    state: ServiceState::Running,
                    request_id: Some(request_id),
                    user_record_id,
                    ..Default::default()
                },
            );
        }
        Err(error) => {
            release_busy(controls.busy, controls.cancel, controls.gate);
            publish(
                controls.snapshot,
                ServiceSnapshot {
                    state: ServiceState::Failed,
                    request_id: Some(request_id),
                    user_record_id,
                    error: Some(error),
                    ..Default::default()
                },
            );
        }
    }
    false
}

fn publish_polled_result<R: LiveTurnRuntime>(
    runtime: &mut R,
    request_id: String,
    active: &mut Option<String>,
    controls: &WorkerControls<'_>,
) {
    match runtime.poll_turn(now()) {
        Ok(Some(result)) => {
            publish(
                controls.snapshot,
                ServiceSnapshot {
                    state: ServiceState::Completed,
                    request_id: Some(request_id),
                    partial: runtime.partial_output(),
                    result: Some(result),
                    ..Default::default()
                },
            );
            *active = None;
            release_busy(controls.busy, controls.cancel, controls.gate);
        }
        Ok(None) => publish(
            controls.snapshot,
            ServiceSnapshot {
                state: ServiceState::Running,
                request_id: Some(request_id),
                partial: runtime.partial_output(),
                ..Default::default()
            },
        ),
        Err(error) => {
            publish(
                controls.snapshot,
                ServiceSnapshot {
                    state: ServiceState::Failed,
                    request_id: Some(request_id),
                    partial: runtime.partial_output(),
                    error: Some(error),
                    ..Default::default()
                },
            );
            *active = None;
            release_busy(controls.busy, controls.cancel, controls.gate);
        }
    }
}

fn cancel_active<R: LiveTurnRuntime>(
    runtime: &mut R,
    request_id: String,
    active: &mut Option<String>,
    controls: &WorkerControls<'_>,
) {
    publish(
        controls.snapshot,
        ServiceSnapshot {
            state: ServiceState::Cancelling,
            request_id: Some(request_id.clone()),
            ..Default::default()
        },
    );
    let error = runtime.cancel(now()).err();
    publish(
        controls.snapshot,
        ServiceSnapshot {
            state: if error.is_some() {
                ServiceState::Failed
            } else {
                ServiceState::Completed
            },
            request_id: Some(request_id),
            error: error.or_else(|| Some("cancelled".into())),
            ..Default::default()
        },
    );
    *active = None;
    release_busy(controls.busy, controls.cancel, controls.gate);
}

fn worker<F: RuntimeFactory>(
    factory: F,
    commands: mpsc::Receiver<Command>,
    snapshot: Arc<RwLock<ServiceSnapshot>>,
    stop: Arc<AtomicBool>,
    cancel: Arc<AtomicBool>,
    busy: Arc<AtomicBool>,
    gate: Arc<Mutex<()>>,
) {
    let mut runtime = match factory.create() {
        Ok(runtime) => runtime,
        Err(error) => {
            publish(
                &snapshot,
                ServiceSnapshot {
                    state: ServiceState::Failed,
                    error: Some(error),
                    ..Default::default()
                },
            );
            release_busy(&busy, &cancel, &gate);
            return;
        }
    };
    publish(
        &snapshot,
        ServiceSnapshot {
            state: ServiceState::Idle,
            ..Default::default()
        },
    );
    let mut active: Option<String> = None;
    let controls = WorkerControls {
        snapshot: &snapshot,
        stop: &stop,
        cancel: &cancel,
        busy: &busy,
        gate: &gate,
    };
    loop {
        if active.is_none() && stop.load(Ordering::Acquire) {
            break;
        }
        if let Ok(command) = commands.try_recv() {
            if active.is_none() && start_command(&mut runtime, command, &mut active, &controls) {
                continue;
            }
        }
        if let Some(request_id) = active.clone() {
            if cancel.swap(false, Ordering::AcqRel) || stop.load(Ordering::Acquire) {
                cancel_active(&mut runtime, request_id, &mut active, &controls);
                if stop.load(Ordering::Acquire) {
                    break;
                }
                continue;
            }
            publish_polled_result(&mut runtime, request_id, &mut active, &controls);
            if active.is_some() {
                thread::sleep(Duration::from_millis(1));
            }
        } else if stop.load(Ordering::Acquire) {
            break;
        } else if let Ok(command) = commands.recv_timeout(Duration::from_millis(10)) {
            if start_command(&mut runtime, command, &mut active, &controls) {
                continue;
            }
        }
    }
    publish(
        &snapshot,
        ServiceSnapshot {
            state: ServiceState::Stopped,
            ..Default::default()
        },
    );
}

impl<T: RpcTransport + Send + 'static> LiveTurnRuntime for AssistantRuntime<T> {
    fn set_dispatch_cancellation(&mut self, cancellation: DispatchCancellation) {
        self.set_cancellation(cancellation);
    }
    fn begin_turn(&mut self, request_id: &str, prompt: &str, now: i64) -> Result<(), String> {
        self.begin_user_turn(request_id, prompt, &[], now)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
    fn begin_conversation_turn(
        &mut self,
        request_id: &str,
        input: &UserTurnInput,
        now: i64,
    ) -> Result<(), String> {
        let record = self
            .record_user_input(request_id, input)
            .map_err(|e| e.to_string())?;
        self.begin_user_turn(request_id, &input.prompt, &[record.id], now)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
    fn user_input_record_id(&self, request_id: &str) -> Option<String> {
        AssistantRuntime::user_input_record_id(self, request_id)
    }
    fn poll_turn(&mut self, now: i64) -> Result<Option<TurnResult>, String> {
        AssistantRuntime::poll_turn(self, now).map_err(|e| e.to_string())
    }
    fn cancel(&mut self, now: i64) -> Result<(), String> {
        AssistantRuntime::cancel(self, now).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Fake {
        polls: usize,
        calls: Arc<AtomicUsize>,
    }
    impl LiveTurnRuntime for Fake {
        fn begin_turn(&mut self, _: &str, _: &str, _: i64) -> Result<(), String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn poll_turn(&mut self, _: i64) -> Result<Option<TurnResult>, String> {
            self.polls += 1;
            if self.polls < 3 {
                Ok(None)
            } else {
                Ok(Some(TurnResult::Complete {
                    turn_id: "t".into(),
                    text: "done".into(),
                    usage: None,
                }))
            }
        }
        fn cancel(&mut self, _: i64) -> Result<(), String> {
            Ok(())
        }
        fn partial_output(&self) -> String {
            format!("polls={}", self.polls)
        }
    }
    #[test]
    fn duplicate_request_is_rejected_without_second_runtime_call() {
        let calls = Arc::new(AtomicUsize::new(0));
        let service = AssistantService::spawn({
            let calls = calls.clone();
            move || Ok(Fake { polls: 0, calls })
        });
        while matches!(service.snapshot().state, ServiceState::Starting) {
            thread::yield_now();
        }
        service.begin("r", "hello").unwrap();
        assert_eq!(service.begin("r", "hello"), Err(ServiceError::Busy));
        assert_eq!(service.begin("other", "hello"), Err(ServiceError::Busy));
        for _ in 0..100 {
            if matches!(
                service.snapshot().state,
                ServiceState::Completed | ServiceState::Failed
            ) {
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        service.begin("other", "hello").unwrap();
        for _ in 0..100 {
            if calls.load(Ordering::SeqCst) == 2 {
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        service.shutdown().unwrap();
        service.join();
    }
    #[test]
    fn cancellation_flag_is_processed_even_when_queue_is_busy() {
        let service = AssistantService::spawn(|| {
            Ok(Fake {
                polls: 0,
                calls: Arc::new(AtomicUsize::new(0)),
            })
        });
        while matches!(service.snapshot().state, ServiceState::Starting) {
            thread::yield_now();
        }
        service.begin("r", "hello").unwrap();
        service.cancel().unwrap();
        for _ in 0..100 {
            if matches!(service.snapshot().state, ServiceState::Completed) {
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        assert!(
            service
                .snapshot()
                .error
                .as_deref()
                .is_some_and(|text| text.starts_with("cancelled"))
        );
        service.shutdown().unwrap();
        service.join();
    }

    #[test]
    fn raw_user_input_is_typed_and_its_record_receipt_survives_polling() {
        struct UserAware {
            observed: Arc<Mutex<Option<UserTurnInput>>>,
            request: Option<String>,
        }
        impl LiveTurnRuntime for UserAware {
            fn begin_turn(&mut self, _: &str, _: &str, _: i64) -> Result<(), String> {
                Err("untyped dispatch forbidden".into())
            }
            fn begin_conversation_turn(
                &mut self,
                id: &str,
                input: &UserTurnInput,
                _: i64,
            ) -> Result<(), String> {
                self.request = Some(id.into());
                *self.observed.lock().unwrap() = Some(input.clone());
                Ok(())
            }
            fn user_input_record_id(&self, id: &str) -> Option<String> {
                (self.request.as_deref() == Some(id)).then(|| "exact-memory-id".into())
            }
            fn poll_turn(&mut self, _: i64) -> Result<Option<TurnResult>, String> {
                Ok(Some(TurnResult::Complete {
                    turn_id: "fake".into(),
                    text: "answer".into(),
                    usage: None,
                }))
            }
            fn cancel(&mut self, _: i64) -> Result<(), String> {
                Ok(())
            }
        }
        let observed = Arc::new(Mutex::new(None));
        let captured = observed.clone();
        let service = AssistantService::spawn(move || {
            Ok(UserAware {
                observed: captured,
                request: None,
            })
        });
        let input = UserTurnInput {
            raw_body: "raw question".into(),
            prompt: "decorated request".into(),
            scope: crate::assistant_memory::Scope {
                project: Some("alpha".into()),
                ..Default::default()
            },
            timestamp: 0,
        };
        service.begin_user("human", input.clone()).unwrap();
        for _ in 0..100 {
            if service.snapshot().state == ServiceState::Completed {
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(*observed.lock().unwrap(), Some(input));
        let receipt = service.snapshot();
        assert_eq!(receipt.state, ServiceState::Completed);
        assert_eq!(receipt.user_record_id.as_deref(), Some("exact-memory-id"));
        service.shutdown().unwrap();
        service.join();
    }
}
