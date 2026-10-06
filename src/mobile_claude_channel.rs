//! Private native MCP channel. Wakes contain UUIDs only; tool-context attestations
//! independently gate literal content. Native hooks never grant permission.
use super::{Binding, alive, native_matches, publish, token_binding};
use crate::{core::Pika, process, store::ReconcileLedger};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    os::{
        fd::AsRawFd,
        unix::{
            fs::{FileTypeExt, MetadataExt, PermissionsExt},
            net::{UnixListener, UnixStream},
        },
    },
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

// JSON escaping can expand a bounded literal by six bytes per input byte.
const FRAME: usize = 512 * 1024;
const TEXT: usize = 65536;
// Leave room for an ordinary native permission prompt; never grant it here.
const TTL: u64 = 5 * 60;

#[derive(Default, Serialize, Deserialize)]
struct Route {
    initialized: bool,
    started: bool,
    revoked: bool,
}
#[derive(Clone, Serialize, Deserialize)]
struct Attestation {
    tool_id: String,
    tool_name: String,
    arguments: Value,
    issued: u64,
}
#[derive(Serialize, Deserialize)]
struct Operation {
    text: String,
    released: bool,
    reply: Option<String>,
    attestation: Option<Attestation>,
    #[serde(default)]
    fetch_tool_id: Option<String>,
    #[serde(default)]
    reply_tool_id: Option<String>,
}

#[derive(Clone)]
pub(crate) struct HistoryOperation {
    pub text: String,
    pub fetch_tool_id: Option<String>,
    pub reply: Option<String>,
    pub reply_tool_id: Option<String>,
}

pub(crate) struct HistoricalSnapshot {
    db: Option<rusqlite::Connection>,
    thread: String,
}

impl HistoricalSnapshot {
    pub(crate) fn open(paths: &crate::paths::Paths, thread: &str) -> Result<Self> {
        ensure!(
            uuid::Uuid::parse_str(thread)?.to_string() == thread,
            "Canonical historical thread required"
        );
        Ok(Self {
            db: crate::store::Store::from_paths(paths).meta_read_snapshot()?,
            thread: thread.into(),
        })
    }

    pub(crate) fn operation(&self, token: &str, id: &str) -> Result<Option<HistoryOperation>> {
        let Some(db) = &self.db else {
            return Ok(None);
        };
        let key = token_operation_key(token, id)?;
        if historical_value(db, &format!("claude-channel:{token}:thread"))?.as_deref()
            != Some(&self.thread)
        {
            return Ok(None);
        }
        historical_value(db, &key)?
            .map(|value| historical_operation(&value))
            .transpose()
    }
}

fn historical_value(db: &rusqlite::Connection, key: &str) -> Result<Option<String>> {
    use rusqlite::OptionalExtension;
    let value: Option<Option<String>> = db.query_row(
        "SELECT CASE WHEN length(CAST(value AS BLOB)) <= ? THEN value ELSE NULL END FROM meta WHERE key=?",
        rusqlite::params![FRAME, key], |row| row.get(0)).optional()?;
    match value {
        Some(Some(value)) => Ok(Some(value)),
        Some(None) => anyhow::bail!("Historical channel proof exceeds its per-record bound"),
        None => Ok(None),
    }
}

#[cfg(test)]
pub(crate) fn history_operation(
    paths: &crate::paths::Paths,
    thread: &str,
    token: &str,
    id: &str,
) -> Result<Option<HistoryOperation>> {
    HistoricalSnapshot::open(paths, thread)?.operation(token, id)
}

fn historical_operation(value: &str) -> Result<HistoryOperation> {
    let op: Operation = serde_json::from_str(value)?;
    ensure!(
        op.text.len() <= TEXT && op.reply.as_ref().is_none_or(|text| text.len() <= TEXT),
        "Historical literal exceeds channel bound"
    );
    ensure!(
        op.fetch_tool_id
            .as_ref()
            .is_none_or(|id| id.starts_with("toolu_") && id.len() <= 128)
            && op
                .reply_tool_id
                .as_ref()
                .is_none_or(|id| id.starts_with("toolu_") && id.len() <= 128),
        "Historical native tool identity invalid"
    );
    Ok(HistoryOperation {
        text: op.text,
        fetch_tool_id: op.fetch_tool_id,
        reply: op.reply,
        reply_tool_id: op.reply_tool_id,
    })
}

