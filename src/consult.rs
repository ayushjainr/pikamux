//! Provider-native private side conversations.
//!
//! Every provider receives an immutable parent identity and creates one separate
//! child for a bounded multi-turn exchange. Pika never types into, resumes, or
//! terminates the parent. Delivery and cleanup are reported independently.

use crate::model::{Provider, Session};
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use uuid::Uuid;

pub const DEFAULT_CODEX_MODEL: &str = "gpt-5.6-sol";
pub const DEFAULT_CODEX_EFFORT: &str = "medium";
pub const FAST_CODEX_MODEL: &str = "gpt-5.6-luna";
pub const FAST_CODEX_EFFORT: &str = "medium";
const MAX_FRAME: usize = 16 * 1024 * 1024;
const MAX_STDERR: usize = 64 * 1024;
const MAX_CODEX_NOTIFICATIONS: usize = 128;
const MAX_CODEX_NOTIFICATION_BYTES: usize = 2 * 1024 * 1024;
const MAX_ANSWER_BYTES: usize = 1024 * 1024;
pub const MAX_QUESTION_BYTES: usize = 64 * 1024;

/// Cooperative cancellation shared by the board and the exact Pika-owned
/// consultation children. Cancelling never signals the parent agent process.
#[derive(Clone, Debug, Default)]
pub struct CancellationToken(Arc<AtomicBool>);

