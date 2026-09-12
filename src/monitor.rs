use crate::{
    consult::CancellationToken,
    model::{Provider, Session, Status},
    usage,
};
use anyhow::Result;
use crossterm::{
    cursor::{Hide, MoveTo, Show},
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute, queue,
    style::{Attribute, Color, Print, ResetColor, SetAttribute, SetForegroundColor},
    terminal::{
        Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
        enable_raw_mode, size,
    },
};
use std::{
    collections::VecDeque,
    io::{self, IsTerminal, Write},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender, TryRecvError},
    },
    thread,
    time::{Duration, Instant},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ExpertAnnotation {
    pub scope: Option<String>,
    pub current_work: Option<String>,
    pub topics: Vec<String>,
    pub freshness: Option<String>,
}

/// Everything required to act on one exact row. Machine identity is part of the
/// key, so an equal provider UUID discovered on two nodes remains two rows.
#[derive(Clone, Debug, PartialEq)]
pub struct BoardItem {
    pub session: Session,
    pub node_id: Option<String>,
    pub node_name: Option<String>,
    pub stale: bool,
    /// Exact pending-launch identity. `None` means this is a durable provider
    /// conversation; `Some` lets the caller act without manufacturing a UUID.
    pub pending_token: Option<String>,
    pub expert: Option<ExpertAnnotation>,
}

impl BoardItem {
    pub fn local(session: Session) -> Self {
        Self {
            session,
            node_id: None,
            node_name: None,
            stale: false,
            pending_token: None,
            expert: None,
        }
    }

    fn key(&self) -> BoardKey {
        (
            self.node_id.clone(),
            self.session.provider,
            self.pending_token
                .as_ref()
                .map(|token| format!("pending:{token}"))
                .unwrap_or_else(|| self.session.session_id.clone()),
        )
    }

    fn node_label(&self) -> Option<&str> {
        self.node_name.as_deref().or(self.node_id.as_deref())
    }
}

type BoardKey = (Option<String>, Provider, String);

/// A one-slot channel whose producer always replaces an unpublished value.
/// The board polls for changes already, so a separate wakeup queue would only
/// reintroduce the stale-snapshot race this channel is meant to avoid.
#[derive(Clone)]
pub(crate) struct LatestSender<T> {
    value: Arc<Mutex<Option<T>>>,
}

pub(crate) struct LatestReceiver<T> {
    value: Arc<Mutex<Option<T>>>,
}

pub(crate) fn latest_channel<T>() -> (LatestSender<T>, LatestReceiver<T>) {
    let value = Arc::new(Mutex::new(None));
    (
        LatestSender {
            value: Arc::clone(&value),
        },
        LatestReceiver { value },
    )
}

impl<T> LatestSender<T> {
    pub(crate) fn publish(&self, value: T) {
        *self.value.lock().expect("latest-value channel poisoned") = Some(value);
    }
}

impl<T> LatestReceiver<T> {
    fn take(&self) -> Option<T> {
        self.value
            .lock()
            .expect("latest-value channel poisoned")
            .take()
    }
}

enum ItemUpdates {
    Queue(Receiver<Vec<BoardItem>>),
    Latest(LatestReceiver<Vec<BoardItem>>),
}

#[derive(Clone, Debug, PartialEq)]
pub enum BoardAction {
    Open(BoardItem),
    Peek(BoardItem),
    Untrack(BoardItem),
    /// Compatibility placeholder for callers compiled against the preview API.
    /// The board never emits it; questions stay in the alternate-screen panel.
    Ask(BoardItem),
    Refresh,
    /// `Some(version)` installs the exact release approved in the board;
    /// `None` performs an explicit update check.
    Update(Option<String>),
    Quit,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConsultationInput {
    Question(String),
    Close,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConsultationEvent {
    Opened {
        child_id: Option<String>,
        policy: Option<String>,
    },
    Progress(String),
    Answer(String),
    Error {
        message: String,
        retry_safe: bool,
    },
}

/// The callback must open exactly one private child, serve all questions from
/// this receiver, and close the child on `Close`. The board owns its lifetime.
pub struct ConsultationIo {
    pub item: BoardItem,
    pub commands: Receiver<ConsultationInput>,
    pub events: SyncSender<ConsultationEvent>,
    /// Flips immediately on the first close request, including while provider
    /// startup or a turn is blocked. Workers must pass it to owned I/O.
    pub cancellation: CancellationToken,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConsultationOutcome {
    pub discarded: bool,
    pub note: Option<String>,
}

impl ConsultationOutcome {
    pub fn discarded() -> Self {
        Self {
            discarded: true,
            note: None,
        }
    }
}

/// Provider- and fleet-independent consultation callback. Pika runs it on a
/// dedicated thread, keeping slow provider startup away from paints and keys.
#[derive(Clone)]
pub struct ConsultationDriver {
    worker: Arc<dyn Fn(ConsultationIo) -> Result<ConsultationOutcome> + Send + Sync + 'static>,
}

impl ConsultationDriver {
    pub fn new<F>(worker: F) -> Self
    where
        F: Fn(ConsultationIo) -> Result<ConsultationOutcome> + Send + Sync + 'static,
    {
        Self {
            worker: Arc::new(worker),
        }
    }

    fn start(&self, item: BoardItem) -> ChatState {
        let name = match item.node_label() {
            Some(node) => format!("{} @{node}", item.session.display_name()),
            None => item.session.display_name(),
        };
        let (command_sender, command_receiver) = mpsc::channel();
        let (event_sender, event_receiver) = mpsc::sync_channel(64);
        let (finish_sender, finish_receiver) = mpsc::channel();
        let cancellation = CancellationToken::default();
        let worker_cancellation = cancellation.clone();
        let worker = Arc::clone(&self.worker);
        let worker_handle = thread::spawn(move || {
            let result = worker(ConsultationIo {
                item,
                commands: command_receiver,
                events: event_sender,
                cancellation: worker_cancellation,
            });
            let _ = finish_sender.send(result.map_err(|error| error.to_string()));
        });
        ChatState::opening(
            name,
            command_sender,
            event_receiver,
            finish_receiver,
            cancellation,
            Some(worker_handle),
        )
    }
}

pub fn interactive_terminal() -> bool {
    io::stdin().is_terminal() && io::stdout().is_terminal()
}

pub fn run(sessions: Vec<Session>) -> Result<BoardAction> {
    run_loop(
        sessions.into_iter().map(BoardItem::local).collect(),
        None,
        None,
        None,
        None,
        None,
    )
}

/// Backwards-compatible local dynamic board. New integrations should supply
/// `BoardItem`s to `run_items_dynamic` so node and expert metadata are retained.
pub fn run_dynamic(sessions: Vec<Session>, updates: Receiver<Vec<Session>>) -> Result<BoardAction> {
    let (sender, item_updates) = mpsc::sync_channel(1);
    thread::spawn(move || {
        for sessions in updates {
            if sender
                .send(sessions.into_iter().map(BoardItem::local).collect())
                .is_err()
            {
                break;
            }
        }
    });
    run_loop(
        sessions.into_iter().map(BoardItem::local).collect(),
        Some(ItemUpdates::Queue(item_updates)),
        None,
        None,
        None,
        None,
    )
}

/// Cached-first dynamic board with exact fleet identity, expert-card facts, and
/// an in-panel asynchronous multi-turn consultation surface. A pending launch is
/// visible only when the caller supplies an item whose status is `Starting`.
pub fn run_items_dynamic(
    items: Vec<BoardItem>,
    updates: Receiver<Vec<BoardItem>>,
    driver: Option<ConsultationDriver>,
) -> Result<BoardAction> {
    run_loop(
        items,
        Some(ItemUpdates::Queue(updates)),
        driver,
        None,
        None,
        None,
    )
}

/// Dynamic board plus a single best-effort background update notice. The
/// notice channel never owns board liveness and network failure is invisible.
pub fn run_items_dynamic_with_notice(
    items: Vec<BoardItem>,
    updates: Receiver<Vec<BoardItem>>,
    driver: Option<ConsultationDriver>,
    update_notice: Receiver<Option<String>>,
) -> Result<BoardAction> {
    run_loop(
        items,
        Some(ItemUpdates::Queue(updates)),
        driver,
        Some(update_notice),
        None,
        None,
    )
}

/// Dynamic board with a coalescing in-place refresh trigger. One render loop
/// and one caller-owned worker set serve the board for its whole lifetime.
pub fn run_items_dynamic_with_notice_and_refresh(
    items: Vec<BoardItem>,
    updates: Receiver<Vec<BoardItem>>,
    driver: Option<ConsultationDriver>,
    update_notice: Receiver<Option<String>>,
    refresh_request: SyncSender<()>,
) -> Result<BoardAction> {
    run_loop(
        items,
        Some(ItemUpdates::Queue(updates)),
        driver,
        Some(update_notice),
        Some(refresh_request),
        None,
    )
}

/// Local observation health is separate from conversation status and cached
/// item updates. A latest-value flag cannot queue a stale warning after recovery.
pub(crate) fn run_items_dynamic_with_local_health(
    items: Vec<BoardItem>,
    updates: LatestReceiver<Vec<BoardItem>>,
    driver: Option<ConsultationDriver>,
    update_notice: Receiver<Option<String>>,
    refresh_request: SyncSender<()>,
    local_refresh_delayed: Arc<AtomicBool>,
) -> Result<BoardAction> {
    run_loop(
        items,
        Some(ItemUpdates::Latest(updates)),
        driver,
        Some(update_notice),
        Some(refresh_request),
        Some(local_refresh_delayed),
    )
}

fn run_loop(
    items: Vec<BoardItem>,
    updates: Option<ItemUpdates>,
    driver: Option<ConsultationDriver>,
    update_notice: Option<Receiver<Option<String>>>,
    refresh_request: Option<SyncSender<()>>,
    local_refresh_delayed: Option<Arc<AtomicBool>>,
) -> Result<BoardAction> {
    let _terminal = TerminalGuard::enter()?;
    let mut board = Board::new(items);
    let mut stdout = io::stdout().lock();
    let mut dirty = true;
    let mut last_draw = Instant::now()
        .checked_sub(Duration::from_secs(1))
        .unwrap_or_else(Instant::now);
    loop {
        if let Some(updates) = &updates {
            match updates {
                ItemUpdates::Queue(updates) => {
                    while let Ok(items) = updates.try_recv() {
                        board.replace_items(items);
                        dirty = true;
                    }
                }
                ItemUpdates::Latest(updates) => {
                    if let Some(items) = updates.take() {
                        board.replace_items(items);
                        dirty = true;
                    }
                }
            }
        }
        if let Some(notice) = &update_notice {
            match notice.try_recv() {
                Ok(version) => {
                    board.update_version = version;
                    dirty = true;
                }
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => {}
            }
        }
        if let Some(delayed) = &local_refresh_delayed {
            dirty |= board.observe_local_refresh(delayed);
        }
        dirty |= board.drain_consultation();
        if board.quit_when_chat_closes && board.chat.is_none() {
            return Ok(BoardAction::Quit);
        }
        let animated = board.chat.as_ref().is_some_and(|chat| {
            matches!(
                chat.phase,
                ChatPhase::Opening | ChatPhase::Waiting | ChatPhase::Closing
            )
        });
        let paint_interval = if animated {
            Duration::from_millis(150)
        } else {
            Duration::from_secs(1)
        };
        if dirty || last_draw.elapsed() >= paint_interval {
            let (width, height) = size().unwrap_or((100, 30));
            board.ensure_visible(height);
            board.draw(&mut stdout, width, height)?;
            stdout.flush()?;
            last_draw = Instant::now();
            dirty = false;
        }
        // Terminal input wakes poll immediately. When no consultation is
        // animating, the one-second ceiling keeps store-backed lifecycle
        // changes within the product's one-second visibility contract without
        // waking once between otherwise scheduled clock paints.
        let input_wait = if animated {
            Duration::from_millis(100)
        } else {
            Duration::from_secs(1)
        };
        if !event::poll(input_wait)? {
            continue;
        }
        match event::read()? {
            Event::Resize(_, _) => dirty = true,
            Event::Key(key) if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
                if let Some(action) = board.key(key, driver.as_ref()) {
                    if let Some(action) = route_board_action(action, refresh_request.as_ref()) {
                        return Ok(action);
                    } else {
                        dirty = true;
                        continue;
                    }
                }
                dirty = true;
            }
            _ => {}
        }
    }
}

fn route_board_action(
    action: BoardAction,
    refresh_request: Option<&SyncSender<()>>,
) -> Option<BoardAction> {
    if action == BoardAction::Refresh
        && let Some(request) = refresh_request
    {
        // Capacity one deliberately coalesces a burst of `r` keys into one
        // fresh observation pass instead of growing work or call-stack depth.
        let _ = request.try_send(());
        return None;
    }
    Some(action)
}

struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        if let Err(error) = execute!(io::stdout(), EnterAlternateScreen, Hide) {
            let _ = disable_raw_mode();
            return Err(error.into());
        }
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), Show, LeaveAlternateScreen, ResetColor);
        let _ = disable_raw_mode();
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ChatPhase {
    Opening,
    Ready,
    Waiting,
    Closing,
    Failed,
    Closed,
}

