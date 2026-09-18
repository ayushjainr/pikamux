//! Shared activity service: one producer, independently subscribed consumers.
use crate::{
    consult::{CancellablePipe, CancellationToken, OwnedChild},
    fleet::{FleetError, SshTransport},
    model::{FleetNode, Provider, Status},
    monitor::BoardItem,
    tmux::Tmux,
};
use anyhow::{Result, bail};
use std::{
    cell::RefCell,
    collections::{BTreeSet, HashSet},
    io::Write,
    process::Stdio,
    sync::{Arc, Mutex, atomic::AtomicBool, mpsc},
    thread,
    time::{Duration, Instant},
};

#[cfg(any(unix, test))]
const MAX_FRAME_BYTES: usize = 512;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Summary {
    pub counts: [usize; 4],
    pub colors: bool,
    pub latest_attention: Option<String>,
    pub attention: Option<AttentionKind>,
}

const MAX_RETURN_INTERESTS: usize = 64;
const MAX_RETURN_SESSION_ID: usize = 96;

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct ReturnInterest {
    node_id: Option<String>,
    provider: Provider,
    session_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ReturnStatus {
    pub provider: Provider,
    pub session_id: String,
    pub status: Option<Status>,
    pub colors: bool,
}

fn valid_return_session_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_RETURN_SESSION_ID
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.:".contains(&byte))
}

fn status_wire(status: Option<Status>) -> &'static str {
    match status {
        None => "unknown",
        Some(Status::Starting) => "starting",
        Some(Status::NeedsYou) => "needs-you",
        Some(Status::OpenTwice) => "open-twice",
        Some(Status::Working) => "working",
        Some(Status::Ready) => "ready",
        Some(Status::Parked) => "parked",
        Some(Status::Unbound) => "unbound",
        Some(Status::Error) => "error",
    }
}

#[cfg(any(unix, test))]
fn parse_status_wire(value: &str) -> Option<Option<Status>> {
    Some(match value {
        "unknown" => None,
        "starting" => Some(Status::Starting),
        "needs-you" => Some(Status::NeedsYou),
        "open-twice" => Some(Status::OpenTwice),
        "working" => Some(Status::Working),
        "ready" => Some(Status::Ready),
        "parked" => Some(Status::Parked),
        "unbound" => Some(Status::Unbound),
        "error" => Some(Status::Error),
        _ => return None,
    })
}

impl ReturnStatus {
    fn encode(&self) -> String {
        format!(
            "s1|{}|{}|{}|{}",
            self.provider.as_str(),
            self.session_id,
            status_wire(self.status),
            usize::from(self.colors)
        )
    }