impl CancellationToken {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ConsultationPolicy {
    pub mode: String,
    pub model: Option<String>,
    pub effort: Option<String>,
}

impl ConsultationPolicy {
    pub fn label(&self) -> String {
        match (&self.model, &self.effort) {
            (Some(model), Some(effort)) => format!("{model} · {effort}"),
            _ => "provider native".to_owned(),
        }
    }
}

pub fn consultation_policy(provider: Provider, fast: bool) -> Result<ConsultationPolicy> {
    match (provider, fast) {
        (Provider::Codex, false) => Ok(ConsultationPolicy {
            mode: "default".to_owned(),
            model: Some(DEFAULT_CODEX_MODEL.to_owned()),
            effort: Some(DEFAULT_CODEX_EFFORT.to_owned()),
        }),
        (Provider::Codex, true) => Ok(ConsultationPolicy {
            mode: "fast".to_owned(),
            model: Some(FAST_CODEX_MODEL.to_owned()),
            effort: Some(FAST_CODEX_EFFORT.to_owned()),
        }),
        (Provider::Claude, false) | (Provider::Opencode, false) => Ok(ConsultationPolicy {
            mode: "provider-native".to_owned(),
            model: None,
            effort: None,
        }),
        (Provider::Claude, true) => {
            bail!("fast consultations are not benchmarked for Claude; omit --fast")
        }
        (Provider::Opencode, true) => {
            bail!("fast consultations are not benchmarked for OpenCode; omit --fast")
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConsultationStage {
    Prepare,
    Turn,
    Response,
    Cleanup,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    NotSent,
    Unknown,
    Confirmed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Cleanup {
    Pending,
    Complete,
    Failed,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ConsultationReceipt {
    pub receipt_version: u8,
    pub stage: ConsultationStage,
    pub elapsed_seconds: f64,
    pub stage_elapsed_seconds: f64,
    pub turn: u32,
    pub delivery: Delivery,
    pub cleanup: Cleanup,
    pub answers_received: u32,
    pub stage_durations_seconds: BTreeMap<String, f64>,
    pub retry_safe: bool,
    pub parent_transcript_unchanged: Option<bool>,
    pub parent_transcript_verification: &'static str,
    pub consultation_mode: String,
    pub model: Option<String>,
    pub effort: Option<String>,
}

#[derive(Debug)]
pub struct ConsultationError {
    message: String,
    pub receipt: Box<ConsultationReceipt>,
    pub cleanup_error: Option<String>,
}

impl ConsultationError {
    fn new(message: impl Into<String>, receipt: ConsultationReceipt) -> Self {
        Self {
            message: message.into(),
            receipt: Box::new(receipt),
            cleanup_error: None,
        }
    }
}

impl fmt::Display for ConsultationError {
    fn fmt(&self, output: &mut fmt::Formatter<'_>) -> fmt::Result {
        output.write_str(&self.message)
    }
}

impl std::error::Error for ConsultationError {}

#[derive(Clone, Debug)]
pub struct ConsultationOptions {
    pub executable: PathBuf,
    pub opencode_database: Option<PathBuf>,
    pub timeout: Duration,
    pub fast: bool,
    pub cancellation: CancellationToken,
}

impl ConsultationOptions {
    pub fn new(executable: impl Into<PathBuf>) -> Self {
        Self {
            executable: executable.into(),
            opencode_database: None,
            timeout: Duration::from_secs(900),
            fast: false,
            cancellation: CancellationToken::default(),
        }
    }
}

pub struct Consultation {
    side: Side,
    policy: ConsultationPolicy,
    started: Instant,
    stage_started: Instant,
    stage: ConsultationStage,
    delivery: Delivery,
    cleanup: Cleanup,
    turn: u32,
    answers_received: u32,
    stage_durations: BTreeMap<String, Duration>,
    close_attempted: bool,
}

impl Consultation {
    pub fn open(
        session: &Session,
        options: ConsultationOptions,
    ) -> Result<Self, ConsultationError> {
        let policy = consultation_policy(session.provider, options.fast).map_err(|error| {
            let policy = ConsultationPolicy {
                mode: "invalid".to_owned(),
                model: None,
                effort: None,
            };
            ConsultationError::new(error.to_string(), initial_receipt(&policy))
        })?;
        let started = Instant::now();
        let result = match session.provider {
            Provider::Codex => CodexSide::open(session, &options, &policy).map(Side::Codex),
            Provider::Claude => ClaudeSide::open(session, &options).map(Side::Claude),
            Provider::Opencode => OpenCodeSide::open(session, &options).map(Side::Opencode),
        };
        let side = match result {
            Ok(side) => side,
            Err(failure) => {
                let mut receipt = initial_receipt(&policy);
                receipt.elapsed_seconds = rounded(started.elapsed());
                receipt.stage_elapsed_seconds = receipt.elapsed_seconds;
                receipt.cleanup = failure.cleanup;
                receipt.retry_safe = failure.delivery == Delivery::NotSent
                    && !matches!(failure.cleanup, Cleanup::Failed | Cleanup::Unknown);
                return Err(ConsultationError::new(failure.message, receipt));
            }
        };
        Ok(Self {
            side,
            policy,
            started,
            stage_started: Instant::now(),
            stage: ConsultationStage::Prepare,
            delivery: Delivery::NotSent,
            cleanup: Cleanup::Pending,
            turn: 0,
            answers_received: 0,
            stage_durations: BTreeMap::new(),
            close_attempted: false,
        })
    }

    pub fn policy(&self) -> &ConsultationPolicy {
        &self.policy
    }

    pub fn parent_id(&self) -> &str {
        self.side.parent_id()
    }

    pub fn child_id(&self) -> Option<&str> {
        self.side.child_id()
    }

    pub fn receipt(&self) -> ConsultationReceipt {
        let mut stage_durations = self.stage_durations.clone();
        *stage_durations
            .entry(stage_name(self.stage).to_owned())
            .or_default() += self.stage_started.elapsed();
        ConsultationReceipt {
            receipt_version: 2,
            stage: self.stage,
            elapsed_seconds: rounded(self.started.elapsed()),
            stage_elapsed_seconds: rounded(self.stage_started.elapsed()),
            turn: self.turn,
            delivery: self.delivery,
            cleanup: self.cleanup,
            answers_received: self.answers_received,
            stage_durations_seconds: stage_durations
                .into_iter()
                .map(|(stage, duration)| (stage, rounded(duration)))
                .collect(),
            retry_safe: self.delivery == Delivery::NotSent
                && matches!(
                    self.stage,
                    ConsultationStage::Prepare | ConsultationStage::Turn
                )
                && !matches!(self.cleanup, Cleanup::Failed | Cleanup::Unknown),
            parent_transcript_unchanged: None,
            parent_transcript_verification: "not_performed",
            consultation_mode: self.policy.mode.clone(),
            model: self.policy.model.clone(),
            effort: self.policy.effort.clone(),
        }
    }

    pub fn ask(&mut self, question: &str) -> Result<String, ConsultationError> {
        self.turn += 1;
        self.set_stage(ConsultationStage::Turn);
        self.delivery = Delivery::NotSent;
        if self.close_attempted {
            return Err(self.failure("side consultation is closed"));
        }
        let question = question.trim();
        if question.is_empty() {
            return Err(self.failure("question cannot be empty"));
        }
        if question.len() > MAX_QUESTION_BYTES {
            return Err(self.failure("question exceeded the 64 KiB safety limit"));
        }
        match self.side.ask(question, &mut self.delivery) {
            Ok(answer) if !answer.trim().is_empty() => {
                self.delivery = Delivery::Confirmed;
                self.answers_received += 1;
                self.set_stage(ConsultationStage::Response);
                Ok(answer.trim().to_owned())
            }
            Ok(_) => Err(self.failure("provider side turn returned no answer")),
            Err(failure) => {
                self.delivery = failure.delivery;
                if failure.cleanup != Cleanup::Pending {
                    self.cleanup = failure.cleanup;
                }
                Err(self.failure(failure.message))
            }
        }
    }

    pub fn close(&mut self) -> Result<(), ConsultationError> {
        if self.close_attempted {
            return if self.cleanup == Cleanup::Complete {
                Ok(())
            } else {
                Err(self.failure("side cleanup was not confirmed; do not resend the question"))
            };
        }
        self.close_attempted = true;
        self.set_stage(ConsultationStage::Cleanup);
        match self.side.close() {
            Ok(()) => {
                self.cleanup = Cleanup::Complete;
                Ok(())
            }
            Err(error) => {
                self.cleanup = Cleanup::Failed;
                let mut result = self.failure(error.to_string());
                result.cleanup_error = Some(error.to_string());
                Err(result)
            }
        }
    }

    fn set_stage(&mut self, stage: ConsultationStage) {
        if stage != self.stage {
            *self
                .stage_durations
                .entry(stage_name(self.stage).to_owned())
                .or_default() += self.stage_started.elapsed();
            self.stage = stage;
            self.stage_started = Instant::now();
        }
    }

    fn failure(&self, message: impl Into<String>) -> ConsultationError {
        ConsultationError::new(message, self.receipt())
    }
}

impl Drop for Consultation {
    fn drop(&mut self) {
        if !self.close_attempted {
            let _ = self.close();
        }
    }
}

fn initial_receipt(policy: &ConsultationPolicy) -> ConsultationReceipt {
    ConsultationReceipt {
        receipt_version: 2,
        stage: ConsultationStage::Prepare,
        elapsed_seconds: 0.0,
        stage_elapsed_seconds: 0.0,
        turn: 0,
        delivery: Delivery::NotSent,
        cleanup: Cleanup::Pending,
        answers_received: 0,
        stage_durations_seconds: BTreeMap::from([("prepare".to_owned(), 0.0)]),
        retry_safe: false,
        parent_transcript_unchanged: None,
        parent_transcript_verification: "not_performed",
        consultation_mode: policy.mode.clone(),
        model: policy.model.clone(),
        effort: policy.effort.clone(),
    }
}

fn stage_name(stage: ConsultationStage) -> &'static str {
    match stage {
        ConsultationStage::Prepare => "prepare",
        ConsultationStage::Turn => "turn",
        ConsultationStage::Response => "response",
        ConsultationStage::Cleanup => "cleanup",
    }
}

fn rounded(duration: Duration) -> f64 {
    (duration.as_secs_f64() * 1000.0).round() / 1000.0
}

struct SideFailure {
    message: String,
    delivery: Delivery,
    cleanup: Cleanup,
}

impl SideFailure {
    fn prepare(error: impl fmt::Display, cleanup: Cleanup) -> Self {
        Self {
            message: error.to_string(),
            delivery: Delivery::NotSent,
            cleanup,
        }
    }

    fn turn(error: impl fmt::Display, delivery: Delivery) -> Self {
        Self {
            message: error.to_string(),
            delivery,
            cleanup: Cleanup::Pending,
        }
    }

    fn turn_with_cleanup(error: impl fmt::Display, delivery: Delivery, cleanup: Cleanup) -> Self {
        Self {
            message: error.to_string(),
            delivery,
            cleanup,
        }
    }
}

enum Side {
    Codex(CodexSide),
    Claude(ClaudeSide),
    Opencode(OpenCodeSide),
}

impl Side {
    fn ask(&mut self, question: &str, delivery: &mut Delivery) -> Result<String, SideFailure> {
        match self {
            Self::Codex(side) => side.ask(question, delivery),
            Self::Claude(side) => side.ask(question, delivery),
            Self::Opencode(side) => side.ask(question, delivery),
        }
    }

    fn close(&mut self) -> Result<()> {
        match self {
            Self::Codex(side) => side.close(),
            Self::Claude(side) => side.close(),
            Self::Opencode(side) => side.close(),
        }
    }

    fn parent_id(&self) -> &str {
        match self {
            Self::Codex(side) => &side.parent_id,
            Self::Claude(side) => &side.parent_id,
            Self::Opencode(side) => &side.parent_id,
        }
    }

    fn child_id(&self) -> Option<&str> {
        match self {
            Self::Codex(side) => (!side.thread_id.is_empty()).then_some(side.thread_id.as_str()),
            Self::Claude(_) => None,
            Self::Opencode(side) => side.thread_id.as_deref(),
        }
    }
}

struct JsonChild {
    child: OwnedChild,
    writes: mpsc::Sender<WriteRequest>,
    frames: Receiver<Result<Value, String>>,
    stderr: Arc<Mutex<Vec<u8>>>,
}

struct WriteRequest {
    bytes: Vec<u8>,
    result: SyncSender<std::result::Result<(), String>>,
}

impl JsonChild {
    fn spawn(mut command: Command) -> Result<Self> {
        command
            .env("PIKA_EPHEMERAL", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = OwnedChild::spawn(&mut command)
            .context("could not start provider side consultation")?;
        let stdin = child
            .stdin
            .take()
            .context("provider stdin was unavailable")?;
        let stdout = child
            .stdout
            .take()
            .context("provider stdout was unavailable")?;
        let stderr_pipe = child
            .stderr
            .take()
            .context("provider stderr was unavailable")?;
        let (sender, frames) = mpsc::sync_channel(1);
        let _ = thread::spawn(move || read_json_frames(stdout, sender));
        let stderr = Arc::new(Mutex::new(Vec::new()));
        let stderr_copy = Arc::clone(&stderr);
        let _ = thread::spawn(move || drain_bounded(stderr_pipe, stderr_copy));
        let (writes, write_requests) = mpsc::channel();
        let _ = thread::spawn(move || write_requests_loop(stdin, write_requests));
        Ok(Self {
            child,
            writes,
            frames,
            stderr,
        })
    }

    fn send(
        &mut self,
        payload: &Value,
        deadline: Instant,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        let mut bytes = serde_json::to_vec(payload)?;
        bytes.push(b'\n');
        if bytes.len() > MAX_FRAME {
            bail!("provider side request exceeded the 16 MiB safety limit");
        }
        let (sender, result) = mpsc::sync_channel(1);
        self.writes
            .send(WriteRequest {
                bytes,
                result: sender,
            })
            .context("provider input writer stopped")?;
        loop {
            if cancellation.is_cancelled() {
                let _ = self.terminate();
                bail!("provider side consultation cancelled");
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                let _ = self.terminate();
                bail!("provider side consultation timed out while sending input");
            }
            match result.recv_timeout(remaining.min(Duration::from_millis(25))) {
                Ok(Ok(())) => return Ok(()),
                Ok(Err(error)) => {
                    let _ = self.terminate();
                    bail!("provider input write failed: {error}")
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    let _ = self.terminate();
                    bail!("provider input writer stopped")
                }
            }
        }
    }

    fn receive(&mut self, deadline: Instant, cancellation: &CancellationToken) -> Result<Value> {
        loop {
            if cancellation.is_cancelled() {
                let _ = self.terminate();
                bail!("provider side consultation cancelled");
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                let _ = self.terminate();
                bail!("provider side consultation timed out");
            }
            match self
                .frames
                .recv_timeout(remaining.min(Duration::from_millis(25)))
            {
                Ok(Ok(value)) => return Ok(value),
                Ok(Err(message)) => bail!(message),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    let status = poll_owned_child(&mut self.child)?.map(|value| value.to_string());
                    let detail = self.stderr_tail();
                    bail!(
                        "provider side consultation exited with status {}{}",
                        status.as_deref().unwrap_or("unknown"),
                        if detail.is_empty() {
                            String::new()
                        } else {
                            format!(": {detail}")
                        }
                    )
                }
            }
        }
    }

    fn stderr_tail(&self) -> String {
        let value = self
            .stderr
            .lock()
            .map(|value| value.clone())
            .unwrap_or_default();
        String::from_utf8_lossy(&value).trim().to_owned()
    }

    fn terminate(&mut self) -> Result<()> {
        terminate_child(&mut self.child)
    }
}

fn write_requests_loop(mut stdin: ChildStdin, requests: Receiver<WriteRequest>) {
    for request in requests {
        let result = stdin
            .write_all(&request.bytes)
            .and_then(|_| stdin.flush())
            .map_err(|error| error.to_string());
        let failed = result.is_err();
        let _ = request.result.send(result);
        if failed {
            return;
        }
    }
}

impl Drop for JsonChild {
    fn drop(&mut self) {
        let _ = self.terminate();
    }
}

fn read_json_frames(reader: impl Read, sender: SyncSender<Result<Value, String>>) {
    let mut reader = reader;
    let mut pending = Vec::new();
    let mut chunk = [0_u8; 64 * 1024];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => {
                while matches!(pending.last(), Some(b'\n' | b'\r')) {
                    pending.pop();
                }
                if !pending.is_empty()
                    && pending.len() <= MAX_FRAME
                    && let Ok(value) = serde_json::from_slice::<Value>(&pending)
                    && value.is_object()
                {
                    let _ = sender.send(Ok(value));
                }
                return;
            }
            Ok(size) => {
                pending.extend_from_slice(&chunk[..size]);
                while let Some(newline) = pending.iter().position(|byte| *byte == b'\n') {
                    if newline > MAX_FRAME {
                        let _ = sender.send(Err(
                            "provider side response exceeded the 16 MiB frame limit".to_owned(),
                        ));
                        return;
                    }
                    let mut bytes = pending.drain(..=newline).collect::<Vec<_>>();
                    while matches!(bytes.last(), Some(b'\n' | b'\r')) {
                        bytes.pop();
                    }
                    if let Ok(value) = serde_json::from_slice::<Value>(&bytes)
                        && value.is_object()
                        && sender.send(Ok(value)).is_err()
                    {
                        return;
                    }
                }
                if pending.len() > MAX_FRAME {
                    let _ = sender.send(Err(
                        "provider side response exceeded the 16 MiB frame limit".to_owned(),
                    ));
                    return;
                }
            }
            Err(error) => {
                let _ = sender.send(Err(format!("provider output read failed: {error}")));
                return;
            }
        }
    }
}

fn drain_bounded(mut reader: impl Read, output: Arc<Mutex<Vec<u8>>>) {
    let mut chunk = [0_u8; 8192];
    loop {
        let Ok(size) = reader.read(&mut chunk) else {
            return;
        };
        if size == 0 {
            return;
        }
        let Ok(mut value) = output.lock() else { return };
        value.extend_from_slice(&chunk[..size]);
        if value.len() > MAX_STDERR {
            let excess = value.len() - MAX_STDERR;
            value.drain(..excess);
        }
    }
}

struct CodexSide {
    parent_id: String,
    thread_id: String,
    process: JsonChild,
    request_id: u64,
    notifications: VecDeque<Value>,
    notification_bytes: usize,
    policy: ConsultationPolicy,
    timeout: Duration,
    cancellation: CancellationToken,
}

impl CodexSide {
    fn open(
        session: &Session,
        options: &ConsultationOptions,
        policy: &ConsultationPolicy,
    ) -> Result<Self, SideFailure> {
        let parent_id = session.provider_thread_id().to_owned();
        let mut command = Command::new(&options.executable);
        command.args(["app-server", "--stdio"]);
        set_cwd(&mut command, session.cwd.as_deref());
        let process = JsonChild::spawn(command)
            .map_err(|error| SideFailure::prepare(error, Cleanup::Complete))?;
        let mut side = Self {
            parent_id,
            thread_id: String::new(),
            process,
            request_id: 0,
            notifications: VecDeque::new(),
            notification_bytes: 0,
            policy: policy.clone(),
            timeout: options.timeout,
            cancellation: options.cancellation.clone(),
        };
        let opened = (|| -> Result<()> {
            side.request(
                "initialize",
                json!({
                    "clientInfo": {"name":"pikamux","title":"Pika side consultation","version":crate::VERSION},
                    "capabilities": {"experimentalApi":true}
                }),
                Duration::from_secs(15),
            )?;
            side.process.send(
                &json!({"method":"initialized"}),
                Instant::now() + Duration::from_secs(15),
                &side.cancellation,
            )?;
            let mut fork = json!({
                "threadId": side.parent_id,
                "ephemeral": true,
                "excludeTurns": true,
                "approvalPolicy": "never",
                "sandbox": "read-only",
                "developerInstructions": "This is an ephemeral side consultation. Answer from inherited conversation context without modifying files or external state. If tools would be required, explain what needs checking instead. Separate dated historical work from later current state."
            });
            fork["model"] = Value::String(policy.model.clone().unwrap_or_default());
            fork["config"] = json!({"model_reasoning_effort": policy.effort});
            let result = side.request("thread/fork", fork, Duration::from_secs(180))?;
            let thread = result.get("thread").and_then(Value::as_object);
            let id = thread
                .and_then(|value| value.get("id"))
                .and_then(Value::as_str);
            let ephemeral = thread
                .and_then(|value| value.get("ephemeral"))
                .and_then(Value::as_bool);
            if id.is_none() || ephemeral != Some(true) {
                bail!("Codex did not confirm an ephemeral fork; refusing to continue");
            }
            let observed_model = result.get("model").and_then(Value::as_str);
            let observed_effort = result.get("reasoningEffort").and_then(Value::as_str);
            if observed_model != policy.model.as_deref()
                || observed_effort != policy.effort.as_deref()
            {
                bail!(
                    "Codex did not confirm the requested consultation profile; requested {}, observed {} · {}",
                    policy.label(),
                    observed_model.unwrap_or("unknown"),
                    observed_effort.unwrap_or("unknown")
                );
            }
            side.thread_id = id.unwrap_or_default().to_owned();
            side.notifications.clear();
            side.notification_bytes = 0;
            Ok(())
        })();
        if let Err(error) = opened {
            let cleanup = if side.process.terminate().is_ok() {
                Cleanup::Complete
            } else {
                Cleanup::Failed
            };
            return Err(SideFailure::prepare(error, cleanup));
        }
        Ok(side)
    }

    fn request(&mut self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        self.request_id += 1;
        let request_id = self.request_id;
        let deadline = Instant::now() + timeout;
        self.process.send(
            &json!({"method":method,"id":request_id,"params":params}),
            deadline,
            &self.cancellation,
        )?;
        loop {
            let message = self.process.receive(deadline, &self.cancellation)?;
            if message.get("id").and_then(Value::as_u64) == Some(request_id)
                && message.get("method").is_none()
            {
                if let Some(error) = message.get("error") {
                    let detail = error
                        .get("message")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .unwrap_or_else(|| error.to_string());
                    bail!("Codex {method} failed: {detail}");
                }
                return Ok(message.get("result").cloned().unwrap_or_else(|| json!({})));
            }
            if message.get("id").is_some() && message.get("method").is_some() {
                self.process.send(
                    &json!({
                        "id": message["id"],
                        "error": {"code":-32000,"message":"Pika side consultations are non-interactive"}
                    }),
                    deadline,
                    &self.cancellation,
                )?;
            } else if message.get("method").is_some() {
                self.retain_notification(message)?;
            }
        }
    }

    fn retain_notification(&mut self, message: Value) -> Result<()> {
        let bytes = serde_json::to_vec(&message)?.len();
        if self.notifications.len() >= MAX_CODEX_NOTIFICATIONS
            || bytes > MAX_CODEX_NOTIFICATION_BYTES
            || self.notification_bytes.saturating_add(bytes) > MAX_CODEX_NOTIFICATION_BYTES
        {
            bail!(
                "Codex side notification backlog exceeded its 2 MiB/128-message limit; delivery state is preserved and the side will be cleaned up"
            );
        }
        self.notification_bytes += bytes;
        self.notifications.push_back(message);
        Ok(())
    }

    fn pop_notification(&mut self) -> Option<Value> {
        let value = self.notifications.pop_front()?;
        self.notification_bytes = self
            .notification_bytes
            .saturating_sub(serde_json::to_vec(&value).map_or(0, |bytes| bytes.len()));
        Some(value)
    }

    fn ask(&mut self, question: &str, delivery: &mut Delivery) -> Result<String, SideFailure> {
        self.notifications.clear();
        self.notification_bytes = 0;
        let mut params = json!({
            "threadId": self.thread_id,
            "input": [{"type":"text","text":question}],
            "model": self.policy.model,
            "effort": self.policy.effort
        });
        // Keep the policy explicit even if provider JSON implementations reorder keys.
        params["model"] = Value::String(self.policy.model.clone().unwrap_or_default());
        params["effort"] = Value::String(self.policy.effort.clone().unwrap_or_default());
        *delivery = Delivery::Unknown;
        let result = self
            .request("turn/start", params, Duration::from_secs(60))
            .map_err(|error| SideFailure::turn(error, *delivery))?;
        let Some(turn_id) = result
            .get("turn")
            .and_then(|value| value.get("id"))
            .and_then(Value::as_str)
            .map(str::to_owned)
        else {
            return Err(SideFailure::turn(
                "Codex did not start the side turn",
                *delivery,
            ));
        };
        *delivery = Delivery::Confirmed;
        let deadline = Instant::now() + self.timeout;
        let mut final_text = String::new();
        let mut deltas = String::new();
        loop {
            let message = if let Some(message) = self.pop_notification() {
                Ok(message)
            } else {
                self.process.receive(deadline, &self.cancellation)
            }
            .map_err(|error| SideFailure::turn(error, *delivery))?;
            if message.get("id").is_some() && message.get("method").is_some() {
                self.process
                    .send(
                        &json!({
                            "id":message["id"],
                            "error":{"code":-32000,"message":"Pika side consultations are non-interactive"}
                        }),
                        deadline,
                        &self.cancellation,
                    )
                    .map_err(|error| SideFailure::turn(error, *delivery))?;
                continue;
            }
            let method = message.get("method").and_then(Value::as_str);
            let Some(params) = message.get("params") else {
                continue;
            };
            if params.get("threadId").and_then(Value::as_str) != Some(&self.thread_id) {
                continue;
            }
            match method {
                Some("item/agentMessage/delta")
                    if params.get("turnId").and_then(Value::as_str) == Some(&turn_id) =>
                {
                    append_answer(
                        &mut deltas,
                        params.get("delta").and_then(Value::as_str).unwrap_or(""),
                    )
                    .map_err(|error| SideFailure::turn(error, *delivery))?;
                }
                Some("item/completed")
                    if params.get("turnId").and_then(Value::as_str) == Some(&turn_id) =>
                {
                    let item = params.get("item");
                    if item
                        .and_then(|value| value.get("type"))
                        .and_then(Value::as_str)
                        == Some("agentMessage")
                    {
                        if let Some(text) = item
                            .and_then(|value| value.get("text"))
                            .and_then(Value::as_str)
                        {
                            if text.len() > MAX_ANSWER_BYTES {
                                return Err(SideFailure::turn(
                                    "Codex side answer exceeded the 1 MiB retention limit; partial answer withheld",
                                    *delivery,
                                ));
                            }
                            final_text = text.to_owned();
                        }
                        let phase = item
                            .and_then(|value| value.get("phase"))
                            .and_then(Value::as_str);
                        if !final_text.trim().is_empty()
                            && matches!(phase, None | Some("final_answer"))
                        {
                            return Ok(final_text);
                        }
                    }
                }
                Some("error") if params.get("turnId").and_then(Value::as_str) == Some(&turn_id) => {
                    let detail = params
                        .get("error")
                        .and_then(|value| value.get("message"))
                        .and_then(Value::as_str)
                        .unwrap_or("unknown error");
                    return Err(SideFailure::turn(
                        format!("Codex side turn failed: {detail}"),
                        *delivery,
                    ));
                }
                Some("turn/completed") => {
                    let turn = params.get("turn");
                    if turn
                        .and_then(|value| value.get("id"))
                        .and_then(Value::as_str)
                        != Some(&turn_id)
                    {
                        continue;
                    }
                    let status = turn
                        .and_then(|value| value.get("status"))
                        .and_then(Value::as_str)
                        .unwrap_or("unknown");
                    if status != "completed" {
                        return Err(SideFailure::turn(
                            format!("Codex side turn ended with status {status}"),
                            *delivery,
                        ));
                    }
                    let answer = if final_text.is_empty() {
                        deltas
                    } else {
                        final_text
                    };
                    return Ok(answer);
                }
                Some("thread/status/changed")
                    if !final_text.is_empty()
                        && params
                            .get("status")
                            .and_then(|value| value.get("type"))
                            .and_then(Value::as_str)
                            == Some("idle") =>
                {
                    return Ok(final_text);
                }
                _ => {}
            }
        }
    }

    fn close(&mut self) -> Result<()> {
        self.process.terminate()?;
        self.thread_id.clear();
        Ok(())
    }
}

fn append_answer(target: &mut String, delta: &str) -> Result<()> {
    if target.len().saturating_add(delta.len()) > MAX_ANSWER_BYTES {
        bail!("Codex side answer exceeded the 1 MiB retention limit; partial answer withheld");
    }
    target.push_str(delta);
    Ok(())
}

struct ClaudeSide {
    parent_id: String,
    process: JsonChild,
    timeout: Duration,
    cancellation: CancellationToken,
}

impl ClaudeSide {
    fn open(session: &Session, options: &ConsultationOptions) -> Result<Self, SideFailure> {
        let version = run_output_bounded_cancellable(
            &options.executable,
            &["--version"],
            Duration::from_secs(5),
            &options.cancellation,
        )
        .map_err(|error| SideFailure::prepare(error, Cleanup::Complete))?;
        let found = version_tuple(&version);
        if found < vec![2, 1, 228] {
            return Err(SideFailure::prepare(
                format!(
                    "Claude side consultations require tested capability 2.1.228+; found {}",
                    if found.is_empty() {
                        "unknown".to_owned()
                    } else {
                        found
                            .iter()
                            .map(u64::to_string)
                            .collect::<Vec<_>>()
                            .join(".")
                    }
                ),
                Cleanup::Complete,
            ));
        }
        let parent_id = session.provider_thread_id().to_owned();
        let mut command = Command::new(&options.executable);
        command.args([
            "-p",
            "--resume",
            &parent_id,
            "--fork-session",
            "--no-session-persistence",
            "--tools",
            "",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "--verbose",
        ]);
        set_cwd(&mut command, session.cwd.as_deref());
        let process = JsonChild::spawn(command)
            .map_err(|error| SideFailure::prepare(error, Cleanup::Complete))?;
        Ok(Self {
            parent_id,
            process,
            timeout: options.timeout,
            cancellation: options.cancellation.clone(),
        })
    }

    fn ask(&mut self, question: &str, delivery: &mut Delivery) -> Result<String, SideFailure> {
        *delivery = Delivery::Unknown;
        let deadline = Instant::now() + self.timeout;
        self.process
            .send(
                &json!({
                    "type":"user",
                    "message":{"role":"user","content":[{"type":"text","text":question}]}
                }),
                deadline,
                &self.cancellation,
            )
            .map_err(|error| SideFailure::turn(error, *delivery))?;
        let mut latest = String::new();
        loop {
            let message = self
                .process
                .receive(deadline, &self.cancellation)
                .map_err(|error| SideFailure::turn(error, *delivery))?;
            match message.get("type").and_then(Value::as_str) {
                Some("assistant") => {
                    *delivery = Delivery::Confirmed;
                    if let Some(content) = message
                        .get("message")
                        .and_then(|value| value.get("content"))
                        .and_then(Value::as_array)
                    {
                        let answer = content
                            .iter()
                            .filter(|item| item.get("type").and_then(Value::as_str) == Some("text"))
                            .filter_map(|item| item.get("text").and_then(Value::as_str))
                            .collect::<String>();
                        if !answer.trim().is_empty() {
                            latest = answer;
                        }
                    }
                }
                Some("result") => {
                    *delivery = Delivery::Confirmed;
                    if message.get("subtype").and_then(Value::as_str) != Some("success")
                        || message.get("is_error").and_then(Value::as_bool) == Some(true)
                    {
                        return Err(SideFailure::turn(
                            format!(
                                "Claude side turn failed: {}",
                                message
                                    .get("result")
                                    .map(Value::to_string)
                                    .unwrap_or_else(|| message.to_string())
                            ),
                            *delivery,
                        ));
                    }
                    let answer = message
                        .get("result")
                        .and_then(Value::as_str)
                        .unwrap_or(&latest)
                        .trim()
                        .to_owned();
                    return Ok(answer);
                }
                _ => {}
            }
        }
    }

    fn close(&mut self) -> Result<()> {
        self.process.terminate()
    }
}

struct OpenCodeSide {
    parent_id: String,
    cwd: Option<PathBuf>,
    executable: PathBuf,
    database: PathBuf,
    model: Option<String>,
    timeout: Duration,
    thread_id: Option<String>,
    fork_uncertain: bool,
    turn_process: Option<OwnedChild>,
    turn_stderr: Option<Arc<Mutex<Vec<u8>>>>,
    fork_server: Option<OwnedChild>,
    closed: bool,
    cancellation: CancellationToken,
}

impl OpenCodeSide {
    fn open(session: &Session, options: &ConsultationOptions) -> Result<Self, SideFailure> {
        let Some(database) = options.opencode_database.clone() else {
            return Err(SideFailure::prepare(
                "OpenCode consultation requires its provider database path",
                Cleanup::Complete,
            ));
        };
        let version = run_output_bounded_cancellable(
            &options.executable,
            &["--version"],
            Duration::from_secs(5),
            &options.cancellation,
        )
        .map_err(|error| SideFailure::prepare(error, Cleanup::Complete))?;
        if version_tuple(&version) < vec![1, 18, 21] {
            return Err(SideFailure::prepare(
                "OpenCode side consultations require opencode >= 1.18.21",
                Cleanup::Complete,
            ));
        }
        Ok(Self {
            parent_id: session.provider_thread_id().to_owned(),
            cwd: valid_cwd(session.cwd.as_deref()).map(Path::to_owned),
            executable: options.executable.clone(),
            database,
            model: session.model.clone(),
            timeout: options.timeout,
            thread_id: None,
            fork_uncertain: false,
            turn_process: None,
            turn_stderr: None,
            fork_server: None,
            closed: false,
            cancellation: options.cancellation.clone(),
        })
    }

    fn fork(&mut self) -> Result<String> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let port = listener.local_addr()?.port();
        drop(listener);
        let username = "opencode";
        let password = Uuid::new_v4().simple().to_string();
        let mut command = Command::new(&self.executable);
        command.args([
            "--pure",
            "serve",
            "--hostname",
            "127.0.0.1",
            "--port",
            &port.to_string(),
        ]);
        command
            .env("PIKA_EPHEMERAL", "1")
            .env("OPENCODE_CONFIG_CONTENT", opencode_readonly_config())
            .env("OPENCODE_SERVER_USERNAME", username)
            .env("OPENCODE_SERVER_PASSWORD", &password)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(cwd) = &self.cwd {
            command.current_dir(cwd);
        }
        let server =
            OwnedChild::spawn(&mut command).context("OpenCode side-session server failed")?;
        self.fork_server = Some(server);
        let base = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
        let health_deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if self.cancellation.is_cancelled() {
                if let Some(server) = &mut self.fork_server {
                    let _ = terminate_child(server);
                }
                self.fork_server = None;
                bail!("OpenCode side consultation cancelled");
            }
            if let Some(status) = poll_owned_child(
                self.fork_server
                    .as_mut()
                    .context("OpenCode fork server handle was lost")?,
            )? {
                bail!("OpenCode side-session server exited before becoming ready: {status}");
            }
            if http_json(
                base,
                HttpRequest {
                    path: "/global/health",
                    username,
                    password: &password,
                    method: "GET",
                    payload: None,
                    timeout: Duration::from_millis(500),
                },
                &self.cancellation,
            )
            .is_ok()
            {
                break;
            }
            if Instant::now() >= health_deadline {
                if let Some(server) = &mut self.fork_server
                    && terminate_child(server).is_ok()
                {
                    self.fork_server = None;
                }
                bail!("OpenCode side-session server did not become ready");
            }
            thread::sleep(Duration::from_millis(50));
        }
        let directory = self
            .cwd
            .as_ref()
            .map(|path| format!("?directory={}", percent_encode(&path.to_string_lossy())))
            .unwrap_or_default();
        let endpoint = format!(
            "/session/{}/fork{directory}",
            percent_encode(&self.parent_id)
        );
        self.fork_uncertain = true;
        let response = http_json(
            base,
            HttpRequest {
                path: &endpoint,
                username,
                password: &password,
                method: "POST",
                payload: Some(b"{}"),
                timeout: Duration::from_secs(30),
            },
            &self.cancellation,
        );
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                if let Some(server) = &mut self.fork_server
                    && terminate_child(server).is_ok()
                {
                    self.fork_server = None;
                }
                return Err(error);
            }
        };
        let id = response
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if valid_opencode_id(id) && id != self.parent_id {
            self.thread_id = Some(id.to_owned());
            self.fork_uncertain = false;
        }
        if let Some(server) = &mut self.fork_server {
            terminate_child(server)?;
        }
        self.fork_server = None;
        if !valid_opencode_id(id) {
            bail!("OpenCode did not return a valid provider-issued fork identity");
        }
        if id == self.parent_id {
            bail!("OpenCode did not fork the parent consultation");
        }
        if let Some(cwd) = &self.cwd
            && response.get("directory").and_then(Value::as_str)
                != Some(cwd.to_string_lossy().as_ref())
        {
            bail!("OpenCode forked the parent in an unexpected working directory");
        }
        Ok(id.to_owned())
    }

    fn ask(&mut self, question: &str, delivery: &mut Delivery) -> Result<String, SideFailure> {
        if self.closed {
            return Err(SideFailure::turn(
                "OpenCode side consultation is closed",
                *delivery,
            ));
        }
        if self.thread_id.is_none() {
            *delivery = Delivery::NotSent;
            if let Err(error) = self.fork() {
                return Err(SideFailure::turn_with_cleanup(
                    error,
                    Delivery::NotSent,
                    if self.fork_uncertain {
                        Cleanup::Unknown
                    } else {
                        Cleanup::Pending
                    },
                ));
            }
        }
        let target = self
            .thread_id
            .clone()
            .expect("fork established exact child");
        let checkpoint = self
            .latest_message_time(&target)
            .map_err(|error| SideFailure::turn(error, Delivery::NotSent))?;
        let mut command = Command::new(&self.executable);
        command.args([
            "--pure",
            "run",
            "--session",
            &target,
            "--format",
            "json",
            "--agent",
            "pika-readonly",
        ]);
        if let Some((model, variant)) = parse_opencode_model(self.model.as_deref()) {
            command.args(["--model", &model]);
            if let Some(variant) = variant {
                command.args(["--variant", &variant]);
            }
        }
        command.arg(format!(
            "This is an ephemeral, read-only Pika side consultation inherited from the parent conversation. Do not modify files or external state. Answer from context; if a mutating tool would be required, explain what needs checking instead.\n\n{question}"
        ));
        command
            .env("PIKA_EPHEMERAL", "1")
            .env("OPENCODE_CONFIG_CONTENT", opencode_readonly_config())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        if let Some(cwd) = &self.cwd {
            command.current_dir(cwd);
        }
        let mut child =
            OwnedChild::spawn(&mut command).map_err(|error| SideFailure::turn(error, *delivery))?;
        *delivery = Delivery::Unknown;
        let stderr = Arc::new(Mutex::new(Vec::new()));
        if let Some(pipe) = child.stderr.take() {
            let stderr_copy = Arc::clone(&stderr);
            let _ = thread::spawn(move || drain_bounded(pipe, stderr_copy));
        }
        self.turn_process = Some(child);
        self.turn_stderr = Some(stderr);
        let deadline = Instant::now() + self.timeout;
        loop {
            if self.cancellation.is_cancelled() {
                let mut terminated = true;
                if let Some(process) = &mut self.turn_process {
                    terminated = terminate_child(process).is_ok();
                }
                if terminated {
                    self.turn_process = None;
                    self.turn_stderr = None;
                }
                return Err(SideFailure::turn_with_cleanup(
                    "OpenCode side consultation cancelled",
                    *delivery,
                    if terminated {
                        Cleanup::Pending
                    } else {
                        Cleanup::Failed
                    },
                ));
            }
            match self.completed_answer(&target, checkpoint) {
                Ok((_seen_user, Some(answer))) => {
                    *delivery = Delivery::Confirmed;
                    let mut terminated = true;
                    if let Some(process) = &mut self.turn_process {
                        terminated = terminate_child(process).is_ok();
                    }
                    if terminated {
                        self.turn_process = None;
                        self.turn_stderr = None;
                    }
                    return Ok(answer);
                }
                Ok((true, None)) => *delivery = Delivery::Confirmed,
                Ok((false, None)) => {}
                Err(_) => {}
            }
            let exited = self
                .turn_process
                .as_mut()
                .and_then(|process| poll_owned_child(process).ok().flatten());
            if exited.is_some() {
                if let Ok((seen_user, Some(answer))) = self.completed_answer(&target, checkpoint) {
                    if seen_user {
                        *delivery = Delivery::Confirmed;
                    }
                    self.turn_process = None;
                    return Ok(answer);
                }
                let detail = self
                    .turn_stderr
                    .as_ref()
                    .map(stderr_tail)
                    .unwrap_or_default();
                self.turn_process = None;
                return Err(SideFailure::turn(
                    format!(
                        "OpenCode side turn exited before a completed answer{}",
                        if detail.is_empty() {
                            String::new()
                        } else {
                            format!(": {detail}")
                        }
                    ),
                    *delivery,
                ));
            }
            if Instant::now() >= deadline {
                let mut terminated = true;
                if let Some(process) = &mut self.turn_process {
                    terminated = terminate_child(process).is_ok();
                }
                if terminated {
                    self.turn_process = None;
                    self.turn_stderr = None;
                }
                return Err(SideFailure::turn(
                    "OpenCode side consultation timed out",
                    *delivery,
                ));
            }
            thread::sleep(Duration::from_millis(100));
        }
    }

    fn latest_message_time(&self, session_id: &str) -> Result<i64> {
        let db = Connection::open_with_flags(&self.database, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        Ok(db
            .query_row(
                "SELECT COALESCE(MAX(time_created),0) FROM message WHERE session_id=?",
                [session_id],
                |row| row.get(0),
            )
            .unwrap_or(0))
    }

    fn completed_answer(&self, session_id: &str, after: i64) -> Result<(bool, Option<String>)> {
        let db = Connection::open_with_flags(&self.database, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let user: Option<String> = db
            .query_row(
                "SELECT id FROM message WHERE session_id=? AND time_created>? AND json_extract(data,'$.role')='user' ORDER BY time_created DESC,id DESC LIMIT 1",
                rusqlite::params![session_id, after],
                |row| row.get(0),
            )
            .optional()?;
        let Some(user) = user else {
            return Ok((false, None));
        };
        let assistant: Option<String> = db
            .query_row(
                "SELECT id FROM message WHERE session_id=? AND json_extract(data,'$.role')='assistant' AND json_extract(data,'$.parentID')=? AND json_extract(data,'$.time.completed') IS NOT NULL AND json_extract(data,'$.finish')='stop' ORDER BY time_created DESC,id DESC LIMIT 1",
                rusqlite::params![session_id, user],
                |row| row.get(0),
            )
            .optional()?;
        let Some(assistant) = assistant else {
            return Ok((true, None));
        };
        let mut statement = db.prepare(
            "SELECT json_extract(data,'$.text') FROM part WHERE session_id=? AND message_id=? AND json_extract(data,'$.type')='text' ORDER BY time_created,id",
        )?;
        let parts = statement
            .query_map(rusqlite::params![session_id, assistant], |row| {
                row.get::<_, Option<String>>(0)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let answer = parts.into_iter().flatten().collect::<String>();
        Ok((true, (!answer.trim().is_empty()).then_some(answer)))
    }

    fn session_exists(&self, session_id: &str) -> Result<bool> {
        let db = Connection::open_with_flags(&self.database, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        Ok(db
            .query_row("SELECT 1 FROM session WHERE id=?", [session_id], |_| Ok(()))
            .optional()?
            .is_some())
    }

    fn close(&mut self) -> Result<()> {
        if self.closed {
            return Ok(());
        }
        if let Some(server) = &mut self.fork_server {
            terminate_child(server)?;
        }
        self.fork_server = None;
        if let Some(process) = &mut self.turn_process {
            terminate_child(process)?;
        }
        self.turn_process = None;
        let Some(thread_id) = self.thread_id.as_deref() else {
            if self.fork_uncertain {
                bail!(
                    "OpenCode fork response was lost; temporary session identity is unknown. Inspect OpenCode sessions before retrying; Pika will not guess which session to delete."
                );
            }
            self.closed = true;
            return Ok(());
        };
        let output = run_output_bounded(
            &self.executable,
            &["--pure", "session", "delete", thread_id],
            Duration::from_secs(15),
        )?;
        if self.session_exists(thread_id).unwrap_or(true) {
            bail!("OpenCode side {thread_id} still exists after cleanup");
        }
        let _ = output;
        self.thread_id = None;
        self.closed = true;
        Ok(())
    }
}

fn opencode_readonly_config() -> String {
    json!({
        "agent": {
            "pika-readonly": {
                "description":"Pika read-only inherited consultation",
                "mode":"primary",
                "permission":{"*":"deny","read":"allow","glob":"allow","grep":"allow","list":"allow"}
            }
        }
    })
    .to_string()
}

fn valid_opencode_id(value: &str) -> bool {
    value.len() >= 8
        && value.len() <= 128
        && value.starts_with("ses_")
        && value[4..]
            .chars()
            .all(|character| character.is_ascii_alphanumeric())
}

fn parse_opencode_model(value: Option<&str>) -> Option<(String, Option<String>)> {
    let mut value = value?.trim().to_owned();
    if value.is_empty() {
        return None;
    }
    let variant = if value.ends_with(']') {
        value.rfind('[').map(|index| {
            let variant = value[index + 1..value.len() - 1].to_owned();
            value.truncate(index);
            variant
        })
    } else {
        None
    };
    Some((value, variant.filter(|value| !value.is_empty())))
}

fn set_cwd(command: &mut Command, cwd: Option<&str>) {
    if let Some(cwd) = valid_cwd(cwd) {
        command.current_dir(cwd);
    }
}

fn valid_cwd(value: Option<&str>) -> Option<&Path> {
    value.map(Path::new).filter(|path| path.is_dir())
}

/// An ephemeral process group whose leader remains waitable until cleanup.
/// The cached status is essential: after reap, even waitid(old_pid) could refer
/// to a different child of this same process if the numeric PID gets reused.
pub(crate) struct OwnedChild {
    child: Child,
    status: Option<ExitStatus>,
    pub(crate) stdin: Option<ChildStdin>,
    pub(crate) stdout: Option<ChildStdout>,
    pub(crate) stderr: Option<ChildStderr>,
}

impl OwnedChild {
    pub(crate) fn spawn(command: &mut Command) -> std::io::Result<Self> {
        owned_process_group(command);
        let mut child = command.spawn()?;
        Ok(Self {
            stdin: child.stdin.take(),
            stdout: child.stdout.take(),
            stderr: child.stderr.take(),
            child,
            status: None,
        })
    }

    fn id(&self) -> u32 {
        self.child.id()
    }

    pub(crate) fn exit_status(&self) -> ExitStatus {
        self.status.expect("owned group cleaned and child reaped")
    }
}

impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = terminate_child(self);
    }
}

// Keep the direct child waitable until group cleanup. An unreaped child reserves
// its PID even after exit, so its PGID cannot be reused by an unrelated group.
// None means another wait already reaped it: never signal that numeric ID again.
#[cfg(unix)]
fn owned_child_state(child: &OwnedChild) -> std::io::Result<Option<bool>> {
    if child.status.is_some() {
        return Ok(None);
    }
    loop {
        // SAFETY: waitid initializes siginfo and WNOWAIT retains this child.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                child.id() as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if result == 0 {
            return Ok(Some(unsafe { info.si_pid() } != 0));
        }
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        if error.raw_os_error() == Some(libc::ECHILD) {
            return Ok(None);
        }
        return Err(error);
    }
}

pub(crate) fn owned_child_exited(child: &mut OwnedChild) -> std::io::Result<bool> {
    if child.status.is_some() {
        return Ok(true);
    }
    #[cfg(unix)]
    {
        owned_child_state(child)?.ok_or_else(|| std::io::Error::from_raw_os_error(libc::ECHILD))
    }
    #[cfg(not(unix))]
    {
        child.status = child.child.try_wait()?;
        Ok(child.status.is_some())
    }
}

pub(crate) fn poll_owned_child(child: &mut OwnedChild) -> Result<Option<ExitStatus>> {
    if !owned_child_exited(child)? {
        return Ok(None);
    }
    terminate_child(child)?;
    Ok(Some(child.exit_status()))
}

pub(crate) fn terminate_child(child: &mut OwnedChild) -> Result<()> {
    if child.status.is_some() {
        return Ok(());
    }
    #[cfg(unix)]
    {
        if let Some(exited) = owned_child_state(child)? {
            // SAFETY: callers create a fresh process group before spawn and do
            // not reap before this point. The waitable leader pins its PGID;
            // only this operation's group is signalled, including descendants
            // whose launcher has already exited.
            if unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) } != 0 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::ESRCH)
                    && !exited_group_is_empty(child.id(), exited, &error)
                {
                    return Err(error).context("could not terminate provider side process group");
                }
            }
        }
    }
    #[cfg(not(unix))]
    if child.child.try_wait()?.is_none() {
        child
            .child
            .kill()
            .context("could not terminate provider side child")?;
    }
    child.status = Some(
        child
            .child
            .wait()
            .context("could not reap provider side child")?,
    );
    Ok(())
}