#[derive(Clone, Copy)]
enum ChatRole {
    You,
    Expert,
    Pika,
}

struct ChatLine {
    role: ChatRole,
    text: String,
}

struct ChatState {
    name: String,
    input: String,
    input_limit_reached: bool,
    lines: VecDeque<ChatLine>,
    retained_bytes: usize,
    history_truncated: bool,
    phase: ChatPhase,
    command_sender: Option<mpsc::Sender<ConsultationInput>>,
    event_receiver: Option<Receiver<ConsultationEvent>>,
    finish_receiver: Option<Receiver<std::result::Result<ConsultationOutcome, String>>>,
    child_id: Option<String>,
    policy: Option<String>,
    scroll: usize,
    close_requested: bool,
    cancellation: CancellationToken,
    worker: Option<thread::JoinHandle<()>>,
}

impl ChatState {
    const MAX_INPUT_BYTES: usize = 64 * 1024;

    fn opening(
        name: String,
        command_sender: mpsc::Sender<ConsultationInput>,
        event_receiver: Receiver<ConsultationEvent>,
        finish_receiver: Receiver<std::result::Result<ConsultationOutcome, String>>,
        cancellation: CancellationToken,
        worker: Option<thread::JoinHandle<()>>,
    ) -> Self {
        Self {
            name,
            input: String::new(),
            input_limit_reached: false,
            lines: VecDeque::from([ChatLine {
                role: ChatRole::Pika,
                text: "Opening a private side conversation…".into(),
            }]),
            retained_bytes: "Opening a private side conversation…".len(),
            history_truncated: false,
            phase: ChatPhase::Opening,
            command_sender: Some(command_sender),
            event_receiver: Some(event_receiver),
            finish_receiver: Some(finish_receiver),
            child_id: None,
            policy: None,
            scroll: 0,
            close_requested: false,
            cancellation,
            worker,
        }
    }

    fn unavailable(name: String) -> Self {
        Self {
            name,
            input: String::new(),
            input_limit_reached: false,
            lines: VecDeque::from([ChatLine {
                role: ChatRole::Pika,
                text: "Private consultation is unavailable in this board invocation.".into(),
            }]),
            retained_bytes: "Private consultation is unavailable in this board invocation.".len(),
            history_truncated: false,
            phase: ChatPhase::Failed,
            command_sender: None,
            event_receiver: None,
            finish_receiver: None,
            child_id: None,
            policy: None,
            scroll: 0,
            close_requested: false,
            cancellation: CancellationToken::default(),
            worker: None,
        }
    }

    fn append_input(&mut self, value: char) {
        if !matches!(
            self.phase,
            ChatPhase::Opening | ChatPhase::Ready | ChatPhase::Waiting
        ) {
            return;
        }
        if self.input.len() + value.len_utf8() > Self::MAX_INPUT_BYTES {
            self.input_limit_reached = true;
            return;
        }
        self.input.push(value);
        self.input_limit_reached = false;
    }

    fn send_question(&mut self) {
        if self.phase != ChatPhase::Ready || self.input.trim().is_empty() {
            return;
        }
        let question = self.input.trim().to_owned();
        self.push_line(ChatRole::You, question.clone());
        self.input.clear();
        self.input_limit_reached = false;
        self.scroll = 0;
        if self
            .command_sender
            .as_ref()
            .is_some_and(|sender| sender.send(ConsultationInput::Question(question)).is_ok())
        {
            self.phase = ChatPhase::Waiting;
        } else {
            self.fail("The private consultation worker stopped.".into());
        }
    }

    /// Returns true when the panel can be removed immediately. The first escape
    /// asks the worker to clean up; a second escape never traps the user in UI.
    fn request_close(&mut self) -> bool {
        if self.close_requested || self.phase == ChatPhase::Closed {
            return true;
        }
        self.close_requested = true;
        self.cancellation.cancel();
        self.phase = ChatPhase::Closing;
        self.push_line(
            ChatRole::Pika,
            "Closing and discarding the private side conversation…".into(),
        );
        self.command_sender
            .as_ref()
            .is_none_or(|sender| sender.send(ConsultationInput::Close).is_err())
    }