    #[cfg(any(unix, test))]
    fn decode(value: &str) -> Result<Self> {
        if value.len() > MAX_FRAME_BYTES {
            bail!("Invalid return status frame");
        }
        let mut fields = value.split('|');
        if fields.next() != Some("s1") {
            bail!("Invalid return status frame");
        }
        let provider = fields
            .next()
            .ok_or_else(|| anyhow::anyhow!("Missing return status provider"))?
            .parse::<Provider>()
            .map_err(|_| anyhow::anyhow!("Invalid return status provider"))?;
        let session_id = fields
            .next()
            .ok_or_else(|| anyhow::anyhow!("Missing return status identity"))?;
        if !valid_return_session_id(session_id) {
            bail!("Invalid return status identity");
        }
        let status = parse_status_wire(
            fields
                .next()
                .ok_or_else(|| anyhow::anyhow!("Missing return status value"))?,
        )
        .ok_or_else(|| anyhow::anyhow!("Invalid return status value"))?;
        let colors = match fields.next() {
            Some("0") => false,
            Some("1") => true,
            _ => bail!("Invalid return status colors"),
        };
        if fields.next().is_some() {
            bail!("Invalid return status frame");
        }
        Ok(Self {
            provider,
            session_id: session_id.to_owned(),
            status,
            colors,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AttentionKind {
    OpenTwice,
    Error,
    Unbound,
    Ready,
}

impl AttentionKind {
    fn label(self) -> &'static str {
        match self {
            Self::OpenTwice => "open twice",
            Self::Error => "error",
            Self::Unbound => "outside Pika",
            Self::Ready => "ready",
        }
    }
    fn wire(self) -> &'static str {
        match self {
            Self::OpenTwice => "open-twice",
            Self::Error => "error",
            Self::Unbound => "unbound",
            Self::Ready => "ready",
        }
    }
}

#[derive(Clone)]
pub(crate) enum Context {
    Source(Source),
    #[cfg(any(unix, test))]
    Remote(String),
}
thread_local! { static CURRENT: RefCell<Option<Context>> = const { RefCell::new(None) }; }
pub(crate) fn current() -> Option<Context> {
    CURRENT.with_borrow(Clone::clone)
}
pub(crate) fn with<T>(context: Option<Context>, action: impl FnOnce() -> T) -> T {
    struct Restore(Option<Context>);
    impl Drop for Restore {
        fn drop(&mut self) {
            CURRENT.replace(self.0.take());
        }
    }
    let _restore = Restore(CURRENT.replace(context));
    action()
}

#[derive(Default)]
struct State {
    items: Vec<BoardItem>,
    filter: String,
    summary: Option<Summary>,
    health: Vec<String>,
    revision: u64,
    return_interests: BTreeSet<ReturnInterest>,
}
#[derive(Clone)]
pub(crate) struct Snapshot {
    pub items: Vec<BoardItem>,
    pub summary: Summary,
    pub health: Vec<String>,
    pub revision: u64,
}

// The producer owns only data and cancellation, never a consumer lease. This
// prevents the producer retaining itself after the last view unsubscribes.
#[derive(Clone)]
pub(crate) struct Publisher {
    state: Arc<Mutex<State>>,
    stop: CancellationToken,
}
impl Publisher {
    pub fn health(&self, health: Vec<String>) {
        if self.stop.is_cancelled() {
            return;
        }
        let mut state = self.state.lock().unwrap();
        state.health = health;
        state.revision = state.revision.saturating_add(1);
    }
    pub fn publish(&self, items: Vec<BoardItem>, health: Vec<String>) {
        if self.stop.is_cancelled() {
            return;
        }
        let mut state = self.state.lock().unwrap();
        state.summary = Some(summarize(&items, &state.filter));
        state.items = items;
        state.health = health;
        state.revision = state.revision.saturating_add(1);
    }
}

pub(crate) struct Subscription {
    source: Source,
    revision: Option<u64>,
}
impl Subscription {
    pub fn take(&mut self) -> Option<Snapshot> {
        let state = self.source.0.state.lock().unwrap();
        if self.revision == Some(state.revision) {
            return None;
        }
        let snapshot = snapshot(&state)?;
        self.revision = Some(snapshot.revision);
        Some(snapshot)
    }
}
fn snapshot(state: &State) -> Option<Snapshot> {
    Some(Snapshot {
        items: state.items.clone(),
        summary: state.summary.clone()?,
        health: state.health.clone(),
        revision: state.revision,
    })
}
struct Owner {
    token: String,
    state: Arc<Mutex<State>>,
    stop: CancellationToken,
    sinks: Mutex<HashSet<String>>,
    workers: Mutex<Vec<thread::JoinHandle<()>>>,
    observer: Mutex<Option<Box<dyn Send>>>,
    refresh: Mutex<Option<mpsc::SyncSender<()>>>,
    delayed: Arc<AtomicBool>,
    observation_enabled: Arc<AtomicBool>,
}
struct ObserverStop(Option<Box<dyn FnOnce() + Send>>);
impl Drop for ObserverStop {
    fn drop(&mut self) {
        if let Some(stop) = self.0.take() {
            stop();
        }
    }
}
impl Drop for Owner {
    fn drop(&mut self) {
        self.stop.cancel();
        self.observer.get_mut().unwrap().take();
        for worker in self.workers.get_mut().unwrap().drain(..) {
            let _ = worker.join();
        }
    }
}
#[derive(Clone)]
pub(crate) struct Source(Arc<Owner>);
impl Default for Source {
    fn default() -> Self {
        Self(Arc::new(Owner {
            token: uuid::Uuid::new_v4().simple().to_string(),
            state: Arc::new(Mutex::new(State::default())),
            stop: CancellationToken::default(),
            sinks: Mutex::new(HashSet::new()),
            workers: Mutex::new(Vec::new()),
            observer: Mutex::new(None),
            refresh: Mutex::new(None),
            delayed: Arc::new(AtomicBool::new(false)),
            observation_enabled: Arc::new(AtomicBool::new(true)),
        }))
    }
}
impl Source {
    pub(crate) fn local_token(&self, tmux: &Tmux) -> String {
        self.local(tmux.clone());
        self.0.token.clone()
    }

    pub fn publisher(&self) -> Publisher {
        Publisher {
            state: self.0.state.clone(),
            stop: self.0.stop.clone(),
        }
    }
    pub fn subscribe(&self) -> Subscription {
        Subscription {
            source: self.clone(),
            revision: None,
        }
    }
    pub fn snapshot(&self) -> Option<Snapshot> {
        snapshot(&self.0.state.lock().unwrap())
    }
    pub fn summary(&self) -> Option<Summary> {
        self.0.state.lock().unwrap().summary.clone()
    }
    pub(crate) fn register_return_interest(
        &self,
        node_id: Option<&str>,
        provider: Provider,
        session_id: &str,
    ) -> bool {
        if node_id.is_some_and(|value| !valid_return_session_id(value))
            || !valid_return_session_id(session_id)
        {
            return false;
        }
        let mut state = self.0.state.lock().unwrap();
        let interest = ReturnInterest {
            node_id: node_id.map(str::to_owned),
            provider,
            session_id: session_id.to_owned(),
        };
        if state.return_interests.contains(&interest) {
            return true;
        }
        if state.return_interests.len() >= MAX_RETURN_INTERESTS {
            return false;
        }
        state.return_interests.insert(interest);
        state.revision = state.revision.saturating_add(1);
        true
    }
    fn return_statuses(state: &Arc<Mutex<State>>, node_id: Option<&str>) -> Vec<ReturnStatus> {
        let state = state.lock().unwrap();
        state
            .return_interests
            .iter()
            .filter(|interest| interest.node_id.as_deref() == node_id)
            .map(|interest| {
                let status = state.items.iter().find(|item| {
                    item.node_id.as_deref() == node_id
                        && item.session.provider == interest.provider
                        && item.session.session_id == interest.session_id
                        && !item.stale
                        && item.pending_token.is_none()
                        && item.session.live
                });
                ReturnStatus {
                    provider: interest.provider,
                    session_id: interest.session_id.clone(),
                    status: status.map(|item| item.session.status),
                    colors: state.summary.as_ref().is_some_and(|summary| summary.colors),
                }
            })
            .collect()
    }
    pub fn own_observer(
        &self,
        refresh: mpsc::SyncSender<()>,
        observer: impl FnOnce() + Send + 'static,
    ) {
        *self.0.refresh.lock().unwrap() = Some(refresh);
        *self.0.observer.lock().unwrap() = Some(Box::new(ObserverStop(Some(Box::new(observer)))));
    }
    pub fn refresh(&self) -> mpsc::SyncSender<()> {
        self.0
            .refresh
            .lock()
            .unwrap()
            .as_ref()
            .expect("activity producer has refresh control")
            .clone()
    }
    pub fn delayed(&self) -> Arc<AtomicBool> {
        self.0.delayed.clone()
    }
    #[cfg(not(windows))]
    pub(crate) fn observation_gate(&self) -> Arc<AtomicBool> {
        self.0.observation_enabled.clone()
    }
    #[cfg(not(windows))]
    pub(crate) fn pause_observation(&self) {
        self.0
            .observation_enabled
            .store(false, std::sync::atomic::Ordering::Release);
    }
    pub(crate) fn activate_observation(&self) {
        self.0
            .observation_enabled
            .store(true, std::sync::atomic::Ordering::Release);
    }
    pub fn filter(&self, filter: &str) {
        let mut state = self.0.state.lock().unwrap();
        if state.filter != filter || state.summary.is_none() {
            state.filter = filter.to_owned();
            state.summary = Some(summarize(&state.items, filter));
            state.revision = state.revision.saturating_add(1);
        }
    }
    fn start(
        &self,
        key: String,
        mut sink: impl FnMut(Option<Summary>) -> Result<()> + Send + 'static,
    ) {
        let mut sinks = self.0.sinks.lock().unwrap();
        if self.0.stop.is_cancelled() || sinks.contains(&key) || sinks.len() >= 64 {
            return;
        }
        sinks.insert(key);
        let state = self.0.state.clone();
        let stop = self.0.stop.clone();
        self.0.workers.lock().unwrap().push(thread::spawn(move || {
            let mut previous = None;
            let mut previous_revision = None;
            let mut sent = Instant::now() - Duration::from_secs(10);
            while !stop.is_cancelled() {
                let (summary, revision) = {
                    let state = state.lock().unwrap();
                    (state.summary.clone(), state.revision)
                };
                if summary.is_some()
                    && (summary != previous
                        || Some(revision) != previous_revision
                        || sent.elapsed() >= Duration::from_secs(5))
                {
                    // Reconnection is read-only presentation, never another
                    // agent launch. Retry at the heartbeat cadence after loss.
                    let _ = sink(summary.clone());
                    previous = summary;
                    previous_revision = Some(revision);
                    sent = Instant::now();
                }
                thread::sleep(Duration::from_millis(100));
            }
            let _ = sink(None);
        }));
    }
    fn local(&self, tmux: Tmux) {
        let token = self.0.token.clone();
        let state = self.0.state.clone();
        self.start("local".into(), move |summary| {
            tmux.publish_board_summary_with_statuses(
                &token,
                summary,
                Some(&Self::return_statuses(&state, None)),
            )
        });
    }
    fn remote(&self, ssh: &SshTransport, node: &FleetNode) -> std::result::Result<(), FleetError> {
        let key = format!("{}:{}", node.node_id, node.ssh_target);
        if self.0.sinks.lock().unwrap().contains(&key) {
            return Ok(());
        }
        let mut command = ssh.command(
            &node.ssh_target,
            &[
                "_board-feed".into(),
                "--expected-node-id".into(),
                node.node_id.clone(),
                "--token".into(),
                self.0.token.clone(),
            ],
            false,
        )?;
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let stop = self.0.stop.clone();
        let extended = node.capabilities.iter().any(|cap| cap == "board-feed-v2");
        let warnings = node.capabilities.iter().any(|cap| cap == "board-feed-v3");
        let attention = node.capabilities.iter().any(|cap| cap == "board-feed-v4");
        let return_status = node
            .capabilities
            .iter()
            .any(|cap| cap == "return-bar-status-v1");
        let state = self.0.state.clone();
        let node_id = node.node_id.clone();
        // Spawning is lazy and off the board renderer. The owned child is reaped
        // on disconnect or board shutdown; its pipe never shares agent input.
        let mut child = None;
        let mut pipe = None;
        self.start(key, move |summary| {
            let Some(summary) = summary else {
                pipe.take();
                child.take();
                return Ok(());
            };
            if child.is_none() {
                let mut owned = OwnedChild::spawn(&mut command)?;
                pipe = Some(CancellablePipe::new(
                    owned.stdin.take().unwrap(),
                    stop.clone(),
                )?);
                child = Some(owned);
            }
            let frame = if attention {
                summary.encode_attention_v4()
            } else if warnings {
                summary.encode_attention()
            } else if extended {
                summary.encode_extended()
            } else {
                summary.encode()
            };
            if let Err(error) = writeln!(pipe.as_mut().unwrap(), "{frame}") {
                pipe.take();
                child.take();
                return Err(error.into());
            }
            if return_status {
                for status in Self::return_statuses(&state, Some(&node_id)) {
                    if let Err(error) = writeln!(pipe.as_mut().unwrap(), "{}", status.encode()) {
                        pipe.take();
                        child.take();
                        return Err(error.into());
                    }
                }
            }
            Ok(())
        });
        Ok(())
    }
}

impl Context {
    pub fn token(&self, tmux: &Tmux) -> &str {
        match self {
            Self::Source(source) => {
                source.local(tmux.clone());
                &source.0.token
            }
            #[cfg(any(unix, test))]
            Self::Remote(token) => token,
        }
    }
}
pub(crate) fn forward(
    arguments: &mut Vec<String>,
    node: &FleetNode,
    ssh: &SshTransport,
) -> std::result::Result<(), FleetError> {
    if arguments.first().is_some_and(|arg| arg == "_fleet-open")
        && node.capabilities.iter().any(|cap| cap == "board-feed-v1")
        && let Some(Context::Source(source)) = current()
    {
        source.remote(ssh, node)?;
        source.activate_observation();
        arguments.extend(["--board-feed".into(), source.0.token.clone()]);
        if node
            .capabilities
            .iter()
            .any(|cap| cap == "return-bar-status-v1")
            && let (Some(provider), Some(session_id)) = (
                arguments.get(4).and_then(|value| value.parse().ok()),
                arguments.get(6),
            )
        {
            source.register_return_interest(Some(&node.node_id), provider, session_id);
        }
    }
    Ok(())
}
pub(crate) fn token(value: &str) -> Result<String> {
    if value.len() != 32
        || !value
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    {
        bail!("Invalid board feed identity");
    }
    Ok(value.to_owned())
}

pub(crate) fn return_option_name(
    token_value: &str,
    provider: Provider,
    session_id: &str,
) -> Option<String> {
    token(token_value).ok()?;
    if !valid_return_session_id(session_id) {
        return None;
    }
    let encoded = session_id
        .bytes()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    Some(format!(
        "@pika_return_{token_value}_{}_{}",
        provider.as_str(),
        encoded
    ))
}

pub(crate) fn return_bar_color(status: Status) -> &'static str {
    match crate::monitor::status_color(status) {
        crossterm::style::Color::Red => "red",
        crossterm::style::Color::Cyan => "cyan",
        crossterm::style::Color::Green => "green",
        crossterm::style::Color::Magenta => "magenta",
        crossterm::style::Color::DarkGrey => "brightblack",
        _ => "default",
    }
}

// This endpoint receives bounded counts plus optional exact-target status frames, after the CLI
// has proved the expected node UUID. EOF, silence or malformed input clears the live feed.
#[cfg(unix)]
pub(crate) fn receive(tmux: &Tmux, token: &str) -> Result<()> {
    use std::io::Read;
    use std::os::fd::{AsRawFd, BorrowedFd};
    struct Clear<'a>(&'a Tmux, &'a str);
    impl Drop for Clear<'_> {
        fn drop(&mut self) {
            let _ = self.0.publish_board_summary(self.1, None);
        }
    }
    let _clear = Clear(tmux, token);
    // Use an unbuffered owned descriptor: stdio buffering could retain bytes
    // after poll sees an empty kernel pipe, falsely timing out a queued frame.
    let mut input = std::fs::File::from(
        unsafe { BorrowedFd::borrow_raw(libc::STDIN_FILENO) }.try_clone_to_owned()?,
    );
    let mut pending = Vec::new();
    let mut last = Instant::now();
    loop {
        if last.elapsed() > Duration::from_secs(15) {
            bail!("Board feed disconnected");
        }
        let mut fd = libc::pollfd {
            fd: input.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: fd is initialized and lives throughout this bounded poll.
        let ready = unsafe { libc::poll(&mut fd, 1, 1000) };
        if ready < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        if ready == 0 {
            continue;
        }
        let mut bytes = [0u8; 256];
        let read = input.read(&mut bytes)?;
        if read == 0 {
            if !pending.is_empty() {
                bail!("Incomplete board feed frame");
            }
            return Ok(());
        }
        for byte in &bytes[..read] {
            if *byte == b'\n' {
                let frame = std::str::from_utf8(&pending)?;
                if frame.starts_with("s1|") {
                    let status = ReturnStatus::decode(frame)?;
                    let _ = tmux.publish_return_status(token, status);
                } else {
                    let summary = Summary::decode(frame)?;
                    // The first frame can arrive before the exact opening creates
                    // a tmux server. A later heartbeat retries presentation only.
                    let _ = tmux.publish_board_summary(token, Some(summary));
                }
                pending.clear();
                last = Instant::now();
            } else {
                if pending.len() >= MAX_FRAME_BYTES {
                    bail!("Board feed frame too large");
                }
                pending.push(*byte);
            }
        }
    }
}

pub(crate) fn matches_filter(item: &BoardItem, needle: &str) -> bool {
    let session = &item.session;
    needle.is_empty()
        || session.display_name().to_lowercase().contains(needle)
        || session.provider.as_str().contains(needle)
        || session
            .cwd
            .as_deref()
            .is_some_and(|value| value.to_lowercase().contains(needle))
        || item
            .node_name
            .as_deref()
            .or(item.node_id.as_deref())
            .is_some_and(|value| value.to_lowercase().contains(needle))
}

pub(crate) fn group(status: crate::model::Status, stale: bool) -> &'static str {
    use crate::model::Status;
    if stale {
        return "PARKED";
    }
    match status {
        Status::NeedsYou | Status::Error | Status::OpenTwice | Status::Unbound => "NEEDS YOU",
        Status::Working | Status::Starting => "WORKING",
        Status::Ready => "READY",
        Status::Parked => "PARKED",
    }
}