#[cfg(unix)]
fn exited_group_is_empty(id: u32, exited: bool, error: &std::io::Error) -> bool {
    #[cfg(target_os = "macos")]
    if exited && error.raw_os_error() == Some(libc::EPERM) {
        // Darwin returns EPERM for a group containing only the zombie leader.
        // Check this specific pinned group; do not suppress permission errors
        // when any other member remains. Two slots suffice to disprove empty.
        let mut members = [0u32; 2];
        let capacity = std::mem::size_of_val(&members) as i32;
        // A zero return can mean either no matches or an error, so clear and
        // check this thread's errno instead of accepting an unknown inventory.
        let count = unsafe {
            *libc::__error() = 0;
            libc::proc_listpids(
                libproc::libproc::proc_pid::ProcType::ProcPGRPOnly as u32,
                id,
                members.as_mut_ptr().cast(),
                capacity,
            )
        };
        return count >= 0
            && count <= capacity
            && (count != 0 || std::io::Error::last_os_error().raw_os_error() == Some(0))
            && count as usize % std::mem::size_of::<u32>() == 0
            && members[..count as usize / std::mem::size_of::<u32>()]
                .iter()
                .all(|member| *member == id);
    }
    let _ = (id, exited, error);
    false
}

/// The owning operation cancels these nonblocking pipe waits before joining.
/// This also bounds cleanup when a descendant leaves the owned process group;
/// that foreign group must not be signalled just to obtain pipe EOF.
pub(crate) struct CancellablePipe<T> {
    pipe: T,
    stop: CancellationToken,
}

