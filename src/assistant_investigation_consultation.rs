//! Exact, explicitly granted private-consultation jobs. No discovery, card
//! interview, transcript-path read, or remote fallback is performed here.
use super::CodexWorkerFactory;
use crate::{
    assistant_investigation::{
        DisposableWorker, WorkerFactory, WorkerPoll, WorkerReceipt, validate_project_scope,
    },
    assistant_memory::Scope,
    assistant_policy::{AssistantPolicy, ReservationState},
    consult::{CancellationToken, Consultation, ConsultationDispatchFence, ConsultationOptions},
    model::Session,
};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, atomic::AtomicBool, mpsc},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// Only the native host creates these after exact user scope approval. This
/// type is deliberately not deserializable from model/worker output.
#[derive(Clone)]
pub struct ConsultationAuthorization {
    pub id: String,
    pub grant_id: String,
    pub authority_node: String,
    pub target_node: String,
    pub parent: Session,
    pub scope: Scope,
    pub destination: String,
    pub executable: PathBuf,
}
impl ConsultationAuthorization {
    pub fn grant_scope(&self) -> String {
        serde_json::json!({"node":self.target_node,"provider":self.parent.provider.as_str(),"conversation":self.parent.provider_thread_id(),"scope":self.scope,"destination":self.destination}).to_string()
    }
    fn validate(
        &self,
        policy: &AssistantPolicy,
        scope: &Scope,
        root_id: &str,
        now: i64,
    ) -> Result<(), String> {
        validate_project_scope(scope).map_err(|e| e.to_string())?;
        if self.scope != *scope
            || self.destination != "codex"
            || self.authority_node != self.target_node
            || uuid::Uuid::parse_str(&self.target_node).is_err()
            || uuid::Uuid::parse_str(self.parent.provider_thread_id()).is_err()
            || !self.executable.is_absolute()
            || self.id.is_empty()
        {
            return Err("Private consultation identity, project, local authority or destination does not match approval".into());
        }
        policy
            .validate_grant(
                &self.grant_id,
                &self.destination,
                &self.grant_scope(),
                "private-consultation",
                now,
            )
            .map_err(|e| e.to_string())?;
        let root = policy
            .reservation(root_id)
            .map_err(|e| e.to_string())?
            .ok_or("Missing consultation root")?;
        if root.parent_id.is_some()
            || root.state != ReservationState::Reserved
            || root.deadline_at <= now
        {
            return Err("Private consultation root is not active".into());
        }
        Ok(())
    }
}

struct Answer {
    text: Option<String>,
    error: Option<String>,
    receipt: WorkerReceipt,
}
trait Backend: Send + 'static {
    fn run(
        self: Box<Self>,
        authorization: &ConsultationAuthorization,
        question: &str,
        cancel: CancellationToken,
        deadline: i64,
        fence: ConsultationDispatchFence,
    ) -> Result<Answer, String>;
}
struct Native;
impl Backend for Native {
    fn run(
        self: Box<Self>,
        a: &ConsultationAuthorization,
        question: &str,
        cancel: CancellationToken,
        deadline: i64,
        fence: ConsultationDispatchFence,
    ) -> Result<Answer, String> {
        let mut options = ConsultationOptions::new(&a.executable);
        options.cancellation = cancel;
        options.dispatch_fence = Some(fence);
        options.timeout = Duration::from_secs(deadline.saturating_sub(now()).max(1) as u64);
        let mut side = match Consultation::open(&a.parent, options) {
            Ok(side) => side,
            Err(error) => {
                return Ok(Answer {
                    text: None,
                    error: Some(error.to_string()),
                    receipt: WorkerReceipt {
                        provider: Some(a.parent.provider.as_str().into()),
                        delivery: Some(format!("{:?}", error.receipt.delivery)),
                        cleanup: Some(format!("{:?}", error.receipt.cleanup)),
                        ..Default::default()
                    },
                });
            }
        };
        let answer = side.ask(question);
        let cleanup = side.close();
        let receipt = side.receipt();
        let usage = side.turn_metrics().and_then(|m| m.usage.as_ref());
        Ok(Answer {
            text: answer.as_ref().ok().cloned(),
            error: answer.err().map(|e| e.to_string()).or_else(|| {
                cleanup
                    .err()
                    .map(|e| format!("Answer obtained; cleanup uncertain: {e}"))
            }),
            receipt: WorkerReceipt {
                provider: Some(a.parent.provider.as_str().into()),
                source_node: Some(a.target_node.clone()),
                source_conversation: Some(a.parent.provider_thread_id().into()),
                thread_id: side.child_id().map(str::to_owned),
                turn_id: None,
                input_tokens: usage.map(|u| u.input_tokens),
                output_tokens: usage.map(|u| u.output_tokens),
                delivery: Some(format!("{:?}", receipt.delivery)),
                cleanup: Some(format!("{:?}", receipt.cleanup)),
            },
        })
    }
}