    fn fail(&mut self, message: String) {
        self.push_line(ChatRole::Pika, format!("ERROR · {message}"));
        self.phase = ChatPhase::Failed;
    }

    fn push_line(&mut self, role: ChatRole, text: String) {
        const MAX_LINE_BYTES: usize = 64 * 1024;
        const MAX_HISTORY_BYTES: usize = 256 * 1024;
        const MAX_HISTORY_LINES: usize = 256;
        let text = if text.len() > MAX_LINE_BYTES {
            let mut boundary = MAX_LINE_BYTES;
            while !text.is_char_boundary(boundary) {
                boundary -= 1;
            }
            format!("{}\n… response truncated by Pika …", &text[..boundary])
        } else {
            text
        };
        self.retained_bytes = self.retained_bytes.saturating_add(text.len());
        self.lines.push_back(ChatLine { role, text });
        while self.lines.len() > MAX_HISTORY_LINES || self.retained_bytes > MAX_HISTORY_BYTES {
            if let Some(removed) = self.lines.pop_front() {
                self.retained_bytes = self.retained_bytes.saturating_sub(removed.text.len());
                self.history_truncated = true;
            } else {
                break;
            }
        }
    }

    fn accept(&mut self, event: ConsultationEvent) {
        match event {
            ConsultationEvent::Opened { child_id, policy } => {
                self.child_id = child_id;
                self.policy = policy;
                if !self.close_requested {
                    self.phase = ChatPhase::Ready;
                }
                self.push_line(
                    ChatRole::Pika,
                    "Private side ready. The parent conversation remains untouched.".into(),
                );
            }
            ConsultationEvent::Progress(message) => self.push_line(ChatRole::Pika, message),
            ConsultationEvent::Answer(answer) => {
                self.push_line(ChatRole::Expert, answer);
                if !self.close_requested {
                    self.phase = ChatPhase::Ready;
                }
            }
            ConsultationEvent::Error {
                message,
                retry_safe,
            } => {
                self.push_line(
                    ChatRole::Pika,
                    if retry_safe {
                        format!("NOT SENT · {message} · safe to retry")
                    } else {
                        format!("DELIVERY UNCERTAIN · {message} · do not resend automatically")
                    },
                );
                self.phase = if self.close_requested {
                    ChatPhase::Closing
                } else if retry_safe {
                    ChatPhase::Ready
                } else {
                    ChatPhase::Failed
                };
            }
        }
    }

    fn finish(&mut self, result: std::result::Result<ConsultationOutcome, String>) -> bool {
        match result {
            Ok(outcome) => {
                if let Some(note) = outcome.note {
                    self.push_line(ChatRole::Pika, note);
                }
                self.phase = if outcome.discarded {
                    ChatPhase::Closed
                } else {
                    ChatPhase::Failed
                };
                self.close_requested && outcome.discarded
            }
            Err(error) => {
                self.fail(error);
                false
            }
        }
    }
}