impl<T> CancellablePipe<T> {
    #[cfg(unix)]
    pub(crate) fn new(pipe: T, stop: CancellationToken) -> std::io::Result<Self>
    where
        T: AsRawFd,
    {
        let fd = pipe.as_raw_fd();
        // SAFETY: fd is borrowed from the live pipe and only its flags change.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags == -1 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1
        {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self { pipe, stop })
    }

    #[cfg(not(unix))]
    pub(crate) fn new(pipe: T, stop: CancellationToken) -> std::io::Result<Self> {
        Ok(Self { pipe, stop })
    }
}

impl<T: Read> Read for CancellablePipe<T> {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        loop {
            if self.stop.is_cancelled() {
                return Ok(0);
            }
            match self.pipe.read(output) {
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(2));
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                result => return result,
            }
        }
    }
}

impl<T: Write> Write for CancellablePipe<T> {
    fn write(&mut self, input: &[u8]) -> std::io::Result<usize> {
        loop {
            if self.stop.is_cancelled() {
                return Err(std::io::ErrorKind::BrokenPipe.into());
            }
            match self.pipe.write(input) {
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(2));
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                result => return result,
            }
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.pipe.flush()
    }
}

fn owned_process_group(command: &mut Command) {
    #[cfg(unix)]
    {
        command.process_group(0);
    }
    #[cfg(not(unix))]
    let _ = command;
}

