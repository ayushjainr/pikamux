use crate::{
    consult::CancellationToken,
    model::{Provider, Session, Status},
    usage,
};
use anyhow::Result;
use crossterm::{
    cursor::{Hide, MoveTo, Show},
    event::{self, DisableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute, queue,
    style::{Attribute, Color, Print, ResetColor, SetAttribute, SetForegroundColor},
    terminal::{
        BeginSynchronizedUpdate, Clear, ClearType, EndSynchronizedUpdate, EnterAlternateScreen,
        LeaveAlternateScreen, disable_raw_mode, enable_raw_mode, size,
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
    pub scope_freshness: Option<String>,
    pub current_work_freshness: Option<String>,
}

impl ExpertAnnotation {
    pub(crate) fn from_remote(remote: &crate::fleet::FleetSession) -> Option<Self> {
        if remote.expert_scope.is_none()
            && remote.expert_current_work.is_none()
            && remote.expert_topics.is_empty()
        {
            return None;
        }
        Some(Self {
            scope: remote.expert_scope.clone(),
            current_work: remote.expert_current_work.clone(),
            topics: remote.expert_topics.clone(),
            freshness: remote.card_status.clone(),
            scope_freshness: remote.scope_updated_at.map(evidence_age),
            current_work_freshness: remote.current_state_updated_at.map(|at| {
                let age = evidence_age(at);
                if remote.current_state_status.as_deref() == Some("STALE") {
                    format!("{age} · newer context exists")
                } else {
                    age
                }
            }),
        })
    }
}

fn evidence_age(at: f64) -> String {
    if at > 0.0 {
        format!("updated {} ago", human_age(at))
    } else {
        "age unknown".into()
    }
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

    fn actionable(&self) -> bool {
        !self.stale
    }

    fn needs_attention(&self) -> bool {
        self.actionable() && self.session.needs_attention()
    }
}

type BoardKey = (Option<String>, Provider, String);

/// Only UI state survives handoff. Activity observation belongs to the feed.
#[derive(Default)]
pub(crate) struct BoardMemory {
    initialized: bool,
    selected_key: Option<BoardKey>,
    filter: String,
    offset: usize,
    update_version: Option<String>,
    pub(crate) notice: Option<String>,
}

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
    pub(crate) fn take(&self) -> Option<T> {
        self.value
            .lock()
            .expect("latest-value channel poisoned")
            .take()
    }
}

enum ItemUpdates {
    Queue(Receiver<Vec<BoardItem>>),
    Feed(crate::activity_feed::Subscription),
}

pub(crate) struct FleetHealthFeed {
    initial: Vec<String>,
    updates: LatestReceiver<Vec<String>>,
    quota: Option<crate::quota::Feed>,
}

impl FleetHealthFeed {
    pub(crate) fn new(initial: Vec<String>, updates: LatestReceiver<Vec<String>>) -> Self {
        Self {
            initial,
            updates,
            quota: None,
        }
    }

    pub(crate) fn with_quota(mut self, quota: crate::quota::Feed) -> Self {
        self.quota = Some(quota);
        self
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum BoardAction {
    /// Explicit discovery; handled inside the board, never by setup.
    Add,
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

/// Client actions run off the input thread. Only one can be in flight: repeated
/// Enter never queues a burst of windows, and an uncertain result is not retried.
pub(crate) struct ActionDriver {
    worker: Arc<dyn Fn(BoardAction) -> Result<String> + Send + Sync>,
    pending: Option<Receiver<Result<String>>>,
    inline_open: bool,
    preview: Option<crate::preview::Driver>,
    add: Option<crate::board_add::Driver>,
}

impl ActionDriver {
    pub(crate) fn new(
        worker: impl Fn(BoardAction) -> Result<String> + Send + Sync + 'static,
    ) -> Self {
        Self {
            worker: Arc::new(worker),
            pending: None,
            inline_open: true,
            preview: None,
            add: None,
        }
    }

    pub(crate) fn with_preview(
        mut self,
        worker: impl Fn(BoardItem, CancellationToken) -> Result<String> + Send + Sync + 'static,
    ) -> Self {
        self.preview = Some(crate::preview::Driver::new(worker));
        self
    }

    pub(crate) fn with_add(mut self, driver: crate::board_add::Driver) -> Self {
        self.add = Some(driver);
        self
    }

    /// Unix attaches take over this terminal; inspection and untracking do not.
    #[cfg(not(windows))]
    pub(crate) fn local(
        worker: impl Fn(BoardAction) -> Result<String> + Send + Sync + 'static,
    ) -> Self {
        Self {
            inline_open: false,
            ..Self::new(worker)
        }
    }

    fn handles(&self, action: &BoardAction) -> bool {
        matches!(action, BoardAction::Peek(_) | BoardAction::Untrack(_))
            || (self.inline_open && matches!(action, BoardAction::Open(_)))
    }

    fn submit(&mut self, action: BoardAction) -> String {
        if self.pending.is_some() {
            return "An action is already in progress; no second request was sent.".into();
        }
        let progress = match &action {
            BoardAction::Peek(item) => format!(
                "PEEK · {} · {}\nReading output · unread preserved",
                item.session.display_name(),
                item.session.session_id
            ),
            BoardAction::Untrack(item) => format!(
                "Stopping observation · {} · {}",
                item.session.display_name(),
                item.session.session_id
            ),
            _ => "Opening request · you can keep using the board".into(),
        };
        let worker = self.worker.clone();
        let summary = crate::activity_feed::current();
        let (send, receive) = mpsc::sync_channel(1);
        self.pending = Some(receive);
        thread::spawn(move || {
            let _ = send.send(crate::activity_feed::with(summary, || worker(action)));
        });
        progress
    }

    fn poll(&mut self) -> Option<String> {
        let result = match self.pending.as_ref()?.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return None,
            Err(TryRecvError::Disconnected) => Err(anyhow::anyhow!(
                "Action worker stopped. Outcome is unknown; check the destination window before retrying."
            )),
        };
        self.pending = None;
        Some(match result {
            Ok(note) => note,
            Err(error) => format!("Not completed · {error:#}"),
        })
    }
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
        proof: Option<String>,
    },
    Progress(String),
    Partial(String),
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

/// Coalesce deltas into a bounded preview. A slow board may skip intermediate
/// snapshots, never block provider progress, and always gets a canonical Answer.
pub(crate) fn consultation_preview_observer(
    events: SyncSender<ConsultationEvent>,
) -> crate::consult::ConsultationOutputObserver {
    let current = Mutex::new((0_u64, String::new(), String::new()));
    Arc::new(move |delta| {
        let Ok(mut current) = current.lock() else {
            return false;
        };
        if current.0 != delta.turn || current.1 != delta.item_id {
            *current = (delta.turn, delta.item_id, String::new());
        }
        let mut bytes = (64 * 1024_usize)
            .saturating_sub(current.2.len())
            .min(delta.text.len());
        while !delta.text.is_char_boundary(bytes) {
            bytes -= 1;
        }
        current.2.push_str(&delta.text[..bytes]);
        !matches!(
            events.try_send(ConsultationEvent::Partial(current.2.clone())),
            Err(mpsc::TrySendError::Disconnected(_))
        )
    })
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
        None,
    )
}

/// Both the board and other panels subscribe to the same activity producer.
pub(crate) fn run_activity_view(
    source: &crate::activity_feed::Source,
    driver: Option<ConsultationDriver>,
    update_notice: Receiver<Option<String>>,
    fleet_health: FleetHealthFeed,
    actions: ActionDriver,
    memory: Option<&mut BoardMemory>,
) -> Result<BoardAction> {
    let initial = source.snapshot().expect("activity producer is seeded");
    run_loop_with_actions(
        initial.items,
        Some(ItemUpdates::Feed(source.subscribe())),
        driver,
        Some(update_notice),
        Some(source.refresh()),
        Some(source.delayed()),
        (Some(fleet_health), Some(actions), memory),
    )
}

fn run_loop(
    items: Vec<BoardItem>,
    updates: Option<ItemUpdates>,
    driver: Option<ConsultationDriver>,
    update_notice: Option<Receiver<Option<String>>>,
    refresh_request: Option<SyncSender<()>>,
    local_refresh_delayed: Option<Arc<AtomicBool>>,
    fleet_health: Option<FleetHealthFeed>,
) -> Result<BoardAction> {
    run_loop_with_actions(
        items,
        updates,
        driver,
        update_notice,
        refresh_request,
        local_refresh_delayed,
        (fleet_health, None, None),
    )
}

fn run_loop_with_actions(
    items: Vec<BoardItem>,
    mut updates: Option<ItemUpdates>,
    driver: Option<ConsultationDriver>,
    update_notice: Option<Receiver<Option<String>>>,
    refresh_request: Option<SyncSender<()>>,
    local_refresh_delayed: Option<Arc<AtomicBool>>,
    (fleet_health, mut actions, mut memory): (
        Option<FleetHealthFeed>,
        Option<ActionDriver>,
        Option<&mut BoardMemory>,
    ),
) -> Result<BoardAction> {
    let _terminal = TerminalGuard::enter()?;
    let mut board = Board::new(items);
    if let Some(memory) = memory.as_deref_mut() {
        board.restore(memory);
    }
    board.quota_enabled = fleet_health
        .as_ref()
        .is_some_and(|feed| feed.quota.is_some());
    board.client_actions = actions.is_some();
    board.client_updates = actions.as_ref().is_some_and(|driver| driver.inline_open);
    board.fleet_health = fleet_health
        .as_ref()
        .map(|feed| feed.initial.clone())
        .unwrap_or_default();
    let mut stdout = io::stdout().lock();
    let mut presenter = FramePresenter::default();
    let mut dirty = true;
    let mut preview_refresh = false;
    let mut last_draw = Instant::now()
        .checked_sub(Duration::from_secs(1))
        .unwrap_or_else(Instant::now);
    loop {
        if let Some(note) = actions.as_mut().and_then(ActionDriver::poll) {
            board.action_notice = Some(note);
            dirty = true;
        }
        if let Some(updates) = &mut updates {
            match updates {
                ItemUpdates::Queue(updates) => {
                    while let Ok(items) = updates.try_recv() {
                        board.replace_items(items);
                        dirty = true;
                    }
                }
                ItemUpdates::Feed(updates) => {
                    if let Some(frame) = updates.take() {
                        board.replace_items(frame.items);
                        board.feed_summary = Some(frame.summary);
                        board.fleet_health = frame.health;
                        dirty = true;
                    }
                }
            }
        }
        if let Some(notice) = &update_notice {
            match notice.try_recv() {
                Ok(version) => {
                    board.receive_update(version);
                    dirty = true;
                }
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => {}
            }
        }
        if let Some(delayed) = &local_refresh_delayed {
            dirty |= board.observe_local_refresh(delayed);
        }
        if let Some(feed) = &fleet_health
            && let Some(health) = feed.updates.take()
        {
            dirty |= board.replace_fleet_health(health);
        }
        dirty |= board.drain_consultation();
        if let Some(panel) = board.add.as_mut() {
            dirty |= panel.poll();
        }
        dirty |= board.offer_update();
        if let Some(preview) = actions
            .as_mut()
            .and_then(|actions| actions.preview.as_mut())
        {
            let selected = board.preview_target();
            if preview.tick(selected, std::mem::take(&mut preview_refresh)) {
                board.preview = preview.view();
                dirty = true;
            }
        }
        if let Some(quota) = fleet_health.as_ref().and_then(|feed| feed.quota.as_ref()) {
            // Subscription allowance belongs to this board's base, not its selected thread.
            quota.select(None);
            if let Some(view) = quota.updates.take() {
                board.quota = view;
                dirty = true;
            }
        }
        if board.quit_when_chat_closes && board.chat.is_none() {
            return Ok(BoardAction::Quit);
        }
        let animated = board
            .add
            .as_ref()
            .is_some_and(crate::board_add::Panel::busy)
            || board.chat.as_ref().is_some_and(|chat| {
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
            if let Some(crate::activity_feed::Context::Source(source)) =
                crate::activity_feed::current()
            {
                source.filter(&board.filter);
                board.feed_summary = source.summary();
            }
            let (width, height) = size().unwrap_or((100, 30));
            board.ensure_visible(height.saturating_sub(board.quota_height(width, height)));
            presenter.present(&mut stdout, (width, height), |frame| {
                board.draw(frame, width, height)
            })?;
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
                let (width, height) = size().unwrap_or((100, 30));
                if board.add_key_blocked(key, width, height) {
                    continue;
                }
                if key.code == KeyCode::Char('r')
                    && !board.filtering
                    && board.chat.is_none()
                    && board.add.is_none()
                {
                    preview_refresh = true;
                }
                if let Some(action) = board.key(key, driver.as_ref()) {
                    if action == BoardAction::Add {
                        board.begin_add(actions.as_ref());
                        dirty = true;
                        continue;
                    }
                    if let Some(action) = route_board_action(action, refresh_request.as_ref()) {
                        if let Some(actions) = actions.as_mut()
                            && actions.handles(&action)
                        {
                            board.action_scroll = 0;
                            board.action_notice = Some(actions.submit(action));
                            dirty = true;
                            continue;
                        }
                        if let Some(memory) = memory.as_deref_mut() {
                            board.remember(memory);
                        }
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

/// Compose before touching stdout: its line buffering otherwise exposes every
/// newline between the clear and the finished board. Synchronized output also
/// protects frames larger than one terminal write. Unsupported terminals ignore
/// the mode and still receive a prebuilt frame rather than incremental drawing.
#[derive(Default)]
struct FramePresenter {
    previous: Vec<u8>,
    scratch: Vec<u8>,
    dimensions: Option<(u16, u16)>,
    rows: Option<Vec<Vec<u8>>>,
}

impl FramePresenter {
    fn present(
        &mut self,
        output: &mut impl Write,
        dimensions: (u16, u16),
        draw: impl FnOnce(&mut Vec<u8>) -> Result<()>,
    ) -> Result<bool> {
        self.scratch.clear();
        queue!(self.scratch, BeginSynchronizedUpdate)?;
        draw(&mut self.scratch)?;
        queue!(self.scratch, EndSynchronizedUpdate)?;
        if self.dimensions == Some(dimensions) && self.previous == self.scratch {
            return Ok(false);
        }
        let rows = crate::terminal_frame::rows(&self.scratch, dimensions);
        if self.dimensions == Some(dimensions) && rows.is_some() && self.rows == rows {
            std::mem::swap(&mut self.previous, &mut self.scratch);
            return Ok(false);
        }
        let mut patch = Vec::new();
        if self.dimensions == Some(dimensions)
            && let (Some(previous), Some(next)) = (&self.rows, &rows)
        {
            queue!(patch, BeginSynchronizedUpdate)?;
            for (y, (old, new)) in previous.iter().zip(next).enumerate() {
                if old != new {
                    queue!(patch, MoveTo(0, y as u16))?;
                    patch.extend_from_slice(new);
                }
            }
            queue!(patch, EndSynchronizedUpdate)?;
        }
        let bytes = if patch.is_empty() {
            &self.scratch
        } else {
            &patch
        };
        if let Err(error) = output.write_all(bytes).and_then(|_| output.flush()) {
            // A partial write must not strand the terminal inside a frozen frame.
            self.dimensions = None;
            let _ = execute!(output, EndSynchronizedUpdate, ResetColor);
            return Err(error.into());
        }
        std::mem::swap(&mut self.previous, &mut self.scratch);
        self.dimensions = Some(dimensions);
        self.rows = rows;
        Ok(true)
    }
}

struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> Result<Self> {
        enable_raw_mode()?;
        let guard = Self;
        // A previous SSH/TUI may have left mouse reporting enabled. This board
        // does not capture mouse input, so clear it before accepting keys.
        execute!(
            io::stdout(),
            DisableMouseCapture,
            EnterAlternateScreen,
            Hide
        )?;
        Ok(guard)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(
            io::stdout(),
            EndSynchronizedUpdate,
            DisableMouseCapture,
            Show,
            LeaveAlternateScreen,
            ResetColor
        );
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
    partial: Option<String>,
    retained_bytes: usize,
    history_truncated: bool,
    phase: ChatPhase,
    command_sender: Option<mpsc::Sender<ConsultationInput>>,
    event_receiver: Option<Receiver<ConsultationEvent>>,
    finish_receiver: Option<Receiver<std::result::Result<ConsultationOutcome, String>>>,
    child_id: Option<String>,
    policy: Option<String>,
    proof: Option<String>,
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
            partial: None,
            history_truncated: false,
            phase: ChatPhase::Opening,
            command_sender: Some(command_sender),
            event_receiver: Some(event_receiver),
            finish_receiver: Some(finish_receiver),
            child_id: None,
            policy: None,
            proof: None,
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
            partial: None,
            history_truncated: false,
            phase: ChatPhase::Failed,
            command_sender: None,
            event_receiver: None,
            finish_receiver: None,
            child_id: None,
            policy: None,
            proof: None,
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
        self.partial = None;
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
            ConsultationEvent::Opened {
                child_id,
                policy,
                proof,
            } => {
                self.child_id = child_id;
                self.policy = policy;
                self.proof = proof;
                if !self.close_requested {
                    self.phase = ChatPhase::Ready;
                }
                self.push_line(
                    ChatRole::Pika,
                    "Private side ready. The parent conversation remains untouched.".into(),
                );
            }
            ConsultationEvent::Progress(message) => self.push_line(ChatRole::Pika, message),
            ConsultationEvent::Partial(text) => {
                if self.phase == ChatPhase::Waiting && !self.close_requested {
                    self.partial = Some(text);
                }
            }
            ConsultationEvent::Answer(answer) => {
                self.partial = None;
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
    feed_summary: Option<crate::activity_feed::Summary>,
    preview: Option<crate::preview::View>,
    quota_enabled: bool,
    quota: crate::quota::View,
    items: Vec<BoardItem>,
    selected_key: Option<BoardKey>,
    filter: String,
    filtering: bool,
    offset: usize,
    chat: Option<ChatState>,
    add: Option<crate::board_add::Panel>,
    quit_when_chat_closes: bool,
    update_version: Option<String>,
    update_prompt: bool,
    update_offer_pending: bool,
    local_refresh_delayed: bool,
    fleet_health: Vec<String>,
    action_notice: Option<String>,
    action_scroll: usize,
    client_actions: bool,
    client_updates: bool,
    confirm_untrack: Option<BoardItem>,
}

impl Board {
    fn restore(&mut self, memory: &mut BoardMemory) {
        if memory.initialized {
            self.filter.clone_from(&memory.filter);
            self.selected_key.clone_from(&memory.selected_key);
            self.offset = memory.offset;
            self.update_version.clone_from(&memory.update_version);
            let needle = self.filter.to_lowercase();
            if !self.items.iter().any(|item| {
                Some(item.key()) == self.selected_key && Self::matches_filter(item, &needle)
            }) {
                self.reselect_first();
            }
        }
        self.action_notice = memory.notice.take();
    }

    fn remember(&self, memory: &mut BoardMemory) {
        memory.initialized = true;
        memory.selected_key.clone_from(&self.selected_key);
        memory.filter.clone_from(&self.filter);
        memory.offset = self.offset;
        memory.update_version.clone_from(&self.update_version);
    }

    fn summary(&self) -> crate::activity_feed::Summary {
        self.feed_summary
            .clone()
            .unwrap_or_else(|| crate::activity_feed::summarize(&self.items, &self.filter))
    }

    fn new(mut items: Vec<BoardItem>) -> Self {
        sort_items(&mut items);
        let selected_key = items.first().map(BoardItem::key);
        Self {
            feed_summary: None,
            quota_enabled: false,
            quota: crate::quota::View::default(),
            items,
            selected_key,
            filter: String::new(),
            filtering: false,
            offset: 0,
            chat: None,
            add: None,
            quit_when_chat_closes: false,
            update_version: None,
            update_prompt: false,
            update_offer_pending: false,
            local_refresh_delayed: false,
            fleet_health: Vec::new(),
            action_notice: None,
            action_scroll: 0,
            client_actions: false,
            client_updates: false,
            confirm_untrack: None,
            preview: None,
        }
    }

    fn receive_update(&mut self, version: Option<String>) {
        if version != self.update_version {
            self.update_offer_pending = version.is_some();
            self.update_version = version;
        }
    }

    fn offer_update(&mut self) -> bool {
        if self.update_offer_pending
            && self.chat.is_none()
            && self.add.is_none()
            && !self.filtering
            && self.confirm_untrack.is_none()
            && self.action_notice.is_none()
        {
            self.update_offer_pending = false;
            self.update_prompt = true;
            return true;
        }
        false
    }

    fn observe_local_refresh(&mut self, delayed: &AtomicBool) -> bool {
        let delayed = delayed.load(Ordering::Relaxed);
        let changed = self.local_refresh_delayed != delayed;
        self.local_refresh_delayed = delayed;
        changed
    }

    fn replace_fleet_health(&mut self, mut health: Vec<String>) -> bool {
        health.sort();
        health.dedup();
        let changed = self.fleet_health != health;
        self.fleet_health = health;
        changed
    }

    fn health_line(&self) -> Option<String> {
        let mut parts = Vec::new();
        if self.local_refresh_delayed {
            parts.push("local refresh delayed".to_owned());
        }
        if let Some(first) = self.fleet_health.first() {
            let first = first
                .chars()
                .map(|character| {
                    if character.is_control() {
                        ' '
                    } else {
                        character
                    }
                })
                .collect::<String>();
            let more = self.fleet_health.len().saturating_sub(1);
            parts.push(if more == 0 {
                format!("fleet visibility limited · {first}")
            } else {
                format!(
                    "fleet visibility limited ({}) · {first} · +{more} more",
                    self.fleet_health.len()
                )
            });
        }
        (!parts.is_empty()).then(|| format!("{} · r retry", parts.join(" · ")))
    }

    fn visible(&self) -> Vec<&BoardItem> {
        let needle = self.filter.to_lowercase();
        self.items
            .iter()
            .filter(|item| Self::matches_filter(item, &needle))
            .collect()
    }

    fn matches_filter(item: &BoardItem, needle: &str) -> bool {
        crate::activity_feed::matches_filter(item, needle)
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

    fn disambiguated_name(&self, item: &BoardItem) -> String {
        let name = item.session.display_name();
        let ambiguous = item.session.name.as_ref().is_some_and(|label| {
            self.items.iter().any(|other| {
                other.node_id == item.node_id
                    && other.session.provider == item.session.provider
                    && other.session.name.as_ref() == Some(label)
                    && other.key() != item.key()
            })
        });
        let name = crate::fleet::sanitize_terminal_text(&name);
        if ambiguous {
            let tail: String = item
                .session
                .session_id
                .chars()
                .rev()
                .take(8)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            format!("{name} · {tail}")
        } else {
            name
        }
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

    fn preview_target(&self) -> Option<BoardItem> {
        if self.chat.is_none()
            && self.add.is_none()
            && self.action_notice.is_none()
            && self.confirm_untrack.is_none()
            && !self.filtering
            && !self.update_prompt
        {
            self.selected()
        } else {
            None
        }
    }

    fn begin_add(&mut self, actions: Option<&ActionDriver>) {
        if actions.is_some_and(|driver| driver.pending.is_some()) {
            self.action_notice =
                Some("An action is already in progress; try Add when it finishes.".into());
        } else if let Some(driver) = actions.and_then(|driver| driver.add.clone()) {
            self.action_notice = None;
            self.action_scroll = 0;
            self.add = Some(crate::board_add::Panel::new(driver));
        } else {
            self.action_notice = Some("Adding conversations is unavailable in this view.".into());
        }
    }

    fn add_key_blocked(&self, key: KeyEvent, width: u16, height: u16) -> bool {
        self.add.is_some()
            && (width < 60 || height < 20)
            && key.code != KeyCode::Esc
            && !(key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
    }

    fn handle_add_key(&mut self, key: KeyEvent) -> bool {
        let Some(panel) = self.add.as_mut() else {
            return false;
        };
        if panel.key(key) {
            self.action_notice = panel.closing_notice();
            self.add = None;
        }
        true
    }

    fn key(&mut self, key: KeyEvent, driver: Option<&ConsultationDriver>) -> Option<BoardAction> {
        if self.handle_add_key(key) {
            return None;
        }
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
        // Holding Enter/x is not approval for another window or for the
        // confirmation that the first keypress just opened. Navigation repeats
        // and ordinary typing in the private panel/filter remain available.
        if !self.filtering
            && key.kind == KeyEventKind::Repeat
            && matches!(
                key.code,
                KeyCode::Enter | KeyCode::Char('x' | 'n' | 'U' | '+')
            )
        {
            return None;
        }
        if let Some(item) = self.confirm_untrack.clone() {
            match key.code {
                KeyCode::Enter | KeyCode::Char('x') => {
                    self.confirm_untrack = None;
                    return Some(BoardAction::Untrack(item));
                }
                KeyCode::Esc | KeyCode::Char('q') => {
                    self.confirm_untrack = None;
                    self.action_notice = None;
                }
                _ => {}
            }
            return None;
        }
        if self.action_notice.is_some() {
            match key.code {
                KeyCode::Esc => {
                    self.action_notice = None;
                    self.action_scroll = 0;
                    return None;
                }
                KeyCode::PageDown => {
                    self.action_scroll = self.action_scroll.saturating_add(8);
                    return None;
                }
                KeyCode::PageUp => {
                    self.action_scroll = self.action_scroll.saturating_sub(8);
                    return None;
                }
                _ => {}
            }
        }
        if self.update_prompt {
            match key.code {
                KeyCode::Char('y' | 'Y') if key.kind != KeyEventKind::Repeat => {
                    self.update_prompt = false;
                    return Some(BoardAction::Update(self.update_version.clone()));
                }
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char('n' | 'N' | 'q') => {
                    self.update_prompt = false
                }
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
        if matches!(key.code, KeyCode::Enter | KeyCode::Char('p' | 'a' | 'x'))
            && let Some(item) = self.selected().filter(|item| !item.actionable())
        {
            self.action_scroll = 0;
            self.action_notice = Some(format!(
                "Cached machine state · {} · {}\nPress r to refresh before acting. No request was sent.",
                item.session.display_name(),
                item.session.session_id
            ));
            return None;
        }
        match key.code {
            KeyCode::Char('+') => return Some(BoardAction::Add),
            KeyCode::Up | KeyCode::Char('k') => self.select(-1),
            KeyCode::Down | KeyCode::Char('j') => self.select(1),
            KeyCode::PageUp => self.select(-8),
            KeyCode::PageDown => self.select(8),
            KeyCode::Home => self.reselect_first(),
            KeyCode::End => {
                self.selected_key = self.visible().last().map(|item| item.key());
            }
            KeyCode::Char('/') => self.filtering = true,
            KeyCode::Char('?') => {
                self.action_scroll = 0;
                self.action_notice = Some("PIKA KEYS\n\n+ · add an existing conversation; choose machine, search, confirm\n↑↓ / j k · select a conversation\nEnter · open the selected exact conversation\np · preview live Pika pane output; unread preserved\na · private expert consultation in this panel\nd · identity details and full expert card\nx · stop watching, after confirmation; agent stays intact\nn · open the oldest attention item\n/ · filter by name or machine\nr · refresh observations\nu · cumulative usage for the selected conversation\nU · review an available update\nPageUp / PageDown · scroll a preview or help\nEsc · dismiss panel or clear filter\nq · leave Pika\n\nInside an agent: use the visible Pika return control.\nPrivate consultation: Enter sends, Ctrl+J adds a newline,\nCtrl+U clears the draft, Esc closes the private side.".into());
            }
            KeyCode::Char('u') => {
                self.action_scroll = 0;
                if self
                    .action_notice
                    .as_deref()
                    .is_some_and(|notice| notice.starts_with("CUMULATIVE USAGE"))
                {
                    self.action_notice = None;
                } else if let Some(item) = self.selected() {
                    let s = &item.session;
                    self.action_notice = Some(format!(
                        "CUMULATIVE USAGE · {}\n{} · {}\n\nTotal tokens: {}\nInput: {}\nOutput: {}\nCached input: {}\nCache writes: {}\n\nProvider-reported accounting, not context-window size or a subscription bill.\nOpenCode includes descendant sessions. Missing counters are unavailable.\n\nu / Esc returns to the inspector.",
                        s.display_name(),
                        s.provider,
                        s.session_id,
                        usage::format_tokens(s.total_tokens),
                        usage::format_tokens(s.input_tokens),
                        usage::format_tokens(s.output_tokens),
                        usage::format_tokens(s.cached_input_tokens),
                        usage::format_tokens(s.cache_write_tokens),
                    ));
                }
            }
            KeyCode::Char('d') => {
                self.action_scroll = 0;
                if self
                    .action_notice
                    .as_deref()
                    .is_some_and(|note| note.starts_with("CONVERSATION DETAILS"))
                {
                    self.action_notice = None;
                } else if let Some(item) = self.selected() {
                    let s = &item.session;
                    self.action_notice = Some(format!(
                        "CONVERSATION DETAILS · {}\n\nProvider: {}\nUUID: {}\nMachine: {}\nPath: {}\nBranch: {}\nModel: {}\nRecorded pane: {}\n\nPane records are not current identity proof. Opening and previewing revalidate the exact conversation.\n\nd / Esc returns to the briefing.",
                        s.display_name(),
                        s.provider,
                        s.session_id,
                        item.node_label().unwrap_or("this machine"),
                        s.cwd.as_deref().unwrap_or("not recorded"),
                        s.branch.as_deref().unwrap_or("not recorded"),
                        s.model.as_deref().unwrap_or("not recorded"),
                        s.tmux_pane.as_deref().unwrap_or("none"),
                    ));
                    if let Some(expert) = &item.expert {
                        self.action_notice.as_mut().unwrap().push_str(&format!(
                            "\n\nEXPERT CARD · {}\n\nUseful for\n{}\n\nLast work update\n{}\n\nTopics\n{}",
                            expert.freshness.as_deref().unwrap_or("age unknown"),
                            expert.scope.as_deref().unwrap_or("not recorded"),
                            expert.current_work.as_deref().unwrap_or("not recorded"),
                            expert.topics.join(" · "),
                        ));
                    }
                }
            }
            KeyCode::Esc => {
                self.filter.clear();
                self.reselect_first();
            }
            KeyCode::Enter => {
                return self
                    .selected()
                    .filter(|item| item.actionable())
                    .map(BoardAction::Open);
            }
            KeyCode::Char('p') => {
                return self
                    .selected()
                    .filter(|item| item.actionable())
                    .map(BoardAction::Peek);
            }
            KeyCode::Char('x') => {
                if self.client_actions {
                    if let Some(item) = self.selected().filter(|item| item.actionable()) {
                        self.action_notice = Some(if item.pending_token.is_some() {
                            format!(
                                "Hide launch entry for {}?\nThe agent, terminal, and any confirmed conversation stay intact.\nEnter / x confirm · Esc cancel",
                                item.session.display_name()
                            )
                        } else {
                            format!(
                                "Stop watching {} @{}?\n{} · {}\nThe agent and its conversation stay intact.\nEnter / x confirm · Esc cancel",
                                item.session.display_name(),
                                item.node_label().unwrap_or("here"),
                                item.session.provider,
                                item.session.session_id
                            )
                        });
                        self.confirm_untrack = Some(item);
                    }
                    return None;
                }
                return self
                    .selected()
                    .filter(|item| item.actionable())
                    .map(BoardAction::Untrack);
            }
            KeyCode::Char('a') if self.selected().is_some_and(|item| item.actionable()) => {
                if self
                    .selected()
                    .is_some_and(|item| item.session.provider == Provider::Muse)
                {
                    self.action_notice = Some("Muse private consultations are not available yet.\nEnter opens the conversation; no prompt was sent.".into());
                } else {
                    self.begin_chat(driver)
                }
            }
            KeyCode::Char('r') => return Some(BoardAction::Refresh),
            KeyCode::Char('U') => {
                if self.update_version.is_some() || self.client_updates {
                    self.update_prompt = true;
                } else {
                    return Some(BoardAction::Update(None));
                }
            }
            KeyCode::Char('n') => {
                return self
                    .items
                    .iter()
                    .find(|item| item.needs_attention())
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
        let budget = usize::from(height.saturating_sub(3)).max(3);
        let mut used = 0;
        let mut prior = "";
        let mut first = selected;
        for index in (self.offset.min(selected)..=selected).rev() {
            let Some(item) = visible.get(index) else {
                break;
            };
            let group = item_group(item);
            let cost = 1 + if group == prior { 0 } else { 2 };
            if used + cost > budget {
                break;
            }
            used += cost;
            first = index;
            prior = group;
        }
        self.offset = first;
    }

    fn draw(&self, output: &mut impl Write, width: u16, height: u16) -> Result<()> {
        if std::env::var_os("NO_COLOR").is_some() {
            let mut frame = Vec::new();
            self.draw_frame(&mut frame, width, height)?;
            output.write_all(&without_colors(&frame))?;
            return Ok(());
        }
        self.draw_frame(output, width, height)
    }

    fn draw_frame(&self, output: &mut impl Write, width: u16, height: u16) -> Result<()> {
        queue!(
            output,
            SetAttribute(Attribute::Reset),
            ResetColor,
            MoveTo(0, 0),
            Clear(ClearType::All)
        )?;
        if self.update_prompt {
            return self.draw_update_prompt(output, usize::from(width), height);
        }
        let terminal_height = height;
        let quota_height = self.quota_height(width, height);
        let height = height.saturating_sub(quota_height);
        let width = usize::from(width);
        let visible = self.visible();
        let [needs, working, ready, parked] = self.summary().counts;
        styled(output, Color::Red, true, "PIKA")?;
        let update = self
            .update_version
            .as_deref()
            .map(|version| format!(" · update {version} · U review"))
            .unwrap_or_default();
        draw_status_summary(
            output,
            [needs, working, ready, parked],
            &update,
            width.saturating_sub(6),
            std::env::var_os("NO_COLOR").is_none(),
        )?;
        queue!(output, Print("\r\n"))?;

        let list_width = self.rail_width(width);
        let selected_index = self.selected_index(&visible);
        let mut line = 1_u16;
        let mut prior_group = "";
        for (index, item) in visible.iter().enumerate().skip(self.offset) {
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
                Provider::Muse => "M",
            };
            let age = human_age(session.last_event_at.max(session.last_activity_at));
            let node = item
                .node_label()
                .map(|value| format!(" @{value}"))
                .unwrap_or_default();
            let stale = if item.stale { " ◌" } else { "" };
            let pending = if item.pending_token.is_some() {
                if session.home_state == "startup-exited" {
                    " startup exited"
                } else if session.status == Status::Starting {
                    " starting"
                } else {
                    " unconfirmed"
                }
            } else {
                ""
            };
            let usable = list_width.saturating_sub(12);
            let label = format!(
                "{}{}{}{}",
                self.disambiguated_name(item),
                node,
                pending,
                stale
            );
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

        if visible.is_empty()
            && self.action_notice.is_none()
            && self.chat.is_none()
            && self.add.is_none()
        {
            let lines = if !self.filter.is_empty() {
                [
                    "No conversations match this filter.",
                    "Press Esc to see all your work.",
                    "",
                ]
            } else {
                [
                    "Your board is ready for your work.",
                    "Run pika NAME to find or start a conversation.",
                    "Press + to add an existing conversation. Setup connects machines.",
                ]
            };
            for (index, text) in lines.iter().enumerate() {
                if 3 + index < height.saturating_sub(2) as usize {
                    queue!(output, MoveTo(0, 3 + index as u16))?;
                    styled(
                        output,
                        if index == 0 {
                            Color::Cyan
                        } else {
                            Color::Reset
                        },
                        false,
                        &truncate(text, width),
                    )?;
                }
            }
        }
        let add_text = self.add.as_ref().map(|panel| {
            if width < 60 || terminal_height < 20 {
                "Resize to at least 60 × 20 to add a conversation.\nEsc returns without selecting hidden choices.".into()
            } else {
                let x = if width >= 100 { list_width + 2 } else { 0 };
                panel.text(width.saturating_sub(x), usize::from(height.saturating_sub(5)))
            }
        });
        if let Some(chat) = &self.chat {
            self.draw_chat(output, chat, list_width, width, height)?;
        } else if let Some(report) = add_text.as_ref().or(self.action_notice.as_ref()) {
            self.draw_notice_panel(output, report, list_width, width, height)?;
        } else if width >= 100
            && let Some(selected) = self.selected()
        {
            self.draw_detail(output, &selected, list_width + 2, width, height)?;
        }
        let height = terminal_height;
        self.draw_quota(output, width, height, quota_height)?;
        queue!(
            output,
            MoveTo(0, height.saturating_sub(2)),
            SetForegroundColor(Color::DarkGrey)
        )?;
        if let Some(panel) = &self.add {
            queue!(output, Print(fit(panel.help(), width)))?;
        } else if let Some(chat) = &self.chat {
            let help = match chat.phase {
                ChatPhase::Ready => "enter send · ^j newline · ↑↓ scroll · esc close + discard",
                ChatPhase::Opening | ChatPhase::Waiting => {
                    "type next question · ↑↓ scroll · esc close (esc again returns)"
                }
                ChatPhase::Closing => "closing… · esc return to board",
                ChatPhase::Failed | ChatPhase::Closed => "esc return to board",
            };
            queue!(output, Print(fit(help, width)))?;
        } else if self.action_notice.is_some() {
            queue!(
                output,
                Print(fit(
                    "esc dismiss · pgup/pgdn scroll result · ↑↓ select · enter open · q leave",
                    width
                ))
            )?;
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
                    "+ add · ↑↓ move · enter open · p peek · a ask · x unwatch · / filter · r refresh · ? keys · q leave",
                    width
                ))
            )?;
        }
        if let Some(health) = self.health_line()
            && height > 1
        {
            queue!(
                output,
                MoveTo(0, height - 1),
                SetForegroundColor(Color::DarkYellow),
                Print(fit(&health, width))
            )?;
        }
        queue!(output, ResetColor)?;
        Ok(())
    }

    fn quota_height(&self, width: u16, height: u16) -> u16 {
        if !self.quota_enabled || height < 8 {
            0
        } else if height < 20 || width < 65 {
            1
        } else if width >= 144 {
            2
        } else {
            3
        }
    }

    fn draw_quota(
        &self,
        output: &mut impl Write,
        width: usize,
        height: u16,
        rows: u16,
    ) -> Result<()> {
        if rows == 0 {
            return Ok(());
        }
        let scope = "this machine";
        let empty = crate::quota::View::default();
        let view = if self.quota.node_id.is_none() {
            &self.quota
        } else {
            &empty
        };
        let at = crate::quota::now();
        let y = height.saturating_sub(2 + rows);
        let lines = if rows == 1 {
            let short = crate::quota::PROVIDERS
                .map(|provider| crate::quota::compact_row(provider, view, at));
            vec![format!("WEEKLY {} | {} · {scope}", short[0], short[1])]
        } else {
            let title = format!("⚡ WEEKLY LEFT · {scope} · resets in local time");
            let readings =
                crate::quota::PROVIDERS.map(|provider| crate::quota::row(provider, view, at, true));
            if rows == 2 {
                let half = width.saturating_sub(3) / 2;
                vec![
                    title,
                    format!("{} │ {}", fit(&readings[0], half), fit(&readings[1], half)),
                ]
            } else {
                vec![title, readings[0].clone(), readings[1].clone()]
            }
        };
        let color = std::env::var_os("NO_COLOR").is_none();
        for (index, line) in lines.iter().enumerate() {
            queue!(output, MoveTo(0, y + index as u16), ResetColor)?;
            let mut previous = None;
            for ch in fit(line, width).chars() {
                let tone = if ch == '█' {
                    Color::Yellow
                } else if ch == '░' || index == 0 && rows > 1 {
                    Color::DarkGrey
                } else {
                    Color::Reset
                };
                if color && previous != Some(tone) {
                    queue!(output, SetForegroundColor(tone))?;
                    previous = Some(tone);
                }
                queue!(output, Print(ch))?;
            }
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
            "New version available. Update now? [y/N]",
            "Running agents stay running; other machines are unchanged.",
            "Pika verifies the download and reopens the board after updating.",
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
            Print(fit("y update · n / enter / esc not now", width)),
            ResetColor
        )?;
        Ok(())
    }

    fn rail_width(&self, width: usize) -> usize {
        if width < 100 {
            return width;
        }
        let content = self
            .visible()
            .iter()
            .map(|item| {
                let node = item
                    .node_label()
                    .map_or(0, |label| UnicodeWidthStr::width(label) + 2);
                UnicodeWidthStr::width(self.disambiguated_name(item).as_str()) + node + 12
            })
            .max()
            .unwrap_or(30);
        content.clamp(30, 48).min(width / 3)
    }

    fn draw_notice_panel(
        &self,
        output: &mut impl Write,
        report: &str,
        list_width: usize,
        width: usize,
        height: u16,
    ) -> Result<()> {
        let x = if width >= 100 { list_width + 2 } else { 0 };
        let available = width.saturating_sub(x);
        let lines = report
            .lines()
            .flat_map(|line| wrap(&crate::fleet::sanitize_terminal_text(line), available))
            .collect::<Vec<_>>();
        let rows = usize::from(height.saturating_sub(5));
        let offset = if self.add.is_some() {
            0
        } else {
            self.action_scroll.min(lines.len().saturating_sub(rows))
        };
        for y in 2..height.saturating_sub(2) {
            queue!(output, MoveTo(x as u16, y), Print(" ".repeat(available)))?;
        }
        for (index, line) in lines.iter().skip(offset).take(rows).enumerate() {
            queue!(
                output,
                MoveTo(x as u16, 2 + index as u16),
                Print(fit(line, available))
            )?;
        }
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
        let bottom = height.saturating_sub(3);
        let title = format!(
            "{}{}",
            self.disambiguated_name(item),
            item.node_label()
                .map(|node| format!(" @{node}"))
                .unwrap_or_default()
        );
        queue!(output, MoveTo(x as u16, 2))?;
        styled(output, Color::Reset, true, &truncate(&title, available))?;
        let state = if item.stale {
            "CACHED"
        } else if item.pending_token.is_some() && session.status == Status::Error {
            "LAUNCH UNCONFIRMED"
        } else {
            session.status.as_str()
        };
        queue!(output, MoveTo(x as u16, 3))?;
        styled(
            output,
            if item.stale {
                Color::Yellow
            } else {
                status_color(session.status)
            },
            true,
            state,
        )?;
        let last = session.last_event_at.max(session.last_activity_at);
        let age = if last > 0.0 {
            format!("last activity {} ago", human_age(last))
        } else {
            "activity time unknown".into()
        };
        let context = format!(" · {} · {age}", session.provider);
        queue!(
            output,
            Print(truncate(&context, available.saturating_sub(state.len())))
        )?;
        let mut location = session
            .cwd
            .as_deref()
            .map(project_location)
            .unwrap_or_default();
        if let Some(branch) = &session.branch {
            location.push_str(&format!(" · {branch}"));
        }
        if let Some(model) = &session.model {
            location.push_str(&format!(" · {model}"));
        }
        detail_row(
            output,
            x,
            4,
            available,
            "",
            location.trim_start_matches(" · "),
        )?;
        let mut y = 6;
        let (heading, message) = briefing_message(item);
        draw_briefing_block(output, x, &mut y, bottom, available, heading, message, 2)?;

        // Parked conversations lead with remembered context. Active ones lead
        // with current evidence; no transcript or model summary is invented.
        if session.status == Status::Parked {
            self.draw_expertise(output, item, x, available, &mut y, bottom)?;
        }
        let reserve = if session.status != Status::Parked {
            item.expert.as_ref().map_or(2, |expert| {
                2 + u16::from(expert.current_work.is_some()) * 4
                    + u16::from(expert.scope.is_some()) * 4
                    + u16::from(!expert.topics.is_empty()) * 2
            })
        } else {
            2
        };
        // Actual output earns a minimum useful window on ordinary terminals.
        // Older card detail contracts first; d retains the complete card.
        let reserve = reserve.min(bottom.saturating_sub(y + 6)).max(2);
        let preview_bottom = bottom.saturating_sub(reserve);
        if let Some(preview) = &self.preview {
            let sampled = preview
                .observed_at
                .map(|at| format!("sampled {} ago", human_age(at)));
            let label = if preview.loading && preview.observed_at.is_none() {
                "Reading selected pane".into()
            } else if preview.error {
                "Preview unavailable".into()
            } else if let Some(sampled) = sampled {
                format!("Pane output · {sampled} · unread preserved")
            } else {
                "Pane preview".into()
            };
            if preview.observed_at.is_some() && !preview.error && y + 3 < preview_bottom {
                queue!(output, MoveTo(x as u16, y))?;
                styled(output, Color::Cyan, true, &truncate(&label, available))?;
                y += 1;
                let lines = preview_lines(&preview.text, available);
                let count = usize::from(preview_bottom.saturating_sub(y)).min(lines.len());
                // Keep the most recent pane lines, not the beginning of its history.
                for line in lines.iter().skip(lines.len().saturating_sub(count)) {
                    detail_row(output, x, y, available, "", line)?;
                    y += 1;
                }
                y += 1;
            } else if preview.observed_at.is_some() && !preview.error && y + 1 < bottom {
                detail_row(
                    output,
                    x,
                    y,
                    available,
                    "",
                    "Pane output available · p expand preview",
                )?;
                y += 2;
            } else {
                draw_briefing_block(
                    output,
                    x,
                    &mut y,
                    bottom,
                    available,
                    &label,
                    &preview.text,
                    2,
                )?;
            }
        }
        if session.status != Status::Parked {
            self.draw_expertise(output, item, x, available, &mut y, bottom)?;
        }
        if item.expert.is_none() && y + 1 < bottom {
            detail_row(output, x, y, available, "", "Expertise not available")?;
            y += 2;
        }
        if y < height.saturating_sub(2) {
            detail_row(
                output,
                x,
                y,
                available,
                "",
                if item.stale {
                    "r refresh · d details · ? keys"
                } else if item.session.provider == Provider::Muse {
                    "Enter open · p expand preview · d details"
                } else {
                    "Enter open · p expand preview · a consult · d details"
                },
            )?;
            y += 2;
        }
        if y < bottom {
            detail_row(output, x, y, available, "", playbook_tip())?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_expertise(
        &self,
        output: &mut impl Write,
        item: &BoardItem,
        x: usize,
        width: usize,
        y: &mut u16,
        bottom: u16,
    ) -> Result<()> {
        if let Some(expert) = &item.expert {
            if bottom.saturating_sub(*y) < 10 {
                if let Some(scope) = &expert.scope {
                    let age = expert
                        .scope_freshness
                        .as_deref()
                        .or(expert.freshness.as_deref())
                        .unwrap_or("age unknown");
                    draw_briefing_block(
                        output,
                        x,
                        y,
                        bottom,
                        width,
                        &format!("Useful for · {age}"),
                        scope,
                        1,
                    )?;
                } else if let Some(current) = &expert.current_work {
                    let age = expert
                        .current_work_freshness
                        .as_deref()
                        .or(expert.freshness.as_deref())
                        .unwrap_or("age unknown");
                    draw_briefing_block(
                        output,
                        x,
                        y,
                        bottom,
                        width,
                        &format!("Last work update · {age}"),
                        current,
                        1,
                    )?;
                }
                return Ok(());
            }
            if let Some(current) = &expert.current_work {
                let age = expert
                    .current_work_freshness
                    .as_deref()
                    .or(expert.freshness.as_deref())
                    .unwrap_or("age unknown");
                draw_briefing_block(
                    output,
                    x,
                    y,
                    bottom,
                    width,
                    &format!("Last work update · {age}"),
                    current,
                    2,
                )?;
            }
            if let Some(scope) = &expert.scope {
                let age = expert
                    .scope_freshness
                    .as_deref()
                    .or(expert.freshness.as_deref())
                    .unwrap_or("age unknown");
                draw_briefing_block(
                    output,
                    x,
                    y,
                    bottom,
                    width,
                    &format!("Useful for · {age}"),
                    scope,
                    2,
                )?;
            }
            if !expert.topics.is_empty() && y.saturating_add(1) < bottom {
                detail_row(
                    output,
                    x,
                    *y,
                    width,
                    "",
                    &expert
                        .topics
                        .iter()
                        .take(3)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(" · "),
                )?;
                *y += 2;
            }
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
            SetAttribute(Attribute::NormalIntensity),
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
        if let Some(proof) = &chat.proof {
            metadata.push_str(" · ");
            metadata.push_str(proof);
        }
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
        if let Some(partial) = &chat.partial {
            rendered.push(
                if chat.phase == ChatPhase::Waiting {
                    "PARTIAL · awaiting completed answer"
                } else {
                    "PARTIAL · no completed answer"
                }
                .into(),
            );
            rendered.extend(wrap(partial, available));
        }
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
    queue!(output, MoveTo(x as u16, y), ResetColor)?;
    let label = if label.is_empty() {
        String::new()
    } else {
        format!("{label} · ")
    };
    let label = truncate(&label, width);
    styled(output, Color::Cyan, false, &label)?;
    queue!(
        output,
        Print(truncate(
            &crate::fleet::sanitize_terminal_text(value),
            width.saturating_sub(label.width())
        ))
    )?;
    Ok(())
}

// Frames contain separate colour and attribute commands. Keep emphasis and
// selection, remove colour commands only; cursor movement must stay intact.
fn without_colors(frame: &[u8]) -> Vec<u8> {
    let mut plain = Vec::with_capacity(frame.len());
    let mut offset = 0;
    while offset < frame.len() {
        if frame[offset..].starts_with(b"\x1b[") {
            let start = offset + 2;
            let end = frame[start..]
                .iter()
                .position(|byte| !byte.is_ascii_digit() && *byte != b';')
                .map(|length| start + length);
            if let Some(end) = end
                && frame[end] == b'm'
                && std::str::from_utf8(&frame[start..end]).is_ok_and(|codes| {
                    codes
                        .split(';')
                        .filter_map(|code| code.parse::<u8>().ok())
                        .any(|code| matches!(code, 30..=49 | 90..=107))
                })
            {
                offset = end + 1;
                continue;
            }
        }
        plain.push(frame[offset]);
        offset += 1;
    }
    plain
}

fn project_location(path: &str) -> String {
    let parts: Vec<_> = path
        .split(['/', '\\'])
        .filter(|part| !part.is_empty())
        .collect();
    if parts.is_empty() {
        return path.to_owned();
    }
    parts[parts.len().saturating_sub(2)..].join("/")
}

fn preview_lines(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return Vec::new();
    }
    let mut rows = Vec::new();
    for line in text.trim_end().lines() {
        let safe = crate::fleet::sanitize_terminal_text(&line.replace('\t', "    "));
        let mut row = String::new();
        let mut cells = 0;
        for ch in safe.chars() {
            let next = ch.width().unwrap_or(0);
            if cells + next > width && !row.is_empty() {
                rows.push(std::mem::take(&mut row));
                cells = 0;
            }
            if next <= width {
                row.push(ch);
                cells += next;
            }
        }
        rows.push(row);
    }
    rows
}

fn briefing_message(item: &BoardItem) -> (&'static str, &str) {
    if item.stale {
        return (
            "Last observation",
            "Refresh this machine before opening or consulting.",
        );
    }
    let s = &item.session;
    if item.pending_token.is_some() && s.status == Status::Error {
        return (
            "Launch not confirmed",
            s.error
                .as_deref()
                .unwrap_or("Enter checks the existing terminal. X hides this launch entry."),
        );
    }
    let reason = s.attention_reason.as_deref().filter(|text| {
        !text.trim().is_empty() && !matches!(*text, "completed" | "working" | "[object Object]")
    });
    match s.status {
        Status::NeedsYou => (
            "Needs your answer",
            reason.unwrap_or("Open the conversation to see the question."),
        ),
        Status::Ready => (
            "Latest result",
            reason.unwrap_or(if s.unread {
                "New output is waiting · unread preserved"
            } else {
                ""
            }),
        ),
        Status::Working => ("In progress", reason.unwrap_or("")),
        Status::Starting => (
            "Opening",
            "The agent is starting. Its first update has not arrived yet.",
        ),
        Status::Parked => ("", ""),
        Status::OpenTwice => (
            "Opening blocked",
            "More than one process claims this conversation. Open for exact recovery steps.",
        ),
        Status::Unbound => (
            "Running elsewhere",
            "Open to check how to reconnect safely.",
        ),
        Status::Error => (
            "Needs a check",
            s.error
                .as_deref()
                .filter(|text| !text.trim().is_empty() && *text != "[object Object]")
                .or(reason)
                .unwrap_or("An error was recorded. Open for recovery steps."),
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_briefing_block(
    output: &mut impl Write,
    x: usize,
    y: &mut u16,
    bottom: u16,
    width: usize,
    heading: &str,
    text: &str,
    max_lines: usize,
) -> Result<()> {
    if text.trim().is_empty() || y.saturating_add(3) >= bottom {
        return Ok(());
    }
    queue!(output, MoveTo(x as u16, *y))?;
    styled(output, Color::Cyan, true, &truncate(heading, width))?;
    *y += 1;
    let lines = wrap(&crate::fleet::sanitize_terminal_text(text), width);
    // Reserve a line for the card's age, even on shorter terminals.
    let count = max_lines
        .min(usize::from(bottom.saturating_sub(*y + 2)))
        .min(lines.len());
    for (index, line) in lines.iter().take(count).enumerate() {
        let line = if index + 1 == count && count < lines.len() {
            format!("{}…", truncate(line, width.saturating_sub(1)))
        } else {
            line.clone()
        };
        detail_row(output, x, *y, width, "", &line)?;
        *y += 1;
    }
    *y += 1;
    Ok(())
}

#[cfg(test)]
fn group(session: &Session) -> &'static str {
    crate::activity_feed::group(session.status, false)
}

fn item_group(item: &BoardItem) -> &'static str {
    crate::activity_feed::group(item.session.status, item.stale)
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

pub(crate) fn status_color(status: Status) -> Color {
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

pub(crate) fn summary_parts([needs, working, ready, parked]: [usize; 4]) -> [(String, Color); 4] {
    [
        (format!("{needs} need you"), group_color("NEEDS YOU")),
        (format!("{working} working"), group_color("WORKING")),
        (format!("{ready} ready"), group_color("READY")),
        (format!("{parked} parked"), group_color("PARKED")),
    ]
}

fn draw_status_summary(
    output: &mut impl Write,
    [needs, working, ready, parked]: [usize; 4],
    update: &str,
    mut width: usize,
    colors: bool,
) -> Result<()> {
    let [needs, working, ready, parked] = summary_parts([needs, working, ready, parked]);
    for (text, color) in [
        ("  ".to_owned(), Color::Reset),
        needs,
        (" · ".to_owned(), Color::Reset),
        working,
        (" · ".to_owned(), Color::Reset),
        ready,
        (" · ".to_owned(), Color::Reset),
        parked,
        (update.to_owned(), Color::Reset),
    ] {
        if width == 0 {
            break;
        }
        let visible = truncate(&text, width);
        width = width.saturating_sub(UnicodeWidthStr::width(visible.as_str()));
        if colors {
            styled(output, color, false, &visible)?;
        } else {
            queue!(output, Print(visible))?;
        }
    }
    Ok(())
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
        queue!(output, SetAttribute(Attribute::NormalIntensity))?;
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

    #[derive(Default)]
    struct FrameOutput {
        bytes: Vec<u8>,
        writes: usize,
        flushes: usize,
        fail_flush_once: bool,
    }

    impl Write for FrameOutput {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.writes += 1;
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            self.flushes += 1;
            if std::mem::take(&mut self.fail_flush_once) {
                return Err(io::Error::other("injected terminal failure"));
            }
            Ok(())
        }
    }

    #[test]
    fn frame_presenter_batches_rows_and_skips_identical_frames_but_not_resizes() {
        let mut presenter = FramePresenter::default();
        let mut output = FrameOutput::default();
        let draw = |frame: &mut Vec<u8>| -> Result<()> {
            queue!(frame, MoveTo(0, 0), Clear(ClearType::All))?;
            for _ in 0..100 {
                writeln!(frame, "Unicode █ quota · a complete row")?;
            }
            Ok(())
        };
        assert!(presenter.present(&mut output, (120, 30), draw).unwrap());
        assert_eq!((output.writes, output.flushes), (1, 1));
        assert!(output.bytes.starts_with(b"\x1b[?2026h"));
        assert!(output.bytes.ends_with(b"\x1b[?2026l"));
        for _ in 0..10 {
            assert!(!presenter.present(&mut output, (120, 30), draw).unwrap());
        }
        assert_eq!((output.writes, output.flushes), (1, 1));
        assert!(presenter.present(&mut output, (100, 30), draw).unwrap());
        assert_eq!((output.writes, output.flushes), (2, 2));
        assert!(
            presenter
                .present(&mut output, (100, 30), |frame| {
                    frame.write_all(b"changed selection")?;
                    Ok(())
                })
                .unwrap()
        );
        assert_eq!((output.writes, output.flushes), (3, 3));
    }

    #[test]
    fn frame_presenter_does_not_clear_the_terminal_when_composition_fails() {
        let mut presenter = FramePresenter::default();
        let mut output = FrameOutput::default();
        assert!(
            presenter
                .present(&mut output, (120, 30), |frame| {
                    queue!(frame, Clear(ClearType::All))?;
                    anyhow::bail!("injected composition failure")
                })
                .is_err()
        );
        assert!(output.bytes.is_empty());
        assert_eq!(output.flushes, 0);
    }

    #[test]
    fn routine_refresh_only_writes_changed_rows_without_clearing_the_screen() {
        let mut presenter = FramePresenter::default();
        let mut output = FrameOutput::default();
        let draw = |frame: &mut Vec<u8>, value: &str| -> Result<()> {
            queue!(
                frame,
                MoveTo(0, 0),
                Clear(ClearType::All),
                Print("PIKA"),
                MoveTo(0, 2),
                Print(value)
            )?;
            Ok(())
        };
        presenter
            .present(&mut output, (40, 10), |frame| {
                draw(frame, "long preview output")
            })
            .unwrap();
        output.bytes.clear();
        presenter
            .present(&mut output, (40, 10), |frame| draw(frame, "short"))
            .unwrap();
        let delta = String::from_utf8(output.bytes.clone()).unwrap();
        assert!(!delta.contains("\x1b[2J"));
        assert!(!delta.contains("PIKA"));
        assert!(delta.contains("short                                   "));
        assert!(delta.contains("\x1b[3;1H"));
        assert!(!delta.contains("\x1b[1;1H"));
        output.bytes.clear();
        presenter
            .present(&mut output, (40, 10), |frame| draw(frame, ""))
            .unwrap();
        assert!(
            String::from_utf8(output.bytes)
                .unwrap()
                .contains(&" ".repeat(40))
        );
    }

    #[test]
    fn actual_board_refresh_uses_incremental_rows_at_supported_sizes() {
        for dimensions in [(80, 24), (100, 20), (140, 40), (220, 70)] {
            let mut board = board(Status::Ready);
            board.preview = Some(crate::preview::View {
                text: "A real result\n  indentation and 界 unicode".into(),
                observed_at: Some(1.0),
                loading: false,
                error: false,
            });
            let mut presenter = FramePresenter::default();
            let mut output = FrameOutput::default();
            presenter
                .present(&mut output, dimensions, |frame| {
                    board.draw(frame, dimensions.0, dimensions.1)
                })
                .unwrap();
            assert!(
                presenter.rows.is_some(),
                "board must not fall back at {dimensions:?}"
            );
            output.bytes.clear();
            board.preview.as_mut().unwrap().text = "A shorter result".into();
            presenter
                .present(&mut output, dimensions, |frame| {
                    board.draw(frame, dimensions.0, dimensions.1)
                })
                .unwrap();
            assert!(presenter.rows.is_some());
            assert!(
                !String::from_utf8(output.bytes.clone())
                    .unwrap()
                    .contains("\x1b[2J")
            );
        }
    }

    #[test]
    fn frame_presenter_ends_sync_after_output_failure_and_does_not_cache_failed_frame() {
        let mut presenter = FramePresenter::default();
        let mut output = FrameOutput {
            fail_flush_once: true,
            ..FrameOutput::default()
        };
        let draw = |frame: &mut Vec<u8>| -> Result<()> {
            frame.write_all(b"board")?;
            Ok(())
        };
        assert!(presenter.present(&mut output, (120, 30), draw).is_err());
        assert!(
            output
                .bytes
                .windows(8)
                .filter(|v| *v == b"\x1b[?2026l")
                .count()
                >= 2
        );
        assert_eq!(presenter.dimensions, None);
        assert!(presenter.present(&mut output, (120, 30), draw).unwrap());
        assert!(!presenter.present(&mut output, (120, 30), draw).unwrap());
    }
    use std::sync::{Arc, Mutex};

    #[test]
    fn client_action_failure_is_an_in_panel_report_without_a_retry() {
        let (started, receive) = mpsc::channel();
        let (release, wait) = mpsc::channel();
        let wait = Arc::new(Mutex::new(wait));
        let mut driver = ActionDriver::new(move |_| {
            started.send(()).unwrap();
            wait.lock().unwrap().recv().unwrap();
            anyhow::bail!("fixture launch failed; no retry")
        });
        let mut board = Board::new(vec![BoardItem::local(session(Status::Working))]);
        let selected = board.selected_key.clone();
        let action = board.key(key(KeyCode::Enter), None).unwrap();
        driver.submit(action.clone());
        receive.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(driver.submit(action).contains("no second request"));
        release.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(note) = driver.poll() {
                board.action_notice = Some(note);
                break;
            }
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(1));
        }
        assert!(
            board
                .action_notice
                .as_ref()
                .unwrap()
                .contains("fixture launch failed")
        );
        assert_eq!(board.selected_key, selected);
        for width in [72, 120] {
            let mut rendered = Vec::new();
            board.draw(&mut rendered, width, 25).unwrap();
            assert!(
                String::from_utf8(rendered)
                    .unwrap()
                    .contains("fixture launch failed")
            );
        }
        assert_eq!(board.key(key(KeyCode::Esc), None), None);
        assert!(board.action_notice.is_none());
        assert_eq!(
            board.key(key(KeyCode::Char('q')), None),
            Some(BoardAction::Quit)
        );
        assert!(
            receive.try_recv().is_err(),
            "failure never automatically retries"
        );
    }

    #[test]
    fn client_stop_watching_confirms_the_original_exact_row() {
        let mut board = Board::new(vec![BoardItem::local(session(Status::Working))]);
        board.client_actions = true;
        let exact = board.selected().unwrap();
        assert_eq!(board.key(key(KeyCode::Char('x')), None), None);
        let mut repeated = key(KeyCode::Char('x'));
        repeated.kind = KeyEventKind::Repeat;
        assert_eq!(board.key(repeated, None), None);
        assert!(board.confirm_untrack.is_some());
        board.replace_items(Vec::new());
        assert_eq!(
            board.key(key(KeyCode::Enter), None),
            Some(BoardAction::Untrack(exact))
        );
    }

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
    fn board_summary_remembered_on_open_matches_header_including_filter_and_remote_cache() {
        let mut items = vec![
            BoardItem::local(session(Status::Ready)),
            BoardItem::local(session(Status::Working)),
        ];
        items[0].session.name = Some("visible-result".into());
        items[1].session.name = Some("other-work".into());
        let mut remote = BoardItem::local(session(Status::NeedsYou));
        remote.node_id = Some("remote".into());
        remote.session.name = Some("visible-remote".into());
        remote.stale = true;
        items.push(remote);
        let mut board = Board::new(items);
        board.filter = "visible".into();
        assert_eq!(board.summary().counts, [0, 0, 1, 1]);
        let mut memory = BoardMemory::default();
        board.remember(&mut memory);
        assert_eq!(
            crate::activity_feed::summarize(&board.items, &memory.filter),
            board.summary()
        );
        let mut frame = Vec::new();
        board.draw(&mut frame, 120, 35).unwrap();
        let frame = String::from_utf8(frame).unwrap();
        for text in ["0 need you", "0 working", "1 ready", "1 parked"] {
            assert!(frame.contains(text));
        }
    }

    #[test]
    fn board_heading_is_just_pika_with_status_counts_retained() {
        let mut output = Vec::new();
        board(Status::Working).draw(&mut output, 120, 35).unwrap();
        let rendered = String::from_utf8(output).unwrap();
        assert!(rendered.contains("PIKA"));
        assert!(!rendered.contains("LIVE OPERATIONS"));
        assert!(rendered.contains("1 working"));
    }

    #[test]
    fn status_summary_matches_section_colors_and_keeps_separators_neutral() {
        let mut output = Vec::new();
        draw_status_summary(&mut output, [1, 1, 5, 20], "", 120, true).unwrap();
        let rendered = String::from_utf8(output).unwrap();
        for (text, color) in [
            ("1 need you", Color::Red),
            ("1 working", Color::Cyan),
            ("5 ready", Color::Green),
            ("20 parked", Color::DarkGrey),
            (" · ", Color::Reset),
        ] {
            assert!(rendered.contains(&format!("{}{text}", SetForegroundColor(color))));
        }
    }

    #[test]
    fn status_summary_is_bounded_and_supports_plain_text() {
        let mut output = Vec::new();
        let update = " · update 0.7.0 · U review";
        draw_status_summary(&mut output, [1, 1, 5, 20], update, 120, false).unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap(),
            format!("  1 need you · 1 working · 5 ready · 20 parked{update}")
        );
        for width in 0..60 {
            let mut output = Vec::new();
            draw_status_summary(&mut output, [1, 1, 5, 20], update, width, false).unwrap();
            let rendered = String::from_utf8(output).unwrap();
            assert!(UnicodeWidthStr::width(rendered.as_str()) <= width);
            assert!(!rendered.contains('\x1b'));
        }
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
            board.key(key(KeyCode::Char('y')), None),
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
    fn fleet_health_is_visible_without_becoming_an_actionable_row() {
        let mut board = board(Status::Working);
        let original = board.selected().unwrap();
        assert!(board.replace_fleet_health(vec![
            "atlas · 3 cached conversations not shown".into(),
            "atlas · 3 cached conversations not shown".into(),
        ]));
        assert_eq!(board.items.len(), 1);
        assert_eq!(board.selected().unwrap(), original);
        assert_eq!(
            board.key(key(KeyCode::Enter), None),
            Some(BoardAction::Open(original))
        );
        let mut rendered = Vec::new();
        board.draw(&mut rendered, 120, 20).unwrap();
        let rendered = String::from_utf8(rendered).unwrap();
        assert!(rendered.contains("fleet visibility limited"));
        assert!(rendered.contains("atlas · 3 cached conversations not shown"));
        assert!(!rendered.contains("Fleet cache unavailable"));
    }

    #[test]
    fn stale_remote_attention_is_visible_but_never_actionable() {
        let mut item = BoardItem::local(session(Status::NeedsYou));
        item.node_id = Some("remote-node".into());
        item.node_name = Some("atlas".into());
        item.stale = true;
        item.session.unread = true;
        let mut board = Board::new(vec![item]);
        assert_eq!(item_group(board.selected().as_ref().unwrap()), "PARKED");
        for code in [
            KeyCode::Enter,
            KeyCode::Char('p'),
            KeyCode::Char('x'),
            KeyCode::Char('a'),
            KeyCode::Char('n'),
        ] {
            assert_eq!(board.key(key(code), None), None);
        }
        assert!(board.chat.is_none());
    }

    #[test]
    fn handoff_restores_exact_node_selection_filter_and_offset_but_not_old_evidence() {
        let here = BoardItem::local(session(Status::Ready));
        let mut remote = here.clone();
        remote.node_id = Some("remote".into());
        let mut before = Board::new(vec![here.clone(), remote.clone()]);
        before.selected_key = Some(remote.key());
        before.filter = "thread".into();
        before.offset = 4;
        before.update_version = Some("9.9.9".into());
        let mut memory = BoardMemory::default();
        before.remember(&mut memory);
        remote.stale = true;
        remote.session.status = Status::Parked;
        let mut after = Board::new(vec![remote.clone(), here.clone()]);
        after.restore(&mut memory);
        assert_eq!(after.selected_key, Some(remote.key()));
        assert_eq!(after.filter, "thread");
        assert_eq!(after.offset, 4);
        assert!(after.selected().unwrap().stale);
        assert_eq!(after.selected().unwrap().session.status, Status::Parked);
        assert_eq!(after.update_version.as_deref(), Some("9.9.9"));
        // An absent row cannot be resurrected by remembered UI selection.
        let mut missing = Board::new(vec![here.clone()]);
        missing.restore(&mut memory);
        assert_eq!(missing.selected_key, Some(here.key()));
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
                proof: Some("v2 · ephemeral verified".into()),
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
        let mut board = board(Status::Working);
        board.chat = Some(chat);
        let mut rendered = Vec::new();
        board.draw(&mut rendered, 120, 30).unwrap();
        assert!(
            String::from_utf8(rendered)
                .unwrap()
                .contains("earlier lines omitted")
        );
    }

    #[test]
    fn consultation_preview_is_provisional_and_final_answer_replaces_it() {
        let mut chat = ChatState::unavailable("stream".into());
        chat.phase = ChatPhase::Waiting;
        chat.accept(ConsultationEvent::Partial("unfinished".into()));
        assert_eq!(chat.phase, ChatPhase::Waiting);
        assert_eq!(chat.partial.as_deref(), Some("unfinished"));
        chat.accept(ConsultationEvent::Answer("complete".into()));
        assert_eq!(chat.phase, ChatPhase::Ready);
        assert!(chat.partial.is_none());
        assert_eq!(chat.lines.back().unwrap().text, "complete");
    }

    #[test]
    fn consultation_preview_coalesces_without_blocking_a_slow_board() {
        let (send, receive) = mpsc::sync_channel(1);
        let output = consultation_preview_observer(send);
        let delta = |text: &str| crate::consult_telemetry::AnswerDelta {
            turn: 1,
            item_id: "a".into(),
            text: text.into(),
        };
        assert!(output(delta("one")));
        assert!(output(delta(" two")));
        assert_eq!(
            receive.recv().unwrap(),
            ConsultationEvent::Partial("one".into())
        );
        assert!(output(delta(" three")));
        assert_eq!(
            receive.recv().unwrap(),
            ConsultationEvent::Partial("one two three".into())
        );
        drop(receive);
        assert!(!output(delta(" closed")));
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
    fn second_escape_detaches_slow_close_after_signalling_cancellation() {
        let (observed_sender, observed_receiver) = mpsc::channel();
        let driver = ConsultationDriver::new(move |io| {
            io.events.send(ConsultationEvent::Opened {
                child_id: None,
                policy: None,
                proof: None,
            })?;
            let command = io.commands.recv()?;
            thread::sleep(Duration::from_millis(50));
            observed_sender.send((command, io.cancellation.is_cancelled()))?;
            Ok(ConsultationOutcome::discarded())
        });
        let mut board = board(Status::Working);
        let selected = board.selected_key.clone();
        board.key(key(KeyCode::Char('a')), Some(&driver));
        board.key(key(KeyCode::Esc), Some(&driver));
        board.key(key(KeyCode::Esc), Some(&driver));
        assert!(board.chat.is_none());
        assert_eq!(board.selected_key, selected);
        assert_eq!(
            observed_receiver
                .recv_timeout(Duration::from_millis(250))
                .unwrap(),
            (ConsultationInput::Close, true)
        );
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
                proof: None,
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
        let (release, wait) = mpsc::channel();
        let wait = Arc::new(Mutex::new(wait));
        let driver = ConsultationDriver::new(move |io| {
            io.events.send(ConsultationEvent::Opened {
                child_id: None,
                policy: None,
                proof: None,
            })?;
            assert_eq!(io.commands.recv()?, ConsultationInput::Close);
            wait.lock().unwrap().recv()?;
            Ok(ConsultationOutcome::discarded())
        });
        let mut board = board(Status::Working);
        board.key(key(KeyCode::Char('a')), Some(&driver));
        let interrupt = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(board.key(interrupt, Some(&driver)), None);
        assert!(board.quit_when_chat_closes);
        release.send(()).unwrap();
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
        let (release, wait) = mpsc::channel();
        let wait = Arc::new(Mutex::new(wait));
        let driver = ConsultationDriver::new(move |io| {
            while !io.cancellation.is_cancelled() {
                thread::sleep(Duration::from_millis(2));
            }
            wait.lock().unwrap().recv()?;
            *observed.lock().unwrap() = true;
            Ok(ConsultationOutcome::discarded())
        });
        let mut board = board(Status::Working);
        board.key(key(KeyCode::Char('a')), Some(&driver));
        let interrupt = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(board.key(interrupt, Some(&driver)), None);
        assert_eq!(board.key(interrupt, Some(&driver)), Some(BoardAction::Quit));
        release.send(()).unwrap();
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
                proof: None,
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
            ..ExpertAnnotation::default()
        });
        let board = Board::new(vec![item]);
        let mut rendered = Vec::new();
        board.draw(&mut rendered, 120, 30).unwrap();
        let rendered = String::from_utf8(rendered).unwrap();
        assert!(rendered.contains("CURRENT"));
        assert!(rendered.contains("Useful for"));
        assert!(rendered.contains("factor publication"));
        assert!(rendered.contains("Last work update"));
        assert!(rendered.contains("validating monthly outputs"));
        assert!(rendered.contains("parquet · S3"));
    }

    #[test]
    fn briefing_keeps_plumbing_in_details_without_changing_open_identity() {
        let mut item = BoardItem::local(session(Status::Parked));
        item.session.cwd = Some("/mnt/users/ajain/tracker/db".into());
        item.expert = Some(ExpertAnnotation {
            scope: Some("Agent consultation and exact recovery".into()),
            freshness: Some("updated 32d ago".into()),
            ..ExpertAnnotation::default()
        });
        let id = item.session.session_id.clone();
        let mut board = Board::new(vec![item.clone()]);
        let mut frame = Vec::new();
        board.draw(&mut frame, 140, 32).unwrap();
        let rendered = String::from_utf8(frame).unwrap();
        assert!(rendered.contains("tracker/db"));
        assert!(rendered.contains("Useful for"));
        assert!(rendered.contains("updated 32d ago"));
        assert!(!rendered.contains(&id));
        assert!(!rendered.contains("No Pika pane"));
        assert!(!rendered.contains("/mnt/users/ajain"));
        assert!(
            !rendered.contains("\x1b[21m"),
            "SGR 21 becomes double underline on common terminals"
        );
        assert!(rendered.contains("\x1b[22m"));
        assert_eq!(board.key(key(KeyCode::Char('d')), None), None);
        let details = board.action_notice.as_deref().unwrap();
        assert!(details.contains(&id));
        assert!(details.contains("/mnt/users/ajain/tracker/db"));
        assert!(details.contains("Agent consultation and exact recovery"));
        board.key(key(KeyCode::Esc), None);
        assert_eq!(
            board.key(key(KeyCode::Enter), None),
            Some(BoardAction::Open(item))
        );
    }

    #[test]
    fn briefing_uses_state_specific_evidence_and_never_fabricates_a_result() {
        let mut item = BoardItem::local(session(Status::NeedsYou));
        item.session.attention_reason = Some("Approval required for publication".into());
        assert_eq!(
            briefing_message(&item),
            ("Needs your answer", "Approval required for publication")
        );
        item.session.status = Status::Ready;
        item.session.attention_reason = Some("completed".into());
        item.session.unread = true;
        assert!(briefing_message(&item).1.contains("New output is waiting"));
        item.session.status = Status::Working;
        item.session.attention_reason = None;
        assert_eq!(briefing_message(&item).1, "");
        item.stale = true;
        assert_eq!(briefing_message(&item).0, "Last observation");
        item.stale = false;
        item.session.status = Status::Parked;
        assert_eq!(briefing_message(&item), ("", ""));
        assert_eq!(project_location(r"C:\Users\user\tracker\db"), "tracker/db");
    }

    #[test]
    fn no_color_preserves_selection_emphasis_cursor_and_unicode() {
        let input =
            b"\x1b[0m\x1b[2;3H\x1b[7m\x1b[38;5;14mPika\x1b[39m\x1b[27m\x1b[1mheading\x1b[22m";
        let output = without_colors(input);
        assert_eq!(
            output,
            b"\x1b[0m\x1b[2;3H\x1b[7mPika\x1b[27m\x1b[1mheading\x1b[22m"
        );
        assert_eq!(without_colors("← Pika".as_bytes()), "← Pika".as_bytes());
    }

    #[test]
    fn briefings_fit_short_terminals_and_disambiguate_only_colliding_names() {
        let mut first = BoardItem::local(session(Status::Working));
        first.session.cwd = Some("/tmp/project".into());
        first.expert = Some(ExpertAnnotation {
            scope: Some("A long but evidence-backed description. ".repeat(40)),
            current_work: Some("A provider-authored work update. ".repeat(40)),
            freshness: Some("updated 32d ago".into()),
            topics: vec!["one".into(), "two".into(), "three".into()],
            ..ExpertAnnotation::default()
        });
        let mut second = first.clone();
        second.session.session_id = "22222222-2222-4222-8222-222222222222".into();
        let board = Board::new(vec![first.clone(), second.clone()]);
        assert_eq!(board.disambiguated_name(&first), "thread · 11111111");
        assert_eq!(board.disambiguated_name(&second), "thread · 22222222");
        let cursor = regex::Regex::new("\\x1b\\[([0-9]+);([0-9]+)H").unwrap();
        for (width, height) in [(100, 16), (100, 20), (120, 25), (140, 32), (80, 24)] {
            let mut output = Vec::new();
            board.draw(&mut output, width, height).unwrap();
            let output = String::from_utf8(output).unwrap();
            for capture in cursor.captures_iter(&output) {
                assert!(
                    capture[1].parse::<u16>().unwrap() <= height,
                    "content below terminal"
                );
                assert!(
                    capture[2].parse::<u16>().unwrap() <= width,
                    "content outside terminal"
                );
            }
            if width >= 100 && height >= 20 {
                assert!(
                    output.contains("updated 32d ago"),
                    "card text must retain its age"
                );
            }
        }
    }

    #[test]
    fn starting_is_visible_only_when_caller_supplies_it() {
        let board = board(Status::Starting);
        assert_eq!(board.visible()[0].session.status, Status::Starting);
        assert_eq!(group(&board.visible()[0].session), "WORKING");
        assert!(Board::new(Vec::new()).visible().is_empty());
    }

    #[test]
    fn pane_rows_preserve_indentation_blank_lines_and_unicode_cells() {
        assert_eq!(
            preview_lines("  alpha\n    beta\n\nlast", 80),
            vec!["  alpha", "    beta", "", "last"]
        );
        assert_eq!(preview_lines("界界x", 4), vec!["界界", "x"]);
        assert_eq!(
            preview_lines("\x1b[31mred\x1b[0m\nnext", 80),
            vec!["red", "next"]
        );
    }

    #[test]
    fn normal_and_large_boards_show_recent_output_before_full_expertise() {
        for state in [Status::Working, Status::Ready, Status::NeedsYou] {
            let mut item = BoardItem::local(session(state));
            item.session.name = Some("strategy_dashboard".into());
            item.session.attention_reason =
                (state == Status::NeedsYou).then(|| "Approve publishing the dashboard?".into());
            item.expert = Some(ExpertAnnotation {
                scope: Some("Dashboard presentation and chart decisions".into()),
                current_work: Some("Preparing the next dashboard release".into()),
                topics: vec!["charts".into(), "presentation".into()],
                scope_freshness: Some("updated 5d ago".into()),
                current_work_freshness: Some("updated 2h ago".into()),
                ..ExpertAnnotation::default()
            });
            let mut board = Board::new(vec![item]);
            board.preview = Some(crate::preview::View {
                text: (1..=35)
                    .map(|n| format!("  recorded output row {n}"))
                    .chain(std::iter::once("LATEST: ready for review".into()))
                    .collect::<Vec<_>>()
                    .join("\n"),
                observed_at: Some(
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_secs_f64(),
                ),
                loading: false,
                error: false,
            });
            for (width, height) in [(120, 24), (140, 32), (180, 48)] {
                let mut output = Vec::new();
                board.draw(&mut output, width, height).unwrap();
                let rendered = String::from_utf8(output).unwrap();
                assert!(
                    rendered.contains("LATEST: ready for review"),
                    "missing actual output at {width}x{height}, {state:?}"
                );
                assert!(!rendered.contains("Open when you're ready"));
                assert!(rendered.contains("Enter open"));
                if height >= 32 {
                    assert!(rendered.contains("Useful for"));
                    assert!(rendered.contains("updated 5d ago"));
                }
                if let Ok(directory) = std::env::var("PIKA_TEST_FRAME_DIR") {
                    std::fs::write(
                        std::path::Path::new(&directory)
                            .join(format!("board-{state:?}-{width}x{height}.ansi")),
                        rendered,
                    )
                    .unwrap();
                }
            }
            // A background refresh retains the last sampled evidence, not a
            // two-line loading placeholder that flickers every two seconds.
            board.preview.as_mut().unwrap().loading = true;
            let mut output = Vec::new();
            board.draw(&mut output, 140, 32).unwrap();
            assert!(
                String::from_utf8(output)
                    .unwrap()
                    .contains("LATEST: ready for review")
            );
        }
    }

    #[test]
    fn adaptive_rail_does_not_expand_short_labels_into_unused_columns() {
        let board = board(Status::Ready);
        assert_eq!(board.rail_width(180), 30);
        assert_eq!(board.rail_width(80), 80);
        assert!(board.rail_width(120) <= 40);
    }

    #[test]
    fn client_update_requires_confirmation_and_returns_an_install_action() {
        let mut board = board(Status::Working);
        board.client_actions = true;
        board.update_version = Some("9.0.0".into());
        assert_eq!(board.key(key(KeyCode::Char('U')), None), None);
        assert!(board.update_prompt);
        let mut rendered = Vec::new();
        board.draw(&mut rendered, 120, 30).unwrap();
        assert!(
            String::from_utf8(rendered)
                .unwrap()
                .contains("Update now? [y/N]")
        );
        assert_eq!(
            board.key(key(KeyCode::Char('Y')), None),
            Some(BoardAction::Update(Some("9.0.0".into())))
        );
    }

    #[test]
    fn update_offer_is_once_per_version_deferred_during_typing_and_defaults_to_no() {
        let mut board = board(Status::Working);
        board.filtering = true;
        board.receive_update(Some("9.0.0".into()));
        assert!(!board.offer_update());
        board.filtering = false;
        assert!(board.offer_update());
        assert_eq!(board.key(key(KeyCode::Enter), None), None);
        assert!(!board.update_prompt);
        board.receive_update(Some("9.0.0".into()));
        assert!(!board.offer_update());
        board.receive_update(Some("9.1.0".into()));
        assert!(board.offer_update());
        let mut repeat = key(KeyCode::Char('y'));
        repeat.kind = KeyEventKind::Repeat;
        assert_eq!(board.key(repeat, None), None);
        assert!(board.update_prompt);
        assert_eq!(board.key(key(KeyCode::Char('n')), None), None);
        assert!(!board.update_prompt);
    }

    #[test]
    fn quota_footer_reserves_space_and_stays_on_base_when_selection_changes() {
        let mut board = board(Status::Working);
        board.quota_enabled = true;
        let at = crate::quota::now();
        board.quota.readings = vec![crate::expert_refresh::QuotaSnapshot {
            provider: Provider::Codex,
            used_percent: 37.0,
            reset_at: at as i64 + 86400,
            observed_at: at,
            source: "test".into(),
        }];
        for (width, height, reserved) in [
            (160, 30, 2),
            (100, 30, 3),
            (80, 24, 3),
            (80, 15, 1),
            (40, 12, 1),
        ] {
            assert_eq!(board.quota_height(width, height), reserved);
            let mut rendered = Vec::new();
            board.draw(&mut rendered, width, height).unwrap();
            let text = String::from_utf8(rendered).unwrap();
            assert!(text.contains("WEEKLY"));
            assert!(text.contains("63%"));
            assert!(text.contains("enter open"));
            assert!(text.contains(&format!("\u{1b}[{};1H", height - 1)));
        }
        let mut remote = BoardItem::local(session(Status::Working));
        remote.node_id = Some("other-machine".into());
        remote.node_name = Some("rs6".into());
        board.items = vec![remote.clone()];
        board.selected_key = Some(remote.key());
        for client_actions in [false, true] {
            board.client_actions = client_actions;
            let mut rendered = Vec::new();
            board.draw(&mut rendered, 100, 30).unwrap();
            let text = String::from_utf8(rendered).unwrap();
            assert!(text.contains("63%"));
            assert!(text.contains("WEEKLY LEFT · this machine"));
            assert!(!text.contains("WEEKLY LEFT · @rs6"));
        }
        board.items.clear();
        let mut rendered = Vec::new();
        board.draw(&mut rendered, 100, 30).unwrap();
        assert!(String::from_utf8(rendered).unwrap().contains("63%"));
        // Never relabel a remote reading as this machine's allowance.
        board.quota.node_id = Some("other-machine".into());
        let mut rendered = Vec::new();
        board.draw(&mut rendered, 100, 30).unwrap();
        let text = String::from_utf8(rendered).unwrap();
        assert!(!text.contains("63%"));
        assert!(text.contains("awaiting usage"));
        assert!(text.contains("no current-week reading"));
    }

    #[test]
    fn selection_remains_visible_above_quota_even_across_groups() {
        let mut items = Vec::new();
        for index in 0..40 {
            let mut item = session(if index % 2 == 0 {
                Status::Working
            } else {
                Status::Ready
            });
            item.session_id = format!("thread-{index}");
            item.name = Some(format!("thread-{index}"));
            items.push(BoardItem::local(item));
        }
        let mut board = Board::new(items);
        board.quota_enabled = true;
        for index in 0..40 {
            let item = board.visible()[index].clone();
            board.selected_key = Some(item.key());
            board.ensure_visible(12);
            let mut rendered = Vec::new();
            board.draw(&mut rendered, 80, 13).unwrap();
            assert!(
                String::from_utf8(rendered)
                    .unwrap()
                    .contains(&item.session.display_name())
            );
        }
    }
}
