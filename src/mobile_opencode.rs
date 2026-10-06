//! Future-launch native OpenCode shared connection. Existing owners are never adopted.
use crate::{
    core::Pika,
    model::Provider,
    paths::Paths,
    process::{self, ProcessRecord},
    store::RecoveryOwner,
};
use anyhow::{Context, Result, ensure};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeMap, fs, path::PathBuf};

#[path = "mobile_opencode_launch.rs"]
mod launcher;
#[path = "mobile_opencode_private.rs"]
pub(crate) mod private;
#[path = "mobile_opencode_transport.rs"]
mod transport;
pub(crate) use launcher::{launch, supports_launch};

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Binding {
    version: u32,
    token: String,
    thread: String,
    cwd: String,
    server_pid: i64,
    server_start: u64,
    native_pid: i64,
    native_start: u64,
    supervisor_pid: i64,
    supervisor_start: u64,
    port: u16,
    password: String,
}

fn binding_path(paths: &Paths, thread: &str) -> Result<PathBuf> {
    ensure!(
        thread.starts_with("ses_")
            && thread.len() <= 128
            && thread
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_'),
        "Invalid exact OpenCode session identity"
    );
    Ok(paths
        .state_dir
        .join("opencode-shared")
        .join(format!("{thread}.json")))
}

fn read_binding(paths: &Paths, thread: &str) -> Result<Option<Binding>> {
    let path = binding_path(paths, thread)?;
    let parent = path
        .parent()
        .context("Private certificate directory missing")?;
    if !parent.exists() {
        return Ok(None);
    }
    private::private_directory(parent)?;
    ensure!(
        fs::canonicalize(parent)? == fs::canonicalize(&paths.state_dir)?.join("opencode-shared"),
        "Native certificate directory alias rejected"
    );
    let Some(bytes) = private::read_private(&path)? else {
        return Ok(None);
    };
    let binding: Binding = serde_json::from_slice(&bytes)?;
    ensure!(
        binding.version == 1 && binding.thread == thread && binding.port != 0,
        "Invalid OpenCode shared certificate"
    );
    Ok(Some(binding))
}

fn alive(pid: i64, start: u64) -> bool {
    process::process_generation(pid).is_some_and(|actual| actual.start_time == start)
}

fn matches_owner(binding: &Binding, owner: &RecoveryOwner) -> bool {
    owner.provider == Provider::Opencode
        && owner.session_id == binding.thread
        && owner.launch_token == binding.token
        && owner.pid == binding.native_pid
        && u64::try_from(owner.start_time).ok() == Some(binding.native_start)
        && alive(binding.native_pid, binding.native_start)
        && alive(binding.server_pid, binding.server_start)
        && pair_processes(binding)
}

fn pair_processes(binding: &Binding) -> bool {
    let Some(native) = process::process_record(binding.native_pid) else {
        return false;
    };
    let Some(server) = process::process_record(binding.server_pid) else {
        return false;
    };
    native.parent_pid == Some(binding.supervisor_pid)
        && server.parent_pid == Some(binding.supervisor_pid)
        && alive(binding.supervisor_pid, binding.supervisor_start)
        && native.provider() == Some(Provider::Opencode)
        && server.provider() == Some(Provider::Opencode)
        && native
            .argv
            .windows(2)
            .any(|pair| pair[0] == "--session" && pair[1] == binding.thread)
        && server.argv.iter().any(|arg| arg == "serve")
}

/// Suppress only this certificate's advisory server lease, never another OpenCode owner.
pub(crate) fn paired_server(paths: &Paths, owner: &RecoveryOwner, server: &ProcessRecord) -> bool {
    read_binding(paths, &owner.session_id)
        .ok()
        .flatten()
        .is_some_and(|binding| {
            matches_owner(&binding, owner)
                && server.pid == binding.server_pid
                && server.start_time == binding.server_start
                && server.provider() == Some(Provider::Opencode)
        })
}

pub(crate) struct Client {
    binding: Binding,
    seen: BTreeMap<String, String>,
    next_poll: std::time::Instant,
}

impl Client {
    pub(crate) fn connect(pika: &Pika, thread: &str) -> Result<Option<Self>> {
        let Some(binding) = read_binding(&pika.paths, thread)? else {
            return Ok(None);
        };
        if !alive(binding.native_pid, binding.native_start) {
            // A validated certificate can survive its terminal; it grants no live control.
            return Ok(None);
        }
        let client = Self {
            binding,
            seen: BTreeMap::new(),
            next_poll: std::time::Instant::now(),
        };
        client.require_loaded(pika)?;
        Ok(Some(client))
    }