fn run_output_bounded(executable: &Path, args: &[&str], timeout: Duration) -> Result<String> {
    run_output_bounded_cancellable(executable, args, timeout, &CancellationToken::default())
}

fn run_output_bounded_cancellable(
    executable: &Path,
    args: &[&str],
    timeout: Duration,
    cancellation: &CancellationToken,
) -> Result<String> {
    let deadline = Instant::now() + timeout;
    let mut command = Command::new(executable);
    command.args(args);
    command
        .env("PIKA_EPHEMERAL", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = OwnedChild::spawn(&mut command)
        .with_context(|| format!("could not run {}", executable.display()))?;
    let stdout = Arc::new(Mutex::new(Vec::new()));
    let stderr = Arc::new(Mutex::new(Vec::new()));
    let stop = CancellationToken::default();
    let pipes = (|| -> std::io::Result<_> {
        Ok((
            CancellablePipe::new(
                child.stdout.take().expect("stdout configured"),
                stop.clone(),
            )?,
            CancellablePipe::new(
                child.stderr.take().expect("stderr configured"),
                stop.clone(),
            )?,
        ))
    })();
    let (out_pipe, err_pipe) = match pipes {
        Ok(pipes) => pipes,
        Err(error) => {
            let _ = terminate_child(&mut child);
            return Err(error.into());
        }
    };
    let target = Arc::clone(&stdout);
    let stdout_thread = thread::spawn(move || drain_bounded(out_pipe, target));
    let target = Arc::clone(&stderr);
    let stderr_thread = thread::spawn(move || drain_bounded(err_pipe, target));
    let outcome: Result<()> = loop {
        if cancellation.is_cancelled() {
            break Err(anyhow::anyhow!("{} cancelled", executable.display()));
        }
        match owned_child_exited(&mut child) {
            Ok(true) if stdout_thread.is_finished() && stderr_thread.is_finished() => break Ok(()),
            Ok(_) => {}
            Err(error) => break Err(error.into()),
        }
        if Instant::now() >= deadline {
            break Err(anyhow::anyhow!("{} timed out", executable.display()));
        }
        thread::sleep(Duration::from_millis(2));
    };
    let cleanup = terminate_child(&mut child);
    stop.cancel();
    let _ = stdout_thread.join();
    let _ = stderr_thread.join();
    cleanup?;
    outcome?;
    let status = child.exit_status();
    let stdout = stdout.lock().map(|value| value.clone()).unwrap_or_default();
    let stderr = stderr.lock().map(|value| value.clone()).unwrap_or_default();
    let out = String::from_utf8_lossy(&stdout).trim().to_owned();
    let err = String::from_utf8_lossy(&stderr).trim().to_owned();
    if !status.success() {
        bail!(
            "{} exited with {status}{}",
            executable.display(),
            if err.is_empty() {
                String::new()
            } else {
                format!(": {err}")
            }
        );
    }
    Ok(if out.is_empty() { err } else { out })
}

fn stderr_tail(value: &Arc<Mutex<Vec<u8>>>) -> String {
    String::from_utf8_lossy(&value.lock().map(|value| value.clone()).unwrap_or_default())
        .trim()
        .to_owned()
}

fn version_tuple(value: &str) -> Vec<u64> {
    let Some(found) = regex::Regex::new(r"\b(\d+)\.(\d+)\.(\d+)\b")
        .expect("static version regex")
        .captures(value)
    else {
        return Vec::new();
    };
    (1..=3)
        .filter_map(|index| found.get(index)?.as_str().parse().ok())
        .collect()
}

fn percent_encode(value: &str) -> String {
    let mut output = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            output.push(char::from(byte));
        } else {
            output.push_str(&format!("%{byte:02X}"));
        }
    }
    output
}

