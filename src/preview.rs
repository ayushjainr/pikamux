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
    pane_available: bool,
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
            pane_available: pane_available(item),
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
                    let provider = item.session.provider;
                    let reply = fetch(item, child_cancel).map_err(|error| error.to_string());
                    // Bounds apply before the reply enters the channel, too.
                    let reply = reply
                        .map(|text| board_output(provider, &bounded_text(&text)))
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
    } else if item.session.session_id.starts_with("unbound:") {
        Some("Preview available after the exact conversation is identified.")
    } else if !pane_available(item) {
        Some("No recorded pane · open this conversation to see its work.")
    } else {
        None
    }
}

fn pane_available(item: &BoardItem) -> bool {
    if item.node_id.is_some() {
        // Fleet deliberately never exports pane IDs. Its validated pane_visible
        // flag is decoded to a session marker; this authorizes only a request.
        // The owning node still proves exact UUID/pane identity during capture.
        item.session
            .tmux_session
            .as_deref()
            .is_some_and(|value| !value.is_empty())
    } else {
        item.session
            .tmux_pane
            .as_deref()
            .is_some_and(|value| !value.is_empty())
    }
}

fn wall_time() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

/// Presentation only: never feed this lossy view into attention/identity checks.
/// Captures are plain text, so only recognize known chrome at the *end* of the
/// provider screen. Unknown prompts and questions remain intact.
/// The CLI's raw peek deliberately does not use this filter.
pub(crate) fn board_output(provider: Provider, text: &str) -> String {
    // Expanded peek and automatic preview must use the same sanitized input.
    // capture-pane -e retains SGR codes, including inside prompt/status labels.
    let clean = crate::fleet::sanitize_terminal_lines(text);
    let text = clean.as_str();
    if provider == Provider::Claude {
        return claude_board_output(text);
    }
    if provider != Provider::Codex {
        return text.to_owned();
    }
    let lines: Vec<&str> = text.lines().collect();
    let mut end = lines.len();
    while end > 0 && lines[end - 1].trim().is_empty() {
        end -= 1;
    }
    // Do not interpret old output as footer chrome deep in scrollback.
    let start = end.saturating_sub(48);
    let status = (start..end).rev().find(|&i| {
        (i + 1..=end.min(i + 8)).any(|stop| {
            let status = lines[i..stop]
                .iter()
                .map(|line| line.trim())
                .collect::<String>();
            codex_status_line(&status)
                && lines[i + 1..stop].iter().all(|line| {
                    // A wrapped status is adjacent metadata, not a new paragraph,
                    // prompt or notification. Preserve uncertain trailing prose.
                    !line.trim().is_empty()
                        && (quota_segment(line)
                            || line.trim().starts_with(['/', '·'])
                            || line.contains("% left"))
                })
                && lines[stop..end]
                    .iter()
                    .all(|line| footer_spacing(line) || footer_hint(line))
        })
    });
    if let Some(index) = status {
        end = index;
    }
    let composer = (start..end).rev().find(|&i| {
        (i + 1..=end.min(i + 4)).any(|stop| {
            let parts: Vec<_> = lines[i..stop].iter().map(|line| line.trim()).collect();
            (codex_suggestion(&parts.join(" ")) || codex_suggestion(&parts.concat()))
                && lines[stop..end]
                    .iter()
                    .all(|line| footer_spacing(line) || footer_hint(line))
        })
    });
    if let Some(index) = composer {
        end = index;
    }
    if status.is_none() && composer.is_none() {
        return text.to_owned();
    }
    // Animated composer decoration is not conversation output. Only trim it
    // immediately beside a positively recognized footer, never inside a result.
    while end > 0 && footer_spacing(lines[end - 1]) {
        end -= 1;
    }
    lines[..end].join("\n")
}

fn footer_spacing(line: &str) -> bool {
    line.chars()
        .all(|ch| ch.is_whitespace() || matches!(ch, '.' | '·' | '⋅'))
}

