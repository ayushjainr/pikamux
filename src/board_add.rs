//! Explicit, bounded adoption of an existing native-provider conversation.
//!
//! This is a small view-state machine. Provider discovery and adoption are
//! supplied by the caller and always run off the terminal thread. Nothing in
//! this module configures providers, installs hooks, or reads conversation
//! content.

use crate::{consult::CancellationToken, model::Candidate};
use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use std::{
    collections::HashSet,
    sync::{
        Arc,
        mpsc::{self, Receiver, Sender, TryRecvError},
    },
    thread,
};

const MAX_CANDIDATES: usize = 200;
const MAX_QUERY_BYTES: usize = 1024;
const MAX_FIELD_CHARS: usize = 180;

/// `None` identifies the local Pika node; remote sources use immutable node UUIDs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Source {
    pub id: Option<String>,
    pub label: String,
}

type Load = dyn Fn(Source, CancellationToken) -> Result<Vec<Candidate>> + Send + Sync;
type Adopt = dyn Fn(Source, Candidate, CancellationToken) -> Result<String> + Send + Sync;

#[derive(Clone)]
pub(crate) struct Driver {
    sources: Vec<Source>,
    load: Arc<Load>,
    adopt: Arc<Adopt>,
}

impl Driver {
    pub(crate) fn new(
        sources: Vec<Source>,
        load: impl Fn(Source, CancellationToken) -> Result<Vec<Candidate>> + Send + Sync + 'static,
        adopt: impl Fn(Source, Candidate, CancellationToken) -> Result<String> + Send + Sync + 'static,
    ) -> Self {
        Self {
            sources,
            load: Arc::new(load),
            adopt: Arc::new(adopt),
        }
    }
}

enum Reply {
    Loaded {
        generation: u64,
        result: Result<Vec<Candidate>, String>,
    },
    Adopted {
        generation: u64,
        result: Result<String, String>,
    },
}

#[derive(Clone)]
struct Pick {
    source: Source,
    candidates: Vec<Candidate>,
    selected: usize,
    query: String,
    error: Option<String>,
}

enum State {
    Sources {
        selected: usize,
    },
    Loading {
        source: Source,
    },
    Candidates(Pick),
    Confirm(Pick),
    Mutating {
        source: Source,
        candidate: Candidate,
    },
    Finished(String),
}

enum KeyAction {
    None,
    Close,
    CancelWorker,
    Load(Source),
    Confirm,
    Back,
    Adopt(Source, Box<Candidate>),
}

/// A picker which never chooses or adopts a conversation without explicit approval.
pub(crate) struct Panel {
    driver: Driver,
    state: State,
    tx: Sender<Reply>,
    rx: Receiver<Reply>,
    cancel: CancellationToken,
    generation: u64,
    closing_notice: Option<String>,
}

impl Panel {
    pub(crate) fn new(driver: Driver) -> Self {
        let (tx, rx) = mpsc::channel();
        let mut this = Self {
            state: State::Sources { selected: 0 },
            driver,
            tx,
            rx,
            cancel: CancellationToken::default(),
            generation: 0,
            closing_notice: None,
        };
        if this.driver.sources.len() == 1 {
            this.start_load(this.driver.sources[0].clone());
        }
        this
    }