fn basic64(value: &str) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes = value.as_bytes();
    let mut output = String::new();
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        let second = *chunk.get(1).unwrap_or(&0);
        let third = *chunk.get(2).unwrap_or(&0);
        output.push(char::from(TABLE[usize::from(first >> 2)]));
        output.push(char::from(
            TABLE[usize::from(((first & 3) << 4) | (second >> 4))],
        ));
        if chunk.len() > 1 {
            output.push(char::from(
                TABLE[usize::from(((second & 15) << 2) | (third >> 6))],
            ));
        } else {
            output.push('=');
        }
        if chunk.len() > 2 {
            output.push(char::from(TABLE[usize::from(third & 63)]));
        } else {
            output.push('=');
        }
    }
    output
}

struct HttpRequest<'a> {
    path: &'a str,
    username: &'a str,
    password: &'a str,
    method: &'a str,
    payload: Option<&'a [u8]>,
    timeout: Duration,
}

fn http_json(
    address: SocketAddr,
    request: HttpRequest<'_>,
    cancellation: &CancellationToken,
) -> Result<Value> {
    if cancellation.is_cancelled() {
        bail!("OpenCode side consultation cancelled");
    }
    let deadline = Instant::now() + request.timeout;
    let mut stream =
        TcpStream::connect_timeout(&address, request.timeout.min(Duration::from_millis(250)))?;
    stream.set_read_timeout(Some(Duration::from_millis(100)))?;
    stream.set_write_timeout(Some(Duration::from_millis(100)))?;
    let payload = request.payload.unwrap_or_default();
    if payload.len() > MAX_QUESTION_BYTES {
        bail!("OpenCode API request exceeded the 64 KiB safety limit");
    }
    let head = format!(
        "{} {} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Basic {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        request.method,
        request.path,
        address.port(),
        basic64(&format!("{}:{}", request.username, request.password)),
        payload.len()
    );
    write_all_cancellable(&mut stream, head.as_bytes(), deadline, cancellation)?;
    write_all_cancellable(&mut stream, payload, deadline, cancellation)?;
    flush_cancellable(&mut stream, deadline, cancellation)?;
    let mut response = Vec::new();
    let mut chunk = [0_u8; 64 * 1024];
    loop {
        if cancellation.is_cancelled() {
            bail!("OpenCode side consultation cancelled");
        }
        if Instant::now() >= deadline {
            bail!("OpenCode API request timed out");
        }
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(size) => {
                response.extend_from_slice(&chunk[..size]);
                if response.len() > MAX_FRAME + 64 * 1024 {
                    bail!("OpenCode API response exceeded the 16 MiB limit");
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::Interrupted
                ) => {}
            Err(error) => return Err(error.into()),
        }
    }
    if response.len() > MAX_FRAME + 64 * 1024 {
        bail!("OpenCode API response exceeded the 16 MiB limit");
    }
    let split = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .context("OpenCode API returned malformed HTTP")?;
    let head = String::from_utf8_lossy(&response[..split]);
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse::<u16>().ok())
        .context("OpenCode API returned malformed status")?;
    if !(200..300).contains(&status) {
        bail!("OpenCode API returned HTTP {status}");
    }
    let encoded_body = &response[split + 4..];
    let body = if head
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        decode_chunked(encoded_body)?
    } else {
        encoded_body.to_vec()
    };
    if body.len() > MAX_FRAME {
        bail!("OpenCode API response exceeded the 16 MiB limit");
    }
    let value: Value =
        serde_json::from_slice(&body).context("OpenCode API returned invalid JSON")?;
    if !value.is_object() {
        bail!("OpenCode API returned an invalid response");
    }
    Ok(value)
}

