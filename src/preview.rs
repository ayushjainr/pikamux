//! Selected-pane observation only: no transcript cache, acknowledgement or turn.
use crate::{consult::CancellationToken, model::Provider, monitor::BoardItem};
use anyhow::Result;
use std::{
    collections::VecDeque,
    sync::{Arc, mpsc},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const DEBOUNCE: Duration = Duration::from_millis(250);
const LOCAL_INTERVAL: Duration = Duration::from_secs(2);
const REMOTE_INTERVAL: Duration = Duration::from_secs(15);
const ERROR_INTERVAL: Duration = Duration::from_secs(30);
const MAX_BYTES: usize = 16 * 1024;
const MAX_LINES: usize = 120;

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct View {
    pub text: String,
    pub observed_at: Option<f64>,
    pub loading: bool,
    pub error: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Identity {
    node: Option<String>,
    provider: Provider,
    uuid: String,
    active: Option<String>,
    pane: Option<String>,
    pending: Option<String>,
    stale: bool,
}

impl From<&BoardItem> for Identity {
    fn from(item: &BoardItem) -> Self {
        Self {
            node: item.node_id.clone(),
            provider: item.session.provider,
            uuid: item.session.session_id.clone(),
            active: item.session.active_thread_id.clone(),
            pane: item.session.tmux_pane.clone(),
            pending: item.pending_token.clone(),
            stale: item.stale,
        }
    }
}

type Fetch = dyn Fn(BoardItem, CancellationToken) -> Result<String> + Send + Sync;

struct Request {
    generation: u64,
    cancel: CancellationToken,
    result: mpsc::Receiver<Result<String, String>>,
}

pub(crate) struct Driver {
    fetch: Arc<Fetch>,
    selected: Option<Identity>,
    generation: u64,
    due: Instant,
    request: Option<Request>,
    view: Option<View>,
}

impl Driver {
    pub(crate) fn new(
        fetch: impl Fn(BoardItem, CancellationToken) -> Result<String> + Send + Sync + 'static,
    ) -> Self {
        Self {
            fetch: Arc::new(fetch),
            selected: None,
            generation: 0,
            due: Instant::now(),
            request: None,
            view: None,
        }
    }

    pub(crate) fn view(&self) -> Option<View> {
        self.view.clone()
    }

    pub(crate) fn tick(&mut self, item: Option<BoardItem>, force: bool) -> bool {
        self.tick_at(item, force, Instant::now(), wall_time())
    }

    fn tick_at(&mut self, item: Option<BoardItem>, force: bool, now: Instant, wall: f64) -> bool {
        let mut changed = false;
        let identity = item.as_ref().map(Identity::from);
        if identity != self.selected {
            if let Some(request) = &self.request {
                request.cancel.cancel();
            }
            self.generation = self.generation.wrapping_add(1);
            self.selected = identity;
            self.due = now + DEBOUNCE;
            self.view = item.as_ref().map(|item| View {
                text: absence(item).unwrap_or("Loading pane preview…").into(),
                observed_at: None,
                loading: absence(item).is_none(),
                error: false,
            });
            changed = true;
        }

        // A cancelled request still occupies the one worker slot until it ends.
        // Slow transports cannot create a fan-out when the user holds an arrow.
        let reply = self
            .request
            .as_ref()
            .and_then(|request| match request.result.try_recv() {
                Ok(result) => Some(result),
                Err(mpsc::TryRecvError::Empty) => None,
                Err(mpsc::TryRecvError::Disconnected) => {
                    Some(Err("Pane preview worker stopped.".into()))
                }
            });
        if let Some(reply) = reply {
            let request = self.request.take().expect("reply has a request");
            if request.generation == self.generation && !request.cancel.is_cancelled() {
                let (text, error) = match reply {
                    Ok(text) => {
                        let text = bounded_text(&text);
                        (
                            if text.trim().is_empty() {
                                "No pane output available.".into()
                            } else {
                                text
                            },
                            false,
                        )
                    }
                    Err(error) => (
                        bounded_text(&format!("Preview unavailable · {error}")),
                        true,
                    ),
                };
                self.view = Some(View {
                    text,
                    observed_at: (!error).then_some(wall),
                    loading: false,
                    error,
                });
                self.due = now
                    + if error {
                        ERROR_INTERVAL
                    } else if self.selected.as_ref().is_some_and(|id| id.node.is_some()) {
                        REMOTE_INTERVAL
                    } else {
                        LOCAL_INTERVAL
                    };
                changed = true;
            }
        }

        let Some(item) = item.filter(|item| absence(item).is_none()) else {
            return changed;
        };
        if force && self.request.is_none() {
            self.due = now;
        }
        if self.request.is_none() && now >= self.due {
            let cancel = CancellationToken::default();
            let child_cancel = cancel.clone();
            let fetch = Arc::clone(&self.fetch);
            let (sender, result) = mpsc::sync_channel(1);
            match thread::Builder::new()
                .name("pika-pane-preview".into())
                .spawn(move || {
                    let reply = fetch(item, child_cancel).map_err(|error| error.to_string());
                    // Bounds apply before the reply enters the channel, too.
                    let reply = reply
                        .map(|text| bounded_text(&text))
                        .map_err(|text| bounded_text(&text));
                    let _ = sender.send(reply);
                }) {
                Ok(_) => {
                    self.request = Some(Request {
                        generation: self.generation,
                        cancel,
                        result,
                    });
                    if let Some(view) = &mut self.view {
                        changed |= !view.loading;
                        view.loading = true;
                    }
                }
                Err(error) => {
                    self.view = Some(View {
                        text: bounded_text(&format!("Preview unavailable · {error}")),
                        observed_at: None,
                        loading: false,
                        error: true,
                    });
                    self.due = now + ERROR_INTERVAL;
                    changed = true;
                }
            }
        }
        changed
    }
}

impl Drop for Driver {
    fn drop(&mut self) {
        if let Some(request) = &self.request {
            request.cancel.cancel();
        }
    }
}

fn absence(item: &BoardItem) -> Option<&'static str> {
    if item.stale {
        Some("Preview paused · machine information is out of date.")
    } else if item.pending_token.is_some() || item.session.session_id.trim().is_empty() {
        Some("Preview available after the conversation starts.")
    } else if item.session.tmux_pane.as_deref().is_none_or(str::is_empty) {
        Some("No recorded pane · open this conversation to see its work.")
    } else {
        None
    }
}

fn wall_time() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

/// Preserve line boundaries, strip terminal programs (including OSC/DCS), and
/// bound before allocation. No terminal string or bidi control reaches rendering.
fn bounded_text(text: &str) -> String {
    #[derive(Clone, Copy)]
    enum Escape {
        None,
        Start,
        Csi,
        String,
        StringEnd,
    }
    let mut state = Escape::None;
    let mut output = VecDeque::with_capacity(text.len().min(MAX_BYTES));
    let mut bytes = 0;
    let mut newlines = 0;
    for ch in text.chars() {
        match state {
            Escape::Start => {
                state = match ch {
                    '[' => Escape::Csi,
                    ']' | 'P' | '^' | '_' => Escape::String,
                    _ => Escape::None,
                };
                continue;
            }
            Escape::Csi => {
                if ('@'..='~').contains(&ch) {
                    state = Escape::None;
                }
                continue;
            }
            Escape::String => {
                if ch == '\x07' || ch == '\u{9c}' {
                    state = Escape::None;
                } else if ch == '\x1b' {
                    state = Escape::StringEnd;
                }
                continue;
            }
            Escape::StringEnd => {
                state = if ch == '\\' {
                    Escape::None
                } else {
                    Escape::String
                };
                continue;
            }
            Escape::None => {}
        }
        match ch {
            '\x1b' => {
                state = Escape::Start;
                continue;
            }
            '\u{9b}' => {
                state = Escape::Csi;
                continue;
            }
            '\u{90}' | '\u{9d}' | '\u{9e}' | '\u{9f}' => {
                state = Escape::String;
                continue;
            }
            '\n' => newlines += 1,
            '\t' => {}
            ch if ch.is_control()
                || matches!(ch, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}') =>
            {
                continue;
            }
            _ => {}
        }
        output.push_back(ch);
        bytes += ch.len_utf8();
        // Keep the most recent output. The ring is bounded while scanning, not
        // after allocating a second complete capture. A final newline does not
        // introduce an extra visible line.
        let allowed_newlines = MAX_LINES - usize::from(ch != '\n');
        while bytes > MAX_BYTES || newlines > allowed_newlines {
            let old = output.pop_front().expect("nonempty bounded tail");
            bytes -= old.len_utf8();
            newlines -= usize::from(old == '\n');
        }
    }
    output.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Session, Status};
    use std::sync::Mutex;

    fn item(uuid: &str) -> BoardItem {
        BoardItem::local(Session {
            provider: Provider::Codex,
            session_id: uuid.into(),
            name: None,
            cwd: None,
            branch: None,
            transcript_path: None,
            tmux_session: None,
            tmux_pane: Some("%1".into()),
            root_pid: None,
            status: Status::Working,
            unread: true,
            model: None,
            source: "test".into(),
            managed: true,
            error: None,
            attention_reason: None,
            created_at: 0.0,
            updated_at: 0.0,
            last_event_at: 0.0,
            last_activity_at: 0.0,
            live: true,
            attached: false,
            home_state: "exact".into(),
            cpu_percent: None,
            rss_kb: None,
            input_tokens: None,
            output_tokens: None,
            cached_input_tokens: None,
            cache_write_tokens: None,
            total_tokens: None,
            estimated_cost_usd: None,
            active_thread_id: None,
        })
    }

    type Started = (String, CancellationToken);
    fn controlled() -> (
        Driver,
        mpsc::Receiver<Started>,
        mpsc::Sender<Result<String>>,
    ) {
        let (started_tx, started) = mpsc::channel();
        let (reply, reply_rx) = mpsc::channel();
        let reply_rx = Mutex::new(reply_rx);
        let driver = Driver::new(move |item, token| {
            started_tx.send((item.session.session_id, token)).unwrap();
            reply_rx.lock().unwrap().recv().unwrap()
        });
        (driver, started, reply)
    }

    fn poll_done(driver: &mut Driver, item: Option<BoardItem>, now: Instant) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while driver.request.is_some() {
            driver.tick_at(item.clone(), false, now, 123.0);
            assert!(Instant::now() < deadline, "preview worker did not complete");
            thread::yield_now();
        }
    }

    #[test]
    fn selected_only_debounced_and_local_cadence() {
        let (mut driver, started, reply) = controlled();
        let now = Instant::now();
        driver.tick_at(Some(item("a")), false, now, 0.0);
        driver.tick_at(Some(item("b")), false, now + DEBOUNCE / 2, 0.0);
        driver.tick_at(Some(item("b")), false, now + DEBOUNCE, 0.0);
        assert!(started.try_recv().is_err());
        let due = now + DEBOUNCE * 2;
        driver.tick_at(Some(item("b")), false, due, 0.0);
        assert_eq!(started.recv_timeout(Duration::from_secs(2)).unwrap().0, "b");
        reply.send(Ok("actual output".into())).unwrap();
        poll_done(&mut driver, Some(item("b")), due);
        assert_eq!(driver.view().unwrap().observed_at, Some(123.0));
        assert!(!driver.tick_at(Some(item("b")), false, due + LOCAL_INTERVAL / 2, 0.0));
        driver.tick_at(Some(item("b")), false, due + LOCAL_INTERVAL, 0.0);
        started.recv_timeout(Duration::from_secs(2)).unwrap();
        reply.send(Ok("updated".into())).unwrap();
    }

    #[test]
    fn cancellation_remains_single_flight_and_obsolete_reply_never_displays() {
        let (mut driver, started, reply) = controlled();
        let now = Instant::now();
        driver.tick_at(Some(item("a")), true, now, 0.0);
        let (_, token) = started.recv_timeout(Duration::from_secs(2)).unwrap();
        driver.tick_at(Some(item("b")), true, now, 0.0);
        assert!(token.is_cancelled());
        for _ in 0..50 {
            driver.tick_at(Some(item("b")), true, now, 0.0);
        }
        assert!(started.try_recv().is_err());
        reply.send(Ok("obsolete a".into())).unwrap();
        poll_done(&mut driver, Some(item("b")), now);
        assert!(!driver.view().unwrap().text.contains("obsolete"));
        driver.tick_at(Some(item("b")), true, now, 0.0);
        assert_eq!(started.recv_timeout(Duration::from_secs(2)).unwrap().0, "b");
        reply.send(Ok("right b".into())).unwrap();
        poll_done(&mut driver, Some(item("b")), now);
        assert_eq!(driver.view().unwrap().text, "right b");
    }

    #[test]
    fn pane_active_identity_stale_and_deselection_cancel_without_reusing_output() {
        for mutation in 0..5 {
            let (mut driver, started, reply) = controlled();
            let now = Instant::now();
            driver.tick_at(Some(item("a")), true, now, 0.0);
            let (_, token) = started.recv_timeout(Duration::from_secs(2)).unwrap();
            let mut next = item("a");
            match mutation {
                0 => next.session.tmux_pane = Some("%2".into()),
                1 => next.session.active_thread_id = Some("fork".into()),
                2 => next.stale = true,
                3 => next.node_id = Some("remote".into()),
                _ => {}
            }
            let next = (mutation != 4).then_some(next);
            driver.tick_at(next.clone(), false, now, 0.0);
            assert!(token.is_cancelled());
            reply.send(Ok("obsolete".into())).unwrap();
            poll_done(&mut driver, next, now);
            assert!(
                driver
                    .view()
                    .is_none_or(|view| !view.text.contains("obsolete"))
            );
        }
    }

    #[test]
    fn unavailable_rows_do_not_fetch_and_drop_cancels() {
        let (mut driver, started, reply) = controlled();
        let now = Instant::now();
        for which in 0..4 {
            let mut row = item("a");
            match which {
                0 => row.stale = true,
                1 => row.pending_token = Some("launch".into()),
                2 => row.session.tmux_pane = None,
                _ => row.session.session_id.clear(),
            }
            driver.tick_at(Some(row), true, now, 0.0);
            assert!(!driver.view().unwrap().loading);
        }
        assert!(started.try_recv().is_err());
        driver.tick_at(Some(item("a")), true, now, 0.0);
        let (_, token) = started.recv_timeout(Duration::from_secs(2)).unwrap();
        drop(driver);
        assert!(token.is_cancelled());
        reply.send(Ok("not rendered".into())).unwrap();
    }

    #[test]
    fn remote_cadence_and_error_backoff() {
        let (mut driver, started, reply) = controlled();
        let now = Instant::now();
        let mut remote = item("a");
        remote.node_id = Some("remote".into());
        driver.tick_at(Some(remote.clone()), true, now, 0.0);
        started.recv_timeout(Duration::from_secs(2)).unwrap();
        reply.send(Ok("remote output".into())).unwrap();
        poll_done(&mut driver, Some(remote.clone()), now);
        driver.tick_at(Some(remote.clone()), false, now + LOCAL_INTERVAL, 0.0);
        assert!(started.try_recv().is_err());
        let next = now + REMOTE_INTERVAL;
        driver.tick_at(Some(remote.clone()), false, next, 0.0);
        started.recv_timeout(Duration::from_secs(2)).unwrap();
        reply.send(Err(anyhow::anyhow!("offline"))).unwrap();
        poll_done(&mut driver, Some(remote.clone()), next);
        let view = driver.view().unwrap();
        assert!(view.error && view.observed_at.is_none());
        driver.tick_at(Some(remote.clone()), false, next + REMOTE_INTERVAL, 0.0);
        assert!(started.try_recv().is_err());
        driver.tick_at(Some(remote), false, next + ERROR_INTERVAL, 0.0);
        started.recv_timeout(Duration::from_secs(2)).unwrap();
        reply.send(Ok("restored".into())).unwrap();
    }

    #[test]
    fn terminal_programs_removed_and_unicode_bounds_preserve_lines() {
        assert_eq!(
            bounded_text("a\x1b[31mred\x1b[0m\n\x1b]52;c;secret\x07b\x1bPbad\x1b\\c\u{202e}!"),
            "ared\nbc!"
        );
        assert_eq!(bounded_text("x\u{9d}hidden\u{9c}y"), "xy");
        let large = bounded_text(&"🦀".repeat(MAX_BYTES));
        assert!(large.len() <= MAX_BYTES);
        assert_eq!(large.chars().count(), MAX_BYTES / 4);
        assert_eq!(
            bounded_text(&"line\n".repeat(200)).lines().count(),
            MAX_LINES
        );
        let recent = bounded_text(&format!("{}latest result", "older output\n".repeat(2000)));
        assert!(recent.ends_with("latest result"));
        assert!(recent.len() <= MAX_BYTES && recent.lines().count() <= MAX_LINES);
        let recent = bounded_text(&format!("{}recent", "🦀".repeat(MAX_BYTES)));
        assert!(recent.ends_with("recent") && recent.len() <= MAX_BYTES);
    }

    #[test]
    fn returning_to_same_identity_does_not_reuse_cancelled_reply() {
        let (mut driver, started, reply) = controlled();
        let now = Instant::now();
        driver.tick_at(Some(item("a")), true, now, 0.0);
        started.recv_timeout(Duration::from_secs(2)).unwrap();
        driver.tick_at(None, false, now, 0.0);
        assert!(driver.view().is_none());
        driver.tick_at(Some(item("a")), false, now, 0.0);
        reply.send(Ok("earlier visit".into())).unwrap();
        poll_done(&mut driver, Some(item("a")), now);
        assert!(!driver.view().unwrap().text.contains("earlier visit"));
        assert_eq!(driver.view().unwrap().observed_at, None);
    }

    #[test]
    fn no_output_and_worker_panic_are_honest_bounded_states() {
        let now = Instant::now();
        let mut empty = Driver::new(|_, _| Ok("\x1b[31m\x1b[0m".into()));
        empty.tick_at(Some(item("a")), true, now, 0.0);
        poll_done(&mut empty, Some(item("a")), now);
        assert_eq!(empty.view().unwrap().text, "No pane output available.");
        let mut stopped = Driver::new(|_, _| panic!("fixture worker panic"));
        stopped.tick_at(Some(item("a")), true, now, 0.0);
        poll_done(&mut stopped, Some(item("a")), now);
        assert!(stopped.view().unwrap().error);
        assert!(!stopped.view().unwrap().loading);
        assert_eq!(stopped.due, now + ERROR_INTERVAL);
    }
}