fn require_authority(ledger: &ReconcileLedger<'_>, binding: &Binding) -> Result<()> {
    let session = ledger
        .get_session(crate::model::Provider::Claude, &binding.thread)?
        .context("Original conversation unavailable")?;
    ensure!(
        session.status != crate::model::Status::OpenTwice,
        "Original conversation has multiple owners"
    );
    ensure!(
        ledger.is_watched(crate::model::Provider::Claude, &binding.thread)?,
        "Original conversation no longer watched"
    );
    ensure!(
        ledger.get_launch_binding(&binding.token)?
            == Some((crate::model::Provider::Claude, binding.thread.clone())),
        "Original launch revoked"
    );
    let owner = ledger
        .get_recovery_owner(crate::model::Provider::Claude, &binding.thread)?
        .context("Original owner revoked")?;
    ensure!(
        owner.pid == binding.native_pid
            && u64::try_from(owner.start_time).ok() == Some(binding.native_start)
            && owner.launch_token == binding.token,
        "Original owner changed"
    );
    Ok(())
}

fn route_key(binding: &Binding) -> String {
    format!("claude-channel:{}:route", binding.token)
}
fn operation_key(binding: &Binding, id: &str) -> Result<String> {
    token_operation_key(&binding.token, id)
}
fn token_operation_key(token: &str, id: &str) -> Result<String> {
    ensure!(
        uuid::Uuid::parse_str(token)?.to_string() == token,
        "Canonical original launch required"
    );
    let id = uuid::Uuid::parse_str(id)?.to_string();
    Ok(format!("claude-channel:{token}:operation:{id}"))
}
fn operation_owner_key(thread: &str, id: &str) -> Result<String> {
    ensure!(
        uuid::Uuid::parse_str(thread)?.to_string() == thread,
        "Canonical original thread required"
    );
    let id = uuid::Uuid::parse_str(id)?.to_string();
    Ok(format!("claude-operation:{thread}:{id}"))
}
fn already_admitted(
    ledger: &ReconcileLedger<'_>,
    binding: &Binding,
    id: &str,
    text: &str,
) -> Result<bool> {
    let Some(token) = ledger.get_meta(&operation_owner_key(&binding.thread, id)?)? else {
        return Ok(false);
    };
    let value = ledger
        .get_meta(&token_operation_key(&token, id)?)?
        .context("Original operation unavailable; never replay")?;
    let op: Operation = serde_json::from_str(&value)?;
    ensure!(op.text == text, "Operation UUID reused for different text");
    Ok(true)
}
fn epoch() -> Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}
fn read_route(ledger: &ReconcileLedger<'_>, binding: &Binding) -> Result<Route> {
    Ok(ledger
        .get_meta(&route_key(binding))?
        .map(|v| serde_json::from_str(&v))
        .transpose()?
        .unwrap_or_default())
}
fn check_route(route: &Route, initialized: bool) -> Result<()> {
    ensure!(
        !route.revoked,
        "Native session lifecycle invalidated this continuation route"
    );
    ensure!(
        !initialized || route.initialized && route.started,
        "Native continuation initialization pending"
    );
    Ok(())
}
pub(crate) fn require_route(pika: &Pika, binding: &Binding, initialized: bool) -> Result<()> {
    let route: Route = pika
        .store
        .get_meta(&route_key(binding))?
        .map(|v| serde_json::from_str(&v))
        .transpose()?
        .unwrap_or_default();
    check_route(&route, initialized)
}
pub(crate) fn ready_to_admit(pika: &Pika, token: &str) -> Result<bool> {
    let binding = token_binding(pika, token)?;
    require_route(pika, &binding, true)
        .map(|()| true)
        .or_else(|_| {
            require_route(pika, &binding, false)?;
            Ok(false)
        })
}
fn change_route(pika: &Pika, binding: &Binding, update: impl FnOnce(&mut Route)) -> Result<()> {
    pika.store.reconcile_transaction(|ledger| {
        let mut route = read_route(ledger, binding)?;
        update(&mut route);
        ledger.set_meta(&route_key(binding), &serde_json::to_string(&route)?)
    })
}