impl Drop for ChatState {
    fn drop(&mut self) {
        let _ = self.request_close();
        // The provider/SSH process group belongs to this panel. Cancellation
        // interrupts every supported worker path; joining here ensures a
        // second Ctrl-C or Esc cannot let main exit while cleanup is detached.
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

struct Board {
    items: Vec<BoardItem>,
    selected_key: Option<BoardKey>,
    filter: String,
    filtering: bool,
    offset: usize,
    chat: Option<ChatState>,
    quit_when_chat_closes: bool,
    update_version: Option<String>,
    update_prompt: bool,
    local_refresh_delayed: bool,
}

impl Board {
    fn new(mut items: Vec<BoardItem>) -> Self {
        sort_items(&mut items);
        let selected_key = items.first().map(BoardItem::key);
        Self {
            items,
            selected_key,
            filter: String::new(),
            filtering: false,
            offset: 0,
            chat: None,
            quit_when_chat_closes: false,
            update_version: None,
            update_prompt: false,
            local_refresh_delayed: false,
        }
    }

    fn observe_local_refresh(&mut self, delayed: &AtomicBool) -> bool {
        let delayed = delayed.load(Ordering::Relaxed);
        let changed = self.local_refresh_delayed != delayed;
        self.local_refresh_delayed = delayed;
        changed
    }

    fn visible(&self) -> Vec<&BoardItem> {
        let needle = self.filter.to_lowercase();
        self.items
            .iter()
            .filter(|item| Self::matches_filter(item, &needle))
            .collect()
    }

    fn matches_filter(item: &BoardItem, needle: &str) -> bool {
        let session = &item.session;
        needle.is_empty()
            || session.display_name().to_lowercase().contains(needle)
            || session.provider.as_str().contains(needle)
            || session
                .cwd
                .as_deref()
                .is_some_and(|value| value.to_lowercase().contains(needle))
            || item
                .node_label()
                .is_some_and(|value| value.to_lowercase().contains(needle))
    }

    fn replace_items(&mut self, mut items: Vec<BoardItem>) {
        let previous = self.selected_key.clone();
        sort_items(&mut items);
        self.items = items;
        let needle = self.filter.to_lowercase();
        self.selected_key = previous.filter(|key| {
            self.items
                .iter()
                .any(|item| item.key() == *key && Self::matches_filter(item, &needle))
        });
        if self.selected_key.is_none() {
            self.reselect_first();
        }
    }

    fn selected_index(&self, visible: &[&BoardItem]) -> usize {
        self.selected_key
            .as_ref()
            .and_then(|key| visible.iter().position(|item| item.key() == *key))
            .unwrap_or(0)
    }

    fn select(&mut self, delta: isize) {
        let visible = self.visible();
        if visible.is_empty() {
            self.selected_key = None;
            return;
        }
        let current = self.selected_index(&visible) as isize;
        let next = (current + delta).rem_euclid(visible.len() as isize) as usize;
        self.selected_key = Some(visible[next].key());
    }

    fn selected(&self) -> Option<BoardItem> {
        let key = self.selected_key.as_ref()?;
        self.items.iter().find(|item| item.key() == *key).cloned()
    }

    fn begin_chat(&mut self, driver: Option<&ConsultationDriver>) {
        let Some(item) = self.selected() else {
            return;
        };
        self.chat = Some(match driver {
            Some(driver) => driver.start(item),
            None => {
                let name = match item.node_label() {
                    Some(node) => format!("{} @{node}", item.session.display_name()),
                    None => item.session.display_name(),
                };
                ChatState::unavailable(name)
            }
        });
    }

    fn drain_consultation(&mut self) -> bool {
        let mut leave = false;
        let mut changed = false;
        if let Some(chat) = &mut self.chat {
            loop {
                match chat.event_receiver.as_ref().map(Receiver::try_recv) {
                    Some(Ok(event)) => {
                        chat.accept(event);
                        changed = true;
                    }
                    Some(Err(TryRecvError::Disconnected)) | None => break,
                    Some(Err(TryRecvError::Empty)) => break,
                }
            }
            match chat.finish_receiver.as_ref().map(Receiver::try_recv) {
                Some(Ok(result)) => {
                    leave = chat.finish(result);
                    changed = true;
                }
                Some(Err(TryRecvError::Disconnected))
                    if !matches!(chat.phase, ChatPhase::Closed | ChatPhase::Failed) =>
                {
                    chat.fail("The private consultation worker stopped unexpectedly.".into());
                    changed = true;
                }
                Some(Err(TryRecvError::Disconnected | TryRecvError::Empty)) | None => {}
            }
        }
        if leave {
            self.chat = None;
        }
        changed || leave
    }

    fn key(&mut self, key: KeyEvent, driver: Option<&ConsultationDriver>) -> Option<BoardAction> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            if let Some(chat) = &mut self.chat {
                if chat.close_requested {
                    return Some(BoardAction::Quit);
                }
                self.quit_when_chat_closes = true;
                if chat.request_close() {
                    self.chat = None;
                    return Some(BoardAction::Quit);
                }
                return None;
            }
            return Some(BoardAction::Quit);
        }
        if self.chat.is_some() {
            self.chat_key(key);
            return None;
        }
        if self.update_prompt {
            match key.code {
                KeyCode::Enter | KeyCode::Char('y') => {
                    return Some(BoardAction::Update(self.update_version.clone()));
                }
                KeyCode::Esc | KeyCode::Char('n') => self.update_prompt = false,
                _ => {}
            }
            return None;
        }
        if self.filtering {
            match key.code {
                KeyCode::Esc | KeyCode::Enter => self.filtering = false,
                KeyCode::Backspace => {
                    self.filter.pop();
                    self.reselect_first();
                }
                KeyCode::Char(value) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.filter.push(value);
                    self.reselect_first();
                }
                _ => {}
            }
            return None;
        }
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.select(-1),
            KeyCode::Down | KeyCode::Char('j') => self.select(1),
            KeyCode::PageUp => self.select(-8),
            KeyCode::PageDown => self.select(8),
            KeyCode::Home => self.reselect_first(),
            KeyCode::End => {
                self.selected_key = self.visible().last().map(|item| item.key());
            }
            KeyCode::Char('/') => self.filtering = true,
            KeyCode::Esc => {
                self.filter.clear();
                self.reselect_first();
            }
            KeyCode::Enter => return self.selected().map(BoardAction::Open),
            KeyCode::Char('p') => return self.selected().map(BoardAction::Peek),
            KeyCode::Char('x') => return self.selected().map(BoardAction::Untrack),
            KeyCode::Char('a') => self.begin_chat(driver),
            KeyCode::Char('r') => return Some(BoardAction::Refresh),
            KeyCode::Char('U') => {
                if self.update_version.is_some() {
                    self.update_prompt = true;
                } else {
                    return Some(BoardAction::Update(None));
                }
            }
            KeyCode::Char('n') => {
                return self
                    .items
                    .iter()
                    .find(|item| item.session.needs_attention())
                    .cloned()
                    .map(BoardAction::Open);
            }
            KeyCode::Char('q') => return Some(BoardAction::Quit),
            _ => {}
        }
        None
    }

    fn chat_key(&mut self, key: KeyEvent) {
        let mut leave = false;
        if let Some(chat) = &mut self.chat {
            match key.code {
                KeyCode::Esc => leave = chat.request_close(),
                KeyCode::Enter if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    chat.append_input('\n')
                }
                KeyCode::Enter => chat.send_question(),
                KeyCode::Backspace => {
                    chat.input.pop();
                    chat.input_limit_reached = false;
                }
                KeyCode::Up | KeyCode::PageUp => {
                    chat.scroll =
                        chat.scroll
                            .saturating_add(if key.code == KeyCode::PageUp { 8 } else { 1 });
                }
                KeyCode::Down | KeyCode::PageDown => {
                    chat.scroll = chat
                        .scroll
                        .saturating_sub(if key.code == KeyCode::PageDown { 8 } else { 1 });
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    chat.input.clear();
                    chat.input_limit_reached = false;
                }
                KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    chat.append_input('\n');
                }
                KeyCode::Char(value) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    chat.append_input(value);
                }
                _ => {}
            }
        }
        if leave {
            self.chat = None;
        }
    }

    fn reselect_first(&mut self) {
        self.selected_key = self.visible().first().map(|item| item.key());
        self.offset = 0;
    }

    fn ensure_visible(&mut self, height: u16) {
        let visible = self.visible();
        let selected = self.selected_index(&visible);
        let rows = usize::from(height.saturating_sub(7)).max(1);
        if selected < self.offset {
            self.offset = selected;
        }
        if selected >= self.offset + rows {
            self.offset = selected + 1 - rows;
        }
    }

    fn draw(&self, output: &mut impl Write, width: u16, height: u16) -> Result<()> {
        queue!(output, MoveTo(0, 0), Clear(ClearType::All))?;
        if self.update_prompt {
            return self.draw_update_prompt(output, usize::from(width), height);
        }
        let width = usize::from(width);
        let visible = self.visible();
        let needs = visible
            .iter()
            .filter(|item| item_group(item) == "NEEDS YOU")
            .count();
        let working = visible
            .iter()
            .filter(|item| item_group(item) == "WORKING")
            .count();
        let ready = visible
            .iter()
            .filter(|item| item_group(item) == "READY")
            .count();
        let parked = visible
            .iter()
            .filter(|item| item_group(item) == "PARKED")
            .count();
        styled(output, Color::Red, true, "PIKA // LIVE OPERATIONS")?;
        let update = self
            .update_version
            .as_deref()
            .map(|version| format!(" · update {version} · U review"))
            .unwrap_or_default();
        queue!(
            output,
            Print(fit(
                &format!(
                    "  {needs} need you · {working} working · {ready} ready · {parked} parked{update}"
                ),
                width.saturating_sub(24)
            )),
            Print("\r\n")
        )?;

        let list_width = if width >= 100 {
            width.min(160) * 36 / 100
        } else {
            width
        };
        let selected_index = self.selected_index(&visible);
        let rows = usize::from(height.saturating_sub(6)).max(1);
        let mut line = 1_u16;
        let mut prior_group = "";
        for (index, item) in visible.iter().enumerate().skip(self.offset).take(rows) {
            let session = &item.session;
            let current_group = item_group(item);
            if current_group != prior_group && line < height.saturating_sub(2) {
                styled(
                    output,
                    group_color(current_group),
                    false,
                    &format!("\r\n{current_group}"),
                )?;
                queue!(output, Print("\r\n"))?;
                line += 2;
                prior_group = current_group;
            }
            if line >= height.saturating_sub(2) {
                break;
            }
            let marker = if index == selected_index { "›" } else { " " };
            let provider = match session.provider {
                Provider::Codex => "C",
                Provider::Claude => "A",
                Provider::Opencode => "O",
            };
            let age = human_age(session.last_event_at.max(session.last_activity_at));
            let node = item
                .node_label()
                .map(|value| format!(" @{value}"))
                .unwrap_or_default();
            let stale = if item.stale { " ◌" } else { "" };
            let pending = if item.pending_token.is_some() {
                " starting"
            } else {
                ""
            };
            let usable = list_width.saturating_sub(12);
            let label = format!("{}{}{}{}", session.display_name(), node, pending, stale);
            let row = format!(
                "{marker} {provider} {:<width$} {age:>5}",
                truncate(&label, usable),
                width = usable
            );
            if index == selected_index {
                queue!(output, SetAttribute(Attribute::Reverse))?;
            }
            styled(
                output,
                status_color(session.status),
                false,
                &fit(&row, list_width),
            )?;
            if index == selected_index {
                queue!(output, SetAttribute(Attribute::NoReverse))?;
            }
            queue!(output, Print("\r\n"))?;
            line += 1;
        }

        if let Some(chat) = &self.chat {
            self.draw_chat(output, chat, list_width, width, height)?;
        } else if width >= 100
            && let Some(selected) = self.selected()
        {
            self.draw_detail(output, &selected, list_width + 2, width, height)?;
        }
        queue!(
            output,
            MoveTo(0, height.saturating_sub(2)),
            SetForegroundColor(Color::DarkGrey)
        )?;
        if let Some(chat) = &self.chat {
            let help = match chat.phase {
                ChatPhase::Ready => "enter send · ^j newline · ↑↓ scroll · esc close + discard",
                ChatPhase::Opening | ChatPhase::Waiting => {
                    "type next question · ↑↓ scroll · esc close (esc again returns)"
                }
                ChatPhase::Closing => "closing… · esc return to board",
                ChatPhase::Failed | ChatPhase::Closed => "esc return to board",
            };
            queue!(output, Print(fit(help, width)))?;
        } else if self.filtering {
            queue!(
                output,
                Print(fit(&format!("FILTER › {}", self.filter), width))
            )?;
        } else if !self.filter.is_empty() {
            queue!(
                output,
                Print(fit(
                    &format!("FILTER {} · / edit · esc clear", self.filter),
                    width
                ))
            )?;
        } else {
            queue!(
                output,
                Print(fit(
                    "↑↓ move · enter open · p peek · a ask · x stop watching · / filter · r refresh · U update · q leave",
                    width
                ))
            )?;
        }
        if self.local_refresh_delayed && height > 1 {
            queue!(
                output,
                MoveTo(0, height - 1),
                SetForegroundColor(Color::DarkYellow),
                Print(fit("local refresh delayed · r retry", width))
            )?;
        }
        queue!(output, ResetColor)?;
        Ok(())
    }

    fn draw_update_prompt(&self, output: &mut impl Write, width: usize, height: u16) -> Result<()> {
        let version = self.update_version.as_deref().unwrap_or("latest");
        styled(
            output,
            Color::Magenta,
            true,
            &fit(&format!("PIKA // UPDATE {version}"), width),
        )?;
        for (row, line) in [
            "",
            "Install this verified release on this machine only?",
            "Running agents stay running; other machines are unchanged.",
            "Reopen Pika afterward. `pika setup` reviews integration changes.",
        ]
        .into_iter()
        .enumerate()
        {
            queue!(output, MoveTo(0, (row + 1) as u16), Print(fit(line, width)))?;
        }
        queue!(
            output,
            MoveTo(0, height.saturating_sub(2)),
            SetForegroundColor(Color::DarkGrey),
            Print(fit("enter / y install · esc / n cancel", width)),
            ResetColor
        )?;
        Ok(())
    }

    fn draw_detail(
        &self,
        output: &mut impl Write,
        item: &BoardItem,
        x: usize,
        width: usize,
        height: u16,
    ) -> Result<()> {
        let session = &item.session;
        let available = width.saturating_sub(x + 1);
        queue!(
            output,
            MoveTo(x as u16, 2),
            SetAttribute(Attribute::Bold),
            Print(fit(&session.display_name(), available)),
            SetAttribute(Attribute::NoBold),
            MoveTo(x as u16, 3),
            Print(fit(
                &format!("{} · {}", session.provider, session.status),
                available
            )),
            MoveTo(x as u16, 4),
            Print(fit(&format!("id      {}", session.session_id), available))
        )?;
        let mut y = 5_u16;
        if let Some(node) = item.node_label() {
            detail_row(output, x, y, available, "machine", node)?;
            y += 1;
        }
        if let Some(cwd) = &session.cwd {
            detail_row(output, x, y, available, "path", cwd)?;
            y += 1;
        }
        if let Some(branch) = &session.branch {
            detail_row(output, x, y, available, "branch", branch)?;
            y += 1;
        }
        if let Some(model) = &session.model {
            detail_row(output, x, y, available, "model", model)?;
            y += 1;
        }
        if session.total_tokens.is_some() {
            detail_row(
                output,
                x,
                y,
                available,
                "tokens",
                &usage::format_tokens(session.total_tokens),
            )?;
            y += 1;
        }
        let detail = session
            .error
            .as_deref()
            .or(session.attention_reason.as_deref())
            .unwrap_or(match session.status {
                Status::NeedsYou => "Waiting for your answer",
                Status::Ready => "A result is available",
                Status::Working | Status::Starting => "The agent is working",
                Status::Parked => "Saved and ready to resume",
                Status::Unbound => "Live outside its protected home",
                Status::OpenTwice => "Multiple exact processes detected",
                Status::Error => "Recovery needs attention",
            });
        queue!(
            output,
            MoveTo(x as u16, y.saturating_add(1)),
            SetForegroundColor(status_color(session.status)),
            Print(fit(detail, available)),
            ResetColor
        )?;
        y = y.saturating_add(3);
        if let Some(expert) = &item.expert {
            if let Some(freshness) = &expert.freshness {
                detail_row(output, x, y, available, "EXPERT", freshness)?;
                y += 1;
            }
            if let Some(scope) = &expert.scope {
                detail_row(output, x, y, available, "KNOWS", scope)?;
                y += 1;
            }
            if let Some(current) = &expert.current_work {
                detail_row(output, x, y, available, "NOW", current)?;
                y += 1;
            }
            if !expert.topics.is_empty() && y < height.saturating_sub(5) {
                detail_row(
                    output,
                    x,
                    y,
                    available,
                    "TOPICS",
                    &expert.topics.join(" · "),
                )?;
            }
        } else if height > y.saturating_add(2) {
            queue!(
                output,
                MoveTo(x as u16, y),
                SetForegroundColor(Color::DarkGrey),
                Print(fit(
                    "Enter opens the exact conversation in its native agent interface.",
                    available
                )),
                ResetColor
            )?;
        }
        if item.stale && height > 15 {
            queue!(
                output,
                MoveTo(x as u16, height.saturating_sub(5)),
                SetForegroundColor(Color::Yellow),
                Print(fit(
                    "Cached machine state · refresh before acting",
                    available
                )),
                ResetColor
            )?;
        }
        if height > 18 {
            queue!(
                output,
                MoveTo(x as u16, height.saturating_sub(4)),
                SetForegroundColor(Color::DarkGrey),
                Print(fit(playbook_tip(), available)),
                ResetColor
            )?;
        }
        Ok(())
    }

    fn draw_chat(
        &self,
        output: &mut impl Write,
        chat: &ChatState,
        list_width: usize,
        width: usize,
        height: u16,
    ) -> Result<()> {
        let (x, available) = if width >= 100 {
            (list_width + 2, width.saturating_sub(list_width + 3))
        } else {
            queue!(output, MoveTo(0, 0), Clear(ClearType::All))?;
            (0, width)
        };
        queue!(
            output,
            MoveTo(x as u16, 1),
            SetForegroundColor(Color::Magenta),
            SetAttribute(Attribute::Bold),
            Print(fit(&format!("PRIVATE ASK // {}", chat.name), available)),
            SetAttribute(Attribute::NoBold),
            ResetColor,
            MoveTo(x as u16, 2),
            SetForegroundColor(Color::DarkGrey),
            Print(fit(
                "Separate child · parent untouched · discarded on close",
                available
            )),
            ResetColor
        )?;
        let mut metadata = match (&chat.policy, &chat.child_id) {
            (Some(policy), Some(child)) => format!("{policy} · child {}", prefix(child, 8)),
            (Some(policy), None) => policy.clone(),
            _ => phase_label(chat.phase).to_owned(),
        };
        if chat.history_truncated {
            metadata.push_str(" · earlier lines omitted");
        }
        queue!(
            output,
            MoveTo(x as u16, 3),
            SetForegroundColor(phase_color(chat.phase)),
            Print(fit(&metadata, available)),
            ResetColor
        )?;

        let mut rendered = Vec::new();
        for line in &chat.lines {
            let label = match line.role {
                ChatRole::You => "YOU",
                ChatRole::Expert => "EXPERT",
                ChatRole::Pika => "PIKA",
            };
            let indent = label.len() + 3;
            for (index, wrapped) in wrap(&line.text, available.saturating_sub(indent))
                .into_iter()
                .enumerate()
            {
                rendered.push(if index == 0 {
                    format!("{label} · {wrapped}")
                } else {
                    format!("{}{}", " ".repeat(indent), wrapped)
                });
            }
            rendered.push(String::new());
        }
        let first_row = 5_usize;
        let composer_row = usize::from(height.saturating_sub(5));
        let rows = composer_row.saturating_sub(first_row).max(1);
        let end = rendered
            .len()
            .saturating_sub(chat.scroll.min(rendered.len()));
        let start = end.saturating_sub(rows);
        for (row, line) in rendered[start..end].iter().enumerate() {
            let color = if line.starts_with("YOU ·") {
                Color::White
            } else if line.starts_with("EXPERT ·") {
                Color::Cyan
            } else {
                Color::DarkGrey
            };
            queue!(
                output,
                MoveTo(x as u16, (first_row + row) as u16),
                SetForegroundColor(color),
                Print(fit(line, available)),
                ResetColor
            )?;
        }
        let prompt = match chat.phase {
            ChatPhase::Opening | ChatPhase::Waiting => format!(
                "{} {} · › {}",
                spinner(),
                phase_label(chat.phase),
                chat.input.replace('\n', " ↵ ")
            ),
            ChatPhase::Ready => format!("› {}", chat.input.replace('\n', " ↵ ")),
            ChatPhase::Closing | ChatPhase::Failed | ChatPhase::Closed => {
                format!("{} {}", spinner(), phase_label(chat.phase))
            }
        };
        queue!(
            output,
            MoveTo(x as u16, composer_row as u16),
            SetForegroundColor(Color::Magenta),
            Print(fit(
                &if chat.input_limit_reached {
                    "64 KiB draft limit · delete text to continue".to_owned()
                } else {
                    format!("ASK {}", chat.name)
                },
                available
            )),
            MoveTo(x as u16, composer_row.saturating_add(1) as u16),
            SetForegroundColor(Color::White),
            SetAttribute(Attribute::Reverse),
            Print(fit(&prompt, available)),
            SetAttribute(Attribute::NoReverse),
            ResetColor
        )?;
        Ok(())
    }
}

