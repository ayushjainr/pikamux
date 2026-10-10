//! Host-owned, bounded maintenance consumer. UI calls touch only atomics/cache;
//! the sole worker thread owns database preparation, promotion and bookkeeping.
use crate::assistant_control::Controller;
use crate::assistant_lifecycle::Outcome;
use crate::assistant_maintenance::{self as domain, Assignment};
use crate::assistant_maintenance_worker::{self, MaintenanceWorkerConfig};
use crate::assistant_memory::Store;
use crate::assistant_provider::TurnResult;
use crate::assistant_service::{AssistantService, DispatchCancellation, ServiceState};
use anyhow::{Context, Result};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock, mpsc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub(crate) struct Driver {
    wake: mpsc::SyncSender<()>,
    shared: Arc<Shared>,
    join: Mutex<Option<std::thread::JoinHandle<()>>>,
}
struct Shared {
    stop: AtomicBool,
    foreground: AtomicBool,
    pending: AtomicBool,
    admission: Mutex<DispatchCancellation>,
    status: RwLock<(Option<String>, Value)>,
}
struct Active {
    assignment: Assignment,
    root_id: String,
    service: AssistantService,
    started: Instant,
    deadline: i64,
    admission: DispatchCancellation,
}
impl Drop for Active {
    fn drop(&mut self) {
        let _ = self.service.shutdown();
        self.service.join();
    }
}

impl Driver {
    pub(crate) fn new(root: PathBuf) -> Result<Self> {
        Self::with_factory(root, assistant_maintenance_worker::spawn)
    }
    fn with_factory(root: PathBuf, factory: WorkerFactory) -> Result<Self> {
        anyhow::ensure!(
            root.is_absolute(),
            "Maintenance authority root must be absolute"
        );
        let (wake, receive) = mpsc::sync_channel(1);
        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            foreground: AtomicBool::new(true),
            pending: AtomicBool::new(false),
            admission: Mutex::new(Default::default()),
            status: RwLock::new((None, json!({"state":"starting"}))),
        });
        let worker = shared.clone();
        let join = std::thread::Builder::new()
            .name("pika-maintenance".into())
            .spawn(move || {
                while !worker.stop.load(Ordering::Acquire) {
                    match run(&root,&worker,&receive,factory) {
                        Ok(())=>break,
                        Err(error) if database_busy(&error)=> {
                            publish(&worker,None,json!({"state":"deferred","notice":"Local state is busy; no call was authorized by this error."}));
                            let _=receive.recv_timeout(Duration::from_millis(100));
                        }
                        Err(error)=> {
                            publish(&worker,None,json!({"state":"unavailable","error":error.to_string()}));
                            break;
                        }
                    }
                }
            })?;
        Ok(Self {
            wake,
            shared,
            join: Mutex::new(Some(join)),
        })
    }

    /// Call before handling a new foreground request, then clear after enqueue.
    /// Cancellation is sticky for the current maintenance admission generation.
    pub(crate) fn foreground_pending(&self, pending: bool) {
        self.shared.pending.store(pending, Ordering::Release);
        if pending {
            self.shared
                .admission
                .lock()
                .expect("maintenance admission")
                .cancel();
        }
        let _ = self.wake.try_send(());
    }
    pub(crate) fn tick(&self, foreground_busy: bool) {
        self.shared
            .foreground
            .store(foreground_busy, Ordering::Release);
        if foreground_busy {
            self.shared
                .admission
                .lock()
                .expect("maintenance admission")
                .cancel();
        }
        let _ = self.wake.try_send(());
    }
    pub(crate) fn status(&self, name: &str) -> Value {
        let cache = self.shared.status.read().expect("maintenance status");
        if cache.0.as_deref().is_some_and(|scope| scope != name) {
            json!({"state":"different_scope"})
        } else {
            cache.1.clone()
        }
    }
    /// Begin shutdown without waiting on the UI. Recovery may touch journals
    /// only after this returns true; construct a new driver after recovery.
    pub(crate) fn quiesce(&self) -> bool {
        self.shared.stop.store(true, Ordering::Release);
        self.shared
            .admission
            .lock()
            .expect("maintenance admission")
            .cancel();
        let _ = self.wake.try_send(());
        self.is_quiescent()
    }
    pub(crate) fn is_quiescent(&self) -> bool {
        self.join
            .lock()
            .expect("maintenance join")
            .as_ref()
            .is_none_or(|thread| thread.is_finished())
    }
}
impl Drop for Driver {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        self.shared
            .admission
            .lock()
            .expect("maintenance admission")
            .cancel();
        let _ = self.wake.try_send(());
        // Host teardown retains the authority lock until this join completes.
        // Navigation only detaches views and does not drop the host driver.
        if let Some(thread) = self.join.get_mut().expect("maintenance join").take() {
            let _ = thread.join();
        }
    }
}