pub struct ScopedWorkerFactory {
    reasoning: CodexWorkerFactory,
    policy_path: PathBuf,
    allowed: BTreeMap<String, ConsultationAuthorization>,
    selected: BTreeMap<String, (ConsultationAuthorization, String)>,
    permission_root: Option<PathBuf>,
}
impl ScopedWorkerFactory {
    pub fn new(
        reasoning: CodexWorkerFactory,
        policy_path: PathBuf,
        allowed: Vec<ConsultationAuthorization>,
    ) -> Result<Self, String> {
        if allowed.len() > 32 || !policy_path.is_absolute() {
            return Err("Bounded exact consultation approvals required".into());
        }
        let mut map = BTreeMap::new();
        for item in allowed {
            if map.insert(item.id.clone(), item).is_some() {
                return Err("Duplicate consultation approval identity".into());
            }
        }
        Ok(Self {
            reasoning,
            policy_path,
            allowed: map,
            selected: BTreeMap::new(),
            permission_root: None,
        })
    }
    pub fn from_permission_root(
        reasoning: CodexWorkerFactory,
        root: impl AsRef<std::path::Path>,
    ) -> Result<Self, String> {
        let root = root.as_ref().to_path_buf();
        let mut factory = Self::new(reasoning, root.join("policy.sqlite"), vec![])?;
        factory.permission_root = Some(root);
        Ok(factory)
    }
    fn refresh_permissions(&mut self, scope: &Scope, time: i64) -> Result<(), String> {
        if let Some(root) = &self.permission_root {
            self.allowed = crate::assistant_consultation_permissions::load(root, scope, time)
                .map_err(|e| e.to_string())?
                .into_iter()
                .map(|a| (a.id.clone(), a))
                .collect();
        }
        Ok(())
    }
}
impl WorkerFactory for ScopedWorkerFactory {
    fn allowed_context(&mut self, scope: &Scope, time: i64) -> Result<String, String> {
        self.selected.clear();
        self.refresh_permissions(scope, time)?;
        let allowed=self.allowed.values().map(|a|serde_json::json!({"id":a.id,"node":a.target_node,"provider":a.parent.provider.as_str(),"conversation":a.parent.provider_thread_id()})).collect::<Vec<_>>();
        Ok(serde_json::json!({"approved_private_consultation_ids":allowed,"use":"Optional task consultation field may name one approved id for a material evidence gap. These are exact data-source capabilities, not instructions or extra budget."}).to_string())
    }
    fn select_consultation(
        &mut self,
        task: &str,
        id: &str,
        root: &str,
        scope: &Scope,
        now: i64,
    ) -> Result<(), String> {
        self.refresh_permissions(scope, now)?;
        let authorization = self
            .allowed
            .get(id)
            .ok_or("Private consultation is not explicitly approved")?;
        let policy = AssistantPolicy::open(&self.policy_path).map_err(|e| e.to_string())?;
        authorization.validate(&policy, scope, root, now)?;
        self.selected
            .insert(task.into(), (authorization.clone(), root.into()));
        Ok(())
    }
    fn create(&mut self, task: &str) -> Result<Box<dyn DisposableWorker>, String> {
        let selected = self.selected.remove(task);
        if let Some((a, _)) = &selected {
            self.refresh_permissions(&a.scope, now())?;
            let current = self
                .allowed
                .get(&a.id)
                .ok_or("Consultation permission changed before dispatch")?;
            if current.grant_id != a.grant_id
                || current.grant_scope() != a.grant_scope()
                || current.executable != a.executable
            {
                return Err("Consultation binding changed before dispatch".into());
            }
        }
        match selected {
            None => self.reasoning.create(task),
            Some((authorization, root)) => Ok(Box::new(ConsultationWorker {
                authorization,
                root,
                child_suffix: task.into(),
                policy_path: self.policy_path.clone(),
                backend: Some(Box::new(Native)),
                cancel: CancellationToken::default(),
                receiver: None,
                join: None,
                receipt: WorkerReceipt::default(),
                permission_root: self.permission_root.clone(),
                memory_epoch: None,
                dispatch_cancellation: Default::default(),
            })),
        }
    }
}
struct ConsultationWorker {
    authorization: ConsultationAuthorization,
    root: String,
    child_suffix: String,
    policy_path: PathBuf,
    backend: Option<Box<dyn Backend>>,
    cancel: CancellationToken,
    receiver: Option<mpsc::Receiver<Result<Answer, String>>>,
    join: Option<std::thread::JoinHandle<()>>,
    receipt: WorkerReceipt,
    permission_root: Option<PathBuf>,
    memory_epoch: Option<(PathBuf, u64)>,
    dispatch_cancellation: crate::assistant_service::DispatchCancellation,
}
struct DispatchLease {
    _memory: rusqlite::Connection,
    _policy: AssistantPolicy,
    _registry: Option<rusqlite::Connection>,
    _admission: crate::assistant_service::DispatchAdmission,
}
fn dispatch_fence(
    memory_epoch: (PathBuf, u64),
    policy_path: PathBuf,
    permission_root: Option<PathBuf>,
    authorization: ConsultationAuthorization,
    root: String,
    child_suffix: String,
    cancellation: (
        CancellationToken,
        crate::assistant_service::DispatchCancellation,
    ),
) -> ConsultationDispatchFence {
    let (cancel, dispatch_cancellation) = cancellation;
    Arc::new(move || {
        if cancel.is_cancelled() {
            anyhow::bail!("Consultation cancelled before dispatch");
        }
        let policy = AssistantPolicy::open(&policy_path)?;
        let memory = fence_memory_epoch(&memory_epoch)?;
        let registry = permission_root
            .as_ref()
            .map(|p| crate::assistant_consultation_permissions::fence_binding(p, &authorization))
            .transpose()?;
        policy.lock_dispatch()?;
        authorization
            .validate(&policy, &authorization.scope, &root, now())
            .map_err(anyhow::Error::msg)?;
        let child =
            exact_dispatched_child(&policy, &root, &child_suffix).map_err(anyhow::Error::msg)?;
        policy.validate_actual_dispatch(&child.id, now())?;
        if cancel.is_cancelled() {
            anyhow::bail!("Consultation cancelled before dispatch");
        }
        let admission = dispatch_cancellation.enter().map_err(anyhow::Error::msg)?;
        Ok(Box::new(DispatchLease {
            _memory: memory,
            _policy: policy,
            _registry: registry,
            _admission: admission,
        }))
    })
}
fn fence_memory_epoch(memory_epoch: &(PathBuf, u64)) -> anyhow::Result<rusqlite::Connection> {
    use rusqlite::OptionalExtension;
    let memory = rusqlite::Connection::open_with_flags(
        &memory_epoch.0,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
    )?;
    memory.busy_timeout(Duration::from_secs(5))?;
    memory.execute_batch("BEGIN IMMEDIATE")?;
    let epoch: Option<String> = memory
        .query_row(
            "SELECT value FROM memory_meta WHERE key='forget_epoch'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    if epoch.as_deref().unwrap_or("0").parse::<u64>()? != memory_epoch.1 {
        anyhow::bail!("Memory forgotten before actual consultation dispatch");
    }
    Ok(memory)
}
fn exact_dispatched_child(
    policy: &AssistantPolicy,
    root: &str,
    suffix: &str,
) -> Result<crate::assistant_policy::Reservation, String> {
    let child = policy
        .reservation(&format!("{root}:{suffix}"))
        .map_err(|e| e.to_string())?
        .ok_or("Missing exact child reservation")?;
    if child.parent_id.as_deref() != Some(root)
        || child.state != ReservationState::Dispatched
        || child.calls != 1
        || child.deadline_at <= now()
    {
        return Err("Exact child budget was not dispatched".into());
    }
    Ok(child)
}
impl DisposableWorker for ConsultationWorker {
    fn dispatches_later(&self) -> bool {
        true
    }
    fn start_with_cancellation(
        &mut self,
        assignment: &str,
        scope: &Scope,
        cancellation: &crate::assistant_service::DispatchCancellation,
    ) -> Result<(), String> {
        self.dispatch_cancellation = cancellation.clone();
        self.start(assignment, scope)
    }
    fn bind_dispatch_epoch(&mut self, memory: &std::path::Path, epoch: u64) -> Result<(), String> {
        self.memory_epoch = Some((memory.into(), epoch));
        Ok(())
    }
    fn start(&mut self, assignment: &str, scope: &Scope) -> Result<(), String> {
        if assignment.is_empty() || assignment.len() > 16 * 1024 || self.receiver.is_some() {
            return Err("Invalid or duplicate consultation dispatch".into());
        }
        let policy = AssistantPolicy::open(&self.policy_path).map_err(|e| e.to_string())?;
        self.authorization
            .validate(&policy, scope, &self.root, now())?;
        let child = exact_dispatched_child(&policy, &self.root, &self.child_suffix)?;
        let backend = self.backend.take().ok_or("Consultation already used")?;
        let authorization = self.authorization.clone();
        let prompt = assignment.to_owned();
        let cancel = self.cancel.clone();
        let deadline = child.deadline_at;
        let fence = dispatch_fence(
            self.memory_epoch
                .clone()
                .ok_or("Missing exact dispatch epoch")?,
            self.policy_path.clone(),
            self.permission_root.clone(),
            authorization.clone(),
            self.root.clone(),
            self.child_suffix.clone(),
            (cancel.clone(), self.dispatch_cancellation.clone()),
        );
        let (sender, receiver) = mpsc::sync_channel(1);
        self.join = Some(
            std::thread::Builder::new()
                .name("pika-private-consult".into())
                .spawn(move || {
                    let _ =
                        sender.send(backend.run(&authorization, &prompt, cancel, deadline, fence));
                })
                .map_err(|e| e.to_string())?,
        );
        self.receiver = Some(receiver);
        self.receipt.delivery = Some("unknown".into());
        self.receipt.cleanup = Some("pending".into());
        Ok(())
    }
    fn poll(&mut self, cancel: &AtomicBool) -> Result<WorkerPoll, String> {
        let policy = AssistantPolicy::open(&self.policy_path).map_err(|e| e.to_string())?;
        if let Err(error) =
            self.authorization
                .validate(&policy, &self.authorization.scope, &self.root, now())
        {
            self.cancel.cancel();
            return Err(format!(
                "Consultation authority changed; result discarded: {error}"
            ));
        }
        if cancel.load(std::sync::atomic::Ordering::Acquire) {
            self.cancel.cancel();
        }
        match self
            .receiver
            .as_ref()
            .ok_or("Consultation not started")?
            .try_recv()
        {
            Ok(Ok(answer)) => {
                self.receipt = answer.receipt;
                match answer.text {
                    Some(text) => Ok(WorkerPoll::Complete(if let Some(error) = answer.error {
                        format!("{text}\n[Consultation receipt caveat: {error}]")
                    } else {
                        text
                    })),
                    None => Ok(WorkerPoll::Failed(
                        answer.error.unwrap_or("No answer".into()),
                    )),
                }
            }
            Ok(Err(e)) => Err(e),
            Err(mpsc::TryRecvError::Empty) => Ok(WorkerPoll::Pending),
            Err(mpsc::TryRecvError::Disconnected) => {
                Err("Consultation worker disconnected; delivery and cleanup unknown".into())
            }
        }
    }
    fn cancel(&mut self) -> Result<(), String> {
        self.cancel.cancel();
        self.receipt.cleanup = Some("unknown".into());
        Ok(())
    }
    fn receipt(&self) -> WorkerReceipt {
        self.receipt.clone()
    }
}
impl Drop for ConsultationWorker {
    fn drop(&mut self) {
        self.cancel.cancel();
        if let Some(join) = self.join.take() {
            let _ = join.join();
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assistant_policy::{Grant, PolicyConfig};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    fn authorization() -> ConsultationAuthorization {
        let parent:Session=serde_json::from_value(serde_json::json!({"provider":"codex","session_id":"aaaaaaaa-aaaa-4aaa-aaaa-aaaaaaaaaaaa","status":"READY","unread":true,"source":"fixture","managed":false,"created_at":1.0,"updated_at":1.0,"last_event_at":1.0,"last_activity_at":1.0,"live":false,"attached":false,"home_state":""})).unwrap();
        ConsultationAuthorization {
            id: "expert-one".into(),
            grant_id: "grant-one".into(),
            authority_node: "bbbbbbbb-bbbb-4bbb-bbbb-bbbbbbbbbbbb".into(),
            target_node: "bbbbbbbb-bbbb-4bbb-bbbb-bbbbbbbbbbbb".into(),
            parent,
            scope: Scope {
                project: Some("project-a".into()),
                ..Default::default()
            },
            destination: "codex".into(),
            executable: PathBuf::from("/not-executed/fake"),
        }
    }
    fn fixture() -> (
        tempfile::TempDir,
        AssistantPolicy,
        ConsultationAuthorization,
    ) {
        let dir = tempfile::tempdir().unwrap();
        crate::assistant_memory::Store::open(dir.path().join("private/memory.sqlite")).unwrap();
        let mut policy = AssistantPolicy::open(dir.path().join("private/policy.sqlite")).unwrap();
        policy
            .configure(&PolicyConfig {
                max_total_calls: 4,
                max_concurrent: 2,
                default_deadline_seconds: 60,
                ..Default::default()
            })
            .unwrap();
        let a = authorization();
        policy
            .grant(&Grant {
                id: a.grant_id.clone(),
                provider: a.destination.clone(),
                scope: a.grant_scope(),
                capability: "private-consultation".into(),
                expires_at: now() + 60,
                revoked_at: None,
            })
            .unwrap();
        policy.reserve_root("root", 1, false, now(), None).unwrap();
        (dir, policy, a)
    }
    struct Fake(Arc<AtomicUsize>);
    struct Delayed {
        entered: mpsc::SyncSender<()>,
        proceed: mpsc::Receiver<()>,
        sends: Arc<AtomicUsize>,
    }
    impl Backend for Delayed {
        fn run(
            self: Box<Self>,
            a: &ConsultationAuthorization,
            prompt: &str,
            cancel: CancellationToken,
            deadline: i64,
            fence: ConsultationDispatchFence,
        ) -> Result<Answer, String> {
            self.entered.send(()).unwrap();
            self.proceed.recv_timeout(Duration::from_secs(2)).unwrap();
            Box::new(Fake(self.sends.clone())).run(a, prompt, cancel, deadline, fence)
        }
    }
    #[test]
    fn deferred_backend_rechecks_forget_revoke_and_cancel_at_actual_send() {
        for change in ["forget", "revoke", "cancel", "service_cancel"] {
            let (_dir, mut policy, a) = fixture();
            let path = policy.path().with_file_name("memory.sqlite");
            let mut memory = crate::assistant_memory::Store::open(&path).unwrap();
            let victim = memory
                .append(crate::assistant_memory::NewRecord {
                    kind: crate::assistant_memory::RecordKind::Finding,
                    origin: crate::assistant_memory::Origin::Human,
                    scope: Scope::default(),
                    body: "private evidence".into(),
                    provenance: "fixture".into(),
                    timestamp: 1,
                    supersedes: None,
                    dependencies: vec![],
                    decision_state: None,
                    protected_policy: false,
                })
                .unwrap();
            policy
                .reserve_child("root:expert", "root", 1, now(), None)
                .unwrap();
            policy.mark_dispatched("root:expert", now()).unwrap();
            let (entered_tx, entered_rx) = mpsc::sync_channel(1);
            let (proceed_tx, proceed_rx) = mpsc::sync_channel(1);
            let sends = Arc::new(AtomicUsize::new(0));
            let mut worker = ConsultationWorker {
                authorization: a.clone(),
                root: "root".into(),
                child_suffix: "expert".into(),
                policy_path: policy.path().into(),
                backend: Some(Box::new(Delayed {
                    entered: entered_tx,
                    proceed: proceed_rx,
                    sends: sends.clone(),
                })),
                cancel: CancellationToken::default(),
                receiver: None,
                join: None,
                receipt: WorkerReceipt::default(),
                permission_root: None,
                memory_epoch: Some((path, 0)),
                dispatch_cancellation: Default::default(),
            };
            let dispatch_cancellation = crate::assistant_service::DispatchCancellation::default();
            worker
                .start_with_cancellation("bounded question", &a.scope, &dispatch_cancellation)
                .unwrap();
            entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            match change {
                "forget" => {
                    memory.forget(&victim.id).unwrap();
                }
                "revoke" => policy.revoke_grant(&a.grant_id, now()).unwrap(),
                "service_cancel" => dispatch_cancellation.cancel(),
                _ => worker.cancel().unwrap(),
            }
            proceed_tx.send(()).unwrap();
            let result = worker
                .receiver
                .as_ref()
                .unwrap()
                .recv_timeout(Duration::from_secs(2))
                .unwrap();
            assert!(
                result.is_err(),
                "{change} must block the delayed actual send"
            );
            assert_eq!(sends.load(Ordering::SeqCst), 0);
        }
    }
    #[test]
    fn actual_send_lease_serializes_revoke_and_releases_before_answer_wait() {
        let (_dir, mut policy, a) = fixture();
        policy
            .reserve_child("root:expert", "root", 1, now(), None)
            .unwrap();
        policy.mark_dispatched("root:expert", now()).unwrap();
        let fence = dispatch_fence(
            (policy.path().with_file_name("memory.sqlite"), 0),
            policy.path().into(),
            None,
            a.clone(),
            "root".into(),
            "expert".into(),
            (CancellationToken::default(), Default::default()),
        );
        let guard = fence().unwrap();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let (done_tx, done_rx) = mpsc::sync_channel(1);
        let path = policy.path().to_path_buf();
        let grant = a.grant_id.clone();
        let join = std::thread::spawn(move || {
            ready_tx.send(()).unwrap();
            let mut policy = AssistantPolicy::open(path).unwrap();
            policy.revoke_grant(&grant, now()).unwrap();
            done_tx.send(()).unwrap();
        });
        ready_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(done_rx.recv_timeout(Duration::from_millis(50)).is_err());
        drop(guard); // send ended; generation may still be in progress
        done_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        join.join().unwrap();
        assert!(fence().is_err());
    }
    impl Backend for Fake {
        fn run(
            self: Box<Self>,
            a: &ConsultationAuthorization,
            prompt: &str,
            _: CancellationToken,
            _: i64,
            fence: ConsultationDispatchFence,
        ) -> Result<Answer, String> {
            let dispatch_guard = fence().map_err(|e| e.to_string())?;
            assert_eq!(
                a.parent.provider_thread_id(),
                "aaaaaaaa-aaaa-4aaa-aaaa-aaaaaaaaaaaa"
            );
            assert_eq!(prompt, "bounded question");
            self.0.fetch_add(1, Ordering::SeqCst);
            drop(dispatch_guard);
            Ok(Answer {
                text: Some("dated expert opinion".into()),
                error: Some("cleanup failed".into()),
                receipt: WorkerReceipt {
                    delivery: Some("Confirmed".into()),
                    cleanup: Some("Failed".into()),
                    ..Default::default()
                },
            })
        }
    }
    #[test]
    fn exact_private_consultation_has_separate_cleanup_and_does_not_mutate_parent() {
        let (_dir, mut policy, a) = fixture();
        let before = a.parent.clone();
        let calls = Arc::new(AtomicUsize::new(0));
        policy
            .reserve_child("root:expert", "root", 1, now(), None)
            .unwrap();
        policy.mark_dispatched("root:expert", now()).unwrap();
        let mut worker = ConsultationWorker {
            authorization: a.clone(),
            root: "root".into(),
            child_suffix: "expert".into(),
            policy_path: policy.path().into(),
            backend: Some(Box::new(Fake(calls.clone()))),
            cancel: CancellationToken::default(),
            receiver: None,
            join: None,
            receipt: WorkerReceipt::default(),
            permission_root: None,
            memory_epoch: Some((policy.path().with_file_name("memory.sqlite"), 0)),
            dispatch_cancellation: Default::default(),
        };
        worker.start("bounded question", &a.scope).unwrap();
        let started = std::time::Instant::now();
        let text = loop {
            match worker.poll(&AtomicBool::new(false)).unwrap() {
                WorkerPoll::Complete(text) => break text,
                WorkerPoll::Pending => {
                    assert!(started.elapsed() < Duration::from_secs(2));
                    std::thread::sleep(Duration::from_millis(1));
                }
                WorkerPoll::Failed(e) => panic!("{e}"),
            }
        };
        assert!(text.contains("cleanup failed"));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(a.parent, before);
        assert_eq!(worker.receipt().cleanup.as_deref(), Some("Failed"));
        assert_eq!(worker.receipt().input_tokens, None);
        assert!(worker.start("duplicate", &a.scope).is_err());
    }
    #[test]
    fn revoked_wrong_identity_destination_scope_and_missing_child_are_denied_before_backend() {
        let (_dir, mut policy, a) = fixture();
        assert!(a.validate(&policy, &a.scope, "root", now()).is_ok());
        let mut bad = a.clone();
        bad.target_node = "cccccccc-cccc-4ccc-cccc-cccccccccccc".into();
        assert!(bad.validate(&policy, &a.scope, "root", now()).is_err());
        let mut bad = a.clone();
        bad.destination = "claude".into();
        assert!(bad.validate(&policy, &a.scope, "root", now()).is_err());
        let mut bad = a.clone();
        bad.parent.session_id = "dddddddd-dddd-4ddd-dddd-dddddddddddd".into();
        assert!(bad.validate(&policy, &a.scope, "root", now()).is_err());
        let scope = Scope {
            project: Some("excluded".into()),
            ..Default::default()
        };
        assert!(a.validate(&policy, &scope, "root", now()).is_err());
        let calls = Arc::new(AtomicUsize::new(0));
        let mut worker = ConsultationWorker {
            authorization: a.clone(),
            root: "root".into(),
            child_suffix: "missing".into(),
            policy_path: policy.path().into(),
            backend: Some(Box::new(Fake(calls.clone()))),
            cancel: CancellationToken::default(),
            receiver: None,
            join: None,
            receipt: WorkerReceipt::default(),
            permission_root: None,
            memory_epoch: Some((policy.path().with_file_name("memory.sqlite"), 0)),
            dispatch_cancellation: Default::default(),
        };
        assert!(worker.start("bounded question", &a.scope).is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        policy.revoke_grant(&a.grant_id, now()).unwrap();
        assert!(a.validate(&policy, &a.scope, "root", now()).is_err());
    }
}
