//! One selected conversation forwarded through an already configured fleet route.
//! The owning node retains provider access and delivery receipts; this relay never
//! creates a scanner, copies transcripts into storage, or retries a mutation.
use crate::{
    consult::{CancellablePipe, CancellationToken, OwnedChild},
    fleet::SshTransport,
    model::FleetNode,
    store::Store,
};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Read, Write},
    process::Stdio,
    sync::mpsc::{self, Receiver, SyncSender},
    thread::JoinHandle,
    time::{Duration, Instant},
};

const FRAME_LIMIT: usize = 16 * 1024 * 1024;
const REQUEST_LIMIT: usize = 128 * 1024;
const TIMEOUT: Duration = Duration::from_secs(15);
type WriteJob = (Vec<u8>, SyncSender<std::io::Result<()>>);

fn configured_node(store: &Store, node_id: &str) -> Result<FleetNode> {
    uuid::Uuid::parse_str(node_id)?;
    let node = store
        .get_fleet_node(node_id)?
        .context("Machine is no longer connected to this board")?;
    if node.node_id != node_id || node.status == "quarantined" {
        bail!("Verify this machine's identity on the computer before connecting");
    }
    Ok(node)
}

fn write_frames(mut input: impl Write, jobs: Receiver<WriteJob>) {
    while let Ok((bytes, done)) = jobs.recv() {
        let result = input.write_all(&bytes).and_then(|()| input.flush());
        let failed = result.is_err();
        let _ = done.send(result);
        if failed {
            break;
        }
    }
}