pub(crate) fn summarize(items: &[BoardItem], filter: &str) -> Summary {
    let needle = filter.to_lowercase();
    let counts = ["NEEDS YOU", "WORKING", "READY", "PARKED"].map(|category| {
        items
            .iter()
            .filter(|item| {
                matches_filter(item, &needle) && group(item.session.status, item.stale) == category
            })
            .count()
    });
    let (latest_attention, attention) = latest_attention(items, &needle)
        .map_or((None, None), |(label, attention)| (Some(label), attention));
    Summary {
        counts,
        colors: std::env::var_os("NO_COLOR").is_none(),
        latest_attention,
        attention,
    }
}

fn latest_attention(items: &[BoardItem], needle: &str) -> Option<(String, Option<AttentionKind>)> {
    let latest = |category| {
        items
            .iter()
            .filter(|item| {
                !item.stale
                    && item.pending_token.is_none()
                    && group(item.session.status, false) == category
                    && matches_filter(item, needle)
            })
            .max_by(|a, b| {
                let time = |item: &BoardItem| {
                    let value = item.session.last_event_at;
                    if value.is_finite() && value > 0.0 {
                        value
                    } else {
                        0.0
                    }
                };
                time(a).total_cmp(&time(b)).then_with(|| {
                    (
                        &a.node_id,
                        a.session.provider.as_str(),
                        &a.session.session_id,
                    )
                        .cmp(&(
                            &b.node_id,
                            b.session.provider.as_str(),
                            &b.session.session_id,
                        ))
                })
            })
    };
    let (latest, suffix) = latest("NEEDS YOU")
        .map(|item| (item, None))
        .or_else(|| latest("READY").map(|item| (item, Some(AttentionKind::Ready))))?;
    let mut label = latest.session.display_name();
    if let Some(machine) = latest.node_name.as_ref().or(latest.node_id.as_ref()) {
        label.push_str(" @");
        label.push_str(machine);
    }
    let label = safe_label(&label);
    let warning = suffix.or(match latest.session.status {
        crate::model::Status::OpenTwice => Some(AttentionKind::OpenTwice),
        crate::model::Status::Error => Some(AttentionKind::Error),
        crate::model::Status::Unbound => Some(AttentionKind::Unbound),
        _ => None,
    });
    (!label.is_empty()).then_some((label, warning))
}

