//! Local foreground ownership and bounded IPC. No TCP listener or installed service.
use anyhow::{Context, Result, bail};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::{
        fs::{OpenOptionsExt, PermissionsExt},
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const PROTOCOL: u32 = 1;
const MAX_FRAME: usize = 64 * 1024;
const MAX_REPLY: usize = 1024 * 1024;
const MAX_VIEWS: usize = 32;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    protocol: u32,
    generation: Option<String>,
    payload: Value,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Reply {
    protocol: u32,
    generation: String,
    payload: Option<Value>,
    error: Option<String>,
}

/// Refuse symlinks and permissive state roots; never chmod someone else's path.
pub(crate) fn private_root(root: &Path) -> Result<()> {
    crate::assistant_storage::directory(root)?;
    Ok(())
}

pub(crate) struct Owner {
    _lock: File,
    listener: UnixListener,
    endpoint: PathBuf,
    generation: String,
}

impl Owner {
    pub(crate) fn acquire(root: &Path) -> Result<Self> {
        private_root(root)?;
        crate::assistant_storage::file(&root.join("owner.lock"))?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(root.join("owner.lock"))?;
        if !lock.metadata()?.is_file() {
            bail!("Invalid assistant owner lock");
        }
        lock.try_lock_exclusive()
            .context("Another foreground Pika owns this profile")?;
        let endpoint = root.join("view.sock");
        // Only the holder of the OS lock may remove a previous owner's socket.
        if let Ok(metadata) = fs::symlink_metadata(&endpoint) {
            use std::os::unix::fs::FileTypeExt;
            if !metadata.file_type().is_socket() {
                bail!("Invalid assistant endpoint; not replacing it");
            }
            fs::remove_file(&endpoint)?;
        }
        let listener =
            UnixListener::bind(&endpoint).context("Cannot bind the private assistant endpoint")?;
        fs::set_permissions(&endpoint, fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        let epoch = root.join("owner.sqlite");
        crate::assistant_storage::database(&epoch)?;
        let mut db = rusqlite::Connection::open(epoch)?;
        db.execute_batch("CREATE TABLE IF NOT EXISTS owner_epoch(id INTEGER PRIMARY KEY CHECK(id=1), epoch INTEGER NOT NULL); INSERT OR IGNORE INTO owner_epoch VALUES(1,0);")?;
        let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let previous: i64 =
            tx.query_row("SELECT epoch FROM owner_epoch WHERE id=1", [], |row| {
                row.get(0)
            })?;
        let generation = previous
            .checked_add(1)
            .context("Assistant owner generation exhausted")?;
        tx.execute("UPDATE owner_epoch SET epoch=? WHERE id=1", [generation])?;
        tx.commit()?;
        Ok(Self {
            _lock: lock,
            listener,
            endpoint,
            generation: generation.to_string(),
        })
    }

    /// Single writer, bounded views and frames. Last view closing ends the host.
    #[cfg(test)]
    pub(crate) fn serve(self, mut handle: impl FnMut(Value) -> Result<Value>) -> Result<()> {
        self.serve_with_idle(move |request| request.map(&mut handle).transpose())
    }

    pub(crate) fn serve_with_idle(
        self,
        mut handle: impl FnMut(Option<Value>) -> Result<Option<Value>>,
    ) -> Result<()> {
        let started = Instant::now();
        let mut last_tick = Instant::now();
        let mut seen_view = false;
        let mut views: Vec<View> = Vec::new();
        loop {
            for _ in 0..MAX_VIEWS {
                match self.listener.accept() {
                    Ok((stream, _)) if views.len() < MAX_VIEWS => {
                        stream.set_nonblocking(true)?;
                        views.push(View {
                            stream,
                            input: Vec::new(),
                            output: Vec::new(),
                            written: 0,
                            touched: Instant::now(),
                        });
                        seen_view = true;
                    }
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(error) => return Err(error.into()),
                }
            }
            views.retain_mut(|view| {
                view.tick(&self.generation, &mut |request| {
                    handle(Some(request))?.context("Missing assistant reply")
                })
                .is_ok()
            });
            if views.is_empty() && (seen_view || started.elapsed() > Duration::from_secs(3)) {
                return Ok(());
            }
            if last_tick.elapsed() >= Duration::from_millis(100) {
                handle(None)?;
                last_tick = Instant::now();
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Owner {
    fn drop(&mut self) {
        // Still hold the lock: a successor cannot have bound this path yet.
        let _ = fs::remove_file(&self.endpoint);
    }
}

struct View {
    stream: UnixStream,
    input: Vec<u8>,
    output: Vec<u8>,
    written: usize,
    touched: Instant,
}

impl View {
    fn tick(
        &mut self,
        generation: &str,
        handle: &mut impl FnMut(Value) -> Result<Value>,
    ) -> Result<()> {
        if self.output.is_empty() {
            let mut chunk = [0; 4096];
            match self.stream.read(&mut chunk) {
                Ok(0) => bail!("View closed"),
                Ok(count) => {
                    if self.input.len() + count > MAX_FRAME {
                        bail!("Assistant frame too large");
                    }
                    self.input.extend_from_slice(&chunk[..count]);
                    self.touched = Instant::now();
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(error.into()),
            }
            if let Some(end) = self.input.iter().position(|byte| *byte == b'\n') {
                let result = decode(&self.input[..end], generation).and_then(handle);
                self.input.drain(..=end);
                let (payload, error) = match result {
                    Ok(value) => (Some(value), None),
                    Err(error) => (None, Some(error.to_string())),
                };
                self.output = serde_json::to_vec(&Reply {
                    protocol: PROTOCOL,
                    generation: generation.into(),
                    payload,
                    error,
                })?;
                if self.output.len() > MAX_REPLY {
                    bail!("Assistant reply exceeds bound");
                }
                self.output.push(b'\n');
                self.written = 0;
                self.touched = Instant::now();
            }
        }
        if !self.output.is_empty() {
            match self.stream.write(&self.output[self.written..]) {
                Ok(0) => bail!("View closed"),
                Ok(count) => self.written += count,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(error.into()),
            }
            if self.written == self.output.len() {
                self.output.clear();
            }
        }
        if (!self.input.is_empty() || !self.output.is_empty())
            && self.touched.elapsed() > Duration::from_secs(2)
        {
            bail!("Slow or incomplete assistant frame");
        }
        Ok(())
    }
}

fn decode(bytes: &[u8], generation: &str) -> Result<Value> {
    let request: Request = serde_json::from_slice(bytes)?;
    if request.protocol != PROTOCOL {
        bail!("Assistant protocol mismatch; reopen using the same Pika build");
    }
    if request
        .generation
        .as_deref()
        .is_some_and(|value| value != generation)
    {
        bail!("Assistant owner changed; action was not sent");
    }
    Ok(request.payload)
}

pub(crate) struct Client {
    stream: UnixStream,
    generation: Option<String>,
    child: Option<Child>,
}

impl Client {
    fn connect(root: &Path) -> Result<Self> {
        let stream = UnixStream::connect(root.join("view.sock"))?;
        stream.set_read_timeout(Some(Duration::from_secs(2)))?;
        stream.set_write_timeout(Some(Duration::from_secs(2)))?;
        Ok(Self {
            stream,
            generation: None,
            child: None,
        })
    }

    pub(crate) fn attach(root: &Path) -> Result<Self> {
        private_root(root)?;
        if let Ok(client) = Self::connect(root) {
            return Ok(client);
        }
        let mut child = Command::new(std::env::current_exe()?)
            .arg("_assistant-host")
            .arg("--root")
            .arg(root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(2) {
            if let Ok(mut client) = Self::connect(root) {
                client.child = Some(child);
                return Ok(client);
            }
            thread::sleep(Duration::from_millis(20));
        }
        // An uncertain owner is never killed or replaced behind its lock.
        let _ = child.try_wait();
        thread::spawn(move || {
            let _ = child.wait();
        });
        bail!("Pika assistant is unavailable. No action was retried; reopen the view to reconnect.")
    }

    pub(crate) fn request(&mut self, payload: Value) -> Result<Value> {
        let request = Request {
            protocol: PROTOCOL,
            generation: self.generation.clone(),
            payload,
        };
        let mut bytes = serde_json::to_vec(&request)?;
        if bytes.len() >= MAX_FRAME {
            bail!("Assistant request exceeds 64 KiB");
        }
        bytes.push(b'\n');
        self.stream.write_all(&bytes)?;
        let mut reply = Vec::new();
        // One outstanding request per connection; unsolicited/pipelined replies
        // are a protocol error, not a second action receipt.
        let started = Instant::now();
        loop {
            if started.elapsed() > Duration::from_secs(2) {
                bail!("Assistant receipt unknown; no automatic retry");
            }
            let mut chunk = [0; 8192];
            let count = self
                .stream
                .read(&mut chunk)
                .context("Assistant receipt unknown; no automatic retry")?;
            if count == 0 {
                bail!("Assistant receipt unknown; connection closed");
            }
            let end = chunk[..count].iter().position(|b| *b == b'\n');
            let payload_len = end.unwrap_or(count);
            if reply.len() + payload_len > MAX_REPLY {
                bail!("Assistant reply exceeds bound");
            }
            reply.extend_from_slice(&chunk[..payload_len]);
            if let Some(end) = end {
                if end + 1 != count {
                    bail!("Unexpected extra assistant receipt");
                }
                break;
            }
        }
        let reply: Reply = serde_json::from_slice(&reply)?;
        if reply.protocol != PROTOCOL
            || self
                .generation
                .as_ref()
                .is_some_and(|old| old != &reply.generation)
        {
            bail!("Assistant owner/protocol changed; receipt not accepted");
        }
        self.generation = Some(reply.generation);
        if let Some(error) = reply.error {
            bail!("{error}");
        }
        reply.payload.context("Assistant reply missing")
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
        if let Some(mut child) = self.child.take() {
            thread::spawn(move || {
                let _ = child.wait();
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn two_views_share_one_owner_and_disconnect_independently() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("a");
        let owner = Owner::acquire(&root).unwrap();
        assert!(Owner::acquire(&root).is_err());
        let mut one = Client::connect(&root).unwrap();
        let mut two = Client::connect(&root).unwrap();
        let thread = thread::spawn(move || {
            let mut counter = 0;
            owner
                .serve(move |_| {
                    counter += 1;
                    Ok(json!(counter))
                })
                .unwrap();
        });
        assert_eq!(one.request(json!(null)).unwrap(), json!(1));
        assert_eq!(two.request(json!(null)).unwrap(), json!(2));
        assert_eq!(one.generation, two.generation);
        drop(one);
        assert_eq!(two.request(json!(null)).unwrap(), json!(3));
        drop(two);
        thread.join().unwrap();
        assert!(!root.join("view.sock").exists());
        Owner::acquire(&root).unwrap();
    }

    #[test]
    fn owner_lock_outlives_closure_owned_cleanup() {
        struct Cleanup {
            entered: std::sync::mpsc::Sender<()>,
            release: std::sync::mpsc::Receiver<()>,
        }
        impl Drop for Cleanup {
            fn drop(&mut self) {
                self.entered.send(()).unwrap();
                self.release.recv_timeout(Duration::from_secs(3)).unwrap();
            }
        }
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("a");
        let owner = Owner::acquire(&root).unwrap();
        let mut view = Client::connect(&root).unwrap();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let cleanup = Cleanup {
            entered: entered_tx,
            release: release_rx,
        };
        let host = thread::spawn(move || {
            owner
                .serve(move |_| {
                    let _owned = &cleanup;
                    Ok(json!("ready"))
                })
                .unwrap();
        });
        view.request(json!(null)).unwrap();
        drop(view);
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let premature = Owner::acquire(&root);
        release_tx.send(()).unwrap();
        host.join().unwrap();
        assert!(
            premature.is_err(),
            "owner escaped before owned services finished cleanup"
        );
        Owner::acquire(&root).unwrap();
    }

    #[test]
    fn stale_endpoint_takeover_requires_lock_and_generation_matches() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("a");
        private_root(&root).unwrap();
        drop(UnixListener::bind(root.join("view.sock")).unwrap());
        let owner = Owner::acquire(&root).unwrap();
        assert!(
            decode(
                br#"{"protocol":1,"generation":"old","payload":null}"#,
                &owner.generation
            )
            .is_err()
        );
        assert!(
            decode(
                br#"{"protocol":2,"generation":null,"payload":null}"#,
                &owner.generation
            )
            .is_err()
        );
    }

    #[test]
    fn symlink_state_and_endpoint_are_not_followed() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("a");
        std::os::unix::fs::symlink(tmp.path(), &root).unwrap();
        assert!(Owner::acquire(&root).is_err());
    }
}