fn footer_hint(line: &str) -> bool {
    matches!(
        line.trim(),
        "? for shortcuts" | "? for shortcuts · / for commands"
    )
}

fn quota_segment(line: &str) -> bool {
    let line = line.trim();
    (line.starts_with("Context ")
        || line.starts_with("context ")
        || line.starts_with("weekly ")
        || line.starts_with("Weekly "))
        && line.contains("% left")
}

fn codex_status_line(line: &str) -> bool {
    let mut segments = line.trim().split('·').map(str::trim);
    let Some(model) = segments.next() else {
        return false;
    };
    let mut words = model.split_whitespace();
    let Some(name) = words.next() else {
        return false;
    };
    if !(name.starts_with("gpt-") || name.starts_with("codex-"))
        || !words.all(|word| {
            matches!(
                word,
                "none" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max" | "ultra"
            )
        })
    {
        return false;
    }
    segments.any(|segment| {
        segment.starts_with('/') || segment.starts_with("~/") || quota_segment(segment)
    })
}

fn codex_suggestion(line: &str) -> bool {
    let Some(prompt) = line.trim().strip_prefix('›') else {
        return false;
    };
    let prompt = prompt.trim_start_matches(|ch: char| ch.is_whitespace() || ch == '·');
    [
        "Ask Codex to do anything",
        "Explain this codebase",
        "Summarize recent commits",
        "Run /review on my current changes",
        "Improve documentation in @filename",
        "Find and fix a bug in @filename",
        "Write tests for @filename",
        "Implement {feature}",
        "Use /skills to list available skills",
    ]
    .iter()
    .any(|suggestion| prompt.strip_prefix(suggestion).is_some_and(footer_spacing))
}

fn claude_board_output(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if let Some(end) = claude_composer_start(&lines) {
        return lines[..end].join("\n").trim_end().to_owned();
    }
    let mut end = lines.len();
    while end > 0 && lines[end - 1].trim().is_empty() {
        end -= 1;
    }
    let original_end = end;
    let start = end.saturating_sub(12);
    // Status-line commands are user-defined. Hide only recognizable metadata,
    // not arbitrary text below a prompt (which can include actionable notices).
    while end > start
        && (lines[end - 1].trim().is_empty()
            || claude_footer_hint(lines[end - 1])
            || claude_status_line(lines[end - 1]))
    {
        end -= 1;
    }
    let composer = (start..end).rev().find(|&i| {
        let Some(prompt) = lines[i].trim().strip_prefix('❯') else {
            return false;
        };
        // Both rules are required: the same words can be actual user input or
        // a quoted example in the conversation. Multiline drafts stay visible.
        i > 0
            && claude_rule(lines[i - 1])
            && i + 1 < end
            && claude_rule(lines[i + 1])
            && lines[i + 2..end].iter().all(|line| line.trim().is_empty())
            && (prompt.trim().is_empty()
                || prompt
                    .trim()
                    .strip_prefix("Try \"")
                    .is_some_and(|suggestion| suggestion.len() > 1 && suggestion.ends_with('"')))
    });
    if let Some(index) = composer {
        end = index - 1;
    }
    if end == original_end {
        return text.to_owned();
    }
    while end > 0 && lines[end - 1].trim().is_empty() {
        end -= 1;
    }
    lines[..end].join("\n")
}