fn busy(shared: &Shared) -> bool {
    shared.foreground.load(Ordering::Acquire) || shared.pending.load(Ordering::Acquire)
}
fn publish(shared: &Shared, scope: Option<String>, status: Value) {
    *shared.status.write().expect("maintenance status") = (scope, status);
}
type WorkerFactory = fn(MaintenanceWorkerConfig) -> std::result::Result<AssistantService, String>;

fn run(
    root: &std::path::Path,
    shared: &Shared,
    receive: &mpsc::Receiver<()>,
    factory: WorkerFactory,
) -> Result<()> {
    let mut control = Controller::attach(root)?;
    let mut memory = Store::open(root.join("memory.sqlite"))?;
    memory.connection.busy_timeout(Duration::from_millis(25))?;
    domain::initialize(&memory)?;
    memory.connection.execute_batch("CREATE TABLE IF NOT EXISTS maintenance_driver_roots(job TEXT PRIMARY KEY,root_id TEXT NOT NULL,settled INTEGER NOT NULL DEFAULT 0)")?;
    recover(root, &mut memory, &mut control)?;
    let mut active: Option<Active> = None;
    let mut last_prepare: Option<Instant> = None;
    while !shared.stop.load(Ordering::Acquire) {
        let _ = receive.recv_timeout(Duration::from_millis(100));
        if let Err(error) = step(
            root,
            shared,
            &mut memory,
            &mut control,
            &mut active,
            &mut last_prepare,
            factory,
        ) {
            if database_busy(&error) {
                publish(
                    shared,
                    None,
                    json!({"state":"deferred","notice":"Local state is busy; no blind paid retry."}),
                );
            } else {
                return Err(error);
            }
        }
    }
    if let Some(job) = active {
        let result = job.service.snapshot().result;
        let _ = job.service.cancel();
        let _ = job.service.shutdown();
        job.service.join();
        finish(
            &mut memory,
            &mut control,
            &job,
            result.or(native_result(root, &job.root_id)?),
            now(),
        )?;
    }
    Ok(())
}

fn step(
    root: &std::path::Path,
    shared: &Shared,
    memory: &mut Store,
    control: &mut Controller,
    active: &mut Option<Active>,
    last_prepare: &mut Option<Instant>,
    factory: WorkerFactory,
) -> Result<()> {
    let timestamp = now();
    if active.is_some() {
        return poll_active(
            root,
            shared,
            memory,
            control,
            active,
            last_prepare,
            timestamp,
        );
    }
    if busy(shared) || last_prepare.is_some_and(|t| t.elapsed() < Duration::from_secs(5)) {
        return Ok(());
    }
    *last_prepare = Some(Instant::now());
    recover(root, memory, control)?;
    let Some(permission) = control.maintenance_permission(timestamp, false)? else {
        publish(
            shared,
            None,
            json!({"state":"not_admitted","notice":"Maintenance is disabled, paused, expired or out of allowance."}),
        );
        return Ok(());
    };
    domain::due(memory, timestamp)?;
    domain::reconcile_outbox(memory)?;
    let Some(assignment) = domain::prepare(memory, &permission.scope, timestamp)? else {
        let mut status = domain::status(memory, &permission.scope)?;
        status["state"] = json!("idle");
        publish(shared, permission.scope.project.clone(), status);
        return Ok(());
    };
    let Some((root_id, deadline, admission)) =
        admit_assignment(shared, memory, control, &assignment, &permission)?
    else {
        return Ok(());
    };
    let config = MaintenanceWorkerConfig {
        root: root.to_owned(),
        executable: permission.executable,
        scope: assignment.scope.clone(),
        root_id: root_id.clone(),
        dependencies: assignment.sources.iter().map(|s| s.id.clone()).collect(),
        deadline_at: deadline,
        admission: admission.clone(),
        assignment: assignment.clone(),
    };
    start_worker(shared, memory, control, active, assignment, config, factory)
}