fn read_frames(output: impl Read, sender: SyncSender<Result<Value>>) {
    let mut output = BufReader::new(output);
    loop {
        let mut bytes = Vec::new();
        let read = (&mut output)
            .take((FRAME_LIMIT + 1) as u64)
            .read_until(b'\n', &mut bytes);
        let frame = match read {
            Ok(0) => break,
            Ok(_) if bytes.len() > FRAME_LIMIT || bytes.last() != Some(&b'\n') => Err(
                anyhow::anyhow!("Remote Pika returned an incomplete or oversized message"),
            ),
            Ok(_) => {
                serde_json::from_slice(&bytes).context("Remote Pika returned an invalid message")
            }
            Err(error) => Err(error.into()),
        };
        let failed = frame.is_err();
        // Overflow closes the connection rather than dropping a resolution.
        if sender.try_send(frame).is_err() || failed {
            break;
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("Remote delivery is unconfirmed; check the original operation receipt before retrying")]
pub(crate) struct OutcomeUnknown;

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct RejectedBeforeDispatch(String);

pub(crate) struct Remote {
    node: FleetNode,
    _child: OwnedChild,
    stop: CancellationToken,
    input: Option<SyncSender<WriteJob>>,
    frames: Receiver<Result<Value>>,
    workers: Vec<JoinHandle<()>>,
    events: Vec<Value>,
    selected: Option<Value>,
    failed: bool,
    mutation_dispatched: bool,
}

impl Remote {
    pub(crate) fn node_id(&self) -> &str {
        &self.node.node_id
    }

    pub(crate) fn connect(store: &Store, node_id: &str) -> Result<Self> {
        Self::connect_with(store, node_id, &SshTransport::default())
    }

    fn connect_with(store: &Store, node_id: &str, transport: &SshTransport) -> Result<Self> {
        let node = configured_node(store, node_id)?;
        let mut command = transport.command(&node.ssh_target, &["_mobile".into()], false)?;
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = OwnedChild::spawn(&mut command)?;
        let stop = CancellationToken::default();
        let input = CancellablePipe::new(
            child.stdin.take().context("Missing SSH input")?,
            stop.clone(),
        )?;
        let output = CancellablePipe::new(
            child.stdout.take().context("Missing SSH output")?,
            stop.clone(),
        )?;
        let (writes, jobs) = mpsc::sync_channel::<WriteJob>(1);
        let writer = std::thread::spawn(move || write_frames(input, jobs));
        let (sender, frames) = mpsc::sync_channel(16);
        let reader = std::thread::spawn(move || read_frames(output, sender));
        let mut remote = Self {
            node,
            _child: child,
            stop,
            input: Some(writes),
            frames,
            workers: vec![writer, reader],
            events: Vec::new(),
            selected: None,
            failed: false,
            mutation_dispatched: false,
        };
        let hello = remote.request(
            store,
            &json!({"v":1,"id":uuid::Uuid::new_v4().to_string(),"method":"hello","params":{}}),
        )?;
        if hello["nodeId"].as_str() != Some(node_id)
            || !hello["capabilities"]["board"].as_bool().unwrap_or(false)
        {
            bail!("The remote machine's Pika identity changed; nothing was opened");
        }
        Ok(remote)
    }

    fn require_route(&self, store: &Store) -> Result<()> {
        if self.failed {
            bail!("Reconnect to this machine before continuing");
        }
        let current = store
            .get_fleet_node(&self.node.node_id)?
            .context("This machine was removed from the board")?;
        if current.node_id != self.node.node_id
            || current.ssh_target != self.node.ssh_target
            || current.status == "quarantined"
        {
            bail!("This machine's connection changed; reopen it from the board");
        }
        Ok(())
    }

    pub(crate) fn request(&mut self, store: &Store, request: &Value) -> Result<Value> {
        self.require_route(store)?;
        self.mutation_dispatched = false;
        let result = self.exchange(request);
        if result.is_err() {
            // No retry, including when the provider might have accepted a send.
            self.failed = true;
            self.stop.cancel();
            if self.mutation_dispatched
                && result
                    .as_ref()
                    .err()
                    .is_none_or(|error| error.downcast_ref::<RejectedBeforeDispatch>().is_none())
            {
                return Err(OutcomeUnknown.into());
            }
        }
        result
    }

    fn exchange(&mut self, request: &Value) -> Result<Value> {
        let (id, method) = self.validate_request(request)?;
        let expected = request["params"].get("identity");
        if method == "conversation/open" {
            self.selected = expected.cloned();
            self.events.clear();
        }
        let deadline = self.write_request(request, method)?;
        loop {
            let frame = self
                .frames
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .context("Remote reply unavailable; delivery is unconfirmed")??;
            self.validate_frame(&frame)?;
            if frame.get("id").is_some() {
                return self.read_response(frame, id, method, expected);
            }
            self.keep_event(frame)?;
        }
    }

    fn validate_request<'a>(&self, request: &'a Value) -> Result<(&'a str, &'a str)> {
        let id = request["id"].as_str().context("Missing request identity")?;
        uuid::Uuid::parse_str(id)?;
        let method = request["method"]
            .as_str()
            .context("Missing mobile method")?;
        if !matches!(
            method,
            "hello"
                | "conversation/open"
                | "conversation/history"
                | "conversation/send"
                | "conversation/answer"
                | "conversation/approve"
                | "conversation/receipt"
                | "conversation/requestStatus"
                | "conversation/candidates"
                | "conversation/adopt"
                | "conversation/create"
                | "projects/list"
        ) {
            bail!("This operation is not a fleet conversation operation");
        }
        let expected = request["params"].get("identity");
        if request["params"]
            .get("nodeId")
            .is_some_and(|node| node != &self.node.node_id)
        {
            bail!("Operation belongs to a different machine");
        }
        if expected.is_some_and(|identity| identity["nodeId"] != self.node.node_id) {
            bail!("Conversation belongs to a different machine");
        }
        Ok((id, method))
    }

    fn write_request(&mut self, request: &Value, method: &str) -> Result<Instant> {
        let mut bytes = serde_json::to_vec(request)?;
        bytes.push(b'\n');
        if bytes.len() > REQUEST_LIMIT {
            bail!("Message is too large to send");
        }
        let deadline = Instant::now() + TIMEOUT;
        let (done, written) = mpsc::sync_channel(1);
        self.input
            .as_ref()
            .context("Remote connection closed")?
            .try_send((bytes, done))
            .map_err(|_| anyhow::anyhow!("Remote connection cannot accept this request"))?;
        self.mutation_dispatched = matches!(
            method,
            "conversation/send"
                | "conversation/answer"
                | "conversation/approve"
                | "conversation/create"
                | "conversation/adopt"
        );
        written
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .context("Remote connection interrupted while sending; delivery is unconfirmed")??;
        Ok(deadline)
    }

    fn read_response(
        &mut self,
        frame: Value,
        id: &str,
        method: &str,
        expected: Option<&Value>,
    ) -> Result<Value> {
        if frame["id"] != id || frame.get("result").is_some() == frame.get("error").is_some() {
            bail!("Remote response does not match this request");
        }
        if frame.get("error").is_some() {
            if frame["error"]["code"] == "rejected_before_dispatch" {
                return Err(RejectedBeforeDispatch(
                    frame["error"]["message"]
                        .as_str()
                        .unwrap_or("Remote request was rejected before dispatch")
                        .to_owned(),
                )
                .into());
            }
            bail!(
                "{}",
                frame["error"]["message"]
                    .as_str()
                    .unwrap_or("Remote operation could not complete")
            );
        }
        let result = frame["result"].clone();
        if expected.is_some_and(|identity| {
            result
                .get("identity")
                .is_some_and(|actual| actual != identity)
        }) {
            bail!("Remote response belongs to another conversation");
        }
        if method == "conversation/create" {
            if let Some(identity) = result
                .get("identity")
                .filter(|identity| !identity.is_null())
            {
                if identity["nodeId"] != self.node.node_id {
                    bail!("New conversation belongs to another machine");
                }
                self.selected = Some(identity.clone());
            }
        }
        Ok(result)
    }

    fn validate_frame(&self, frame: &Value) -> Result<()> {
        if frame["v"] != 1 || !frame.is_object() {
            bail!("Remote Pika protocol is incompatible");
        }
        Ok(())
    }

    fn keep_event(&mut self, frame: Value) -> Result<()> {
        if !matches!(
            frame["event"].as_str(),
            Some("conversation/event" | "conversation/disconnected")
        ) {
            bail!("Unexpected event on the conversation connection");
        }
        if self.selected.as_ref() != frame["params"].get("identity") {
            bail!("Remote event belongs to another conversation");
        }
        if self.events.len() >= 128 {
            bail!("Remote conversation advanced too quickly; reopen to refresh");
        }
        self.events.push(frame);
        Ok(())
    }

    pub(crate) fn poll(&mut self, store: &Store) -> Result<Vec<Value>> {
        self.require_route(store)?;
        loop {
            match self.frames.try_recv() {
                Ok(frame) => {
                    let frame = frame?;
                    self.validate_frame(&frame)?;
                    self.keep_event(frame)?;
                }
                Err(mpsc::TryRecvError::Empty) => return Ok(std::mem::take(&mut self.events)),
                Err(mpsc::TryRecvError::Disconnected) => {
                    bail!("Connection to this machine was interrupted")
                }
            }
        }
    }
}

impl Drop for Remote {
    fn drop(&mut self) {
        self.stop.cancel();
        self.input.take();
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
        // OwnedChild subsequently terminates/reaps only this SSH process group.
    }
}
