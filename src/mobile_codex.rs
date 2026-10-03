//! Connect-only access to one existing Codex server; never spawns a provider.
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, VecDeque},
    fs,
    os::{
        fd::AsRawFd,
        unix::{
            fs::{FileTypeExt, MetadataExt},
            net::UnixStream,
        },
    },
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use tungstenite::{Message, WebSocket, client::client_with_config, protocol::WebSocketConfig};

// A single persisted turn may contain multi-megabyte tool output. This is a
// message bound, not a history/page bound; keep it aligned with the mobile wire.
const MAX_FRAME: usize = 16 * 1024 * 1024;
#[derive(Debug, thiserror::Error)]
#[error("Provider rejected the operation: {0}")]
pub(crate) struct Rejected(pub(crate) Value);
pub(crate) struct Client {
    wire: WebSocket<UnixStream>,
    socket: PathBuf,
    inode: u64,
    owner: crate::process::ProcessGeneration,
    counter: u64,
    pub(crate) thread: String,
    pub(crate) events: Vec<Value>,
    pub(crate) questions: BTreeMap<String, Value>,
    resolved: VecDeque<Value>,
}
fn peer_pid(stream: &UnixStream) -> Result<i64> {
    #[cfg(target_os = "macos")]
    {
        let mut pid: libc::pid_t = 0;
        let mut len = std::mem::size_of_val(&pid) as libc::socklen_t;
        let status = unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_LOCAL,
                libc::LOCAL_PEERPID,
                (&mut pid as *mut libc::pid_t).cast(),
                &mut len,
            )
        };
        if status != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(pid as i64)
    }
    #[cfg(target_os = "linux")]
    {
        let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of_val(&credentials) as libc::socklen_t;
        let status = unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut credentials as *mut libc::ucred).cast(),
                &mut len,
            )
        };
        if status != 0 || credentials.uid != unsafe { libc::geteuid() } {
            bail!("Provider socket owner could not be verified");
        }
        Ok(credentials.pid as i64)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = stream;
        bail!("Provider socket verification is unsupported on this host")
    }
}
impl Client {
    pub(crate) fn connect(socket: &Path, thread: &str) -> Result<Self> {
        uuid::Uuid::parse_str(thread)?;
        let mut client = Self::connect_server(socket)?;
        client.thread = thread.into();
        client.require_loaded()?;
        let response = client.rpc(
            "thread/resume",
            json!({"threadId":thread,"excludeTurns":true}),
        )?;
        if response["thread"]["id"] != thread {
            bail!("Provider resumed a different identity");
        }
        Ok(client)
    }
    /// Only explicit creation calls this without an existing identity.
    pub(crate) fn connect_server(socket: &Path) -> Result<Self> {
        let socket = socket
            .canonicalize()
            .context("The existing provider has no shared connection")?;
        let metadata = fs::metadata(&socket)?;
        if !metadata.file_type().is_socket() || metadata.uid() != unsafe { libc::geteuid() } {
            bail!("Provider socket is not owned by this user");
        }
        let stream = UnixStream::connect(&socket)?;
        stream.set_read_timeout(Some(Duration::from_millis(100)))?;
        stream.set_write_timeout(Some(Duration::from_secs(2)))?;
        let pid = peer_pid(&stream)?;
        let record = crate::process::process_record(pid)
            .context("The exact kernel socket peer could not be read")?;
        if !record.argv.iter().any(|a| a == "app-server")
            || !record
                .argv
                .iter()
                .any(|a| Path::new(a).file_name().is_some_and(|n| n == "codex"))
        {
            bail!("Socket peer is not a proven Codex app-server");
        }
        let owner = record.generation();
        let config = WebSocketConfig::default()
            .max_message_size(Some(MAX_FRAME))
            .max_frame_size(Some(MAX_FRAME));
        let (wire, _) = client_with_config("ws://localhost/", stream, Some(config))
            .map_err(|e| anyhow::anyhow!("Provider handshake failed: {e}"))?;
        let mut client = Self {
            wire,
            socket,
            inode: metadata.ino(),
            owner,
            counter: 10,
            thread: String::new(),
            events: Vec::new(),
            questions: BTreeMap::new(),
            resolved: VecDeque::new(),
        };
        client.rpc("initialize",json!({"clientInfo":{"name":"pika-mobile","version":crate::VERSION},"capabilities":{"experimentalApi":true}}))?;
        client.write(json!({"method":"initialized","params":{}}))?;
        Ok(client)
    }
    pub(crate) fn require_owner(&self) -> Result<()> {
        if !crate::process::process_alive(self.owner.pid, Some(self.owner.start_time))
            || fs::metadata(&self.socket)?.ino() != self.inode
        {
            bail!("Provider connection generation changed; no action was sent");
        }
        Ok(())
    }
    pub(crate) fn require_loaded(&mut self) -> Result<()> {
        let mut cursor = Value::Null;
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut cursors = std::collections::BTreeSet::new();
        loop {
            if Instant::now() >= deadline {
                bail!("Loaded-thread proof timed out; no action was sent");
            }
            let page = self.rpc("thread/loaded/list", json!({"limit":64,"cursor":cursor}))?;
            if page["data"]
                .as_array()
                .is_some_and(|ids| ids.iter().any(|v| v == &self.thread))
            {
                return Ok(());
            }
            cursor = page["nextCursor"].clone();
            if cursor.is_null() {
                break;
            }
            if !cursor.is_string() || !cursors.insert(cursor.to_string()) {
                bail!("Provider loaded-thread pagination did not advance");
            }
        }
        bail!("Conversation is not already loaded on this exact server; no provider was launched")
    }
    fn write(&mut self, value: Value) -> Result<()> {
        self.require_owner()?;
        self.wire.send(Message::Text(value.to_string().into()))?;
        Ok(())
    }
    fn receive(&mut self) -> Result<Option<Value>> {
        match self.wire.read() {
            Ok(Message::Text(text)) => Ok(Some(serde_json::from_str(&text)?)),
            Ok(Message::Ping(_)) | Ok(Message::Pong(_)) => {
                self.wire.flush()?;
                Ok(None)
            }
            Ok(Message::Close(_)) => bail!("Provider disconnected"),
            Ok(_) => bail!("Unexpected provider frame"),
            Err(tungstenite::Error::Io(error))
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                Ok(None)
            }
            Err(error) => Err(error.into()),
        }
    }
    fn event(&mut self, mut value: Value) -> Result<()> {
        // Global server events and another thread's requests never inherit the
        // selected identity merely because this is its current connection.
        if value["params"]["threadId"].as_str() != Some(self.thread.as_str()) {
            return Ok(());
        }
        if matches!(
            value["method"].as_str(),
            Some(
                "item/tool/requestUserInput"
                    | "item/commandExecution/requestApproval"
                    | "item/fileChange/requestApproval"
            )
        ) {
            self.questions
                .insert(value["id"].to_string(), value.clone());
        } else if value["method"] == "serverRequest/resolved" {
            if let Some(request) = self
                .questions
                .remove(&value["params"]["requestId"].to_string())
            {
                // A request ID alone can be reused by a later server process.
                // Enrich only from a request actually observed on this stream.
                value["params"]["turnId"] = request["params"]["turnId"].clone();
                value["params"]["itemId"] = request["params"]["itemId"].clone();
                value["params"]["requestMethod"] = request["method"].clone();
                self.resolved.push_back(request);
                if self.resolved.len() > 256 {
                    self.resolved.pop_front();
                }
            }
        }
        if self.events.len() >= 256 {
            bail!("Provider event buffer exceeded its bound; reopen this conversation");
        }
        if value.get("method").is_some() {
            self.events.push(value);
        }
        Ok(())
    }
    pub(crate) fn rpc(&mut self, method: &str, params: Value) -> Result<Value> {
        self.counter += 1;
        let id = self.counter;
        self.write(json!({"id":id,"method":method,"params":params}))?;
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if let Some(value) = self.receive()? {
                if value["id"] == id
                    && (value.get("result").is_some() || value.get("error").is_some())
                {
                    if let Some(error) = value.get("error") {
                        return Err(Rejected(error.clone()).into());
                    }
                    return Ok(value["result"].clone());
                }
                self.event(value)?;
            }
        }
        bail!("Provider acknowledgement is unknown; this operation was not replayed")
    }
    pub(crate) fn poll(&mut self) -> Result<()> {
        if let Some(value) = self.receive()? {
            self.event(value)?;
        }
        Ok(())
    }
    pub(crate) fn request_status(
        &mut self,
        id: &Value,
        turn: &str,
        item: &str,
    ) -> Result<&'static str> {
        self.require_owner()?;
        self.poll()?;
        let exact = |request: &Value| {
            request["id"] == *id
                && request["params"]["threadId"] == self.thread
                && request["params"]["turnId"] == turn
                && request["params"]["itemId"] == item
        };
        if let Some(request) = self.questions.get(&id.to_string()) {
            if !exact(request) {
                bail!("Native request identity changed");
            }
            return Ok("pending");
        }
        if self.resolved.iter().any(exact) {
            return Ok("resolved");
        }
        Ok("unknown")
    }
    pub(crate) fn answer(
        &mut self,
        request_id: &Value,
        turn: &str,
        item: &str,
        answers: Value,
    ) -> Result<()> {
        let question = self
            .questions
            .get(&request_id.to_string())
            .context("This question is no longer pending")?;
        if question["method"] != "item/tool/requestUserInput" {
            bail!("This request requires a typed approval, not a question answer");
        }
        if question["params"]["threadId"] != self.thread
            || question["params"]["turnId"] != turn
            || question["params"]["itemId"] != item
        {
            bail!("Question identity changed; no answer was sent");
        }
        let expected = question["params"]["questions"]
            .as_array()
            .context("Malformed provider question")?;
        let supplied = answers
            .as_object()
            .context("Answers must be keyed by question ID")?;
        if expected.len() != supplied.len()
            || expected.iter().any(|q| {
                q["id"].as_str().is_none_or(|id| {
                    supplied.get(id).is_none_or(|a| {
                        a["answers"].as_array().is_none_or(|list| {
                            list.is_empty() || list.iter().any(|s| !s.is_string())
                        })
                    })
                })
            })
        {
            bail!("Answer does not match the pending questions");
        }
        self.write(json!({"id":request_id,"result":{"answers":answers}}))?;
        Ok(())
    }
    pub(crate) fn approve(
        &mut self,
        request_id: &Value,
        turn: &str,
        item: &str,
        decision: &str,
    ) -> Result<()> {
        let request = self
            .questions
            .get(&request_id.to_string())
            .context("This approval is no longer pending")?;
        if !matches!(
            request["method"].as_str(),
            Some("item/commandExecution/requestApproval" | "item/fileChange/requestApproval")
        ) || request["params"]["threadId"] != self.thread
            || request["params"]["turnId"] != turn
            || request["params"]["itemId"] != item
        {
            bail!("Approval identity changed; no decision was sent");
        }
        if !matches!(decision, "accept" | "decline" | "cancel") {
            bail!("Only this request's one-time approval or refusal is supported");
        }
        if request["params"]["availableDecisions"]
            .as_array()
            .is_some_and(|choices| !choices.iter().any(|v| v == decision))
        {
            bail!("The provider did not offer this decision");
        }
        self.write(json!({"id":request_id,"result":{"decision":decision}}))?;
        Ok(())
    }
}