fn read_operation(ledger: &ReconcileLedger<'_>, binding: &Binding, id: &str) -> Result<Operation> {
    let value = ledger
        .get_meta(&operation_key(binding, id)?)?
        .context("Unknown operation; never infer or replay it")?;
    Ok(serde_json::from_str(&value)?)
}
fn write_operation(
    ledger: &ReconcileLedger<'_>,
    binding: &Binding,
    id: &str,
    op: &Operation,
) -> Result<()> {
    ledger.set_meta(&operation_key(binding, id)?, &serde_json::to_string(op)?)
}
fn admit(pika: &Pika, binding: &Binding, id: &str, text: &str) -> Result<bool> {
    let id = uuid::Uuid::parse_str(id)?.to_string();
    ensure!(
        !text.trim().is_empty() && text.len() <= TEXT,
        "Message exceeds native channel bound"
    );
    pika.store.reconcile_transaction(|ledger| {
        require_authority(ledger, binding)?;
        check_route(&read_route(ledger, binding)?, true)?;
        ledger.set_meta(
            &format!("claude-channel:{}:thread", binding.token),
            &binding.thread,
        )?;
        if already_admitted(ledger, binding, &id, text)? {
            return Ok(false);
        }
        ledger.set_meta(&operation_owner_key(&binding.thread, &id)?, &binding.token)?;
        write_operation(
            ledger,
            binding,
            &id,
            &Operation {
                text: text.into(),
                released: false,
                reply: None,
                attestation: None,
                fetch_tool_id: None,
                reply_tool_id: None,
            },
        )?;
        Ok(true)
    })
}

fn tool_name(binding: &Binding, short: &str) -> String {
    format!("mcp__{}__{short}", binding.server_name)
}
fn canonical_arguments(short: &str, input: &Value) -> Result<(String, Value)> {
    let id = input["operation_id"]
        .as_str()
        .context("Operation UUID missing")?;
    let canonical_id = uuid::Uuid::parse_str(id)?.to_string();
    let canonical = match short {
        "fetch_message" => json!({"operation_id":id}),
        "reply" => {
            let text = input["text"].as_str().context("Literal reply missing")?;
            ensure!(
                !text.trim().is_empty() && text.len() <= TEXT,
                "Reply exceeds native channel bound"
            );
            json!({"operation_id":id,"text":text})
        }
        _ => bail!("Unknown channel tool"),
    };
    ensure!(*input == canonical, "Unexpected native tool arguments");
    Ok((canonical_id, canonical))
}
fn install_attestation(pika: &Pika, binding: &Binding, input: &Value) -> Result<()> {
    ensure!(
        input["hook_event_name"] == "PreToolUse" && input["session_id"] == binding.thread,
        "Wrong native tool session"
    );
    ensure!(
        input.get("agent_id").is_none_or(Value::is_null),
        "Subagent content release refused"
    );
    let name = input["tool_name"]
        .as_str()
        .context("Native tool name missing")?;
    let short = ["fetch_message", "reply"]
        .into_iter()
        .find(|s| tool_name(binding, s) == name)
        .context("Native tool name not this channel")?;
    let (id, arguments) = canonical_arguments(short, &input["tool_input"])?;
    let tool_id = input["tool_use_id"]
        .as_str()
        .filter(|v| v.starts_with("toolu_") && v.len() <= 128)
        .context("Native tool-use identity missing")?;
    let issued = epoch()?;
    pika.store.reconcile_transaction(|ledger| {
        require_authority(ledger, binding)?;
        check_route(&read_route(ledger, binding)?, true)?;
        let mut op = read_operation(ledger, binding, &id)?;
        ensure!(op.reply.is_none(), "Operation already replied; no replay");
        if short == "fetch_message" {
            ensure!(!op.released, "Content already released; no refetch");
        }
        op.attestation = Some(Attestation {
            tool_id: tool_id.into(),
            tool_name: name.into(),
            arguments,
            issued,
        });
        write_operation(ledger, binding, &id, &op)
    })
}