    fn require_owner(&self, pika: &Pika) -> Result<()> {
        ensure!(
            pika.store
                .is_watched(Provider::Opencode, &self.binding.thread)?,
            "Original native conversation is no longer watched"
        );
        let session = pika
            .store
            .get_session(Provider::Opencode, &self.binding.thread)?
            .context("Original named conversation disappeared")?;
        ensure!(
            session.status != crate::model::Status::OpenTwice,
            "Original native conversation has competing owners"
        );
        let binding = read_binding(&pika.paths, &self.binding.thread)?
            .context("Shared owner certificate disappeared")?;
        ensure!(binding == self.binding, "Shared owner certificate changed");
        let owner = pika
            .store
            .get_recovery_owner(Provider::Opencode, &binding.thread)?
            .context("Original native owner unavailable")?;
        ensure!(
            matches_owner(&binding, &owner),
            "Original native owner changed"
        );
        ensure!(
            pika.store.get_launch_binding(&binding.token)?
                == Some((Provider::Opencode, binding.thread.clone())),
            "Native launch identity changed"
        );
        Ok(())
    }

    fn request(
        &self,
        pika: &Pika,
        method: &str,
        suffix: &str,
        body: Option<Value>,
    ) -> Result<Value> {
        self.require_owner(pika)?;
        let result = transport::request(
            &self.binding,
            method,
            &format!("/session/{}{suffix}", self.binding.thread),
            body,
        )?;
        self.require_owner(pika)?;
        Ok(result)
    }

    pub(crate) fn require_loaded(&self, pika: &Pika) -> Result<()> {
        let session = self.request(pika, "GET", "", None)?;
        ensure!(
            session["id"].as_str() == Some(&self.binding.thread),
            "Native session mismatch"
        );
        ensure!(
            session["revert"].is_null(),
            "Native reverted history needs terminal recovery before shared mobile access"
        );
        let directory = session["directory"]
            .as_str()
            .context("Native session directory missing")?;
        ensure!(
            fs::canonicalize(directory)? == fs::canonicalize(&self.binding.cwd)?,
            "Native session directory mismatch"
        );
        Ok(())
    }