    /// Process worker replies. `true` means visible state changed.
    pub(crate) fn poll(&mut self) -> bool {
        let mut changed = false;
        loop {
            match self.rx.try_recv() {
                Ok(Reply::Loaded { generation, result }) if generation == self.generation => {
                    if let State::Loading { source } = &self.state {
                        let source = source.clone();
                        let (candidates, error) = match result {
                            Ok(candidates) => (candidates, None),
                            Err(error) => (Vec::new(), Some(error)),
                        };
                        self.state = State::Candidates(Pick {
                            source,
                            candidates,
                            selected: 0,
                            query: String::new(),
                            error,
                        });
                        if let State::Candidates(pick) = &mut self.state {
                            normalize_candidates(&mut pick.candidates);
                        }
                        changed = true;
                    }
                }
                Ok(Reply::Adopted { generation, result }) if generation == self.generation => {
                    self.state = State::Finished(match result {
                        Ok(receipt) => format!("Added to board: {}", safe(&receipt)),
                        Err(error) => format!(
                            "Add did not complete · {}. Check the board before retrying.",
                            safe(&error)
                        ),
                    });
                    changed = true;
                }
                Ok(_) => {}
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }
        changed
    }

    /// Handle a terminal key. Returns true when the caller should close the panel.
    pub(crate) fn key(&mut self, key: KeyEvent) -> bool {
        if key.kind == KeyEventKind::Release {
            return false;
        }
        self.poll();
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            if matches!(self.state, State::Mutating { .. }) {
                self.closing_notice = Some(
                    "Adoption is still running; its outcome may be unknown if this panel closes."
                        .into(),
                );
                self.cancel.cancel();
            }
            return true;
        }
        if matches!(self.state, State::Mutating { .. }) {
            if key.code == KeyCode::Esc {
                self.closing_notice = Some(
                    "Adoption is still running; its outcome may be unknown if this panel closes."
                        .into(),
                );
                self.cancel.cancel();
                return true;
            }
            return false;
        }
        if matches!(self.state, State::Finished(_)) {
            return key.kind != KeyEventKind::Repeat
                && matches!(key.code, KeyCode::Enter | KeyCode::Esc);
        }

        let action = match &mut self.state {
            State::Sources { selected } => source_key(selected, &self.driver.sources, key),
            State::Loading { .. } if key.code == KeyCode::Esc => KeyAction::CancelWorker,
            State::Candidates(pick) => candidate_key(pick, key),
            State::Confirm(pick) => confirm_key(pick, key),
            _ => KeyAction::None,
        };
        self.apply_key_action(action)
    }

    fn apply_key_action(&mut self, action: KeyAction) -> bool {
        match action {
            KeyAction::None => false,
            KeyAction::Close => true,
            KeyAction::CancelWorker => {
                self.cancel.cancel();
                true
            }
            KeyAction::Load(source) => {
                self.start_load(source);
                false
            }
            KeyAction::Confirm => {
                if let State::Candidates(pick) = &self.state {
                    self.state = State::Confirm(pick.clone());
                }
                false
            }
            KeyAction::Back => {
                if let State::Confirm(pick) = &self.state {
                    self.state = State::Candidates(pick.clone());
                }
                false
            }
            KeyAction::Adopt(source, candidate) => {
                self.start_adopt(source, *candidate);
                false
            }
        }
    }

