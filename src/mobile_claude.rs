//! Explicit experimental, continuation-only Claude channel binding.
//! Existing native owners are never adopted and lifecycle changes never redirect.
use crate::{core::Pika, model::Provider, paths::Paths, process};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    os::unix::ffi::OsStrExt,
    os::unix::fs::MetadataExt,
    path::PathBuf,
    time::{Duration, Instant},
};

#[path = "mobile_claude_channel.rs"]
pub(crate) mod channel;
#[path = "mobile_claude_launch.rs"]
pub(crate) mod launcher;
pub(crate) use crate::mobile_opencode::private;

pub(crate) fn endpoint_path(paths: &Paths, token: &str) -> Result<PathBuf> {
    let token = uuid::Uuid::parse_str(token)?.simple().to_string();
    let path = paths
        .state_dir
        .join("claude-shared")
        .join(format!("{}.sock", &token[..16]));
    let capacity =
        std::mem::size_of_val(&unsafe { std::mem::zeroed::<libc::sockaddr_un>() }.sun_path);
    ensure!(
        path.as_os_str().as_bytes().len() < capacity,
        "Private Claude endpoint path is too long; choose a shorter Pika state directory"
    );
    Ok(path)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Binding {
    pub version: u32,
    pub token: String,
    pub thread: String,
    pub cwd: String,
    pub native_pid: i64,
    pub native_start: u64,
    pub mcp_pid: i64,
    pub mcp_start: u64,
    pub endpoint: String,
    pub server_name: String,
    pub ready: bool,
}

pub(crate) fn binding_path(paths: &Paths, thread: &str) -> Result<PathBuf> {
    ensure!(
        uuid::Uuid::parse_str(thread)?.to_string() == thread,
        "Canonical Claude UUID required"
    );
    Ok(paths
        .state_dir
        .join("claude-shared")
        .join(format!("{thread}.json")))
}

pub(crate) fn read_binding(paths: &Paths, thread: &str) -> Result<Option<Binding>> {
    let path = binding_path(paths, thread)?;
    let parent = path.parent().context("Native binding parent missing")?;
    if !parent.exists() {
        return Ok(None);
    }
    private::private_directory(parent)?;
    ensure!(
        fs::canonicalize(parent)? == fs::canonicalize(&paths.state_dir)?.join("claude-shared"),
        "Native binding alias rejected"
    );
    let Some(bytes) = private::read_private(&path)? else {
        return Ok(None);
    };
    let binding: Binding = serde_json::from_slice(&bytes)?;
    ensure!(
        binding.version == 1 && binding.thread == thread,
        "Invalid Claude binding"
    );
    uuid::Uuid::parse_str(&binding.token)?;
    ensure!(
        binding.endpoint == endpoint_path(paths, &binding.token)?.to_string_lossy(),
        "Native endpoint escaped binding directory"
    );
    ensure!(
        binding.server_name == format!("pika_{}", uuid::Uuid::parse_str(&binding.token)?.simple()),
        "Native channel name mismatch"
    );
    Ok(Some(binding))
}

pub(crate) fn publish(paths: &Paths, binding: &Binding) -> Result<()> {
    private::write_private(
        &binding_path(paths, &binding.thread)?,
        &serde_json::to_vec(binding)?,
    )
}

pub(crate) fn token_binding(pika: &Pika, token: &str) -> Result<Binding> {
    uuid::Uuid::parse_str(token)?;
    let thread = match pika.store.get_launch_binding(token)? {
        Some((Provider::Claude, id)) => id,
        Some(_) => anyhow::bail!("Wrong provider launch"),
        None => {
            let pending = pika
                .store
                .get_pending(token)?
                .context("Native launch unavailable")?;
            ensure!(
                pending.provider == Provider::Claude,
                "Wrong provider pending launch"
            );
            pending
                .expected_session_id
                .context("Native exact UUID missing")?
        }
    };
    let binding = read_binding(&pika.paths, &thread)?.context("Native binding unavailable")?;
    ensure!(binding.token == token, "Native launch token changed");
    Ok(binding)
}

pub(crate) fn alive(pid: i64, start: u64) -> bool {
    process::process_generation(pid).is_some_and(|g| g.start_time == start)
}

pub(crate) fn native_matches(binding: &Binding) -> bool {
    let Some(record) = process::process_record(binding.native_pid) else {
        return false;
    };
    alive(binding.native_pid, binding.native_start)
        && record.provider() == Some(Provider::Claude)
        && record.argv.windows(2).any(|p| {
            matches!(p[0].as_str(), "--session-id" | "--resume" | "-r") && p[1] == binding.thread
        })
}

pub(crate) fn mark_ready(pika: &Pika, token: &str) -> Result<()> {
    let mut binding = token_binding(pika, token)?;
    ensure!(
        native_matches(&binding) && alive(binding.mcp_pid, binding.mcp_start),
        "Native generation unavailable"
    );
    channel::require_route(pika, &binding, true)?;
    binding.ready = true;
    publish(&pika.paths, &binding)
}

pub(crate) struct Client {
    binding: Binding,
    seen: BTreeMap<String, String>,
    next_poll: Instant,
    source: Option<PathBuf>,
    source_stamp: Option<(u64, u64, u64, i64, i64)>,
}

fn source_stamp(path: &std::path::Path) -> Result<(u64, u64, u64, i64, i64)> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "Native history source replaced"
    );
    Ok((
        metadata.dev(),
        metadata.ino(),
        metadata.len(),
        metadata.mtime(),
        metadata.mtime_nsec(),
    ))
}

