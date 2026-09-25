//! One bounded foreground assistant worker shared by all views.

use crate::assistant_provider::{RpcTransport, TurnResult};
use crate::assistant_runtime::AssistantRuntime;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
    pub partial: String,
    pub result: Option<TurnResult>,
    pub error: Option<String>,
}
impl Default for ServiceSnapshot {
    fn default() -> Self {
        Self {
            state: ServiceState::Starting,
            request_id: None,
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

pub trait LiveTurnRuntime: Send + 'static {
    fn begin_turn(&mut self, request_id: &str, prompt: &str, now: i64) -> Result<(), String>;
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
    Begin { request_id: String, prompt: String },
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
        let _gate = self.gate.lock().expect("assistant command gate");
        let request_id = request_id.into();
        let prompt = prompt.into();
        if request_id.is_empty()
            || request_id.len() > 256
            || prompt.is_empty()
            || prompt.len() > 16 * 1024
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
        if let Err(error) = self.command.try_send(Command::Begin {
            request_id: request_id.clone(),
            prompt,
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
            }
            Ok(())
        }
    }
    pub fn shutdown(&self) -> Result<(), ServiceError> {
        self.stop.store(true, Ordering::Release);
        self.cancel.store(true, Ordering::Release);
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
fn publish(cell: &Arc<RwLock<ServiceSnapshot>>, value: ServiceSnapshot) {
    *cell.write().expect("assistant snapshot lock") = value;
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
    let Command::Begin { request_id, prompt } = command;
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
    match runtime.begin_turn(&request_id, &prompt, now()) {
        Ok(()) => {
            *active = Some(request_id.clone());
            publish(
                controls.snapshot,
                ServiceSnapshot {
                    state: ServiceState::Running,
                    request_id: Some(request_id),
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
    fn begin_turn(&mut self, request_id: &str, prompt: &str, now: i64) -> Result<(), String> {
        self.begin_user_turn(request_id, prompt, &[], now)
            .map(|_| ())
            .map_err(|e| e.to_string())
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
}