    /// Return a bounded, scrollable-by-caller textual projection.
    pub(crate) fn text(&self, width: usize, rows: usize) -> String {
        let lines = limit_lines(
            state_text(&self.state, &self.driver.sources, rows),
            &self.state,
            rows,
        );
        lines
            .into_iter()
            .map(|line| clip_width(&line, width.max(1)))
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub(crate) fn help(&self) -> &str {
        "↑/↓ choose · type to search · Enter review · Enter confirm · Esc back/close"
    }

    pub(crate) fn busy(&self) -> bool {
        matches!(self.state, State::Loading { .. } | State::Mutating { .. })
    }

    pub(crate) fn closing_notice(&self) -> Option<String> {
        self.closing_notice.clone()
    }

    fn start_load(&mut self, source: Source) {
        self.cancel.cancel();
        self.cancel = CancellationToken::default();
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        let cancel = self.cancel.clone();
        let load = self.driver.load.clone();
        let tx = self.tx.clone();
        self.state = State::Loading {
            source: source.clone(),
        };
        thread::spawn(move || {
            let result = load(source, cancel).map_err(|e| e.to_string());
            let _ = tx.send(Reply::Loaded { generation, result });
        });
    }

    fn start_adopt(&mut self, source: Source, candidate: Candidate) {
        self.cancel.cancel();
        self.cancel = CancellationToken::default();
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        let cancel = self.cancel.clone();
        let adopt = self.driver.adopt.clone();
        let tx = self.tx.clone();
        self.state = State::Mutating {
            source: source.clone(),
            candidate: candidate.clone(),
        };
        thread::spawn(move || {
            let result = adopt(source, candidate, cancel).map_err(|e| e.to_string());
            let _ = tx.send(Reply::Adopted { generation, result });
        });
    }
}

fn state_text(state: &State, sources: &[Source], rows: usize) -> Vec<String> {
    match state {
        State::Sources { selected } => source_text(sources, *selected),
        State::Loading { source } => vec![
            format!(
                "Looking for personally named conversations on {}…",
                safe(&source.label)
            ),
            "Esc closes; discovery runs off the UI thread.".into(),
        ],
        State::Candidates(pick) => candidate_text(pick, rows),
        State::Confirm(pick) => confirmation_text(pick),
        State::Mutating { source, candidate } => vec![format!(
            "Adding {} {} on {}… · Esc closes with outcome possibly unknown",
            candidate.provider,
            short(&candidate.session_id),
            safe(&source.label)
        )],
        State::Finished(message) => vec![message.clone(), "Enter or Esc close".into()],
    }
}

fn source_text(sources: &[Source], selected: usize) -> Vec<String> {
    let mut lines = vec!["Add named conversation · choose a source".into()];
    lines.extend(sources.iter().enumerate().map(|(index, source)| {
        format!(
            "{} {}",
            if index == selected { "›" } else { " " },
            safe(&source.label)
        )
    }));
    if sources.is_empty() {
        lines.push("No sources available.".into());
    }
    lines.push("↑/↓ choose · Enter inspect · Esc close".into());
    lines
}

fn candidate_text(pick: &Pick, rows: usize) -> Vec<String> {
    let matches = matching(pick);
    let heading = format!(
        "{} · {} candidate(s){}",
        safe(&pick.source.label),
        matches.len(),
        if pick.query.is_empty() {
            String::new()
        } else {
            format!(" · /{}", safe(&pick.query))
        }
    );
    let mut lines = vec![heading];
    if let Some(error) = &pick.error {
        lines.push(format!("Discovery failed: {} · r retry", safe(error)));
    }
    if matches.is_empty() && pick.error.is_none() {
        lines.push("No matching personally named conversations to add.".into());
    }
    candidate_rows(pick, &matches, rows, &mut lines);
    if let Some(candidate) = pick
        .candidates
        .get(pick.selected)
        .filter(|_| matches.contains(&pick.selected))
    {
        lines.push(format!("UUID {}", safe(&candidate.session_id)));
        lines.push(format!(
            "Project {} · Node {}",
            safe(candidate.cwd.as_deref().unwrap_or("—")),
            safe(&pick.source.label)
        ));
    }
    lines.push("↑/↓ select · type filter · Enter review · Esc close".into());
    lines
}

fn candidate_rows(pick: &Pick, matches: &[usize], rows: usize, lines: &mut Vec<String>) {
    let selected_pos = matches
        .iter()
        .position(|&index| index == pick.selected)
        .unwrap_or(0);
    let room = rows.saturating_sub(5).min(12);
    let start = selected_pos
        .saturating_sub(room.saturating_sub(1) / 2)
        .min(matches.len().saturating_sub(room));
    for &index in matches.iter().skip(start).take(room) {
        let candidate = &pick.candidates[index];
        lines.push(format!(
            "{} {} · {} · {}",
            if index == pick.selected { "›" } else { " " },
            candidate.provider,
            safe(candidate.name.as_deref().unwrap_or("(unnamed)")),
            short(&candidate.session_id)
        ));
    }
}

fn confirmation_text(pick: &Pick) -> Vec<String> {
    let Some(candidate) = pick.candidates.get(pick.selected) else {
        return Vec::new();
    };
    vec![
        format!(
            "Confirm add · {}",
            safe(candidate.name.as_deref().unwrap_or("(unnamed)"))
        ),
        format!("Provider {}", candidate.provider),
        format!("UUID {}", safe(&candidate.session_id)),
        format!("Node {}", safe(&pick.source.label)),
        format!(
            "Node ID {}",
            safe(pick.source.id.as_deref().unwrap_or("local"))
        ),
        format!("Project {}", safe(candidate.cwd.as_deref().unwrap_or("—"))),
        "Enter add · Esc back (no change yet)".into(),
    ]
}

fn limit_lines(mut lines: Vec<String>, state: &State, rows: usize) -> Vec<String> {
    let cap = rows.max(1);
    if lines.len() <= cap {
        return lines;
    }
    if matches!(state, State::Candidates(_)) && cap >= 3 {
        return compact_candidates(lines, cap);
    }
    lines.truncate(cap);
    lines
}

fn compact_candidates(lines: Vec<String>, cap: usize) -> Vec<String> {
    let mut compact = vec![lines[0].clone()];
    if cap == 3 {
        if let Some(id) = lines.iter().find(|line| line.starts_with("UUID ")) {
            compact.push(id.clone());
        } else if lines.len() > 1 {
            compact.push(lines[1].clone());
        }
        compact.push(lines.last().cloned().unwrap_or_default());
    } else {
        let tail_start = lines.len().saturating_sub(cap - 1);
        compact.extend(lines.into_iter().skip(tail_start));
    }
    compact
}

fn source_key(selected: &mut usize, sources: &[Source], key: KeyEvent) -> KeyAction {
    if key.kind == KeyEventKind::Repeat {
        return KeyAction::None;
    }
    match key.code {
        KeyCode::Esc => KeyAction::Close,
        KeyCode::Up => {
            *selected = selected.saturating_sub(1);
            KeyAction::None
        }
        KeyCode::Down => {
            *selected = selected
                .saturating_add(1)
                .min(sources.len().saturating_sub(1));
            KeyAction::None
        }
        KeyCode::Home => {
            *selected = 0;
            KeyAction::None
        }
        KeyCode::End => {
            *selected = sources.len().saturating_sub(1);
            KeyAction::None
        }
        KeyCode::Enter => sources
            .get(*selected)
            .cloned()
            .map_or(KeyAction::None, KeyAction::Load),
        _ => KeyAction::None,
    }
}

fn candidate_key(pick: &mut Pick, key: KeyEvent) -> KeyAction {
    if key.kind == KeyEventKind::Repeat && key.code == KeyCode::Enter {
        return KeyAction::None;
    }
    let matches = matching(pick);
    let current = matches
        .iter()
        .position(|&index| index == pick.selected)
        .unwrap_or(0);
    match key.code {
        KeyCode::Esc => KeyAction::Close,
        KeyCode::Up => {
            if let Some(&index) = matches.get(current.saturating_sub(1)) {
                pick.selected = index;
            }
            KeyAction::None
        }
        KeyCode::Down => {
            if let Some(&index) = matches.get((current + 1).min(matches.len().saturating_sub(1))) {
                pick.selected = index;
            }
            KeyAction::None
        }
        KeyCode::Home => {
            if let Some(&index) = matches.first() {
                pick.selected = index;
            }
            KeyAction::None
        }
        KeyCode::End => {
            if let Some(&index) = matches.last() {
                pick.selected = index;
            }
            KeyAction::None
        }
        KeyCode::Enter if !matches.is_empty() => KeyAction::Confirm,
        KeyCode::Char('r') if pick.error.is_some() && key.kind != KeyEventKind::Repeat => {
            KeyAction::Load(pick.source.clone())
        }
        KeyCode::Backspace => {
            pick.query.pop();
            pick.selected = matching(pick).first().copied().unwrap_or(0);
            KeyAction::None
        }
        KeyCode::Char(_)
            if key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER) =>
        {
            KeyAction::None
        }
        KeyCode::Char(character) => {
            if pick.query.len() + character.len_utf8() <= MAX_QUERY_BYTES {
                pick.query.push(character);
            }
            pick.selected = matching(pick).first().copied().unwrap_or(0);
            KeyAction::None
        }
        _ => KeyAction::None,
    }
}