fn snapshot_items(page: &Value) -> Vec<Value> {
    page["turns"]["data"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|turn| turn["items"].as_array().into_iter().flatten())
        .cloned()
        .collect()
}

impl Client {
    pub(crate) fn connect(pika: &Pika, thread: &str) -> Result<Option<Self>> {
        let Some(binding) = read_binding(&pika.paths, thread)? else {
            return Ok(None);
        };
        if !alive(binding.native_pid, binding.native_start) {
            // A validated archive can outlive its owner; it grants history, never control.
            return Ok(None);
        }
        let client = Self {
            binding,
            seen: BTreeMap::new(),
            next_poll: Instant::now(),
            source: None,
            source_stamp: None,
        };
        client.require_loaded(pika)?;
        Ok(Some(client))
    }

    pub(crate) fn require_loaded(&self, pika: &Pika) -> Result<()> {
        let current = read_binding(&pika.paths, &self.binding.thread)?
            .context("Native binding disappeared")?;
        ensure!(
            current == self.binding && current.ready,
            "Experimental native channel is not ready"
        );
        ensure!(
            pika.store.is_watched(Provider::Claude, &current.thread)?,
            "Original conversation no longer watched"
        );
        let session = pika
            .store
            .get_session(Provider::Claude, &current.thread)?
            .context("Original named conversation disappeared")?;
        ensure!(
            session.status != crate::model::Status::OpenTwice,
            "Competing native owners"
        );
        let owner = pika
            .store
            .get_recovery_owner(Provider::Claude, &current.thread)?
            .context("Certified native owner unavailable")?;
        ensure!(
            owner.pid == current.native_pid
                && u64::try_from(owner.start_time).ok() == Some(current.native_start)
                && owner.launch_token == current.token,
            "Original native owner changed"
        );
        ensure!(
            pika.store.get_launch_binding(&current.token)?
                == Some((Provider::Claude, current.thread.clone())),
            "Exact launch identity changed"
        );
        ensure!(
            native_matches(&current) && alive(current.mcp_pid, current.mcp_start),
            "Original native generation ended"
        );
        channel::native_child(current.mcp_pid, &current)?;
        channel::require_route(pika, &current, true)
    }

    pub(crate) fn snapshot(&mut self, pika: &Pika) -> Result<Value> {
        let mut result = self.history(pika, None)?;
        self.seen = snapshot_items(&result)
            .into_iter()
            .filter_map(|item| Some((item["id"].as_str()?.to_owned(), item.to_string())))
            .collect();
        result["activeTurnId"] = Value::Null;
        result["capabilities"] = json!({"read":true,"send":true,"answer":false,"approvalTypes":[],"experimentalNotice":"Experimental Claude connection. Approve its channel and tools in the native terminal if prompted. Delivery stays unconfirmed until an attested Claude reply arrives. Do not resend an uncertain operation. Changing sessions in the terminal disconnects this route. A racing notification may still cause Claude activity, but cannot reveal your message in the changed session."});
        Ok(result)
    }