fn execute_tool(
    pika: &Pika,
    binding: &Binding,
    short: &str,
    arguments: &Value,
    native_id: &str,
) -> Result<String> {
    let (id, canonical) = canonical_arguments(short, arguments)?;
    let now = epoch()?;
    pika.store.reconcile_transaction(|ledger| {
        require_authority(ledger, binding)?;
        check_route(&read_route(ledger, binding)?, true)?;
        let mut op = read_operation(ledger, binding, &id)?;
        let proof = op
            .attestation
            .take()
            .context("Native tool attestation missing; content withheld")?;
        validate_attestation(&proof, binding, short, native_id, &canonical, now)?;
        let result = if short == "fetch_message" {
            ensure!(
                !op.released && op.reply.is_none(),
                "Content already released; no refetch"
            );
            op.released = true;
            op.fetch_tool_id = Some(native_id.into());
            op.text.clone()
        } else {
            ensure!(
                op.released && op.reply.is_none(),
                "Reply has no released operation or already exists"
            );
            op.reply = Some(canonical["text"].as_str().context("Reply missing")?.into());
            op.reply_tool_id = Some(native_id.into());
            "Reply recorded for the original Pika operation.".into()
        };
        // Commit one-use consumption and released/replied state BEFORE IPC output.
        write_operation(ledger, binding, &id, &op)?;
        Ok(result)
    })
}

fn validate_attestation(
    proof: &Attestation,
    binding: &Binding,
    short: &str,
    native_id: &str,
    canonical: &Value,
    now: u64,
) -> Result<()> {
    ensure!(
        proof.tool_id == native_id
            && proof.tool_name == tool_name(binding, short)
            && proof.arguments == *canonical,
        "Native tool attestation mismatch"
    );
    ensure!(
        now >= proof.issued && now - proof.issued <= TTL,
        "Native tool attestation expired"
    );
    Ok(())
}

pub(crate) fn receipt(pika: &Pika, binding: &Binding, id: &str) -> Result<Option<Value>> {
    thread_receipt(pika, &binding.thread, id)
}
pub(crate) fn thread_receipt(pika: &Pika, thread: &str, id: &str) -> Result<Option<Value>> {
    ensure!(
        pika.store
            .is_watched(crate::model::Provider::Claude, thread)?,
        "Original conversation no longer watched"
    );
    let Some(token) = pika.store.get_meta(&operation_owner_key(thread, id)?)? else {
        return Ok(None);
    };
    ensure!(
        pika.store
            .get_meta(&format!("claude-channel:{token}:thread"))?
            .as_deref()
            == Some(thread),
        "Original operation belongs to a different thread"
    );
    let Some(value) = pika.store.get_meta(&token_operation_key(&token, id)?)? else {
        return Ok(None);
    };
    let op: Operation = serde_json::from_str(&value)?;
    Ok(Some(
        json!({"state":if op.reply.is_some(){"delivered"}else{"unknown"},"turnId":null,"messageId":id,"message":if op.reply.is_some(){"Native attested reply recorded"}else{"Native delivery remains unconfirmed; do not resend this operation"}}),
    ))
}
fn peer_pid(stream: &UnixStream) -> Result<i64> {
    #[cfg(target_os = "linux")]
    {
        let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of_val(&cred) as libc::socklen_t;
        let result = unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut cred as *mut libc::ucred).cast(),
                &mut len,
            )
        };
        ensure!(
            result == 0 && cred.uid == unsafe { libc::geteuid() },
            "Private native peer not same user"
        );
        Ok(i64::from(cred.pid))
    }
    #[cfg(target_os = "macos")]
    {
        let (mut uid, mut gid) = (0, 0);
        let result = unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) };
        ensure!(
            result == 0 && uid == unsafe { libc::geteuid() },
            "Private native peer not same user"
        );
        let mut pid: libc::pid_t = 0;
        let mut len = std::mem::size_of_val(&pid) as libc::socklen_t;
        let result = unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_LOCAL,
                libc::LOCAL_PEERPID,
                (&mut pid as *mut libc::pid_t).cast(),
                &mut len,
            )
        };
        ensure!(result == 0, "Private native peer PID unavailable");
        Ok(i64::from(pid))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = stream;
        bail!("Native peer verification unsupported")
    }
}
pub(super) fn native_child(pid: i64, binding: &Binding) -> Result<()> {
    ensure!(
        native_matches(binding),
        "Original native generation changed"
    );
    let mut current = pid;
    let mut edges = vec![];
    for _ in 0..32 {
        if current == binding.native_pid {
            break;
        }
        let record =
            process::process_record(current).context("Native hook ancestry unavailable")?;
        current = record.parent_pid.context("Native hook ancestry ended")?;
        edges.push(record);
    }
    ensure!(
        current == binding.native_pid,
        "Helper is not an original native child"
    );
    for expected in edges {
        let actual = process::process_record(expected.pid).context("Native child disappeared")?;
        ensure!(
            actual.start_time == expected.start_time && actual.parent_pid == expected.parent_pid,
            "Native child ancestry changed"
        );
    }
    ensure!(
        native_matches(binding),
        "Original native generation changed"
    );
    Ok(())
}
fn read_packet(reader: &mut impl BufRead) -> Result<Option<Value>> {
    let mut bytes = vec![];
    reader
        .take((FRAME + 1) as u64)
        .read_until(b'\n', &mut bytes)?;
    if bytes.is_empty() {
        return Ok(None);
    }
    ensure!(
        bytes.len() <= FRAME && bytes.last() == Some(&b'\n'),
        "Native packet exceeds bound or is incomplete"
    );
    Ok(Some(serde_json::from_slice(&bytes)?))
}
fn write_packet(writer: &mut impl Write, value: &Value) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    ensure!(bytes.len() < FRAME, "Native output exceeds bound");
    writer.write_all(&bytes)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(())
}