fn confirm_key(pick: &Pick, key: KeyEvent) -> KeyAction {
    if key.kind == KeyEventKind::Repeat {
        return KeyAction::None;
    }
    match key.code {
        KeyCode::Esc => KeyAction::Back,
        KeyCode::Enter => pick
            .candidates
            .get(pick.selected)
            .cloned()
            .map(|candidate| KeyAction::Adopt(pick.source.clone(), Box::new(candidate)))
            .unwrap_or(KeyAction::None),
        _ => KeyAction::None,
    }
}

impl Drop for Panel {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

fn normalize_candidates(candidates: &mut Vec<Candidate>) {
    candidates.sort_by(|a, b| {
        b.updated_at
            .total_cmp(&a.updated_at)
            .then_with(|| a.provider.cmp(&b.provider))
            .then_with(|| a.session_id.cmp(&b.session_id))
    });
    let mut seen = HashSet::new();
    candidates.retain(|candidate| seen.insert((candidate.provider, candidate.session_id.clone())));
    candidates.truncate(MAX_CANDIDATES);
}

fn matching(pick: &Pick) -> Vec<usize> {
    let q = pick.query.to_lowercase();
    pick.candidates
        .iter()
        .enumerate()
        .filter_map(|(i, c)| {
            let hay = [
                c.provider.as_str(),
                c.name.as_deref().unwrap_or(""),
                &c.session_id,
                c.cwd.as_deref().unwrap_or(""),
                c.branch.as_deref().unwrap_or(""),
                c.model.as_deref().unwrap_or(""),
            ]
            .join(" ")
            .to_lowercase();
            (q.is_empty() || hay.contains(&q)).then_some(i)
        })
        .collect()
}

fn safe(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_control())
        .take(MAX_FIELD_CHARS)
        .collect()
}

