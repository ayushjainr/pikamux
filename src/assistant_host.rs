//! Local ownership and bounded IPC. Foreground by default; separately approved
//! background lifetime uses the same lock. No TCP listener or installed service.
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
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const PROTOCOL: u32 = 1;
const MAX_FRAME: usize = 64 * 1024;
const MAX_REPLY: usize = 1024 * 1024;
const MAX_VIEWS: usize = 32;
const ATTACH_HELLO: &str = "__pika_transport_hello";

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

pub(crate) fn verify_existing_profile(root: &Path, profile: &str) -> Result<()> {
    if !root.is_absolute() || uuid::Uuid::parse_str(profile)?.to_string() != profile {
        bail!("Existing assistant attachment needs an absolute root and canonical profile ID");
    }
    let path = root.join("memory.sqlite");
    crate::assistant_storage::existing_database(&path)?;
    let db =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    db.busy_timeout(Duration::from_millis(50))?;
    let actual: String = db.query_row(
        "SELECT value FROM memory_meta WHERE key='profile_id'",
        [],
        |row| row.get(0),
    )?;
    if actual != profile {
        bail!("Assistant profile changed; existing-authority attachment refused");
    }
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
        let generation = next_owner_generation(root)?;
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

    #[cfg(test)]
    pub(crate) fn serve_with_idle(
        self,
        handle: impl FnMut(Option<Value>) -> Result<Option<Value>>,
    ) -> Result<()> {
        self.serve_with_lifetime(handle, crate::assistant_lifecycle::LifetimeGate::default())
    }

    /// Only the independently approved lifecycle gate can extend ownership
    /// beyond the last view. Provider text and ordinary view traffic cannot.
    pub(crate) fn serve_with_lifetime(
        self,
        mut handle: impl FnMut(Option<Value>) -> Result<Option<Value>>,
        lifetime: crate::assistant_lifecycle::LifetimeGate,
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
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .ok()
                    .and_then(|time| i64::try_from(time.as_secs()).ok());
                if !now.is_some_and(|now| lifetime.keeps_alive(now)) {
                    return Ok(());
                }
            }
            if last_tick.elapsed() >= Duration::from_millis(100) {
                handle(None)?;
                last_tick = Instant::now();
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
}

fn next_owner_generation(root: &Path) -> Result<i64> {
    let epoch = root.join("owner.sqlite");
    crate::assistant_storage::database(&epoch)?;
    let mut db = rusqlite::Connection::open(epoch)?;
    db.execute_batch("CREATE TABLE IF NOT EXISTS owner_epoch(id INTEGER PRIMARY KEY CHECK(id=1), epoch INTEGER NOT NULL); INSERT OR IGNORE INTO owner_epoch VALUES(1,0);")?;
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let previous: i64 = tx.query_row("SELECT epoch FROM owner_epoch WHERE id=1", [], |row| {
        row.get(0)
    })?;
    let generation = previous
        .checked_add(1)
        .context("Assistant owner generation exhausted")?;
    tx.execute("UPDATE owner_epoch SET epoch=? WHERE id=1", [generation])?;
    tx.commit()?;
    Ok(generation)
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
            self.read_input()?;
            if let Some(end) = self.input.iter().position(|byte| *byte == b'\n') {
                let result = decode(&self.input[..end], generation).and_then(|payload| {
                    if payload == Value::String(ATTACH_HELLO.into()) {
                        // Attachment proves this connection reached a serving owner.
                        // It must never enter the domain handler or start work.
                        Ok(Value::String(ATTACH_HELLO.into()))
                    } else {
                        handle(payload)
                    }
                });
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
                    self.output = serde_json::to_vec(&Reply {
                        protocol: PROTOCOL,
                        generation: generation.into(),
                        payload: None,
                        error: Some("Response exceeds the display limit. The request may have completed; do not retry it blindly. Use /memory pages or /memory-record ID chunks to inspect saved data.".into()),
                    })?;
                }
                self.output.push(b'\n');
                self.written = 0;
                self.touched = Instant::now();
            }
        }
        if !self.output.is_empty() {
            self.write_output()?;
        }
        if (!self.input.is_empty() || !self.output.is_empty())
            && self.touched.elapsed() > Duration::from_secs(2)
        {
            bail!("Slow or incomplete assistant frame");
        }
        Ok(())
    }

    fn read_input(&mut self) -> Result<()> {
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
        Ok(())
    }

    fn write_output(&mut self) -> Result<()> {
        match self.stream.write(&self.output[self.written..]) {
            Ok(0) => bail!("View closed"),
            Ok(count) => {
                self.written += count;
                self.touched = Instant::now();
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(error.into()),
        }
        if self.written == self.output.len() {
            self.output.clear();
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
        Self::attach_profile(root, None)
    }

    pub(crate) fn attach_existing_profile(root: &Path, profile: &str) -> Result<Self> {
        verify_existing_profile(root, profile)?;
        Self::attach_profile(root, Some(profile))
    }

    fn attach_profile(root: &Path, profile: Option<&str>) -> Result<Self> {
        private_root(root)?;
        Self::attach_with_start(root, || Self::start_owner(root, profile))
    }

    fn start_owner(root: &Path, profile: Option<&str>) -> Result<Child> {
        let mut command = Command::new(std::env::current_exe()?);
        command.arg("_assistant-host").arg("--root").arg(root);
        if let Some(profile) = profile {
            command.arg("--expected-profile-id").arg(profile);
        }
        Ok(command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?)
    }

    fn connect_ready(root: &Path) -> Result<Self> {
        let mut client = Self::connect(root)?;
        if client.request(Value::String(ATTACH_HELLO.into()))? != Value::String(ATTACH_HELLO.into())
        {
            bail!("Assistant attachment handshake mismatch");
        }
        Ok(client)
    }

    fn attach_with_start(root: &Path, mut start: impl FnMut() -> Result<Child>) -> Result<Self> {
        let mut child = None;
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(2) {
            // Retry only an inert hello, before any user operation is sent. A
            // successful connect alone may be queued on an exiting listener.
            if let Ok(mut client) = Self::connect_ready(root) {
                client.child = child;
                return Ok(client);
            }
            if child.is_none() && owner_lock_available(root)? {
                child = Some(start()?);
            }
            thread::sleep(Duration::from_millis(20));
        }
        // An uncertain owner is never killed or replaced behind its lock.
        if let Some(mut child) = child {
            thread::spawn(move || {
                let _ = child.wait();
            });
        }
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
        // The socket's read timeout bounds each stalled read. A whole-reply
        // deadline would reject a bounded reply that is still making progress.
        loop {
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

fn owner_lock_available(root: &Path) -> Result<bool> {
    let lock = match OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(root.join("owner.lock"))
    {
        Ok(lock) => lock,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(error) => return Err(error.into()),
    };
    if !lock.metadata()?.is_file() {
        bail!("Invalid assistant owner lock");
    }
    match lock.try_lock_exclusive() {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(false),
        Err(error) => Err(error.into()),
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
    fn client_accepts_a_progressing_reply_beyond_two_seconds_without_replay() {
        let (stream, mut peer) = UnixStream::pair().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let server = thread::spawn(move || {
            let mut input = std::io::BufReader::new(peer.try_clone().unwrap());
            let mut line = String::new();
            std::io::BufRead::read_line(&mut input, &mut line).unwrap();
            assert_eq!(
                serde_json::from_str::<Request>(&line).unwrap().payload,
                "once"
            );
            let mut bytes = serde_json::to_vec(&Reply {
                protocol: PROTOCOL,
                generation: "one".into(),
                payload: Some(json!("complete")),
                error: None,
            })
            .unwrap();
            bytes.push(b'\n');
            // Each read progresses before the socket idle deadline, but the
            // complete reply crosses the old absolute two-second cutoff.
            for piece in bytes.chunks(bytes.len().div_ceil(5)) {
                thread::sleep(Duration::from_millis(550));
                peer.write_all(piece).unwrap();
            }
            peer.set_nonblocking(true).unwrap();
            let mut extra = [0];
            assert_eq!(
                peer.read(&mut extra).unwrap_err().kind(),
                std::io::ErrorKind::WouldBlock
            );
        });
        let mut client = Client {
            stream,
            generation: None,
            child: None,
        };
        assert_eq!(client.request(json!("once")).unwrap(), "complete");
        server.join().unwrap();
    }

    #[test]
    fn partial_reply_progress_renews_idle_deadline_but_a_stalled_reader_expires() {
        use std::os::fd::AsRawFd;
        let (writer, mut reader) = UnixStream::pair().unwrap();
        writer.set_nonblocking(true).unwrap();
        reader.set_nonblocking(true).unwrap();
        let size: libc::c_int = 4096;
        // Only this disposable socket: force partial writes on every host.
        assert_eq!(
            unsafe {
                libc::setsockopt(
                    writer.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_SNDBUF,
                    (&size as *const libc::c_int).cast(),
                    std::mem::size_of_val(&size) as _,
                )
            },
            0
        );
        let expected = vec![b'x'; MAX_REPLY];
        let mut view = View {
            stream: writer,
            input: Vec::new(),
            output: expected.clone(),
            written: 0,
            touched: Instant::now() - Duration::from_secs(3),
        };
        let mut received = Vec::new();
        let mut pieces = 0;
        while !view.output.is_empty() {
            assert!(pieces < MAX_REPLY, "reply stopped making progress");
            // Model a delayed scheduling interval, without a flaky wall-clock sleep.
            view.touched = Instant::now() - Duration::from_secs(3);
            let before = view.written;
            view.tick("test", &mut |_| panic!("no request may be replayed"))
                .unwrap();
            assert!(view.written > before);
            assert!(view.touched.elapsed() < Duration::from_secs(2));
            let mut chunk = [0; 8192];
            loop {
                match reader.read(&mut chunk) {
                    Ok(0) => panic!("reader closed before reply completed"),
                    Ok(n) => received.extend_from_slice(&chunk[..n]),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(e) => panic!("{e}"),
                }
            }
            pieces += 1;
        }
        assert!(pieces > 1, "fixture must exercise partial output");
        assert_eq!(received, expected);

        view.output = expected;
        view.written = 0;
        loop {
            let before = view.written;
            view.write_output().unwrap();
            assert!(!view.output.is_empty(), "fixture must fill the socket");
            if view.written == before {
                break;
            }
        }
        view.touched = Instant::now() - Duration::from_secs(3);
        let error = view
            .tick("test", &mut |_| panic!("no request expected"))
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("Slow or incomplete assistant frame")
        );
    }

    #[test]
    fn attachment_hello_is_inert_and_pins_the_serving_generation() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("a");
        let owner = Owner::acquire(&root).unwrap();
        let generation = owner.generation.clone();
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let handled = calls.clone();
        let server = thread::spawn(move || {
            owner
                .serve(move |_| {
                    handled.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Ok(json!("saved once"))
                })
                .unwrap();
        });
        let mut client = Client::connect_ready(&root).unwrap();
        assert_eq!(client.generation.as_deref(), Some(generation.as_str()));
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(client.request(json!("save")).unwrap(), "saved once");
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        drop(client);
        server.join().unwrap();
    }

    #[test]
    fn a_lost_action_receipt_is_unknown_and_never_replayed() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("a");
        private_root(&root).unwrap();
        let listener = UnixListener::bind(root.join("view.sock")).unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            std::io::BufRead::read_line(&mut reader, &mut line).unwrap();
            let hello: Request = serde_json::from_str(&line).unwrap();
            assert_eq!(hello.payload, ATTACH_HELLO);
            let mut receipt = serde_json::to_vec(&Reply {
                protocol: PROTOCOL,
                generation: "fixture-owner".into(),
                payload: Some(json!(ATTACH_HELLO)),
                error: None,
            })
            .unwrap();
            receipt.push(b'\n');
            stream.write_all(&receipt).unwrap();
            line.clear();
            std::io::BufRead::read_line(&mut reader, &mut line).unwrap();
            let action: Request = serde_json::from_str(&line).unwrap();
            assert_eq!(action.generation.as_deref(), Some("fixture-owner"));
            assert_eq!(action.payload, "save exactly once");
            // Simulate a committed action followed by failure before its receipt.
            drop(reader);
            drop(stream);
            listener
        });
        let mut client = Client::connect_ready(&root).unwrap();
        let error = client
            .request(json!("save exactly once"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("receipt unknown"), "{error}");
        let listener = server.join().unwrap();
        listener.set_nonblocking(true).unwrap();
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }

    #[test]
    fn attachment_waits_for_closing_owner_then_sends_action_once_to_successor() {
        struct Cleanup(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                self.0.send(()).unwrap();
                self.1.recv_timeout(Duration::from_secs(3)).unwrap();
            }
        }
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("a");
        let owner = Owner::acquire(&root).unwrap();
        let old_generation = owner.generation.clone();
        let backlog = owner.listener.try_clone().unwrap();
        let mut first = Client::connect(&root).unwrap();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let cleanup = Cleanup(entered_tx, release_rx);
        let old_host = thread::spawn(move || {
            owner
                .serve(move |_| {
                    let _cleanup = &cleanup;
                    Ok(json!("old owner"))
                })
                .unwrap();
        });
        first.request(json!(null)).unwrap();
        drop(first);
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(!owner_lock_available(&root).unwrap());
        backlog.set_nonblocking(false).unwrap();
        let attach_root = root.clone();
        let (successor_tx, successor_rx) = std::sync::mpsc::channel();
        let (client_tx, client_rx) = std::sync::mpsc::channel();
        let attaching = thread::spawn(move || {
            let client = Client::attach_with_start(&attach_root, || {
                let owner = Owner::acquire(&attach_root)?;
                let server = thread::spawn(move || {
                    let mut calls = 0;
                    owner
                        .serve(move |_| {
                            calls += 1;
                            Ok(json!(calls))
                        })
                        .unwrap();
                });
                successor_tx.send(server).unwrap();
                Ok(Command::new("/bin/sh").args(["-c", "exit 0"]).spawn()?)
            });
            client_tx.send(client).unwrap();
        });
        // The retiring listener can accept a connection, but only a hello may
        // enter its backlog. Attachment must not expose it as a ready client.
        let (mut queued, _) = backlog.accept().unwrap();
        queued
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut bytes = String::new();
        std::io::BufRead::read_line(&mut std::io::BufReader::new(&mut queued), &mut bytes).unwrap();
        let hello: Request = serde_json::from_str(&bytes).unwrap();
        assert_eq!(hello.payload, ATTACH_HELLO);
        assert!(client_rx.try_recv().is_err());
        assert!(successor_rx.try_recv().is_err());
        drop(queued);
        drop(backlog);
        release_tx.send(()).unwrap();
        old_host.join().unwrap();
        let mut client = client_rx
            .recv_timeout(Duration::from_secs(3))
            .unwrap()
            .unwrap();
        assert_ne!(client.generation.as_deref(), Some(old_generation.as_str()));
        assert_eq!(client.request(json!("save")).unwrap(), 1);
        drop(client);
        attaching.join().unwrap();
        successor_rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn oversized_reply_returns_explicit_uncertainty_without_disconnecting() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("a");
        let owner = Owner::acquire(&root).unwrap();
        let mut client = Client::connect(&root).unwrap();
        let server = thread::spawn(move || {
            owner
                .serve(|input| {
                    if input == "large" {
                        Ok(json!("\u{1}".repeat(MAX_REPLY)))
                    } else {
                        Ok(json!("still connected"))
                    }
                })
                .unwrap()
        });
        let failure = client.request(json!("large")).unwrap_err().to_string();
        assert!(failure.contains("may have completed"));
        assert_eq!(client.request(json!("small")).unwrap(), "still connected");
        drop(client);
        server.join().unwrap();
    }

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

    #[test]
    fn approved_background_retains_one_owner_after_detach_until_disabled() {
        use crate::{
            assistant_lifecycle::{BackgroundConfig, Lifecycle},
            assistant_memory::{Origin, Scope},
        };
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("private");
        let owner = Owner::acquire(&root).unwrap();
        let mut lifecycle = Lifecycle::open(&root).unwrap();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let scope = Scope {
            project: Some("fixture".into()),
            ..Scope::default()
        };
        let executable = PathBuf::from("/fake/provider");
        lifecycle
            .approve(
                Origin::Human,
                BackgroundConfig {
                    scope: scope.clone(),
                    executable: executable.clone(),
                    max_calls: 1,
                    expires_at: now + 60,
                    job_timeout_secs: 10,
                },
                &scope,
                &executable,
                1,
                now,
            )
            .unwrap();
        let gate = lifecycle.gate();
        let (idle_tx, idle_rx) = std::sync::mpsc::channel();
        let (exit_tx, exit_rx) = std::sync::mpsc::channel();
        let host = thread::spawn(move || {
            owner
                .serve_with_lifetime(
                    move |request| {
                        if request.is_none() {
                            let _ = idle_tx.send(());
                        }
                        Ok(request.map(|_| json!("ready")))
                    },
                    gate,
                )
                .unwrap();
            exit_tx.send(()).unwrap();
        });
        let mut first = Client::connect(&root).unwrap();
        first.request(json!(null)).unwrap();
        drop(first);
        idle_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(exit_rx.try_recv().is_err());
        assert!(Owner::acquire(&root).is_err());
        assert_eq!(lifecycle.snapshot(now).unwrap().reserved_calls, 0);
        // Reasoning pause is independent of lifetime; a new view reconnects to
        // the same owner without another process or observation service.
        lifecycle.pause(Origin::Human, now).unwrap();
        let mut second = Client::connect(&root).unwrap();
        second.request(json!(null)).unwrap();
        drop(second);
        lifecycle.disable(Origin::Human, now).unwrap();
        exit_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        host.join().unwrap();
        assert!(!root.join("view.sock").exists());
        Owner::acquire(&root).unwrap();
    }
}