impl Drop for Board {
    fn drop(&mut self) {
        if let Some(chat) = &mut self.chat {
            let _ = chat.request_close();
        }
    }
}

fn detail_row(
    output: &mut impl Write,
    x: usize,
    y: u16,
    width: usize,
    label: &str,
    value: &str,
) -> Result<()> {
    queue!(
        output,
        MoveTo(x as u16, y),
        SetForegroundColor(Color::DarkGrey),
        Print(fit(&format!("{label:<7} {value}"), width)),
        ResetColor
    )?;
    Ok(())
}

fn group(session: &Session) -> &'static str {
    match session.status {
        Status::NeedsYou | Status::Error | Status::OpenTwice | Status::Unbound => "NEEDS YOU",
        Status::Working | Status::Starting => "WORKING",
        Status::Ready => "READY",
        Status::Parked => "PARKED",
    }
}

fn item_group(item: &BoardItem) -> &'static str {
    if item.stale {
        "PARKED"
    } else {
        group(&item.session)
    }
}

fn sort_items(items: &mut [BoardItem]) {
    let rank = |item: &BoardItem| match item_group(item) {
        "NEEDS YOU" => 0_u8,
        "WORKING" => 1,
        "READY" => 2,
        _ => 3,
    };
    items.sort_by(|left, right| {
        rank(left)
            .cmp(&rank(right))
            .then_with(|| right.session.unread.cmp(&left.session.unread))
            .then_with(|| {
                right
                    .session
                    .last_activity_at
                    .total_cmp(&left.session.last_activity_at)
            })
            .then_with(|| {
                left.session
                    .display_name()
                    .to_lowercase()
                    .cmp(&right.session.display_name().to_lowercase())
            })
            .then_with(|| left.node_id.cmp(&right.node_id))
            .then_with(|| left.session.session_id.cmp(&right.session.session_id))
    });
}