fn write_all_cancellable(
    stream: &mut TcpStream,
    mut bytes: &[u8],
    deadline: Instant,
    cancellation: &CancellationToken,
) -> Result<()> {
    while !bytes.is_empty() {
        if cancellation.is_cancelled() {
            bail!("OpenCode side consultation cancelled");
        }
        if Instant::now() >= deadline {
            bail!("OpenCode API request timed out");
        }
        match stream.write(bytes) {
            Ok(0) => bail!("OpenCode API connection closed while sending request"),
            Ok(size) => bytes = &bytes[size..],
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::Interrupted
                ) => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn flush_cancellable(
    stream: &mut TcpStream,
    deadline: Instant,
    cancellation: &CancellationToken,
) -> Result<()> {
    loop {
        if cancellation.is_cancelled() {
            bail!("OpenCode side consultation cancelled");
        }
        if Instant::now() >= deadline {
            bail!("OpenCode API request timed out");
        }
        match stream.flush() {
            Ok(()) => return Ok(()),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::Interrupted
                ) => {}
            Err(error) => return Err(error.into()),
        }
    }
}

fn decode_chunked(mut value: &[u8]) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    loop {
        let split = value
            .windows(2)
            .position(|window| window == b"\r\n")
            .context("OpenCode API returned malformed chunk framing")?;
        let size_text = std::str::from_utf8(&value[..split])?
            .split(';')
            .next()
            .unwrap_or_default();
        let size = usize::from_str_radix(size_text.trim(), 16)
            .context("OpenCode API returned an invalid chunk size")?;
        value = &value[split + 2..];
        if size == 0 {
            return Ok(output);
        }
        if size > MAX_FRAME.saturating_sub(output.len()) || value.len() < size + 2 {
            bail!("OpenCode API response exceeded its bound or was truncated");
        }
        output.extend_from_slice(&value[..size]);
        if &value[size..size + 2] != b"\r\n" {
            bail!("OpenCode API returned malformed chunk framing");
        }
        value = &value[size + 2..];
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[cfg(unix)]
    #[test]
    fn bounded_output_deadline_includes_exited_launcher_pipes() {
        for _ in 0..20 {
            let started = Instant::now();
            let error = run_output_bounded(
                Path::new("/bin/sh"),
                &["-c", "sleep 5 & exit 0"],
                Duration::from_millis(10),
            )
            .unwrap_err();
            assert!(error.to_string().contains("timed out"), "{error:#}");
            assert!(started.elapsed() < Duration::from_millis(150));
        }
    }

    #[cfg(unix)]
    #[test]
    fn bounded_output_cancels_after_launcher_exit() {
        let cancellation = CancellationToken::default();
        let signal = cancellation.clone();
        let cancel = thread::spawn(move || {
            thread::sleep(Duration::from_millis(30));
            signal.cancel();
        });
        let started = Instant::now();
        let error = run_output_bounded_cancellable(
            Path::new("/bin/sh"),
            &["-c", "sleep 5 & exit 0"],
            Duration::from_secs(1),
            &cancellation,
        )
        .unwrap_err();
        cancel.join().unwrap();
        assert!(error.to_string().contains("cancelled"), "{error:#}");
        assert!(started.elapsed() < Duration::from_millis(150));
    }

    #[cfg(unix)]
    #[test]
    fn owned_group_cleanup_retains_exited_leader_until_descendants_are_signalled() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "sleep 5 & exit 0"]);
        let mut child = OwnedChild::spawn(command.stdout(Stdio::piped())).unwrap();
        let mut pipe = child.stdout.take().unwrap();
        let (send, receive) = mpsc::sync_channel(1);
        let reader = thread::spawn(move || {
            let _ = send.send(pipe.read_to_end(&mut Vec::new()));
        });
        let started = Instant::now();
        while !owned_child_exited(&mut child).unwrap() {
            assert!(started.elapsed() < Duration::from_millis(150));
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(owned_child_state(&child).unwrap(), Some(true));
        terminate_child(&mut child).unwrap();
        assert_eq!(owned_child_state(&child).unwrap(), None);
        receive
            .recv_timeout(Duration::from_millis(150))
            .unwrap()
            .unwrap();
        reader.join().unwrap();
        // Repeated cleanup uses the cached status, never the now-unpinned PGID.
        terminate_child(&mut child).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn cached_reap_never_probes_or_signals_a_reused_child_id() {
        let mut first_command = Command::new("/bin/sh");
        first_command.args(["-c", "exit 7"]);
        let mut first = OwnedChild::spawn(&mut first_command).unwrap();
        while !owned_child_exited(&mut first).unwrap() {
            thread::sleep(Duration::from_millis(1));
        }
        terminate_child(&mut first).unwrap();

        let mut next_command = Command::new("/bin/sleep");
        next_command.arg("5");
        let mut next = OwnedChild::spawn(&mut next_command).unwrap();
        // Deterministically model PID reuse by making the cached old handle's
        // numeric lookup point at a new child owned by this same parent. No
        // waitid or signal is permitted through an already-reaped handle.
        std::mem::swap(&mut first.child, &mut next.child);
        assert!(owned_child_exited(&mut first).unwrap());
        terminate_child(&mut first).unwrap();
        let still_running = first.child.try_wait().unwrap().is_none();
        std::mem::swap(&mut first.child, &mut next.child);
        terminate_child(&mut next).unwrap();
        assert!(still_running, "cached cleanup signalled a reused child ID");
    }

    #[cfg(unix)]
    #[test]
    fn cancellable_pipe_workers_stop_without_peer_eof_or_available_capacity() {
        use std::os::unix::net::UnixStream;
        let (reader, _held_writer) = UnixStream::pair().unwrap();
        let stop = CancellationToken::default();
        let mut reader = CancellablePipe::new(reader, stop.clone()).unwrap();
        let read = thread::spawn(move || reader.read(&mut [0; 1]));

        let (writer, _held_reader) = UnixStream::pair().unwrap();
        let mut writer = CancellablePipe::new(writer, stop.clone()).unwrap();
        let write = thread::spawn(move || writer.write_all(&vec![0; 4 * 1024 * 1024]));
        thread::sleep(Duration::from_millis(10));
        let started = Instant::now();
        stop.cancel();
        assert_eq!(read.join().unwrap().unwrap(), 0);
        assert_eq!(
            write.join().unwrap().unwrap_err().kind(),
            std::io::ErrorKind::BrokenPipe
        );
        assert!(started.elapsed() < Duration::from_millis(150));
    }

    #[cfg(unix)]
    #[test]
    fn bounded_output_retains_success_failure_and_size_limits() {
        assert_eq!(
            run_output_bounded(
                Path::new("/bin/sh"),
                &["-c", "printf ready"],
                Duration::from_secs(1)
            )
            .unwrap(),
            "ready"
        );
        let error = run_output_bounded(
            Path::new("/bin/sh"),
            &["-c", "printf diagnostic >&2; exit 7"],
            Duration::from_secs(1),
        )
        .unwrap_err();
        assert!(error.to_string().contains("diagnostic"));
        let output = run_output_bounded(
            Path::new("/bin/sh"),
            &["-c", "head -c 131072 /dev/zero | tr '\\000' x"],
            Duration::from_secs(1),
        )
        .unwrap();
        assert_eq!(output.len(), MAX_STDERR);
    }

    #[test]
    fn base64_is_rfc_4648_compatible() {
        assert_eq!(basic64("opencode:secret"), "b3BlbmNvZGU6c2VjcmV0");
    }

    #[test]
    fn readonly_agent_denies_every_non_read_tool() {
        let value: Value = serde_json::from_str(&opencode_readonly_config()).unwrap();
        let permission = &value["agent"]["pika-readonly"]["permission"];
        assert_eq!(permission["*"], "deny");
        assert_eq!(permission["read"], "allow");
        assert_eq!(permission["glob"], "allow");
        assert_eq!(permission["grep"], "allow");
        assert_eq!(permission["list"], "allow");
    }

    #[test]
    fn model_variant_is_split_once() {
        assert_eq!(
            parse_opencode_model(Some("opencode/x-preview[max]")),
            Some(("opencode/x-preview".to_owned(), Some("max".to_owned())))
        );
    }

    #[test]
    fn final_json_frame_does_not_require_a_trailing_newline() {
        let (sender, receiver) = mpsc::sync_channel(1);
        read_json_frames(Cursor::new(br#"{"id":1}"#), sender);
        assert_eq!(receiver.recv().unwrap().unwrap(), json!({"id":1}));
    }

    #[test]
    fn oversized_partial_frame_fails_before_unbounded_growth() {
        let (sender, receiver) = mpsc::sync_channel(1);
        read_json_frames(Cursor::new(vec![b'x'; MAX_FRAME + 1]), sender);
        assert!(
            receiver
                .recv()
                .unwrap()
                .unwrap_err()
                .contains("16 MiB frame limit")
        );
    }

    #[test]
    fn many_small_answer_deltas_have_one_aggregate_limit() {
        let mut answer = String::new();
        let delta = "x".repeat(1024);
        for _ in 0..1024 {
            append_answer(&mut answer, &delta).unwrap();
        }
        assert_eq!(answer.len(), MAX_ANSWER_BYTES);
        assert!(
            append_answer(&mut answer, "x")
                .unwrap_err()
                .to_string()
                .contains("1 MiB")
        );
        assert_eq!(answer.len(), MAX_ANSWER_BYTES);
    }
}