    pub(crate) fn history(&mut self, pika: &Pika, cursor: Option<&str>) -> Result<Value> {
        self.require_loaded(pika)?;
        let Some(source) = self.history_source(pika)? else {
            ensure!(cursor.is_none(), "Empty native history has no older cursor");
            let session = pika
                .store
                .get_session(Provider::Claude, &self.binding.thread)?
                .context("Original conversation missing")?;
            self.require_loaded(pika)?;
            return Ok(
                json!({"thread":{"id":self.binding.thread,"title":session.name,"cwd":session.cwd},"turns":{"data":[],"nextCursor":null,"order":"chronological"},"readOnly":false}),
            );
        };
        let before = source_stamp(&source)?;
        let mut result = crate::mobile_history::page(
            &pika.paths,
            &pika.config,
            Provider::Claude,
            &self.binding.thread,
            cursor,
        )?;
        self.require_loaded(pika)?;
        result["readOnly"] = json!(false);
        if cursor.is_none() {
            self.source_stamp = (before == source_stamp(&source)?).then_some(before);
            self.source = Some(source);
        }
        Ok(result)
    }

    fn history_source(&self, pika: &Pika) -> Result<Option<PathBuf>> {
        if let Some(source) = &self.source {
            return Ok(Some(source.clone()));
        }
        match crate::mobile_history::source_path(
            &pika.paths,
            &pika.config,
            Provider::Claude,
            &self.binding.thread,
        ) {
            Ok(source) => Ok(Some(source)),
            Err(error) => {
                if crate::mobile_history::claude_registry_without_history(
                    &pika.paths,
                    &pika.config,
                    &self.binding.thread,
                )? {
                    Ok(None)
                } else {
                    Err(error)
                }
            }
        }
    }

    pub(crate) fn send(
        &mut self,
        pika: &Pika,
        id: &str,
        text: &str,
        expected: Option<&str>,
    ) -> Result<Value> {
        self.require_loaded(pika)?;
        ensure!(
            expected.is_none(),
            "Native steering is unavailable; use its terminal"
        );
        let result = channel::rpc(
            &self.binding,
            json!({"action":"send","operation_id":id,"text":text}),
        )?;
        self.require_loaded(pika)?;
        Ok(result)
    }

    pub(crate) fn receipt(&mut self, pika: &Pika, id: &str) -> Result<Option<Value>> {
        channel::receipt(pika, &self.binding, id)
    }

    pub(crate) fn poll(&mut self, pika: &Pika) -> Result<Vec<Value>> {
        if Instant::now() < self.next_poll {
            return Ok(vec![]);
        }
        self.next_poll = Instant::now() + Duration::from_secs(1);
        self.require_loaded(pika)?;
        if let Some(source) = &self.source {
            if self.source_stamp == Some(source_stamp(source)?) {
                return Ok(vec![]);
            }
        }
        let page = self.history(pika, None)?;
        let mut events = vec![];
        let mut seen = BTreeMap::new();
        for item in snapshot_items(&page) {
            let id = item["id"]
                .as_str()
                .context("Native projected item identity missing")?
                .to_owned();
            let encoded = item.to_string();
            if self.seen.get(&id) != Some(&encoded) {
                events.push(json!({"method":"item/completed","params":{"threadId":self.binding.thread,"turnId":id,"item":item}}));
            }
            seen.insert(id, encoded);
        }
        self.seen = seen;
        Ok(events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Seek, SeekFrom, Write};

    #[test]
    fn idle_large_history_probe_reads_metadata_only_and_notices_append() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("synthetic-history.jsonl");
        let mut file = fs::File::create(&path).unwrap();
        file.set_len(12 * 1024 * 1024).unwrap();
        let original = source_stamp(&path).unwrap();
        let start = Instant::now();
        for _ in 0..1000 {
            assert_eq!(source_stamp(&path).unwrap(), original);
        }
        eprintln!(
            "Claude idle 12MiB source metadata probes1000 elapsed {:?}",
            start.elapsed()
        );
        file.seek(SeekFrom::End(0)).unwrap();
        file.write_all(b"append").unwrap();
        assert_ne!(source_stamp(&path).unwrap(), original);
        let link = root.path().join("replacement");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(source_stamp(&link).is_err());
    }
}