fn status_color(status: Status) -> Color {
    match status {
        Status::NeedsYou | Status::Error | Status::OpenTwice => Color::Red,
        Status::Working | Status::Starting => Color::Cyan,
        Status::Ready => Color::Green,
        Status::Unbound => Color::Magenta,
        Status::Parked => Color::DarkGrey,
    }
}

fn group_color(value: &str) -> Color {
    match value {
        "NEEDS YOU" => Color::Red,
        "WORKING" => Color::Cyan,
        "READY" => Color::Green,
        _ => Color::DarkGrey,
    }
}

fn phase_color(phase: ChatPhase) -> Color {
    match phase {
        ChatPhase::Opening | ChatPhase::Waiting | ChatPhase::Closing => Color::Yellow,
        ChatPhase::Ready => Color::Green,
        ChatPhase::Failed => Color::Red,
        ChatPhase::Closed => Color::DarkGrey,
    }
}

fn phase_label(phase: ChatPhase) -> &'static str {
    match phase {
        ChatPhase::Opening => "opening private side…",
        ChatPhase::Ready => "ready",
        ChatPhase::Waiting => "waiting for answer…",
        ChatPhase::Closing => "closing private side…",
        ChatPhase::Failed => "consultation needs attention",
        ChatPhase::Closed => "private side discarded",
    }
}

fn spinner() -> char {
    const FRAMES: &[char] = &['◐', '◓', '◑', '◒'];
    let tick = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |value| value.as_millis() / 150);
    FRAMES[(tick as usize) % FRAMES.len()]
}

fn prefix(value: &str, chars: usize) -> &str {
    value
        .char_indices()
        .nth(chars)
        .map_or(value, |(index, _)| &value[..index])
}

fn styled(output: &mut impl Write, color: Color, bold: bool, value: &str) -> Result<()> {
    queue!(output, SetForegroundColor(color))?;
    if bold {
        queue!(output, SetAttribute(Attribute::Bold))?;
    }
    queue!(output, Print(value))?;
    if bold {
        queue!(output, SetAttribute(Attribute::NoBold))?;
    }
    queue!(output, ResetColor)?;
    Ok(())
}

fn fit(value: &str, width: usize) -> String {
    let mut value = truncate(value, width);
    let used = UnicodeWidthStr::width(value.as_str());
    if used < width {
        value.push_str(&" ".repeat(width - used));
    }
    value
}

fn truncate(value: &str, width: usize) -> String {
    if UnicodeWidthStr::width(value) <= width {
        return value.to_owned();
    }
    if width <= 1 {
        return "…".chars().take(width).collect();
    }
    let mut output = String::new();
    let mut used = 0;
    for character in value.chars() {
        let next = character.width().unwrap_or(0);
        if used + next > width - 1 {
            break;
        }
        output.push(character);
        used += next;
    }
    output.push('…');
    output
}

fn wrap(value: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![String::new()];
    }
    let mut result = Vec::new();
    for paragraph in value.lines() {
        if paragraph.is_empty() {
            result.push(String::new());
            continue;
        }
        let mut line = String::new();
        for word in paragraph.split_whitespace() {
            let separator = usize::from(!line.is_empty());
            if UnicodeWidthStr::width(line.as_str()) + separator + UnicodeWidthStr::width(word)
                > width
                && !line.is_empty()
            {
                result.push(std::mem::take(&mut line));
            }
            if !line.is_empty() {
                line.push(' ');
            }
            if UnicodeWidthStr::width(word) > width {
                if !line.is_empty() {
                    result.push(std::mem::take(&mut line));
                }
                let mut chunk = String::new();
                let mut cells = 0;
                for character in word.chars() {
                    let next = character.width().unwrap_or(0);
                    if cells + next > width && !chunk.is_empty() {
                        result.push(std::mem::take(&mut chunk));
                        cells = 0;
                    }
                    chunk.push(character);
                    cells += next;
                }
                line = chunk;
            } else {
                line.push_str(word);
            }
        }
        if !line.is_empty() {
            result.push(line);
        }
    }
    if result.is_empty() {
        result.push(String::new());
    }
    result
}

fn human_age(timestamp: f64) -> String {
    if timestamp <= 0.0 {
        return "—".into();
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |value| value.as_secs_f64());
    let seconds = (now - timestamp).max(0.0) as u64;
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m", seconds / 60)
    } else if seconds < 172800 {
        format!("{}h", seconds / 3600)
    } else {
        format!("{}d", seconds / 86400)
    }
}