fn admit_assignment(
    shared: &Shared,
    memory: &mut Store,
    control: &mut Controller,
    assignment: &Assignment,
    permission: &crate::assistant_lifecycle::BackgroundConfig,
) -> Result<Option<(String, i64, DispatchCancellation)>> {
    let admission = {
        let mut slot = shared.admission.lock().expect("maintenance admission");
        if busy(shared) {
            return Ok(None);
        }
        *slot = DispatchCancellation::default();
        slot.clone()
    };
    let Ok(_priority) = admission.enter() else {
        return Ok(None);
    };
    if busy(shared) {
        return Ok(None);
    }
    let root_id = format!("maintenance-root-{}", uuid::Uuid::new_v4());
    // Record the deterministic bookkeeping link before reserving; a crash or
    // SQLite busy between stores can then release a proven-unsent admission.
    memory.connection.execute("INSERT INTO maintenance_driver_roots(job,root_id) VALUES(?,?) ON CONFLICT(job) DO UPDATE SET root_id=excluded.root_id,settled=0",params![assignment.id,root_id])?;
    let deadline = match control.reserve_maintenance(&root_id, permission, now()) {
        Ok(deadline) => deadline,
        Err(error) => {
            memory.connection.execute(
                "UPDATE maintenance_driver_roots SET settled=1 WHERE job=?",
                [&assignment.id],
            )?;
            publish(
                shared,
                permission.scope.project.clone(),
                json!({"state":"deferred","error":error.to_string()}),
            );
            return Ok(None);
        }
    };
    if let Err(error) = domain::claim(memory, assignment, now()) {
        if !control.release_undispatched_maintenance(&root_id, now())? {
            control.finish_maintenance(&root_id, Outcome::Failed, now())?;
        }
        memory.connection.execute(
            "UPDATE maintenance_driver_roots SET settled=1 WHERE job=?",
            [&assignment.id],
        )?;
        publish(
            shared,
            permission.scope.project.clone(),
            json!({"state":"deferred","error":error.to_string()}),
        );
        return Ok(None);
    }
    Ok(Some((root_id, deadline, admission)))
}

fn start_worker(
    shared: &Shared,
    memory: &mut Store,
    control: &mut Controller,
    active: &mut Option<Active>,
    assignment: Assignment,
    config: MaintenanceWorkerConfig,
    factory: WorkerFactory,
) -> Result<()> {
    let root_id = config.root_id.clone();
    let deadline = config.deadline_at;
    let admission = config.admission.clone();
    match factory(config) {
        Ok(service) => {
            if let Err(error) = service.begin_background(&root_id, &assignment.prompt) {
                let _ = service.shutdown();
                domain::outcome(memory, &assignment.id, "unknown")?;
                control.finish_maintenance(&root_id, Outcome::Unknown, now())?;
                publish(
                    shared,
                    assignment.scope.project.clone(),
                    json!({"state":"unknown","error":format!("{error:?}")}),
                );
                return Ok(());
            }
            publish(
                shared,
                assignment.scope.project.clone(),
                json!({"state":"running","job":assignment.id}),
            );
            *active = Some(Active {
                assignment,
                root_id,
                service,
                started: Instant::now(),
                deadline,
                admission,
            });
        }
        Err(error) => {
            domain::outcome(memory, &assignment.id, "unknown")?;
            control.finish_maintenance(&root_id, Outcome::Unknown, now())?;
            publish(
                shared,
                assignment.scope.project.clone(),
                json!({"state":"unknown","error":error}),
            );
        }
    }
    Ok(())
}