pub(crate) fn rpc(binding: &Binding, packet: Value) -> Result<Value> {
    let before = fs::symlink_metadata(&binding.endpoint)?;
    ensure!(
        before.file_type().is_socket()
            && before.uid() == unsafe { libc::geteuid() }
            && before.mode() & 0o077 == 0,
        "Native endpoint is not private"
    );
    let mut stream = UnixStream::connect(&binding.endpoint)?;
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    stream.set_write_timeout(Some(Duration::from_secs(3)))?;
    ensure!(
        peer_pid(&stream)? == binding.mcp_pid && alive(binding.mcp_pid, binding.mcp_start),
        "Native endpoint owner changed"
    );
    let after = fs::symlink_metadata(&binding.endpoint)?;
    ensure!(
        before.dev() == after.dev() && before.ino() == after.ino(),
        "Native endpoint path changed"
    );
    write_packet(&mut stream, &packet)?;
    let response =
        read_packet(&mut BufReader::new(stream))?.context("Native acknowledgement unavailable")?;
    ensure!(response["error"].is_null(), "{}", response["error"]);
    Ok(response["result"].clone())
}

fn hook_input() -> Result<Value> {
    let mut bytes = vec![];
    std::io::stdin()
        .take((FRAME + 1) as u64)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= FRAME, "Native hook exceeds bound");
    Ok(serde_json::from_slice(&bytes)?)
}
fn wait_binding(pika: &Pika, token: &str) -> Result<Binding> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let binding = token_binding(pika, token)?;
        if native_matches(&binding) {
            return Ok(binding);
        }
        ensure!(
            Instant::now() < deadline,
            "Native owner initialization timed out"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}
pub(crate) fn attest(pika: &Pika, token: &str) -> Result<i32> {
    let input = hook_input()?;
    let binding = wait_binding(pika, token)?;
    native_child(i64::from(std::process::id()), &binding)?;
    install_attestation(pika, &binding, &input)?;
    println!("{{}}");
    Ok(0)
}
pub(crate) fn session_event(pika: &Pika, token: &str) -> Result<i32> {
    let input = hook_input()?;
    let binding = wait_binding(pika, token)?;
    native_child(i64::from(std::process::id()), &binding)?;
    let event = input["hook_event_name"]
        .as_str()
        .context("Native lifecycle event missing")?;
    ensure!(
        matches!(event, "SessionStart" | "SessionEnd"),
        "Wrong native lifecycle hook"
    );
    change_route(pika, &binding, |route| {
        if event == "SessionEnd" || input["session_id"] != binding.thread {
            route.revoked = true;
        } else {
            route.started = true;
        }
    })?;
    println!("{{}}");
    Ok(0)
}