    fn messages(&self, pika: &Pika, cursor: Option<&str>) -> Result<(Vec<Value>, Option<String>)> {
        let suffix = match cursor {
            None => "/message?limit=100".to_owned(),
            Some(cursor) => {
                let native = self.decode_cursor(cursor)?;
                format!("/message?limit=100&before={}", query_escape(&native))
            }
        };
        let value = self.request(pika, "GET", &suffix, None)?;
        let data = value["data"]
            .as_array()
            .context("Invalid native message page")?;
        ensure!(data.len() <= 100, "Native page exceeded bounded size");
        ensure!(
            data.iter()
                .all(|message| message["info"]["sessionID"].as_str() == Some(&self.binding.thread)),
            "Native message page belongs to a different session"
        );
        let next=value["nativeNextCursor"].as_str().map(|native|base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json!({"version":1,"thread":self.binding.thread,"launch":self.binding.token,"before":native}).to_string()));
        Ok((data.clone(), next))
    }

    fn decode_cursor(&self, cursor: &str) -> Result<String> {
        ensure!(cursor.len() <= 4096, "Native history cursor too large");
        let value: Value = serde_json::from_slice(
            &base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(cursor)?,
        )?;
        ensure!(
            value["version"] == 1
                && value["thread"].as_str() == Some(&self.binding.thread)
                && value["launch"].as_str() == Some(&self.binding.token),
            "History cursor belongs to another native launch"
        );
        Ok(value["before"]
            .as_str()
            .context("Native cursor missing")?
            .to_owned())
    }

    pub(crate) fn history(&mut self, pika: &Pika, cursor: Option<&str>) -> Result<Value> {
        self.require_loaded(pika)?;
        let (mut data, next) = self.messages(pika, cursor)?;
        sort_messages(&mut data);
        let turns = data.iter().filter_map(canonical_turn).collect::<Vec<_>>();
        Ok(
            json!({"thread":{"id":self.binding.thread},"turns":{"data":turns,"nextCursor":next,"order":"chronological"},"activeTurnId":null}),
        )
    }

    pub(crate) fn snapshot(&mut self, pika: &Pika) -> Result<Value> {
        let value = self.history(pika, None)?;
        for turn in value["turns"]["data"].as_array().into_iter().flatten() {
            for item in turn["items"].as_array().into_iter().flatten() {
                if let (Some(id), Some(text)) = (item["id"].as_str(), item["text"].as_str()) {
                    self.seen.insert(id.to_owned(), text.to_owned());
                }
            }
        }
        Ok(value)
    }

    pub(crate) fn send(
        &mut self,
        pika: &Pika,
        id: &str,
        text: &str,
        expected_turn: Option<&str>,
    ) -> Result<Value> {
        ensure!(
            expected_turn.is_none(),
            "Native concurrent-turn steering is unavailable"
        );
        ensure!(
            !text.is_empty() && text.len() <= 64 * 1024,
            "Native message is empty or oversized"
        );
        self.require_loaded(pika)?;
        let native = self.allocate_native_id(pika, id)?;
        let result = self.request(
            pika,
            "POST",
            "/prompt_async",
            Some(json!({"messageID":native,"parts":[{"type":"text","text":text}]})),
        );
        let state = if result.is_ok() {
            "accepted"
        } else {
            "unknown"
        };
        Ok(json!({"state":state,"messageId":id,"nativeMessageId":native,"turnId":null}))
    }

    pub(crate) fn receipt(&mut self, pika: &Pika, id: &str) -> Result<Option<Value>> {
        let Some(native) = pika.store.get_meta(&operation_key(&self.binding, id)?)? else {
            return Ok(None);
        };
        let message = self.request(pika, "GET", &format!("/message/{native}"), None)?;
        ensure!(
            message["info"]["id"].as_str() == Some(&native)
                && message["info"]["role"] == "user"
                && message["info"]["sessionID"].as_str() == Some(&self.binding.thread),
            "Native exact receipt mismatch"
        );
        Ok(Some(
            json!({"state":"delivered","messageId":id,"nativeMessageId":native,"turnId":null}),
        ))
    }

    pub(crate) fn poll(&mut self, pika: &Pika) -> Result<Vec<Value>> {
        if std::time::Instant::now() < self.next_poll {
            return Ok(Vec::new());
        }
        self.next_poll = std::time::Instant::now() + std::time::Duration::from_millis(500);
        self.require_loaded(pika)?;
        let (mut messages, _) = self.messages(pika, None)?;
        sort_messages(&mut messages);
        let mut events = Vec::new();
        let mut current = BTreeMap::new();
        for turn in messages.iter().filter_map(canonical_turn) {
            for item in turn["items"].as_array().into_iter().flatten() {
                let id = item["id"].as_str().context("Native item ID missing")?;
                let text = item["text"].as_str().context("Native text missing")?;
                if self.seen.get(id).is_none_or(|old| old != text) {
                    events.push(json!({"method":"item/completed","params":{"threadId":self.binding.thread,"turnId":turn["id"],"item":item}}));
                }
                current.insert(id.to_owned(), text.to_owned());
            }
        }
        self.seen = current;
        Ok(events)
    }

    fn allocate_native_id(&self, pika: &Pika, id: &str) -> Result<String> {
        let key = operation_key(&self.binding, id)?;
        ensure!(
            pika.store.get_meta(&key)?.is_none(),
            "Native operation already allocated; reconcile receipt without replay"
        );
        let millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_millis();
        let base = ((millis << 12) & 0xffffffffffff) as u64;
        let native_max = self
            .messages(pika, None)?
            .0
            .iter()
            .filter_map(|message| message["info"]["id"].as_str())
            .filter_map(|id| id.strip_prefix("msg_"))
            .filter_map(|id| id.get(..12))
            .filter_map(|id| u64::from_str_radix(id, 16).ok())
            .max()
            .unwrap_or(0);
        let timestamp = base.max(native_max.checked_add(1).context("Native ID overflow")?);
        ensure!(
            timestamp <= 0xffffffffffff,
            "Native timestamp counter exhausted"
        );
        let random = uuid::Uuid::new_v4();
        let alphabet = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
        let suffix = random.as_bytes()[..14]
            .iter()
            .map(|byte| char::from(alphabet[usize::from(*byte) % 62]))
            .collect::<String>();
        let native = format!("msg_{timestamp:012x}{suffix}");
        pika.store.set_meta(&key, &native)?;
        Ok(native)
    }
}

fn operation_key(binding: &Binding, id: &str) -> Result<String> {
    let uuid = uuid::Uuid::parse_str(id).context("Stable native message UUID required")?;
    Ok(format!(
        "opencode-mobile-message:{}:{}:{}",
        binding.token,
        binding.thread,
        uuid.simple()
    ))
}

fn sort_messages(data: &mut [Value]) {
    data.sort_by(|a, b| {
        a["info"]["time"]["created"]
            .as_u64()
            .cmp(&b["info"]["time"]["created"].as_u64())
            .then_with(|| a["info"]["id"].as_str().cmp(&b["info"]["id"].as_str()))
    });
}

fn query_escape(value: &str) -> String {
    value
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || b"_-".contains(&byte) {
                char::from(byte).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

fn canonical_turn(message: &Value) -> Option<Value> {
    let info = &message["info"];
    let id = info["id"].as_str()?;
    let kind = match info["role"].as_str()? {
        "user" => "userMessage",
        "assistant" => "agentMessage",
        _ => return None,
    };
    let items = message["parts"]
        .as_array()?
        .iter()
        .filter(|part| {
            part["type"] == "text"
                && !part["synthetic"].as_bool().unwrap_or(false)
                && !part["ignored"].as_bool().unwrap_or(false)
        })
        .filter_map(|part| {
            Some(json!({"id":part["id"].as_str()?,"type":kind,"text":part["text"].as_str()?}))
        })
        .collect::<Vec<_>>();
    Some(
        json!({"id":id,"status":if kind == "userMessage" || info["time"]["completed"].is_number(){"completed"}else{"inProgress"},"items":items}),
    )
}