fn playbook_tip() -> &'static str {
    const TIPS: &[&str] = &[
        "TIP · pika NAME returns to the exact conversation.",
        "TIP · a asks this agent privately without changing its parent thread.",
        "TIP · x stops watching; it never stops or archives the agent.",
        "TIP · pika experts QUERY finds prior project expertise.",
        "TIP · p peeks without clearing an unread result.",
    ];
    let epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |value| value.as_secs());
    TIPS[((epoch / 300) as usize) % TIPS.len()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn session(status: Status) -> Session {
        Session {
            provider: Provider::Codex,
            session_id: "11111111-1111-4111-8111-111111111111".into(),
            name: Some("thread".into()),
            cwd: None,
            branch: None,
            transcript_path: None,
            tmux_session: None,
            tmux_pane: None,
            root_pid: None,
            status,
            unread: false,
            model: None,
            source: "test".into(),
            managed: true,
            error: None,
            attention_reason: None,
            created_at: 0.0,
            updated_at: 0.0,
            last_event_at: 0.0,
            last_activity_at: 0.0,
            live: false,
            attached: false,
            home_state: "saved-idle".into(),
            cpu_percent: None,
            rss_kb: None,
            input_tokens: None,
            output_tokens: None,
            cached_input_tokens: None,
            cache_write_tokens: None,
            total_tokens: None,
            estimated_cost_usd: None,
            active_thread_id: None,
        }
    }

    #[test]
    fn latest_channel_keeps_newest_snapshot_during_a_burst() {
        let (sender, receiver) = latest_channel();
        for value in 0..2_000 {
            sender.publish(vec![value]);
        }
        assert_eq!(receiver.take(), Some(vec![1_999]));
        assert_eq!(receiver.take(), None);
    }

    #[test]
    fn board_renders_two_thousand_rows_with_bounded_output() {
        let started = Instant::now();
        let items = (0..2_000)
            .map(|index| {
                let mut value = session(Status::Working);
                value.session_id = format!("{index:08}-1111-4111-8111-111111111111");
                value.name = Some(format!("thread-{index}"));
                BoardItem::local(value)
            })
            .collect();
        let board = Board::new(items);
        let mut rendered = Vec::new();
        board.draw(&mut rendered, 120, 35).unwrap();
        assert!(rendered.len() < 128 * 1024);
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn board(status: Status) -> Board {
        Board::new(vec![BoardItem::local(session(status))])
    }

    #[test]
    fn four_groups_do_not_change_raw_status() {
        assert_eq!(group(&session(Status::OpenTwice)), "NEEDS YOU");
        assert_eq!(group(&session(Status::Ready)), "READY");
        assert_eq!(group(&session(Status::Starting)), "WORKING");
    }

    #[test]
    fn arrow_keys_never_quit() {
        let mut board = board(Status::Parked);
        assert_eq!(board.key(key(KeyCode::Down), None), None);
        assert_eq!(board.key(key(KeyCode::Up), None), None);
    }

    #[test]
    fn update_notice_requires_explicit_review_and_confirmation() {
        let mut board = board(Status::Parked);
        board.update_version = Some("0.7.0".into());
        assert_eq!(board.key(key(KeyCode::Char('U')), None), None);
        assert!(board.update_prompt);
        assert_eq!(board.key(key(KeyCode::Esc), None), None);
        assert!(!board.update_prompt);
        board.key(key(KeyCode::Char('U')), None);
        assert_eq!(
            board.key(key(KeyCode::Enter), None),
            Some(BoardAction::Update(Some("0.7.0".into())))
        );
    }

    #[test]
    fn manual_update_key_still_checks_when_no_notice_is_cached() {
        let mut board = board(Status::Parked);
        assert_eq!(
            board.key(key(KeyCode::Char('U')), None),
            Some(BoardAction::Update(None))
        );
    }

    #[test]
    fn repeated_manual_refreshes_are_consumed_and_coalesced_in_place() {
        let (sender, receiver) = mpsc::sync_channel(1);
        for _ in 0..1_000 {
            assert_eq!(
                route_board_action(BoardAction::Refresh, Some(&sender)),
                None
            );
        }
        assert_eq!(receiver.try_recv(), Ok(()));
        assert!(matches!(receiver.try_recv(), Err(TryRecvError::Empty)));
        assert_eq!(
            route_board_action(BoardAction::Quit, Some(&sender)),
            Some(BoardAction::Quit)
        );
    }

    #[test]
    fn delayed_local_refresh_notice_preserves_rows_cached_updates_and_retry() {
        let mut board = board(Status::Working);
        let original = board.selected().unwrap();
        let delayed = AtomicBool::new(true);
        assert!(board.observe_local_refresh(&delayed));
        assert!(!board.observe_local_refresh(&delayed));
        assert_eq!(board.selected().unwrap(), original);
        assert_eq!(item_group(&board.selected().unwrap()), "WORKING");
        for width in [40, 72, 120] {
            let mut rendered = Vec::new();
            board.draw(&mut rendered, width, 20).unwrap();
            let rendered = String::from_utf8(rendered).unwrap();
            assert!(rendered.contains("local refresh delayed · r retry"));
            assert!(!rendered.contains("ERROR"));
        }

        // A fresh hook publication updates the row immediately, without
        // pretending that the full local observation has recovered.
        let mut hook_update = original;
        hook_update.session.status = Status::Ready;
        hook_update.session.unread = true;
        board.replace_items(vec![hook_update.clone()]);
        assert_eq!(board.selected().unwrap(), hook_update);
        assert!(board.local_refresh_delayed);
        let (sender, receiver) = mpsc::sync_channel(1);
        let retry = board.key(key(KeyCode::Char('r')), None).unwrap();
        assert_eq!(route_board_action(retry, Some(&sender)), None);
        assert_eq!(receiver.try_recv(), Ok(()));
        assert!(
            board.local_refresh_delayed,
            "retry is not proof of recovery"
        );

        delayed.store(false, Ordering::Relaxed);
        assert!(board.observe_local_refresh(&delayed));
        assert!(!board.observe_local_refresh(&delayed));
        let mut rendered = Vec::new();
        board.draw(&mut rendered, 72, 20).unwrap();
        assert!(
            !String::from_utf8(rendered)
                .unwrap()
                .contains("local refresh delayed")
        );
        assert_eq!(board.selected().unwrap(), hook_update);
    }

    #[test]
    fn replacement_preserves_node_qualified_selection() {
        let base = session(Status::Working);
        let mut here = BoardItem::local(base.clone());
        here.node_id = Some("here".into());
        let mut remote = BoardItem::local(base);
        remote.node_id = Some("remote".into());
        remote.node_name = Some("research-node".into());
        let mut board = Board::new(vec![here.clone(), remote.clone()]);
        board.select(1);
        let selected = board.selected_key.clone();
        remote.session.status = Status::Ready;
        board.replace_items(vec![remote, here]);
        assert_eq!(board.selected_key, selected);
        assert_eq!(
            board.selected().unwrap().node_name.as_deref(),
            Some("research-node")
        );
    }

    #[test]
    fn filter_treats_q_as_text() {
        let mut board = board(Status::Parked);
        board.key(key(KeyCode::Char('/')), None);
        assert_eq!(board.key(key(KeyCode::Char('q')), None), None);
        assert_eq!(board.filter, "q");
    }

    #[test]
    fn refresh_reselects_a_visible_row_when_selected_name_stops_matching_filter() {
        let first = session(Status::Working);
        let mut second = first.clone();
        second.session_id = "22222222-2222-4222-8222-222222222222".into();
        second.name = Some("quant".into());
        let mut board = Board::new(vec![BoardItem::local(first), BoardItem::local(second)]);
        board.filter = "quant".into();
        board.reselect_first();
        let mut renamed = board.selected().unwrap();
        renamed.session.name = Some("renamed".into());
        let mut replacement = session(Status::Ready);
        replacement.session_id = "33333333-3333-4333-8333-333333333333".into();
        replacement.name = Some("quant-new".into());
        board.replace_items(vec![renamed, BoardItem::local(replacement)]);
        assert_eq!(
            board.selected().unwrap().session.name.as_deref(),
            Some("quant-new")
        );
    }

    #[test]
    fn unicode_truncation_respects_cells() {
        assert_eq!(UnicodeWidthStr::width(truncate("hello世界", 6).as_str()), 6);
    }

    #[test]
    fn ask_never_leaves_board_and_supports_multiple_turns() {
        let questions = Arc::new(Mutex::new(Vec::new()));
        let observed = Arc::clone(&questions);
        let driver = ConsultationDriver::new(move |io| {
            io.events.send(ConsultationEvent::Opened {
                child_id: Some("child-12345678".into()),
                policy: Some("fast".into()),
            })?;
            for command in &io.commands {
                match command {
                    ConsultationInput::Question(question) => {
                        observed.lock().unwrap().push(question);
                        io.events
                            .send(ConsultationEvent::Progress("thinking…".into()))?;
                        io.events.send(ConsultationEvent::Answer(format!(
                            "answer {}",
                            observed.lock().unwrap().len()
                        )))?;
                    }
                    ConsultationInput::Close => break,
                }
            }
            Ok(ConsultationOutcome::discarded())
        });
        let mut board = board(Status::Working);
        assert_eq!(board.key(key(KeyCode::Char('a')), Some(&driver)), None);
        assert!(board.chat.is_some());
        wait_for_phase(&mut board, ChatPhase::Ready);

        for question in ["first", "second"] {
            for character in question.chars() {
                assert_eq!(
                    board.key(key(KeyCode::Char(character)), Some(&driver)),
                    None
                );
            }
            assert_eq!(board.key(key(KeyCode::Enter), Some(&driver)), None);
            wait_for_phase(&mut board, ChatPhase::Ready);
        }
        assert_eq!(&*questions.lock().unwrap(), &["first", "second"]);
        assert_eq!(board.selected().unwrap().session.display_name(), "thread");
        assert_eq!(board.key(key(KeyCode::Esc), Some(&driver)), None);
        for _ in 0..50 {
            board.drain_consultation();
            if board.chat.is_none() {
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        assert!(board.chat.is_none());
    }

    #[test]
    fn consultation_panel_retains_a_bounded_tail_with_a_visible_receipt() {
        let mut chat = ChatState::unavailable("bounded".into());
        for index in 0..1_000 {
            chat.push_line(ChatRole::Expert, format!("{index}:{}", "x".repeat(1024)));
        }
        assert!(chat.lines.len() <= 256);
        assert!(chat.retained_bytes <= 256 * 1024);
        assert!(chat.history_truncated);
        assert!(chat.lines.back().unwrap().text.starts_with("999:"));
    }

    #[test]
    fn consultation_draft_caps_all_character_and_newline_paths_with_visible_feedback() {
        let mut board = board(Status::Working);
        let mut chat = ChatState::unavailable("bounded".into());
        chat.phase = ChatPhase::Ready;
        board.chat = Some(chat);
        for _ in 0..ChatState::MAX_INPUT_BYTES / 4 {
            board.chat_key(key(KeyCode::Char('🦀')));
        }
        for key in [
            key(KeyCode::Char('x')),
            key(KeyCode::Char('🦀')),
            key(KeyCode::Char('\n')),
            KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL),
        ] {
            board.chat_key(key);
            let chat = board.chat.as_ref().unwrap();
            assert_eq!(chat.input.len(), ChatState::MAX_INPUT_BYTES);
            assert!(chat.input_limit_reached);
        }
        let mut rendered = Vec::new();
        board.draw(&mut rendered, 120, 30).unwrap();
        assert!(
            String::from_utf8(rendered)
                .unwrap()
                .contains("64 KiB draft limit")
        );
        board.chat_key(key(KeyCode::Backspace));
        board.chat_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL));
        assert_eq!(
            board.chat.as_ref().unwrap().input.len(),
            ChatState::MAX_INPUT_BYTES - 3
        );
        assert!(!board.chat.as_ref().unwrap().input_limit_reached);
        board.chat_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert!(board.chat.as_ref().unwrap().input.is_empty());
    }

    #[test]
    fn consultation_draft_never_splits_utf8_or_sends_more_than_the_cap() {
        let (sender, receiver) = mpsc::channel();
        let mut chat = ChatState::unavailable("bounded".into());
        chat.phase = ChatPhase::Ready;
        chat.command_sender = Some(sender);
        for _ in 0..ChatState::MAX_INPUT_BYTES - 1 {
            chat.append_input('x');
        }
        chat.append_input('é');
        assert_eq!(chat.input.len(), ChatState::MAX_INPUT_BYTES - 1);
        assert!(chat.input_limit_reached);
        chat.append_input('y');
        chat.send_question();
        let ConsultationInput::Question(question) = receiver.recv().unwrap() else {
            panic!("question expected")
        };
        assert_eq!(question.len(), ChatState::MAX_INPUT_BYTES);
        assert!(question.ends_with('y'));
        assert!(chat.input.is_empty());
        assert!(!chat.input_limit_reached);
    }

    fn wait_for_phase(board: &mut Board, phase: ChatPhase) {
        for _ in 0..50 {
            board.drain_consultation();
            if board.chat.as_ref().is_some_and(|chat| chat.phase == phase) {
                return;
            }
            thread::sleep(Duration::from_millis(2));
        }
        panic!("chat never reached {phase:?}");
    }

    #[test]
    fn second_escape_cancels_slow_close_without_changing_selection() {
        let driver = ConsultationDriver::new(|io| {
            io.events.send(ConsultationEvent::Opened {
                child_id: None,
                policy: None,
            })?;
            let _ = io.commands.recv();
            thread::sleep(Duration::from_millis(50));
            Ok(ConsultationOutcome::discarded())
        });
        let mut board = board(Status::Working);
        let selected = board.selected_key.clone();
        board.key(key(KeyCode::Char('a')), Some(&driver));
        board.key(key(KeyCode::Esc), Some(&driver));
        board.key(key(KeyCode::Esc), Some(&driver));
        assert!(board.chat.is_none());
        assert_eq!(board.selected_key, selected);
    }

    #[test]
    fn first_escape_cancels_blocked_startup_promptly() {
        let cancelled = Arc::new(Mutex::new(false));
        let observed = Arc::clone(&cancelled);
        let driver = ConsultationDriver::new(move |io| {
            while !io.cancellation.is_cancelled() {
                thread::sleep(Duration::from_millis(2));
            }
            *observed.lock().unwrap() = true;
            Ok(ConsultationOutcome::discarded())
        });
        let mut board = board(Status::Working);
        board.key(key(KeyCode::Char('a')), Some(&driver));
        let started = Instant::now();
        board.key(key(KeyCode::Esc), Some(&driver));
        for _ in 0..100 {
            board.drain_consultation();
            if board.chat.is_none() {
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        assert!(board.chat.is_none());
        assert!(*cancelled.lock().unwrap());
        assert!(started.elapsed() < Duration::from_millis(500));
    }

    #[test]
    fn first_escape_cancels_blocked_turn_promptly() {
        let driver = ConsultationDriver::new(move |io| {
            io.events.send(ConsultationEvent::Opened {
                child_id: Some("owned-child".into()),
                policy: None,
            })?;
            assert!(matches!(
                io.commands.recv()?,
                ConsultationInput::Question(_)
            ));
            while !io.cancellation.is_cancelled() {
                thread::sleep(Duration::from_millis(2));
            }
            Ok(ConsultationOutcome {
                discarded: true,
                note: Some("Owned private child discarded.".into()),
            })
        });
        let mut board = board(Status::Working);
        board.key(key(KeyCode::Char('a')), Some(&driver));
        wait_for_phase(&mut board, ChatPhase::Ready);
        board.key(key(KeyCode::Char('q')), Some(&driver));
        board.key(key(KeyCode::Enter), Some(&driver));
        assert_eq!(board.chat.as_ref().unwrap().phase, ChatPhase::Waiting);
        let started = Instant::now();
        board.key(key(KeyCode::Esc), Some(&driver));
        for _ in 0..100 {
            board.drain_consultation();
            if board.chat.is_none() {
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        assert!(board.chat.is_none());
        assert!(started.elapsed() < Duration::from_millis(500));
    }

    #[test]
    fn first_control_c_requests_cleanup_before_quitting() {
        let driver = ConsultationDriver::new(|io| {
            io.events.send(ConsultationEvent::Opened {
                child_id: None,
                policy: None,
            })?;
            assert_eq!(io.commands.recv()?, ConsultationInput::Close);
            Ok(ConsultationOutcome::discarded())
        });
        let mut board = board(Status::Working);
        board.key(key(KeyCode::Char('a')), Some(&driver));
        let interrupt = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(board.key(interrupt, Some(&driver)), None);
        assert!(board.quit_when_chat_closes);
        for _ in 0..50 {
            board.drain_consultation();
            if board.chat.is_none() {
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        assert!(board.chat.is_none());
    }

    #[test]
    fn forced_board_exit_joins_private_consultation_cleanup() {
        let cleaned = Arc::new(Mutex::new(false));
        let observed = Arc::clone(&cleaned);
        let driver = ConsultationDriver::new(move |io| {
            while !io.cancellation.is_cancelled() {
                thread::sleep(Duration::from_millis(2));
            }
            *observed.lock().unwrap() = true;
            Ok(ConsultationOutcome::discarded())
        });
        let mut board = board(Status::Working);
        board.key(key(KeyCode::Char('a')), Some(&driver));
        let interrupt = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(board.key(interrupt, Some(&driver)), None);
        assert_eq!(board.key(interrupt, Some(&driver)), Some(BoardAction::Quit));
        drop(board);
        assert!(*cleaned.lock().unwrap());
    }

    #[test]
    fn uncertain_delivery_still_requests_child_cleanup() {
        let closed = Arc::new(Mutex::new(false));
        let observed = Arc::clone(&closed);
        let driver = ConsultationDriver::new(move |io| {
            io.events.send(ConsultationEvent::Opened {
                child_id: Some("child".into()),
                policy: None,
            })?;
            io.events.send(ConsultationEvent::Error {
                message: "provider stopped after delivery".into(),
                retry_safe: false,
            })?;
            if io.commands.recv()? == ConsultationInput::Close {
                *observed.lock().unwrap() = true;
            }
            Ok(ConsultationOutcome::discarded())
        });
        let mut board = board(Status::Working);
        board.key(key(KeyCode::Char('a')), Some(&driver));
        wait_for_phase(&mut board, ChatPhase::Failed);
        board.key(key(KeyCode::Esc), Some(&driver));
        for _ in 0..50 {
            board.drain_consultation();
            if board.chat.is_none() {
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        assert!(*closed.lock().unwrap());
        assert!(board.chat.is_none());
    }

    #[test]
    fn expert_annotations_are_rendered_when_supplied() {
        let mut item = BoardItem::local(session(Status::Working));
        item.expert = Some(ExpertAnnotation {
            scope: Some("factor publication".into()),
            current_work: Some("validating monthly outputs".into()),
            topics: vec!["parquet".into(), "S3".into()],
            freshness: Some("CURRENT".into()),
        });
        let board = Board::new(vec![item]);
        let mut rendered = Vec::new();
        board.draw(&mut rendered, 120, 30).unwrap();
        let rendered = String::from_utf8(rendered).unwrap();
        assert!(rendered.contains("EXPERT  CURRENT"));
        assert!(rendered.contains("KNOWS   factor publication"));
        assert!(rendered.contains("NOW     validating monthly outputs"));
        assert!(rendered.contains("TOPICS  parquet · S3"));
    }

    #[test]
    fn starting_is_visible_only_when_caller_supplies_it() {
        let board = board(Status::Starting);
        assert_eq!(board.visible()[0].session.status, Status::Starting);
        assert_eq!(group(&board.visible()[0].session), "WORKING");
        assert!(Board::new(Vec::new()).visible().is_empty());
    }
}