// Composer borders plus a recognized footer identify a UI region regardless of
// the draft/suggestion's wording. Submitted transcript prompts outside that
// region stay visible. Never apply this crop to identity or attention evidence.
fn claude_composer_start(lines: &[&str]) -> Option<usize> {
    let start = lines.len().saturating_sub(48);
    for i in (start..lines.len()).rev() {
        let Some(prompt) = lines[i].trim().strip_prefix('❯') else {
            continue;
        };
        if i == 0 || !claude_composer_rule(lines[i - 1]) || numbered_choice(prompt) {
            continue;
        }
        let Some(lower) = (i + 1..lines.len()).find(|&j| claude_rule(lines[j])) else {
            continue;
        };
        // Numbered choices are interactive questions, not the text composer.
        if lines[i + 1..lower]
            .iter()
            .any(|line| numbered_choice(line.trim()))
        {
            continue;
        }
        let mut footer = lower + 1;
        while footer < lines.len() && claude_composer_rule(lines[footer]) {
            footer += 1;
        }
        let tail = &lines[footer..];
        if !claude_footer(tail) {
            continue;
        }
        let mut upper = i - 1;
        while upper > 0 && claude_composer_rule(lines[upper - 1]) {
            upper -= 1;
        }
        // Claude's context-saving hint may itself be split by terminal layout.
        for n in 1..=3.min(upper) {
            let hint = lines[upper - n..upper]
                .iter()
                .map(|line| line.trim())
                .collect::<String>();
            let hint = hint.to_ascii_lowercase();
            if hint.starts_with("new task? /clear to save ") && hint.ends_with(" tokens") {
                upper -= n;
                break;
            }
        }
        return Some(upper);
    }
    None
}

fn numbered_choice(text: &str) -> bool {
    text.trim_start()
        .chars()
        .next()
        .is_some_and(|ch| ch.is_ascii_digit())
}

fn claude_composer_rule(line: &str) -> bool {
    let line = line.trim();
    claude_rule(line)
        || (line.starts_with("────────")
            && line.ends_with('─')
            && line.chars().filter(|ch| *ch == '─').count() >= 12)
}

fn claude_footer(lines: &[&str]) -> bool {
    let mut recognized = false;
    for paragraph in lines.split(|line| line.trim().is_empty()) {
        if paragraph.is_empty() {
            continue;
        }
        if paragraph
            .iter()
            .all(|line| claude_status_line(line) || claude_footer_hint(line))
        {
            recognized = true;
            continue;
        }
        let text = paragraph
            .iter()
            .map(|line| line.trim())
            .collect::<Vec<_>>()
            .join(" ");
        // Shell-style custom status, including wrapped branch/model/usage rows.
        let shell_status = text.split_whitespace().next().is_some_and(|word| {
            word.contains('@') && (word.contains(":~/") || word.contains(":/"))
        }) && text.contains("ctx ")
            && text.contains('%')
            && ["Opus ", "Sonnet ", "Haiku ", "Fable ", "claude-"]
                .iter()
                .any(|name| text.contains(name))
            && !text.contains('?')
            && !text.contains("failed")
            && !text.contains("required");
        if shell_status
            || claude_footer_hint(&text)
            || (recognized && text.starts_with("⧉ ") && text.contains(" · "))
        {
            recognized = true;
        } else {
            return false;
        }
    }
    recognized
}

fn claude_rule(line: &str) -> bool {
    let line = line.trim();
    line.chars().count() >= 8 && line.chars().all(|ch| matches!(ch, '─' | '━'))
}

fn claude_footer_hint(line: &str) -> bool {
    let line = line.trim();
    if footer_hint(line) {
        return true;
    }
    let line = line.trim_start_matches(|ch: char| {
        ch.is_whitespace() || matches!(ch, '⏵' | '▶' | '⏸' | '▸' | '»')
    });
    [
        "bypass permissions on",
        "accept edits on",
        "plan mode on",
        "auto mode on",
    ]
    .iter()
    .any(|mode| {
        line.strip_prefix(mode).is_some_and(|rest| {
            matches!(
                rest.trim(),
                "(shift+tab to cycle)"
                    | "(shift+tab to cycle) · ? for shortcuts"
                    | "(shift+tab to cycle) · ← for agents"
            )
        })
    })
}