// Text is embedded inside nested tmux formats and strftime. An allowlist makes
// provider names inert at every expansion layer, including #(), #{}, styles,
// commas, percent directives, terminal escapes and bidirectional controls.
fn safe_label(value: &str) -> String {
    let mut result = String::new();
    for ch in value
        .chars()
        .filter(|ch| ch.is_alphanumeric() || " _-./@()".contains(*ch))
    {
        if result.len() + ch.len_utf8() > 160 {
            break;
        }
        result.push(ch);
    }
    result.trim().to_owned()
}

impl Summary {
    pub fn encode(&self) -> String {
        let [a, b, c, d] = self.counts;
        format!("{a},{b},{c},{d},{}", usize::from(self.colors))
    }
    pub fn encode_extended(&self) -> String {
        format!(
            "v2|{}|{}",
            self.encode(),
            if self.attention.is_none() {
                self.latest_attention.as_deref().unwrap_or_default()
            } else {
                // Older receivers always draw an agent-request arrow.
                ""
            }
        )
    }
    pub fn encode_attention(&self) -> String {
        let (kind, label) = if self.attention == Some(AttentionKind::Ready) {
            // v3 peers must never receive the new Ready suffix kind. Keep the
            // counts while omitting the label they would render as a request.
            ("request", "")
        } else {
            (
                self.attention.map_or("request", AttentionKind::wire),
                self.latest_attention.as_deref().unwrap_or_default(),
            )
        };
        format!("v3|{}|{}|{}", self.encode(), kind, label,)
    }
    pub fn encode_attention_v4(&self) -> String {
        format!(
            "v4|{}|{}|{}",
            self.encode(),
            self.attention.map_or("request", AttentionKind::wire),
            self.latest_attention.as_deref().unwrap_or_default()
        )
    }
    #[cfg(any(unix, test))]
    pub fn decode(value: &str) -> Result<Self> {
        if value.len() > MAX_FRAME_BYTES {
            bail!("Invalid board summary");
        }
        let (value, warning) = if let Some(value) = value.strip_prefix("v4|") {
            let mut parts = value.splitn(3, '|');
            let counts = parts.next().unwrap_or_default();
            let warning = match parts.next() {
                Some("request") => None,
                Some("open-twice") => Some(AttentionKind::OpenTwice),
                Some("error") => Some(AttentionKind::Error),
                Some("unbound") => Some(AttentionKind::Unbound),
                Some("ready") => Some(AttentionKind::Ready),
                _ => bail!("Invalid board attention kind"),
            };
            let label = parts
                .next()
                .ok_or_else(|| anyhow::anyhow!("Missing board attention label"))?;
            (format!("v2|{counts}|{label}"), warning)
        } else if let Some(value) = value.strip_prefix("v3|") {
            let mut parts = value.splitn(3, '|');
            let counts = parts.next().unwrap_or_default();
            let warning = match parts.next() {
                Some("request") => None,
                Some("open-twice") => Some(AttentionKind::OpenTwice),
                Some("error") => Some(AttentionKind::Error),
                Some("unbound") => Some(AttentionKind::Unbound),
                _ => bail!("Invalid board attention kind"),
            };
            let label = parts
                .next()
                .ok_or_else(|| anyhow::anyhow!("Missing board attention label"))?;
            (format!("v2|{counts}|{label}"), warning)
        } else {
            (value.to_owned(), None)
        };
        let (value, latest_attention) = if let Some(value) = value.strip_prefix("v2|") {
            let (counts, label) = value
                .split_once('|')
                .ok_or_else(|| anyhow::anyhow!("Invalid board summary"))?;
            if label != safe_label(label) {
                bail!("Invalid board request label");
            }
            (counts, (!label.is_empty()).then(|| label.to_owned()))
        } else {
            (value.as_str(), None)
        };
        if value.len() > 40 {
            bail!("Invalid board summary");
        }
        let values = value
            .split(',')
            .map(str::parse::<usize>)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let [a, b, c, d, colors] = values.as_slice() else {
            bail!("Invalid board summary");
        };
        if [a, b, c, d].iter().any(|n| **n > 1_000_000) || *colors > 1 {
            bail!("Invalid board summary");
        }
        if *a == 0 && latest_attention.is_some() && warning != Some(AttentionKind::Ready) {
            bail!("Request label without an attention count");
        }
        if warning == Some(AttentionKind::Ready) && *c == 0 {
            bail!("Ready label without a ready count");
        }
        if warning.is_some() && latest_attention.is_none() {
            bail!("Warning without an attention label");
        }
        Ok(Self {
            counts: [*a, *b, *c, *d],
            colors: *colors == 1,
            latest_attention,
            attention: warning,
        })
    }
    pub fn tmux_text(&self) -> String {
        use crossterm::style::Color;
        let mut text = crate::monitor::summary_parts(self.counts)
            .into_iter()
            .map(|(text, color)| {
                if !self.colors {
                    return text;
                }
                let color = match color {
                    Color::Red => "red",
                    Color::Cyan => "cyan",
                    Color::Green => "green",
                    Color::DarkGrey => "brightblack",
                    _ => "default",
                };
                format!("#[fg={color}]{text}#[fg=default]")
            })
            .collect::<Vec<_>>()
            .join(" · ");
        if let Some(label) = &self.latest_attention {
            use unicode_width::UnicodeWidthStr;
            let label = safe_label(label);
            let plain = Self {
                colors: false,
                latest_attention: None,
                ..self.clone()
            }
            .tmux_text();
            // Reserve navigation and separators first. Use three deterministic
            // widths; tmux reevaluates these branches when the client resizes.
            let mut suffix = String::new();
            let ready = self.attention == Some(AttentionKind::Ready);
            let marker = if ready {
                "✓"
            } else if self.attention.is_some() {
                "⚠"
            } else {
                "↑"
            };
            let marker_color = if ready { "green" } else { "red" };
            let reason = self
                .attention
                .map(|warning| format!(" · {}", warning.label()))
                .unwrap_or_default();
            for width in [8, 20, 48] {
                let short = shorten_label(&label, width);
                let minimum = plain.width() + 20 + short.width() + reason.width();
                let value = if self.colors {
                    format!(" · #[fg={marker_color}]{marker} {short}{reason}#[fg=default]")
                } else {
                    format!(" · {marker} {short}{reason}")
                };
                let condition = ["#{e|>=:#{client_width},", &minimum.to_string(), "}"].concat();
                suffix = format!("#{{?{condition},{value},{suffix}}}");
            }
            text.push_str(&suffix);
        }
        text
    }
}