fn clip_width(s: &str, width: usize) -> String {
    use unicode_width::UnicodeWidthChar;
    let mut result = String::new();
    let mut used = 0;
    for ch in s.chars() {
        let character_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + character_width > width {
            break;
        }
        result.push(ch);
        used += character_width;
    }
    result
}

fn short(id: &str) -> String {
    id.chars().take(8).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Provider;
    use std::{
        sync::{
            Mutex,
            atomic::{AtomicUsize, Ordering as AtomicOrdering},
            mpsc,
        },
        time::{Duration, Instant},
    };

    fn candidate(id: &str, name: Option<&str>, updated_at: f64) -> Candidate {
        Candidate {
            provider: Provider::Codex,
            session_id: id.into(),
            name: name.map(str::to_owned),
            cwd: Some("/project".into()),
            branch: None,
            transcript_path: None,
            model: None,
            updated_at,
            live: false,
            pid: None,
            source: "test".into(),
            parent_session_id: None,
            created_at: 0.0,
            lifecycle_status: None,
        }
    }
    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, crossterm::event::KeyModifiers::NONE)
    }
    fn wait(panel: &mut Panel) {
        let until = Instant::now() + Duration::from_secs(2);
        while panel.busy() && Instant::now() < until {
            panel.poll();
            thread::sleep(Duration::from_millis(2));
        }
        panel.poll();
        assert!(!panel.busy(), "worker should finish promptly");
    }
    fn local_driver(
        load: impl Fn(Source, CancellationToken) -> Result<Vec<Candidate>> + Send + Sync + 'static,
        adopt: impl Fn(Source, Candidate, CancellationToken) -> Result<String> + Send + Sync + 'static,
    ) -> Driver {
        Driver::new(
            vec![Source {
                id: None,
                label: "local".into(),
            }],
            load,
            adopt,
        )
    }

    #[test]
    fn cancel_and_search_never_adopts_until_two_distinct_enters() {
        let writes = Arc::new(AtomicUsize::new(0));
        let w = writes.clone();
        let mut panel = Panel::new(local_driver(
            |_, _| {
                Ok(vec![
                    candidate("uuid-a", Some("alpha"), 1.0),
                    candidate("uuid-b", Some("beta"), 2.0),
                ])
            },
            move |_, _, _| {
                w.fetch_add(1, AtomicOrdering::SeqCst);
                Ok("receipt".into())
            },
        ));
        wait(&mut panel);
        panel.key(KeyEvent::new(KeyCode::Char('B'), KeyModifiers::SHIFT));
        assert!(panel.text(100, 20).contains("uuid-b"));
        panel.key(press(KeyCode::Enter));
        assert!(panel.text(100, 20).contains("Confirm add"));
        let mut repeat = press(KeyCode::Enter);
        repeat.kind = KeyEventKind::Repeat;
        panel.key(repeat);
        assert_eq!(writes.load(AtomicOrdering::SeqCst), 0);
        panel.key(press(KeyCode::Esc));
        assert!(panel.text(100, 20).contains("beta"));
        assert_eq!(writes.load(AtomicOrdering::SeqCst), 0);
        panel.key(press(KeyCode::Esc));
        assert_eq!(writes.load(AtomicOrdering::SeqCst), 0);
    }

    #[test]
    fn approval_adopts_exact_selected_identity_once_and_shows_receipt() {
        let source = Source {
            id: Some("12345678-1234-1234-1234-123456789abc".into()),
            label: "workstation".into(),
        };
        let first = candidate(
            "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
            Some("same label"),
            2.0,
        );
        let mut second = candidate(
            "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
            Some("same label"),
            2.0,
        );
        second.provider = Provider::Claude;
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let recorded_by_worker = recorded.clone();
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let release_rx = Mutex::new(release_rx);
        let mut panel = Panel::new(Driver::new(
            vec![source.clone()],
            move |_, _| Ok(vec![first.clone(), second.clone()]),
            move |source, candidate, _| {
                recorded_by_worker.lock().unwrap().push((source, candidate));
                started_tx.send(()).unwrap();
                release_rx.lock().unwrap().recv().unwrap();
                Ok("adopted".into())
            },
        ));
        wait(&mut panel);
        assert!(
            recorded.lock().unwrap().is_empty(),
            "discovery must not adopt"
        );

        panel.key(press(KeyCode::Down));
        panel.key(press(KeyCode::Enter));
        let confirm = panel.text(45, 20);
        assert!(
            confirm
                .lines()
                .any(|line| line == "UUID bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb")
        );
        assert!(
            confirm
                .lines()
                .any(|line| line == "Node ID 12345678-1234-1234-1234-123456789abc")
        );
        assert!(confirm.contains("Project /project"));
        assert!(
            recorded.lock().unwrap().is_empty(),
            "review is not approval"
        );

        let mut repeat = press(KeyCode::Enter);
        repeat.kind = KeyEventKind::Repeat;
        panel.key(repeat);
        assert!(
            recorded.lock().unwrap().is_empty(),
            "repeat Enter must not approve"
        );
        panel.key(press(KeyCode::Enter));
        started_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("approved callback starts");
        {
            let calls = recorded.lock().unwrap();
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].0, source);
            assert_eq!(calls[0].1.provider, Provider::Claude);
            assert_eq!(
                calls[0].1.session_id,
                "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb"
            );
            assert_eq!(calls[0].1.name.as_deref(), Some("same label"));
        }
        panel.key(press(KeyCode::Enter)); // Busy state ignores all non-close input.
        release_tx.send(()).unwrap();
        wait(&mut panel);
        assert!(panel.text(45, 20).contains("Added to board: adopted"));
        assert_eq!(
            recorded.lock().unwrap().len(),
            1,
            "one approval calls adoption once"
        );
    }

    #[test]
    fn failed_adoption_reports_unknown_outcome_without_retrying() {
        let writes = Arc::new(AtomicUsize::new(0));
        let attempts = writes.clone();
        let mut panel = Panel::new(local_driver(
            |_, _| Ok(vec![candidate("exact-uuid", Some("entry"), 1.0)]),
            move |_, _, _| {
                attempts.fetch_add(1, AtomicOrdering::SeqCst);
                anyhow::bail!("connection lost after request")
            },
        ));
        wait(&mut panel);
        panel.key(press(KeyCode::Enter));
        assert_eq!(writes.load(AtomicOrdering::SeqCst), 0);
        panel.key(press(KeyCode::Enter));
        wait(&mut panel);
        let text = panel.text(90, 10);
        assert!(text.contains("Add did not complete"));
        assert!(text.contains("Check the board before retrying"));
        assert!(text.contains("connection lost after request"));
        assert_eq!(
            writes.load(AtomicOrdering::SeqCst),
            1,
            "errors are not retried silently"
        );
    }

    #[test]
    fn closing_during_adoption_warns_that_result_may_be_unknown() {
        let (started_tx, started_rx) = mpsc::channel();
        let (cancelled_tx, cancelled_rx) = mpsc::channel();
        let mut panel = Panel::new(local_driver(
            |_, _| Ok(vec![candidate("exact-uuid", Some("entry"), 1.0)]),
            move |_, _, cancel| {
                started_tx.send(()).unwrap();
                while !cancel.is_cancelled() {
                    thread::sleep(Duration::from_millis(1));
                }
                cancelled_tx.send(()).unwrap();
                anyhow::bail!("worker stopped after cancel request")
            },
        ));
        wait(&mut panel);
        panel.key(press(KeyCode::Enter));
        panel.key(press(KeyCode::Enter));
        started_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("mutation worker started");
        assert!(panel.key(press(KeyCode::Esc)), "Esc closes the busy panel");
        cancelled_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("worker saw cooperative cancellation");
        assert!(
            panel
                .closing_notice()
                .unwrap()
                .contains("outcome may be unknown")
        );
    }

    #[test]
    fn only_explicitly_selected_source_is_loaded_and_cancel_closes_chooser() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let loaded = calls.clone();
        let mut panel = Panel::new(Driver::new(
            vec![
                Source {
                    id: None,
                    label: "local".into(),
                },
                Source {
                    id: Some("remote-node-uuid".into()),
                    label: "remote".into(),
                },
            ],
            move |source, _| {
                loaded.lock().unwrap().push(source.id.clone());
                Ok(vec![])
            },
            |_, _, _| Ok("unused".into()),
        ));
        assert!(!panel.busy(), "multiple sources require explicit selection");
        assert!(
            calls.lock().unwrap().is_empty(),
            "opening chooser loads no source"
        );
        panel.key(press(KeyCode::Down));
        panel.key(press(KeyCode::Enter));
        wait(&mut panel);
        assert_eq!(
            *calls.lock().unwrap(),
            vec![Some("remote-node-uuid".into())]
        );

        let mut chooser = Panel::new(Driver::new(
            vec![
                Source {
                    id: None,
                    label: "local".into(),
                },
                Source {
                    id: Some("remote".into()),
                    label: "remote".into(),
                },
            ],
            |_, _| Ok(vec![]),
            |_, _, _| Ok("unused".into()),
        ));
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(chooser.key(ctrl_c), "Ctrl-C remains a visible exit path");
    }

    #[test]
    fn search_accepts_j_and_k_and_deduplicates_non_adjacent_identity() {
        let mut many = vec![
            candidate("same", Some("top"), 3.0),
            candidate("other", Some("middle"), 2.0),
            candidate("same", Some("old"), 1.0),
        ];
        normalize_candidates(&mut many);
        assert_eq!(many.len(), 2);
        assert_eq!(many[0].name.as_deref(), Some("top"));

        let mut panel = Panel::new(local_driver(
            |_, _| {
                Ok(vec![
                    candidate("j-name", Some("alpha"), 2.0),
                    candidate("k-name", Some("beta"), 1.0),
                ])
            },
            |_, _, _| Ok("ok".into()),
        ));
        wait(&mut panel);
        panel.key(press(KeyCode::Char('j')));
        assert!(panel.text(100, 20).contains("j-name"));
        panel.key(press(KeyCode::Backspace));
        panel.key(press(KeyCode::Char('k')));
        assert!(panel.text(100, 20).contains("k-name"));
    }

    #[test]
    fn selected_identity_stays_visible_in_bounded_candidate_viewport() {
        let inventory = (0..50)
            .map(|n| candidate(&format!("uuid-{n:03}"), Some("shared label"), n as f64))
            .collect::<Vec<_>>();
        let mut panel = Panel::new(local_driver(
            move |_, _| Ok(inventory.clone()),
            |_, _, _| Ok("unused".into()),
        ));
        wait(&mut panel);
        panel.key(press(KeyCode::End));
        let view = panel.text(45, 8);
        assert!(view.lines().count() <= 8);
        assert!(view.lines().any(|line| line == "UUID uuid-000"));
        assert!(view.contains("select · type filter"));
    }

    #[test]
    fn dropping_panel_signals_load_cancellation() {
        let seen = Arc::new(AtomicUsize::new(0));
        let s = seen.clone();
        let panel = Panel::new(local_driver(
            move |_, cancel| {
                while !cancel.is_cancelled() {
                    thread::sleep(Duration::from_millis(2));
                }
                s.fetch_add(1, AtomicOrdering::SeqCst);
                Ok(vec![])
            },
            |_, _, _| Ok("ok".into()),
        ));
        assert!(panel.busy());
        drop(panel);
        let until = Instant::now() + Duration::from_secs(1);
        while seen.load(AtomicOrdering::SeqCst) == 0 && Instant::now() < until {
            thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(seen.load(AtomicOrdering::SeqCst), 1);
    }

    #[test]
    fn late_reply_from_prior_generation_is_ignored() {
        let mut panel = Panel::new(local_driver(
            |_, _| Ok(vec![candidate("current-uuid", Some("current"), 2.0)]),
            |_, _, _| Ok("unused".into()),
        ));
        wait(&mut panel);
        let before = panel.text(70, 10);
        panel
            .tx
            .send(Reply::Loaded {
                generation: panel.generation.wrapping_sub(1),
                result: Ok(vec![candidate("stale-uuid", Some("stale"), 99.0)]),
            })
            .unwrap();
        assert!(
            !panel.poll(),
            "stale worker reply must not change the current view"
        );
        assert_eq!(panel.text(70, 10), before);
    }

    #[test]
    fn failures_are_visible_and_inventory_is_bounded_and_deduplicated() {
        let mut many = (0..250)
            .map(|n| candidate(&format!("uuid-{n:03}"), None, n as f64))
            .collect::<Vec<_>>();
        many.push(candidate("uuid-249", Some("duplicate"), 249.0));
        normalize_candidates(&mut many);
        assert_eq!(many.len(), MAX_CANDIDATES);
        assert_eq!(many[0].session_id, "uuid-249");
        let mut panel = Panel::new(local_driver(
            |_, _| anyhow::bail!("fake discovery error"),
            |_, _, _| anyhow::bail!("fake write error"),
        ));
        wait(&mut panel);
        assert!(panel.text(100, 20).contains("fake discovery error"));
        panel.key(press(KeyCode::Char('r')));
        wait(&mut panel);
        assert!(panel.text(100, 20).contains("fake discovery error"));
    }
}