type Output = Arc<Mutex<std::io::Stdout>>;
fn require_current_binding(pika: &Pika, binding: &Binding) -> Result<()> {
    let mut current =
        super::read_binding(&pika.paths, &binding.thread)?.context("Native descriptor revoked")?;
    // Readiness is the launcher's final transition, not a different owner.
    current.ready = binding.ready;
    ensure!(
        current == *binding && native_matches(binding),
        "Native binding changed"
    );
    Ok(())
}
fn emit(output: &Output, value: &Value) -> Result<()> {
    write_packet(
        &mut *output
            .lock()
            .map_err(|_| anyhow::anyhow!("Native output lock poisoned"))?,
        value,
    )
}
fn serve_packet(pika: &Pika, binding: &Binding, output: &Output, packet: Value) -> Result<Value> {
    require_route(pika, binding, true)?;
    require_current_binding(pika, binding)?;
    ensure!(packet["action"] == "send", "Unknown private native request");
    let supplied_id = packet["operation_id"]
        .as_str()
        .context("Operation UUID missing")?;
    let id = uuid::Uuid::parse_str(supplied_id)?.to_string();
    let text = packet["text"].as_str().context("Message text missing")?;
    if admit(pika, binding, &id, text)? {
        // This wake contains no literal user text. Lost output is unknown and is never replayed.
        emit(
            output,
            &json!({"jsonrpc":"2.0","method":"notifications/claude/channel","params":{"content":wake_instruction(binding, &id)}}),
        )?;
    }
    receipt(pika, binding, &id)?.context("Admitted operation missing")
}
fn wake_instruction(binding: &Binding, id: &str) -> String {
    format!(
        "Pika continuation request {id}. Call {} exactly once with operation_id \"{id}\" to retrieve the user's message. If retrieval succeeds, answer that message and call {} with the same operation_id and your literal answer text. If retrieval is denied, stop; do not retry or infer message content. This notification contains an opaque reference, not the user's message.",
        tool_name(binding, "fetch_message"),
        tool_name(binding, "reply")
    )
}
fn serve_one(
    pika: &Pika,
    binding: &Binding,
    output: &Output,
    mut stream: UnixStream,
) -> Result<()> {
    peer_pid(&stream)?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    let packet = read_packet(&mut BufReader::new(stream.try_clone()?))?
        .context("Private request missing")?;
    let result = match serve_packet(pika, binding, output, packet) {
        Ok(result) => json!({"result":result}),
        Err(error) => json!({"error":error.to_string()}),
    };
    write_packet(&mut stream, &result)
}