fn poll_active(
    root: &std::path::Path,
    shared: &Shared,
    memory: &mut Store,
    control: &mut Controller,
    active: &mut Option<Active>,
    last_prepare: &mut Option<Instant>,
    timestamp: i64,
) -> Result<()> {
    if let Some(job) = active.as_ref() {
        let current = match domain::validate_assignment(memory, &job.assignment) {
            Ok(()) => true,
            Err(error) if database_busy(&error) => return Err(error),
            Err(_) => false,
        };
        let permission = control.maintenance_permission(timestamp, true)?;
        let cancelled = !current
            || permission
                .as_ref()
                .is_none_or(|p| p.scope != job.assignment.scope)
            || timestamp >= job.deadline
            || job.started.elapsed() >= Duration::from_secs(120);
        if cancelled {
            let _ = job.service.cancel();
        }
        let snapshot = job.service.snapshot();
        if cancelled
            || matches!(
                snapshot.state,
                ServiceState::Completed | ServiceState::Failed | ServiceState::Stopped
            )
        {
            let job = active.take().expect("active maintenance");
            let _ = job.service.shutdown();
            // Join is off the UI thread; transport requests and owned process
            // cleanup retain their existing bounded cancellation behavior.
            job.service.join();
            let worker_error = snapshot.error.or_else(|| match &snapshot.result {
                Some(TurnResult::Failed { text, .. }) => Some(text.clone()),
                _ => None,
            });
            let result = finish(
                memory,
                control,
                &job,
                snapshot.result.or(native_result(root, &job.root_id)?),
                timestamp,
            );
            let notice = result
                .err()
                .map(|e| e.to_string())
                .or(worker_error)
                .map(|message| message.chars().take(4096).collect::<String>());
            let mut status = domain::status(memory, &job.assignment.scope)?;
            status["state"] = json!("idle");
            status["last_error"] = json!(notice);
            publish(shared, job.assignment.scope.project.clone(), status);
            *last_prepare = Some(Instant::now());
        }
        return Ok(());
    }
    Ok(())
}

fn database_busy(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| matches!(cause.downcast_ref::<rusqlite::Error>(),Some(rusqlite::Error::SqliteFailure(detail,_)) if matches!(detail.code,rusqlite::ErrorCode::DatabaseBusy|rusqlite::ErrorCode::DatabaseLocked)))
}

fn finish(
    memory: &mut Store,
    control: &mut Controller,
    job: &Active,
    result: Option<TurnResult>,
    timestamp: i64,
) -> Result<()> {
    if result.is_none()
        && (job.admission.enter().is_err() || control.maintenance_child_released(&job.root_id)?)
        && control.release_undispatched_maintenance(&job.root_id, timestamp)?
    {
        domain::release_pending(memory, &job.assignment.id)?;
        memory.connection.execute(
            "UPDATE maintenance_driver_roots SET settled=1 WHERE job=?",
            [&job.assignment.id],
        )?;
        return Ok(());
    }
    let outcome = record_result(memory, control, job, result, timestamp)?;
    control.finish_maintenance(&job.root_id, outcome, timestamp)?;
    memory.connection.execute(
        "UPDATE maintenance_driver_roots SET settled=1 WHERE job=?",
        [&job.assignment.id],
    )?;
    Ok(())
}

fn record_result(
    memory: &mut Store,
    control: &mut Controller,
    job: &Active,
    result: Option<TurnResult>,
    timestamp: i64,
) -> Result<Outcome> {
    let outcome = match result {
        Some(TurnResult::Complete { text, .. }) => {
            // Delivery accounting is completed even if native validation rejects
            // the content; no extra model is recruited for repair.
            control.finish_maintenance(&job.root_id, Outcome::Completed, timestamp)?;
            domain::checkpoint_result(memory, &job.assignment.id, &text)?;
            domain::commit(memory, &job.assignment, &text, timestamp)?;
            domain::reconcile_outbox(memory)?;
            Outcome::Completed
        }
        Some(TurnResult::Failed { .. }) => {
            domain::outcome(memory, &job.assignment.id, "failed")?;
            Outcome::Failed
        }
        None => {
            domain::outcome(memory, &job.assignment.id, "unknown")?;
            Outcome::Unknown
        }
    };
    Ok(outcome)
}