fn claude_status_line(line: &str) -> bool {
    let mut segments = line.trim().split(['·', '|']).map(str::trim);
    let Some(model) = segments.next() else {
        return false;
    };
    let model = model.trim_matches(['[', ']']);
    let mut words = model.split_whitespace();
    let Some(name) = words.next() else {
        return false;
    };
    if !(name.starts_with("claude-") || matches!(name, "Opus" | "Sonnet" | "Haiku" | "Fable"))
        || !words.all(|word| {
            word.chars().all(|ch| ch.is_ascii_digit() || ch == '.')
                || matches!(word, "low" | "medium" | "high" | "xhigh" | "max" | "(1M)")
        })
    {
        return false;
    }
    segments.any(|segment| {
        segment.starts_with('/') || segment.starts_with("~/") || quota_segment(segment)
    })
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
        remote.session.tmux_pane = None;
        remote.session.tmux_session = Some("remote".into());
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
    fn remote_missing_stale_pending_and_unbound_rows_never_capture() {
        let (mut driver, started, _) = controlled();
        for mutation in 0..5 {
            let mut row = item("a");
            row.node_id = Some("remote".into());
            row.session.tmux_pane = None;
            row.session.tmux_session = Some("remote".into());
            match mutation {
                0 => row.session.tmux_session = None,
                1 => row.stale = true,
                2 => row.pending_token = Some("launch".into()),
                3 => row.session.session_id = "unbound:%1".into(),
                _ => row.session.tmux_session = Some(String::new()),
            }
            driver.tick(Some(row), true);
            assert!(!driver.view().unwrap().loading);
            assert!(started.try_recv().is_err());
        }
        // A session name is not enough evidence for a local capture.
        let mut local = item("a");
        local.session.tmux_pane = None;
        local.session.tmux_session = Some("local-session".into());
        driver.tick(Some(local), true);
        assert!(started.try_recv().is_err());
    }

    #[test]
    fn remote_pane_availability_change_cancels_and_fences_old_output() {
        let (mut driver, started, reply) = controlled();
        let now = Instant::now();
        let mut row = item("a");
        row.node_id = Some("remote".into());
        row.session.tmux_pane = None;
        row.session.tmux_session = Some("remote".into());
        driver.tick_at(Some(row.clone()), true, now, 0.0);
        let (_, token) = started.recv_timeout(Duration::from_secs(2)).unwrap();
        row.session.tmux_session = None;
        driver.tick_at(Some(row.clone()), false, now, 0.0);
        assert!(token.is_cancelled());
        reply.send(Ok("obsolete pane".into())).unwrap();
        poll_done(&mut driver, Some(row.clone()), now);
        assert!(driver.view().unwrap().text.contains("No recorded pane"));
        row.session.tmux_session = Some("remote".into());
        driver.tick_at(Some(row.clone()), true, now, 0.0);
        started.recv_timeout(Duration::from_secs(2)).unwrap();
        reply.send(Ok("revalidated pane".into())).unwrap();
        poll_done(&mut driver, Some(row), now);
        assert_eq!(driver.view().unwrap().text, "revalidated pane");
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
    fn codex_footer_is_hidden_but_output_and_queued_questions_survive() {
        let output = "Result table\n  storage    45/69\n\nNothing removed.\n\n— Worked for 3m 11s —\n\n• Queued follow-up inputs\n  ? 1 question\n    shift + ← to answer";
        let capture = format!(
            "{output}\n .    .\n\n› Ask Codex to do anything\n   .      .\n\ngpt-6-astra medium · /project/research · Context 22% left · weekly 58% left · 114M…\n\n"
        );
        assert_eq!(board_output(Provider::Codex, &capture), output);
        for provider in [Provider::Claude, Provider::Opencode] {
            assert_eq!(board_output(provider, &capture), capture);
        }
    }

    #[test]
    fn footer_filter_preserves_actual_input_questions_and_non_footer_prose() {
        let actual = "Result\n› Why were three IDs mismatched?";
        assert_eq!(
            board_output(
                Provider::Codex,
                &format!("{actual}\n\ngpt-6-astra medium · ~/project · weekly 58% left")
            ),
            actual
        );
        for text in [
            "Should I overwrite the file?\n› 1. Yes\n  2. No",
            "› Ask Codex to do anything\nThis is an example of the placeholder.",
            "gpt-6-astra is faster · /project/results contains measurements",
            "gpt-6-astra medium · ~/project\nThis is quoted output, not a footer.",
            "A result ending in dots...\n",
            "› Ask Codex to do anything except overwrite my files",
        ] {
            assert_eq!(board_output(Provider::Codex, text), text);
        }
    }

    #[test]
    fn known_suggestions_and_status_only_footer_are_removed() {
        assert_eq!(
            board_output(
                Provider::Codex,
                "Done.\n\n›·Ask Codex to do anything    ·    ."
            ),
            "Done."
        );
        for suggestion in [
            "Ask Codex to do anything",
            "Run /review on my current changes",
            "Improve documentation in @filename",
            "Implement {feature}",
        ] {
            assert_eq!(
                board_output(
                    Provider::Codex,
                    &format!("Done.\n\n› {suggestion}\n? for shortcuts\n")
                ),
                "Done."
            );
        }
        assert_eq!(
            board_output(
                Provider::Codex,
                "Done.\n\ngpt-5.6-sol xhigh · ~/project · main\n"
            ),
            "Done."
        );
        assert_eq!(
            board_output(
                Provider::Codex,
                "Done.\n\ngpt-6-astra medium · /long/project\n Context 22% left · weekly 58% left\n? for shortcuts"
            ),
            "Done."
        );
        assert_eq!(
            board_output(
                Provider::Codex,
                "› Ask Codex to do anything\ngpt-6-astra medium · ~/project"
            ),
            ""
        );
    }

    #[test]
    fn claude_bordered_suggestions_and_status_line_are_hidden() {
        for prompt in [
            "",
            "Try \"write a test for parser.rs\"",
            "Try \"edit app.rs to...\"",
        ] {
            for status in [
                "Opus 4.6 · ~/project · main",
                "[Sonnet 4.6] | /project | Context 22% left",
                "claude-fable-5-1 · ~/project · weekly 58% left",
            ] {
                let output = "● Updated the parser.\nShould the old format still be supported?";
                let capture = format!(
                    "{output}\n\n────────────────────\n❯ {prompt}\n────────────────────\n  ⏵⏵ bypass permissions on (shift+tab to cycle)\n{status}\n"
                );
                assert_eq!(board_output(Provider::Claude, &capture), output);
                assert_eq!(board_output(Provider::Opencode, &capture), capture);
                assert_eq!(board_output(Provider::Codex, &capture), capture);
            }
        }
    }

    #[test]
    fn claude_composer_drafts_are_hidden_but_transcript_and_approval_questions_survive() {
        let draft = "Done.\n────────────────────\n❯ Keep the old format too\n  and add regression tests\n────────────────────";
        assert_eq!(
            board_output(
                Provider::Claude,
                &format!("{draft}\nSonnet 4.6 · ~/project")
            ),
            "Done."
        );
        for text in [
            "Do you want to make this edit?\n❯ 1. Yes\n  2. No\nEsc to cancel",
            "❯ Try \"write a test for parser.rs\"",
            "────────────────────\n❯ Try \"write a test for parser.rs\"\n────────────────────\nPermission required: allow writing to /project?",
            "────────────────────\n❯\n────────────────────\nAuto-update failed · Try claude doctor",
            "Opus explains this · /project has the examples",
            "Example:\n────────────────────\n❯ Try \"edit app.rs\"\n────────────────────\nThis shows how the composer looks.",
            "Result\n────────────────────\n❯\n────────────────────\ncustom status text without recognized metadata",
        ] {
            assert_eq!(board_output(Provider::Claude, text), text);
        }
    }

    #[test]
    fn claude_wrapped_custom_footer_and_unquoted_suggestion_are_not_work_output() {
        let output =
            "❯ Keep\n● Kept.\n\nKeep as a redeploy reminder, or fold it into the next deploy?";
        let footer = "\n\n          n\new task? /clear to save 922k tokens\n────────────────────────────\n──────────────── demo ─\n❯ fold it in\n────────────────────────────\n────────────────\n  user@host:~/agentic/demo git feature-a\nbranch Fable 5.1 xhigh\n  ctx █████░ 92% 922k/1.0M  7d 19% (6d3h) $199.05\n\n  ▸▸ auto mode on (shift+tab to cycle) · ← for agents\n\n  ⧉ playbooks · playbooks-v2\n";
        assert_eq!(
            board_output(Provider::Claude, &format!("{output}{footer}")),
            output
        );
        for warning in [
            "Permission required: allow this?",
            "Connection failed",
            "A new message needs your answer",
        ] {
            let capture = format!("{output}{footer}\n{warning}");
            assert!(board_output(Provider::Claude, &capture).contains(warning));
        }
        let question = "Keep this change?\n────────────────────\n❯ 1. Yes\n  2. No\n────────────────────\nFable 5.1 · ~/demo";
        assert!(board_output(Provider::Claude, question).contains("2. No"));
    }

    #[test]
    fn expanded_peek_filters_ansi_chrome_and_wrapped_codex_suggestions_too() {
        for (provider, footer) in [
            (
                Provider::Codex,
                "\x1b[2m› Ask Codex to do\nanything\x1b[0m\n\x1b[32mgpt-6-astra medium · /project\nContext 22% left · weekly 58% left\x1b[0m",
            ),
            (
                Provider::Claude,
                "────────────────────\n\x1b[2m❯ fold it in\x1b[0m\n────────────────────\n\x1b[32mFable 5.1 xhigh · ~/demo\x1b[0m",
            ),
        ] {
            let output = "Real result.\nDo you want to keep the old format?";
            let filtered = board_output(provider, &format!("{output}\n\n{footer}"));
            assert_eq!(filtered, output);
            assert_eq!(board_output(provider, &filtered), filtered);
        }
    }

    #[test]
    fn claude_footer_hints_do_not_strip_permission_requests() {
        for hint in [
            "? for shortcuts",
            "⏵⏵ accept edits on (shift+tab to cycle)",
            "⏸ plan mode on (shift+tab to cycle) · ? for shortcuts",
        ] {
            assert_eq!(
                board_output(
                    Provider::Claude,
                    &format!("Done.\n────────────────────\n❯\n────────────────────\n{hint}")
                ),
                "Done."
            );
        }
        let notice = "bypass permissions on requires your approval";
        assert_eq!(board_output(Provider::Claude, notice), notice);
    }

    #[test]
    fn shared_worker_filters_local_and_remote_preview_after_terminal_sanitization() {
        for (provider, footer) in [
            (
                Provider::Codex,
                "› Ask Codex to do anything\ngpt-6-astra medium · ~/project",
            ),
            (
                Provider::Claude,
                "────────────────────\n❯ Try \"edit app.rs\"\n────────────────────\nOpus 4.6 · ~/project",
            ),
        ] {
            for remote in [false, true] {
                let now = Instant::now();
                let mut row = item("a");
                row.session.provider = provider;
                if remote {
                    row.node_id = Some("remote".into());
                    row.session.tmux_pane = None;
                    row.session.tmux_session = Some("remote".into());
                }
                let mut driver =
                    Driver::new(move |_, _| Ok(format!("\x1b[32mDone.\x1b[0m\n\n{footer}")));
                driver.tick_at(Some(row.clone()), true, now, 0.0);
                poll_done(&mut driver, Some(row.clone()), now);
                assert_eq!(driver.view().unwrap().text, "Done.");
                assert!(row.session.unread);
                assert_eq!(driver.view().unwrap().observed_at, Some(123.0));
            }
        }
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