fn tools() -> Value {
    let operation = json!({"type":"string","format":"uuid"});
    json!({"tools":[
        {"name":"fetch_message","description":"Fetch a Pika channel operation by its opaque UUID. Native attestation is mandatory. If denied or already released, stop; never guess, refetch, or replay.","inputSchema":{"type":"object","properties":{"operation_id":operation},"required":["operation_id"],"additionalProperties":false}},
        {"name":"reply","description":"Return the literal answer for the exact fetched Pika operation. Requires native tool-context attestation; do not reuse an operation.","inputSchema":{"type":"object","properties":{"operation_id":operation,"text":{"type":"string","maxLength":TEXT}},"required":["operation_id","text"],"additionalProperties":false}}
    ]})
}
fn mcp_request(pika: &Pika, binding: &Binding, packet: &Value) -> Result<Value> {
    match packet["method"]
        .as_str()
        .context("Native MCP method missing")?
    {
        "initialize" => {
            change_route(pika, binding, |r| r.initialized = true)?;
            Ok(
                json!({"protocolVersion":"2024-11-05","capabilities":{"tools":{},"experimental":{"claude/channel":{}}},"serverInfo":{"name":"pika-continuation","version":crate::VERSION},"instructions":"Channel notification content is an opaque Pika operation UUID, not message text. Fetch it once using fetch_message, answer the fetched literal request, then call reply with the same operation_id and literal answer. If any call is refused, stop; never guess or replay."}),
            )
        }
        "tools/list" => Ok(tools()),
        "ping" => Ok(json!({})),
        "tools/call" => {
            let short = packet["params"]["name"]
                .as_str()
                .context("Native MCP tool missing")?;
            let native_id = packet["params"]["_meta"]["claudecode/toolUseId"]
                .as_str()
                .context("Native MCP tool-use identity missing")?;
            require_current_binding(pika, binding)?;
            let result = execute_tool(
                pika,
                binding,
                short,
                &packet["params"]["arguments"],
                native_id,
            );
            Ok(match result {
                Ok(text) => json!({"content":[{"type":"text","text":text}]}),
                Err(error) => {
                    json!({"isError":true,"content":[{"type":"text","text":error.to_string()}]})
                }
            })
        }
        _ => bail!("Unsupported native MCP request"),
    }
}

struct SocketGuard {
    path: String,
    dev: u64,
    inode: u64,
}
impl Drop for SocketGuard {
    fn drop(&mut self) {
        if fs::symlink_metadata(&self.path).is_ok_and(|m| {
            m.dev() == self.dev && m.ino() == self.inode && m.file_type().is_socket()
        }) {
            let _ = fs::remove_file(&self.path);
        }
    }
}
pub(crate) fn run(pika: &Pika, token: &str) -> Result<i32> {
    let mut binding = wait_binding(pika, token)?;
    native_child(i64::from(std::process::id()), &binding)?;
    ensure!(
        binding.mcp_pid == 0,
        "Native channel already owned; no replacement"
    );
    binding.mcp_pid = i64::from(std::process::id());
    binding.mcp_start = process::process_generation(binding.mcp_pid)
        .context("Native channel generation unavailable")?
        .start_time;
    let listener = UnixListener::bind(&binding.endpoint)?;
    fs::set_permissions(&binding.endpoint, fs::Permissions::from_mode(0o600))?;
    let meta = fs::symlink_metadata(&binding.endpoint)?;
    let _socket = SocketGuard {
        path: binding.endpoint.clone(),
        dev: meta.dev(),
        inode: meta.ino(),
    };
    listener.set_nonblocking(true)?;
    publish(&pika.paths, &binding)?;
    let output: Output = Arc::new(Mutex::new(std::io::stdout()));
    let stop = Arc::new(AtomicBool::new(false));
    let worker = spawn_listener(
        pika.clone(),
        binding.clone(),
        listener,
        output.clone(),
        stop.clone(),
    );
    let result = mcp_loop(pika, &binding, &output);
    stop.store(true, Ordering::Relaxed);
    let _ = worker.join();
    let _ = change_route(pika, &binding, |r| r.revoked = true);
    result.map(|()| 0)
}
fn spawn_listener(
    pika: Pika,
    binding: Binding,
    listener: UnixListener,
    output: Output,
    stop: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) && native_matches(&binding) {
            match listener.accept() {
                Ok((stream, _)) => {
                    let _ = serve_one(&pika, &binding, &output, stream);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(25))
                }
                Err(_) => break,
            }
        }
    })
}
fn mcp_loop(pika: &Pika, binding: &Binding, output: &Output) -> Result<()> {
    let mut input = std::io::stdin().lock();
    while let Some(packet) = read_packet(&mut input)? {
        let Some(id) = packet.get("id") else {
            continue;
        };
        let reply = match mcp_request(pika, binding, &packet) {
            Ok(result) => json!({"jsonrpc":"2.0","id":id,"result":result}),
            Err(error) => {
                json!({"jsonrpc":"2.0","id":id,"error":{"code":-32602,"message":error.to_string()}})
            }
        };
        emit(output, &reply)?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "mobile_claude_channel_tests.rs"]
mod tests;