fn shorten_label(value: &str, width: usize) -> String {
    use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
    if value.width() <= width {
        return value.to_owned();
    }
    let mut result = String::new();
    let mut cells = 0;
    for ch in value.chars() {
        let size = ch.width().unwrap_or(0);
        if cells + size >= width {
            break;
        }
        result.push(ch);
        cells += size;
    }
    result.push('…');
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    fn request(name: &str, at: f64) -> BoardItem {
        let session = serde_json::from_value(serde_json::json!({
            "provider": "codex", "session_id": format!("id-{name}"), "name": name,
            "status": "NEEDS YOU", "unread": false, "source": "fixture", "managed": true,
            "created_at": 0.0, "updated_at": at, "last_event_at": at, "last_activity_at": at,
            "live": true, "attached": false, "home_state": "exact"
        }))
        .unwrap();
        BoardItem::local(session)
    }

    #[test]
    fn newest_attention_includes_warnings_excludes_results_and_resolves_without_recounting() {
        use crate::model::Status;
        let old = request("old", 10.0);
        let mut latest = request("ts_quality", 20.0);
        latest.session.provider = crate::model::Provider::Claude;
        latest.node_name = Some("rs2a".into());
        latest.node_id = Some("remote".into());
        let mut result = request("result", 90.0);
        result.session.status = Status::Ready;
        let mut error = request("error", 95.0);
        error.session.status = Status::Error;
        let mut stale = request("offline", 100.0);
        stale.stale = true;
        let mut rows = vec![old, latest, result, error, stale];
        let source = Source::default();
        let publisher = source.publisher();
        publisher.publish(rows.clone(), vec![]);
        let snapshot = source.snapshot().unwrap();
        assert_eq!(snapshot.summary.counts, [3, 0, 1, 1]);
        assert_eq!(snapshot.summary.latest_attention.as_deref(), Some("error"));
        assert_eq!(snapshot.summary.attention, Some(AttentionKind::Error));
        rows.reverse();
        publisher.publish(rows.clone(), vec![]);
        assert_eq!(source.summary().unwrap(), snapshot.summary);
        rows.iter_mut()
            .find(|row| row.session.name.as_deref() == Some("error"))
            .unwrap()
            .session
            .status = Status::Parked;
        publisher.publish(rows.clone(), vec![]);
        assert_eq!(
            source.summary().unwrap().latest_attention.as_deref(),
            Some("ts_quality @rs2a")
        );
        assert_eq!(source.summary().unwrap().attention, None);
        rows.iter_mut()
            .find(|row| row.session.name.as_deref() == Some("ts_quality"))
            .unwrap()
            .session
            .status = Status::Working;
        publisher.publish(rows.clone(), vec![]);
        assert_eq!(
            source.summary().unwrap().latest_attention.as_deref(),
            Some("old")
        );
        source.filter("result");
        assert_eq!(
            source.summary().unwrap().latest_attention.as_deref(),
            Some("result")
        );
        source.filter("");
        rows.iter_mut()
            .find(|row| row.session.name.as_deref() == Some("old"))
            .unwrap()
            .session
            .status = Status::Working;
        publisher.publish(rows, vec![]);
        assert_eq!(
            source.summary().unwrap().latest_attention.as_deref(),
            Some("result")
        );
        assert_eq!(
            source.summary().unwrap().attention,
            Some(AttentionKind::Ready)
        );
    }

    #[test]
    fn equal_request_times_have_a_stable_identity_tiebreaker() {
        let a = request("a", 5.0);
        let b = request("b", 5.0);
        assert_eq!(
            summarize(&[a.clone(), b.clone()], ""),
            summarize(&[b, a], "")
        );
    }

    #[test]
    fn latest_ready_is_used_only_when_no_needs_you_row_exists() {
        use crate::model::Status;
        let mut ready = request("ready", 20.0);
        ready.session.status = Status::Ready;
        let mut needs_you = request("needs-you", 1.0);
        needs_you.session.status = Status::NeedsYou;
        let summary = summarize(&[ready.clone(), needs_you], "");
        assert_eq!(summary.latest_attention.as_deref(), Some("needs-you"));
        assert_eq!(summary.attention, None);

        let summary = summarize(&[ready], "");
        assert_eq!(summary.latest_attention.as_deref(), Some("ready"));
        assert_eq!(summary.attention, Some(AttentionKind::Ready));
        assert!(summary.tmux_text().contains("✓ ready · ready"));
        assert!(summary.tmux_text().contains("fg=green"));
        assert!(!summary.tmux_text().contains("⚠"));
    }

    #[test]
    fn latest_ready_reuses_filter_tie_break_and_exclusions() {
        use crate::model::Status;
        let mut alpha = request("alpha", 5.0);
        alpha.session.status = Status::Ready;
        let mut beta = request("beta", 5.0);
        beta.session.status = Status::Ready;
        let mut stale = request("stale", 50.0);
        stale.session.status = Status::Ready;
        stale.stale = true;
        let mut pending = request("pending", 60.0);
        pending.session.status = Status::Ready;
        pending.pending_token = Some("launch".into());
        assert_eq!(
            summarize(&[alpha.clone(), beta.clone(), stale, pending], "")
                .latest_attention
                .as_deref(),
            Some("beta")
        );
        assert_eq!(
            summarize(&[alpha, beta], "alpha")
                .latest_attention
                .as_deref(),
            Some("alpha")
        );
        let mut working = request("working", 10.0);
        working.session.status = Status::Working;
        assert_eq!(summarize(&[working], "").latest_attention, None);
    }

    #[test]
    fn warnings_are_typed_and_never_sent_as_requests_to_older_receivers() {
        use crate::model::Status;
        for (status, kind, reason) in [
            (Status::OpenTwice, AttentionKind::OpenTwice, "open twice"),
            (Status::Error, AttentionKind::Error, "error"),
            (Status::Unbound, AttentionKind::Unbound, "outside Pika"),
        ] {
            let mut row = request("ts_quality", 10.0);
            row.session.status = status;
            let summary = summarize(&[row.clone()], "");
            assert_eq!(summary.counts, [1, 0, 0, 0]);
            assert_eq!(summary.attention, Some(kind));
            assert_eq!(
                Summary::decode(&summary.encode_attention()).unwrap(),
                summary
            );
            let legacy = Summary::decode(&summary.encode_extended()).unwrap();
            assert_eq!(legacy.counts, summary.counts);
            assert_eq!(legacy.latest_attention, None);
            assert_eq!(legacy.attention, None);
            let rendered = summary.tmux_text();
            assert!(rendered.contains(&format!("⚠ ts_quality · {reason}")));
            assert!(!rendered.contains("↑"));
            row.pending_token = Some("pending".into());
            assert_eq!(summarize(&[row], "").latest_attention, None);
        }
        let mut ready = request("ready", 10.0);
        ready.session.status = Status::Ready;
        let ready = summarize(&[ready], "");
        assert_eq!(ready.attention, Some(AttentionKind::Ready));
        assert_eq!(
            Summary::decode(&ready.encode_attention())
                .unwrap()
                .attention,
            None
        );
        assert_eq!(
            Summary::decode(&ready.encode_attention_v4()).unwrap(),
            ready
        );
        assert!(ready.tmux_text().contains("✓ ready · ready"));
        for frame in [
            "v3|1,0,0,0,0|shell|name",
            "v3|1,0,0,0,0|error|",
            "v3|0,0,0,0,0|error|name",
            "v3|1,0,0,0,0|error|#{client_pid}",
            "v3|1,0,0,0,0|request",
            "v3|0,0,1,0,0|ready|ready",
            "v4|0,0,0,0,0|ready|ready",
            "v4|0,0,1,0,0|ready|",
        ] {
            assert!(Summary::decode(frame).is_err(), "{frame}");
        }
    }

    #[test]
    fn request_labels_are_bounded_inert_and_versioned() {
        let label = "ts_quality @rs2a";
        let summary = Summary {
            latest_attention: Some(label.into()),
            ..summary()
        };
        assert_eq!(
            Summary::decode(&summary.encode_extended()).unwrap(),
            summary
        );
        assert_eq!(
            Summary::decode(&summary.encode()).unwrap().latest_attention,
            None
        );
        for label in [
            "#(touch /tmp/unsafe)",
            "#{client_pid}",
            "#[fg=red]",
            "%H",
            "name,else}",
            "hello\nworld",
            "\x1b[2J",
            "a|b",
            "a\u{202e}b",
        ] {
            assert!(
                Summary::decode(&format!("v2|1,2,4,4,0|{label}")).is_err(),
                "{label:?}"
            );
        }
        assert!(Summary::decode(&format!("v2|1,2,4,4,0|{}", "a".repeat(161))).is_err());
        assert!(Summary::decode("v2|0,1,0,0,0|resolved").is_err());
        let hostile = summarize(&[request("#{client_pid},%H\x1b[2J", 5.0)], "");
        let label = hostile.latest_attention.unwrap();
        assert!(!label.contains(['#', '%', ',', '{', '}', '\x1b']));
        assert_eq!(shorten_label("策略测试abcdef", 8), "策略测…");
        let text = summary.tmux_text();
        assert!(text.contains("client_width"));
        assert!(text.contains("↑ ts_quality @rs2a"));
        assert!(!text.contains("#[fg="));
    }

    #[test]
    fn independent_consumers_keep_headless_producer_alive_until_last_unsubscribe() {
        use std::sync::atomic::Ordering;
        let source = Source::default();
        let publisher = source.publisher();
        let stopped = Arc::new(AtomicBool::new(false));
        let stopped_worker = stopped.clone();
        let (refresh, _requests) = mpsc::sync_channel(1);
        source.own_observer(refresh, move || {
            stopped_worker.store(true, Ordering::SeqCst)
        });
        let mut board = source.subscribe();
        let mut panel = source.subscribe();
        publisher.publish(vec![], vec![]);
        let first = board.take().unwrap();
        let other = panel.take().unwrap();
        assert_eq!(first.revision, other.revision);
        assert_eq!(first.summary, other.summary);
        assert!(board.take().is_none());
        drop(board);
        drop(source);
        assert!(!stopped.load(Ordering::SeqCst));
        publisher.health(vec!["Host disconnected; cached inventory retained".into()]);
        publisher.health(vec!["Host reconnected".into()]);
        let latest = panel.take().unwrap();
        assert_eq!(latest.revision, first.revision + 2);
        assert_eq!(latest.summary, first.summary);
        assert_eq!(latest.health, ["Host reconnected"]);
        assert!(panel.take().is_none());
        drop(panel);
        assert!(stopped.load(Ordering::SeqCst));
        publisher.publish(vec![], vec!["Must not resurrect the service".into()]);
        assert_eq!(publisher.state.lock().unwrap().revision, latest.revision);
    }

    fn summary() -> Summary {
        Summary {
            counts: [1, 2, 4, 4],
            colors: false,
            latest_attention: None,
            attention: None,
        }
    }

    #[test]
    fn one_live_source_coalesces_updates_and_cleans_up_its_sinks() {
        let source = Source::default();
        let (send, receive) = std::sync::mpsc::channel();
        source.0.state.lock().unwrap().summary = Some(summary());
        source.start("fixture".into(), move |value| {
            send.send(value)?;
            Ok(())
        });
        assert_eq!(
            receive.recv_timeout(Duration::from_secs(2)).unwrap(),
            Some(summary())
        );
        let changed = Summary {
            counts: [2, 1, 4, 4],
            ..summary()
        };
        source.0.state.lock().unwrap().summary = Some(changed.clone());
        assert_eq!(
            receive.recv_timeout(Duration::from_secs(2)).unwrap(),
            Some(changed.clone())
        );
        assert!(receive.recv_timeout(Duration::from_millis(200)).is_err());
        // A replacement request is a live update even when counts do not change.
        for name in ["first @node", "next @node"] {
            let labelled = Summary {
                latest_attention: Some(name.into()),
                ..changed.clone()
            };
            source.0.state.lock().unwrap().summary = Some(labelled.clone());
            assert_eq!(
                receive.recv_timeout(Duration::from_secs(2)).unwrap(),
                Some(labelled)
            );
        }
        drop(source);
        assert_eq!(receive.recv_timeout(Duration::from_secs(2)).unwrap(), None);
    }

    #[cfg(unix)]
    #[test]
    fn remote_mirror_sends_origin_counts_not_inventory_and_is_capability_gated() {
        remote_mirror(1);
    }
    #[cfg(unix)]
    #[test]
    fn remote_mirror_adds_request_label_only_for_v2_hosts() {
        remote_mirror(2);
    }
    #[cfg(unix)]
    #[test]
    fn remote_mirror_sends_typed_warnings_only_to_v3_hosts() {
        remote_mirror(3);
    }
    #[cfg(unix)]
    #[test]
    fn remote_mirror_sends_ready_suffix_only_to_v4_hosts() {
        remote_mirror(4);
    }
    #[cfg(unix)]
    fn remote_mirror(version: u8) {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let executable = root.path().join("ssh");
        let frames = root.path().join("frames");
        let trace = root.path().join("args");
        std::fs::write(&executable, format!("#!/bin/sh\nprintf '%s\\n' \"$@\" >> {}\nwhile IFS= read -r frame; do printf '%s\\n' \"$frame\" >> {}; done\n", shell_words::quote(trace.to_str().unwrap()), shell_words::quote(frames.to_str().unwrap()))).unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let ssh = SshTransport::new(executable, Duration::from_secs(1), Duration::from_secs(2));
        let mut node = FleetNode {
            node_id: uuid::Uuid::new_v4().to_string(),
            alias: "remote".into(),
            ssh_target: "fake-host".into(),
            sources: vec![],
            status: "ready".into(),
            protocol_version: None,
            package_version: None,
            capabilities: vec![],
            last_seen: 0.0,
            last_attempt_at: 0.0,
            last_error: None,
            created_at: 0.0,
            updated_at: 0.0,
        };
        let source = Source::default();
        source.0.state.lock().unwrap().summary = Some(Summary {
            latest_attention: Some("ts_quality @rs2a".into()),
            ..summary()
        });
        with(Some(Context::Source(source.clone())), || {
            let mut args = vec!["_fleet-open".into()];
            forward(&mut args, &node, &ssh).unwrap();
            assert_eq!(args.len(), 1);
            assert!(!trace.exists());
            node.capabilities.push("board-feed-v1".into());
            if version >= 2 {
                node.capabilities.push("board-feed-v2".into());
            }
            if version >= 3 {
                node.capabilities.push("board-feed-v3".into());
            }
            if version >= 4 {
                node.capabilities.push("board-feed-v4".into());
            }
            forward(&mut args, &node, &ssh).unwrap();
            assert_eq!(args, ["_fleet-open", "--board-feed", &source.0.token]);
            // A second window shares the same source->destination stream.
            forward(&mut vec!["_fleet-open".into()], &node, &ssh).unwrap();
        });
        let wait = |expected: &str| {
            let until = Instant::now() + Duration::from_secs(3);
            while !std::fs::read_to_string(&frames)
                .unwrap_or_default()
                .contains(expected)
            {
                assert!(Instant::now() < until, "missing frame {expected}");
                thread::sleep(Duration::from_millis(10));
            }
        };
        wait("1,2,4,4,0");
        let first = std::fs::read_to_string(&frames).unwrap();
        assert_eq!(
            first.contains("v2|1,2,4,4,0|ts_quality @rs2a"),
            version == 2
        );
        assert_eq!(first.contains("ts_quality"), version >= 2);
        source.0.state.lock().unwrap().summary = Some(Summary {
            latest_attention: Some("warning-only".into()),
            attention: Some(AttentionKind::OpenTwice),
            ..summary()
        });
        if version == 3 {
            wait("v3|1,2,4,4,0|open-twice|warning-only");
        } else if version >= 4 {
            wait("v4|1,2,4,4,0|open-twice|warning-only");
        }
        source.0.state.lock().unwrap().summary = Some(Summary {
            latest_attention: Some("ready-only".into()),
            attention: Some(AttentionKind::Ready),
            ..summary()
        });
        if version == 3 {
            wait("v3|1,2,4,4,0|request|");
        } else if version >= 4 {
            wait("v4|1,2,4,4,0|ready|ready-only");
        }
        source.0.state.lock().unwrap().summary = Some(Summary {
            counts: [0, 3, 4, 4],
            ..summary()
        });
        wait("0,3,4,4,0");
        assert_eq!(
            std::fs::read_to_string(&frames)
                .unwrap()
                .contains("warning-only"),
            version >= 3
        );
        drop(source);
        let trace = std::fs::read_to_string(trace).unwrap();
        assert_eq!(trace.lines().filter(|line| *line == "-T").count(), 1);
        assert!(trace.contains("_board-feed"));
        assert!(trace.contains(&node.node_id));
        assert!(trace.contains("RemoteCommand=none"));
        assert!(!trace.contains("_fleet-open"));
        assert!(!trace.contains("snapshot"));
    }
    #[test]
    fn numeric_frames_and_feed_tokens_cannot_carry_terminal_programs() {
        let summary = Summary {
            counts: [1, 2, 4, 4],
            colors: true,
            latest_attention: None,
            attention: None,
        };
        assert_eq!(Summary::decode(&summary.encode()).unwrap(), summary);
        for bad in [
            "",
            "1,2,3",
            "1,2,3,4,2",
            "1000001,0,0,0,1",
            "-1,0,0,0,1",
            "1,2,3,4,1;sh",
            "#{client_pid}",
            "1,2,3,4,1,0",
        ] {
            assert!(Summary::decode(bad).is_err());
        }
        assert!(token("#{run-shell}").is_err());
        assert!(token(&uuid::Uuid::new_v4().simple().to_string()).is_ok());
        assert_eq!(
            Summary {
                colors: false,
                ..summary
            }
            .tmux_text(),
            "1 need you · 2 working · 4 ready · 4 parked"
        );
    }

    #[test]
    fn return_status_frames_are_typed_bounded_and_preserve_no_color() {
        let status = ReturnStatus {
            provider: Provider::Codex,
            session_id: "id-target".into(),
            status: Some(Status::Ready),
            colors: true,
        };
        assert_eq!(ReturnStatus::decode(&status.encode()).unwrap(), status);
        let plain = ReturnStatus {
            colors: false,
            ..status.clone()
        };
        assert_eq!(ReturnStatus::decode(&plain.encode()).unwrap(), plain);
        for frame in [
            "s1|codex|id-target|ready|2",
            "s1|shell|id-target|ready|1",
            "s1|codex|#{client_pid}|ready|1",
            "s1|codex|id-target|fg=red|1",
            "s1|codex|id-target|ready|1|extra",
        ] {
            assert!(ReturnStatus::decode(frame).is_err(), "{frame}");
        }
    }

    #[test]
    fn return_interest_projection_tracks_exact_target_without_inventory() {
        let source = Source::default();
        let publisher = source.publisher();
        let mut row = request("target", 1.0);
        row.session.status = Status::Working;
        assert!(source.register_return_interest(None, Provider::Codex, "id-target"));
        publisher.publish(vec![row.clone()], vec![]);
        let projected = Source::return_statuses(&source.0.state, None);
        assert_eq!(projected.len(), 1);
        assert_eq!(projected[0].status, Some(Status::Working));
        assert!(projected[0].colors);
        row.session.status = Status::Ready;
        publisher.publish(vec![row], vec![]);
        assert_eq!(
            Source::return_statuses(&source.0.state, None)[0].status,
            Some(Status::Ready)
        );
        let mut stale = request("target", 2.0);
        stale.stale = true;
        publisher.publish(vec![stale], vec![]);
        assert_eq!(
            Source::return_statuses(&source.0.state, None)[0].status,
            None
        );
    }

    #[test]
    fn return_interest_projection_is_node_and_uuid_scoped() {
        let source = Source::default();
        let publisher = source.publisher();
        let mut local = request("same", 1.0);
        local.session.session_id = "same-id".into();
        local.session.status = Status::Working;
        let mut remote = local.clone();
        remote.node_id = Some("node-a".into());
        remote.session.status = Status::Ready;
        let mut other = local.clone();
        other.node_id = Some("node-b".into());
        other.session.session_id = "other-id".into();
        other.session.status = Status::Error;
        publisher.publish(vec![local, remote, other], vec![]);
        assert!(source.register_return_interest(None, Provider::Codex, "same-id"));
        assert!(source.register_return_interest(Some("node-a"), Provider::Codex, "same-id"));
        assert!(source.register_return_interest(Some("node-b"), Provider::Codex, "other-id"));
        assert_eq!(
            Source::return_statuses(&source.0.state, None)[0].status,
            Some(Status::Working)
        );
        assert_eq!(
            Source::return_statuses(&source.0.state, Some("node-a"))[0].status,
            Some(Status::Ready)
        );
        assert_eq!(
            Source::return_statuses(&source.0.state, Some("node-b"))[0].status,
            Some(Status::Error)
        );
    }

    #[test]
    fn context_is_scoped_to_the_action_not_other_threads() {
        let feed = "a".repeat(32);
        with(Some(Context::Remote(feed.clone())), || {
            assert!(matches!(current(), Some(Context::Remote(token)) if token == feed));
            thread::spawn(|| assert!(current().is_none()))
                .join()
                .unwrap();
            with(None, || assert!(current().is_none()));
            assert!(current().is_some());
        });
        assert!(current().is_none());
    }
}