fn native_result(root: &std::path::Path, request: &str) -> Result<Option<TurnResult>> {
    let path = root.join("maintenance-runtime.sqlite");
    if !path.exists() {
        return Ok(None);
    }
    crate::assistant_storage::database(&path)?;
    let db =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let row:Option<(String,String)>=db.query_row("SELECT turn_id,reply FROM assistant_runtime_turns WHERE request_id=? AND state='completed' AND turn_id IS NOT NULL AND reply IS NOT NULL AND length(reply)<=8192",[request],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
    Ok(row.map(|(turn_id, text)| TurnResult::Complete {
        turn_id,
        text,
        usage: None,
    }))
}

fn recover(root: &std::path::Path, memory: &mut Store, control: &mut Controller) -> Result<()> {
    let rows: Vec<(String, String, String)> = {
        let mut query=memory.connection.prepare("SELECT d.job,d.root_id,COALESCE(j.state,'unclaimed') FROM maintenance_driver_roots d LEFT JOIN maintenance_jobs j ON j.id=d.job WHERE d.settled=0 LIMIT 64")?;
        query
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?
    };
    for (job, root_id, state) in rows {
        recover_root(root, memory, control, &job, &root_id, &state)?;
    }
    domain::recover(memory, now()).context("Maintenance local recovery")
}
fn recover_root(
    root: &std::path::Path,
    memory: &mut Store,
    control: &mut Controller,
    job: &str,
    root_id: &str,
    state: &str,
) -> Result<()> {
    if recover_unsent(memory, control, job, root_id, state)? {
        return Ok(());
    }
    if state == "unclaimed" {
        control.release_undispatched_maintenance(root_id, now())?;
        memory.connection.execute(
            "UPDATE maintenance_driver_roots SET settled=1 WHERE job=?",
            [job],
        )?;
        return Ok(());
    }
    let outcome =
        if let Some(TurnResult::Complete { text: reply, .. }) = native_result(root, root_id)? {
            if state != "completed" {
                // A revoked generation may discard content while its confirmed
                // delivery remains charged. It never restores forgotten text.
                let _ = domain::checkpoint_result(memory, job, &reply);
            }
            Outcome::Completed
        } else {
            Outcome::Unknown
        };
    // Startup lifecycle recovery made dispatched roots unknown; preserve
    // that accounting until authoritative receipts can reconcile it.
    control.finish_maintenance(root_id, outcome, now())?;
    memory.connection.execute(
        "UPDATE maintenance_driver_roots SET settled=1 WHERE job=?",
        [job],
    )?;
    Ok(())
}

fn recover_unsent(
    memory: &mut Store,
    control: &mut Controller,
    job: &str,
    root_id: &str,
    state: &str,
) -> Result<bool> {
    if state != "claimed"
        || !(control.maintenance_child_released(root_id)?
            || control.maintenance_root_released(root_id)?)
    {
        return Ok(false);
    }
    if !control.release_undispatched_maintenance(root_id, now())? {
        return Ok(false);
    }
    domain::release_pending(memory, job)?;
    memory.connection.execute(
        "UPDATE maintenance_driver_roots SET settled=1 WHERE job=?",
        [job],
    )?;
    Ok(true)
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .min(i64::MAX as u64) as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assistant_provider::{ProviderError, RpcTransport, ServerEvent};
    #[test]
    fn startup_memory_busy_retries_without_spawning_provider() {
        fn forbidden_worker(
            _: MaintenanceWorkerConfig,
        ) -> std::result::Result<AssistantService, String> {
            panic!("No evidence or grant may spawn a maintenance provider");
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("assistant");
        let _control = Controller::open(&root).unwrap();
        let writer = rusqlite::Connection::open(root.join("memory.sqlite")).unwrap();
        writer.execute_batch("BEGIN IMMEDIATE").unwrap();
        let driver = Driver::with_factory(root.clone(), forbidden_worker).unwrap();
        driver.tick(false);
        let deadline = Instant::now() + Duration::from_secs(8);
        while driver.status("fixture")["state"] != "deferred" && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(driver.status("fixture")["state"], "deferred");
        // Initialization has not yet created the driver table under the held
        // writer lock: this is startup, not an ordinary post-start step.
        let initialized: bool = writer
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='maintenance_driver_roots')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!initialized);
        writer.execute_batch("ROLLBACK").unwrap();
        driver.tick(false);
        while driver.status("fixture")["state"] != "not_admitted" && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(driver.status("fixture")["state"], "not_admitted");
        driver.quiesce();
        drop(driver);
        let initialized: bool = writer
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='maintenance_driver_roots')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(initialized);
    }
    #[test]
    fn database_busy_defers_without_blocking_ui_or_killing_driver() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("assistant");
        let _control = Controller::open(&root).unwrap();
        let writer = rusqlite::Connection::open(root.join("owner.sqlite")).unwrap();
        writer.execute_batch("BEGIN EXCLUSIVE").unwrap();
        let driver = Driver::new(root).unwrap();
        driver.tick(false);
        let deadline = Instant::now() + Duration::from_secs(3);
        while driver.status("fixture")["state"] != "deferred" && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(driver.status("fixture")["state"], "deferred");
        let before = Instant::now();
        driver.foreground_pending(true);
        driver.foreground_pending(false);
        assert!(before.elapsed() < Duration::from_millis(100));
        writer.execute_batch("ROLLBACK").unwrap();
        while driver.status("fixture")["state"] != "not_admitted" && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(driver.status("fixture")["state"], "not_admitted");
        driver.quiesce();
        drop(driver);
    }
    #[derive(Default)]
    struct FakeProvider {
        events: Vec<ServerEvent>,
    }
    impl RpcTransport for FakeProvider {
        fn request(
            &mut self,
            method: &str,
            params: Value,
        ) -> std::result::Result<Value, ProviderError> {
            Ok(match method {
                "thread/start" => {
                    json!({"thread":{"id":"fake-maintenance-thread"},"activePermissionProfile":{"id":"pika-assistant","extends":null},"sandbox":{"type":"readOnly","networkAccess":false},"approvalPolicy":"never","model":crate::assistant_provider::DEFAULT_MODEL})
                }
                "turn/start" => {
                    let prompt = params["input"][0]["text"].as_str().unwrap();
                    let selected = prompt
                        .split("Selected evidence: ")
                        .nth(1)
                        .unwrap()
                        .split("\nContext:")
                        .next()
                        .unwrap();
                    let covered: Value = serde_json::from_str(selected).unwrap();
                    let text=json!({"purpose":"consolidation","covered":covered,"learning":[],"workshop":[]}).to_string();
                    self.events = vec![
                        ServerEvent::AgentDelta {
                            thread_id: Some("fake-maintenance-thread".into()),
                            turn_id: "fake-turn".into(),
                            text,
                        },
                        ServerEvent::Completed {
                            thread_id: Some("fake-maintenance-thread".into()),
                            turn_id: "fake-turn".into(),
                            usage: None,
                        },
                    ];
                    json!({"turn":{"id":"fake-turn"}})
                }
                _ => json!({}),
            })
        }
        fn notify(&mut self, _: &str, _: Value) -> std::result::Result<(), ProviderError> {
            Ok(())
        }
        fn notifications(&mut self) -> std::result::Result<Vec<ServerEvent>, ProviderError> {
            Ok(std::mem::take(&mut self.events))
        }
        fn interrupt(&mut self, _: &str, _: &str) -> std::result::Result<(), ProviderError> {
            Ok(())
        }
    }
    fn fake_factory(
        config: MaintenanceWorkerConfig,
    ) -> std::result::Result<AssistantService, String> {
        assistant_maintenance_worker::fake_service(config, FakeProvider::default())
    }

    #[test]
    fn maintenance_off_during_startup_prevents_actual_paid_dispatch() {
        struct Slow {
            entered: mpsc::SyncSender<()>,
            proceed: mpsc::Receiver<()>,
            sends: Arc<std::sync::atomic::AtomicUsize>,
            inner: FakeProvider,
        }
        impl RpcTransport for Slow {
            fn request(
                &mut self,
                method: &str,
                params: Value,
            ) -> std::result::Result<Value, ProviderError> {
                if method == "initialize" {
                    self.entered.send(()).unwrap();
                    self.proceed.recv_timeout(Duration::from_secs(3)).unwrap();
                }
                if method == "turn/start" {
                    self.sends.fetch_add(1, Ordering::SeqCst);
                }
                self.inner.request(method, params)
            }
            fn notify(
                &mut self,
                method: &str,
                params: Value,
            ) -> std::result::Result<(), ProviderError> {
                self.inner.notify(method, params)
            }
            fn notifications(&mut self) -> std::result::Result<Vec<ServerEvent>, ProviderError> {
                self.inner.notifications()
            }
            fn interrupt(
                &mut self,
                thread: &str,
                turn: &str,
            ) -> std::result::Result<(), ProviderError> {
                self.inner.interrupt(thread, turn)
            }
        }
        use crate::assistant_memory::{NewRecord, Origin, RecordKind, Scope};
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("assistant");
        let mut control = Controller::open(&root).unwrap();
        let mut memory = Store::open(root.join("memory.sqlite")).unwrap();
        let scope = Scope {
            project: Some("fixture".into()),
            ..Default::default()
        };
        memory
            .append_user(NewRecord {
                kind: RecordKind::Finding,
                origin: Origin::Human,
                scope: scope.clone(),
                body: "A bounded original human fact".into(),
                provenance: "synthetic human fixture".into(),
                timestamp: now(),
                supersedes: None,
                dependencies: vec![],
                decision_state: None,
                protected_policy: false,
            })
            .unwrap();
        domain::configure(&mut memory, &scope, 3600, true, now()).unwrap();
        let assignment = domain::prepare(&mut memory, &scope, now())
            .unwrap()
            .unwrap();
        domain::claim(&mut memory, &assignment, now()).unwrap();
        let mut policy =
            crate::assistant_policy::AssistantPolicy::open(root.join("policy.sqlite")).unwrap();
        policy
            .configure(&crate::assistant_policy::PolicyConfig {
                max_total_calls: 2,
                background_calls: 2,
                ..Default::default()
            })
            .unwrap();
        control
            .approve_maintenance("fixture", std::path::Path::new("/fake/not-run"), 2, 2, 1)
            .unwrap();
        let permission = control
            .maintenance_permission(now(), false)
            .unwrap()
            .unwrap();
        let deadline = control
            .reserve_maintenance("fixture-root", &permission, now())
            .unwrap();
        memory.connection.execute_batch("CREATE TABLE maintenance_driver_roots(job TEXT PRIMARY KEY,root_id TEXT NOT NULL,settled INTEGER NOT NULL DEFAULT 0)").unwrap();
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (proceed_tx, proceed_rx) = mpsc::sync_channel(1);
        let sends = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let prompt = assignment.prompt.clone();
        let service = assistant_maintenance_worker::fake_service(
            MaintenanceWorkerConfig {
                root: root.clone(),
                executable: "/fake/not-run".into(),
                scope: scope.clone(),
                root_id: "fixture-root".into(),
                dependencies: assignment.sources.iter().map(|s| s.id.clone()).collect(),
                deadline_at: deadline,
                admission: Default::default(),
                assignment: assignment.clone(),
            },
            Slow {
                entered: entered_tx,
                proceed: proceed_rx,
                sends: sends.clone(),
                inner: Default::default(),
            },
        )
        .unwrap();
        service.begin_background("fixture-root", prompt).unwrap();
        entered_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        domain::configure(&mut memory, &scope, 3600, false, now()).unwrap();
        proceed_tx.send(()).unwrap();
        let until = Instant::now() + Duration::from_secs(3);
        while service.snapshot().state != ServiceState::Failed && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(service.snapshot().state, ServiceState::Failed);
        assert_eq!(sends.load(Ordering::SeqCst), 0);
        assert_eq!(
            policy
                .reservation("assistant:fixture-root")
                .unwrap()
                .unwrap()
                .state,
            crate::assistant_policy::ReservationState::Released
        );
        service.shutdown().unwrap();
        service.join();
        let job = Active {
            assignment,
            root_id: "fixture-root".into(),
            service,
            started: Instant::now(),
            deadline,
            admission: Default::default(),
        };
        // Simulate a crash between the durable no-send refund and local claim
        // cleanup; recovery must finish this without another provider call.
        assert!(
            control
                .release_undispatched_maintenance(&job.root_id, now())
                .unwrap()
        );
        recover_root(
            &root,
            &mut memory,
            &mut control,
            &job.assignment.id,
            &job.root_id,
            "claimed",
        )
        .unwrap();
        assert_eq!(
            policy.reservation("fixture-root").unwrap().unwrap().state,
            crate::assistant_policy::ReservationState::Released
        );
        let owner = rusqlite::Connection::open(root.join("owner.sqlite")).unwrap();
        let confirmed: i64 = owner
            .query_row(
                "SELECT confirmed_calls FROM assistant_background_roots WHERE id='fixture-root'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(confirmed, 0);
        domain::configure(&mut memory, &scope, 3600, true, now()).unwrap();
        assert!(
            domain::prepare(&mut memory, &scope, now())
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn driver_commits_exact_no_change_coverage_with_one_background_call() {
        use crate::assistant_memory::{NewRecord, Origin, RecordKind, Scope};
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("assistant");
        let mut control = Controller::open(&root).unwrap();
        let mut policy =
            crate::assistant_policy::AssistantPolicy::open(root.join("policy.sqlite")).unwrap();
        policy
            .configure(&crate::assistant_policy::PolicyConfig {
                max_total_calls: 3,
                ..Default::default()
            })
            .unwrap();
        control
            .approve_maintenance(
                "fixture",
                std::path::Path::new("/fake/not-executed"),
                3,
                2,
                1,
            )
            .unwrap();
        let scope = Scope {
            project: Some("fixture".into()),
            ..Default::default()
        };
        let mut memory = Store::open(root.join("memory.sqlite")).unwrap();
        memory
            .append_user(NewRecord {
                kind: RecordKind::Finding,
                origin: Origin::Human,
                scope: scope.clone(),
                body: "Keep the accepted design rationale separate from implementation status"
                    .into(),
                provenance: "human test fixture".into(),
                timestamp: now(),
                supersedes: None,
                dependencies: vec![],
                decision_state: None,
                protected_policy: false,
            })
            .unwrap();
        domain::configure(&mut memory, &scope, 3600, true, now()).unwrap();
        let driver = Driver::with_factory(root.clone(), fake_factory).unwrap();
        driver.tick(false);
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let done: bool = memory
                .connection
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM maintenance_jobs WHERE state='completed')",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            if done {
                break;
            }
            assert!(Instant::now() < deadline, "{}", driver.status("fixture"));
            std::thread::sleep(Duration::from_millis(10));
        }
        driver.quiesce();
        drop(driver);
        let db = rusqlite::Connection::open(root.join("policy.sqlite")).unwrap();
        let counts:(u64,u64)=db.query_row("SELECT COUNT(*),SUM(calls) FROM assistant_reservations WHERE parent_id IS NULL AND background=1 AND state='completed'",[],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();
        assert_eq!(counts, (1, 1));
        let covered: u64 = memory
            .connection
            .query_row("SELECT COUNT(*) FROM maintenance_coverage", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(covered, 1);
        assert!(!root.join("provider-home").exists());
    }
    #[test]
    fn no_evidence_starts_no_provider_and_quiescence_finishes_before_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("assistant");
        let mut control = Controller::open(&root).unwrap();
        let mut policy =
            crate::assistant_policy::AssistantPolicy::open(root.join("policy.sqlite")).unwrap();
        policy
            .configure(&crate::assistant_policy::PolicyConfig {
                max_total_calls: 3,
                ..Default::default()
            })
            .unwrap();
        control
            .approve_maintenance(
                "fixture",
                std::path::Path::new("/fake/must-not-execute"),
                3,
                2,
                1,
            )
            .unwrap();
        let driver = Driver::new(root.clone()).unwrap();
        driver.tick(false);
        let deadline = Instant::now() + Duration::from_secs(3);
        while driver.status("fixture")["state"] == "starting" && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(driver.status("fixture")["state"], "idle");
        assert!(!root.join("provider-home").exists());
        driver.foreground_pending(true);
        assert!(driver.shared.admission.lock().unwrap().enter().is_err());
        driver.quiesce();
        while !driver.is_quiescent() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(driver.is_quiescent());
        drop(driver);
        let db = rusqlite::Connection::open(root.join("policy.sqlite")).unwrap();
        assert_eq!(
            db.query_row::<u64, _, _>("SELECT COUNT(*) FROM assistant_reservations", [], |r| r
                .get(0))
                .unwrap(),
            0
        );
    }
}
