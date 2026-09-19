//! A small, read-only filesystem browser used by the native terminal UI.
//! It deliberately has no connection to provider state, transcripts, or the daemon.

use crate::files_data::{self, Entry, GitSnapshot};
use crate::{files_layout, files_markdown, files_syntax};
use anyhow::{Context, Result};
use crossterm::{
    cursor::{Hide, MoveTo, Show},
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
        MouseButton, MouseEvent, MouseEventKind,
    },
    execute, queue,
    style::{Attribute, Color, Print, ResetColor, SetAttribute, SetForegroundColor},
    terminal::{
        self, BeginSynchronizedUpdate, Clear, ClearType, EndSynchronizedUpdate,
        EnterAlternateScreen, LeaveAlternateScreen,
    },
};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver},
    thread,
    time::Duration,
};
use unicode_width::UnicodeWidthStr;

const MAX_LINES: usize = 10_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Focus {
    Tree,
    File,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ViewMode {
    File,
    Diff,
}

#[derive(Clone, Debug)]
struct Node {
    entry: Entry,
    path: PathBuf,
    depth: usize,
}

#[derive(Clone, Copy)]
struct Geometry {
    tree_visible: bool,
    file_visible: bool,
    tree_width: u16,
    file_x: u16,
    file_width: u16,
    divider: Option<u16>,
}

struct State {
    project: PathBuf,
    current: PathBuf,
    entries: Vec<Entry>,
    cache: HashMap<PathBuf, Vec<Entry>>,
    expanded: HashSet<PathBuf>,
    selected: usize,
    focus: Focus,
    tree_hidden: bool,
    narrow: bool,
    viewport: (u16, u16),
    preferred_tree_width: Option<u16>,
    dragging_divider: bool,
    tree_scroll: usize,
    mode: ViewMode,
    text: String,
    file_path: Option<PathBuf>,
    scroll: usize,
    horizontal: usize,
    wrap: bool,
    markdown: bool,
    rows: Vec<files_layout::Row>,
    layout_width: usize,
    layout_dirty: bool,
    page_size: usize,
    snapshot: GitSnapshot,
    notice: Option<String>,
    dirty: bool,
    changed_only: bool,
    git_rx: Option<GitTask>,
}

struct GitTask {
    rx: Receiver<Result<GitSnapshot, String>>,
    cancel: crate::consult::CancellationToken,
    worker: Option<thread::JoinHandle<()>>,
}
impl Drop for GitTask {
    fn drop(&mut self) {
        self.cancel.cancel();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

struct TerminalGuard;
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(
            io::stdout(),
            DisableMouseCapture,
            Show,
            LeaveAlternateScreen,
            ResetColor
        );
        let _ = terminal::disable_raw_mode();
    }
}

impl State {
    fn new(project: PathBuf) -> Result<Self> {
        let project = absolute(project)?;
        let entries = files_data::list_dir(&project).context("list project directory")?;
        let (git_rx, snapshot) = start_git_snapshot(project.clone());
        Ok(Self {
            project: project.clone(),
            current: project,
            entries,
            cache: HashMap::new(),
            expanded: HashSet::new(),
            selected: 0,
            focus: Focus::Tree,
            tree_hidden: false,
            narrow: false,
            viewport: (0, 0),
            preferred_tree_width: None,
            dragging_divider: false,
            tree_scroll: 0,
            mode: ViewMode::File,
            text: String::new(),
            file_path: None,
            scroll: 0,
            horizontal: 0,
            wrap: true,
            markdown: true,
            rows: Vec::new(),
            layout_width: 0,
            layout_dirty: true,
            page_size: 20,
            snapshot,
            notice: Some("Reading Git status…".into()),
            dirty: true,
            changed_only: false,
            git_rx: Some(git_rx),
        })
    }

    fn nodes(&self) -> Vec<Node> {
        if self.changed_only {
            return self
                .snapshot
                .changes
                .keys()
                .take(MAX_LINES)
                .map(|path| {
                    let metadata = std::fs::symlink_metadata(path).ok();
                    let entry = Entry {
                        path: path.clone(),
                        name: path
                            .strip_prefix(&self.snapshot.root)
                            .unwrap_or(path)
                            .display()
                            .to_string(),
                        is_dir: metadata.as_ref().is_some_and(|m| m.is_dir()),
                        is_symlink: metadata.as_ref().is_some_and(|m| m.is_symlink()),
                    };
                    Node {
                        entry,
                        path: path.clone(),
                        depth: 0,
                    }
                })
                .collect();
        }
        let mut out = Vec::new();
        self.append_nodes(&self.current, &self.entries, 0, &mut out);
        out.truncate(MAX_LINES);
        out
    }

    fn geometry(&self) -> Geometry {
        let (w, _) = self.viewport;
        let narrow = w < 80;
        let tree_visible = !self.tree_hidden && (!narrow || self.focus == Focus::Tree);
        let file_visible = self.tree_hidden || !narrow || self.focus == Focus::File;
        let tree_width = if narrow {
            w
        } else {
            self.preferred_tree_width
                .unwrap_or((w / 3).clamp(26, 40))
                .clamp(18, w.saturating_sub(34).max(18))
        };
        let divider = (!narrow && tree_visible).then_some(tree_width);
        let file_x = divider.map_or(0, |x| x + 2);
        Geometry {
            tree_visible,
            file_visible,
            tree_width,
            file_x,
            file_width: w.saturating_sub(file_x),
            divider,
        }
    }

    fn reveal_tree_selection(&mut self) {
        let visible = self.viewport.1.saturating_sub(5).max(1) as usize;
        if self.selected < self.tree_scroll {
            self.tree_scroll = self.selected;
        } else if self.selected >= self.tree_scroll + visible {
            self.tree_scroll = self.selected.saturating_sub(visible - 1);
        }
    }

    fn append_nodes(&self, _parent: &Path, entries: &[Entry], depth: usize, out: &mut Vec<Node>) {
        if depth > 64 {
            return;
        }
        for entry in entries.iter().take(MAX_LINES) {
            if out.len() >= MAX_LINES {
                return;
            }
            let path = entry.path.clone();
            if !self.changed_only || self.snapshot.changes.contains_key(&path) || entry.is_dir {
                out.push(Node {
                    entry: entry.clone(),
                    path: path.clone(),
                    depth,
                });
            }
            if entry.is_dir && self.expanded.contains(&path) {
                if let Some(children) = self.cache.get(&path) {
                    self.append_nodes(&path, children, depth + 1, out);
                }
            }
        }
    }

    fn selected_node(&self) -> Option<Node> {
        self.nodes().get(self.selected).cloned()
    }

    fn refresh(&mut self) {
        if let Ok(entries) = files_data::list_dir(&self.current) {
            self.entries = entries;
        } else {
            self.notice = Some("Unable to refresh directory".into());
        }
        self.cache.clear();
        self.expanded.clear();
        if self.git_rx.is_none() {
            let (rx, _) = start_git_snapshot(self.current.clone());
            self.git_rx = Some(rx);
            self.notice = Some("Reading Git status…".into());
        }
        if let Some(path) = self.file_path.clone() {
            self.text = if self.mode == ViewMode::Diff {
                files_data::git_diff(&path, &self.snapshot.root)
                    .unwrap_or_else(|_| "Diff unavailable".into())
            } else {
                files_data::read_text(&path).unwrap_or_else(|_| "Unable to read file".into())
            };
        }
        self.selected = self.selected.min(self.nodes().len().saturating_sub(1));
        self.layout_dirty = true;
        self.dirty = true;
    }

    fn open_selected(&mut self) {
        let Some(node) = self.selected_node() else {
            return;
        };
        let path = node.path.clone();
        if node.entry.is_symlink && path.is_dir() {
            match std::fs::canonicalize(&path) {
                Ok(path) => self.navigate(path),
                Err(e) => {
                    self.notice = Some(e.to_string());
                    self.dirty = true;
                }
            }
            return;
        }
        if node.entry.is_dir {
            if !self.expanded.insert(path.clone()) {
                self.expanded.remove(&path);
            }
            if !self.cache.contains_key(&path) {
                match files_data::list_dir(&path) {
                    Ok(entries)
                        if self.cache.values().map(Vec::len).sum::<usize>() + entries.len()
                            <= MAX_LINES =>
                    {
                        self.cache.insert(path, entries);
                    }
                    Ok(_) => {
                        self.expanded.remove(&path);
                        self.notice =
                            Some("Tree limit reached · enter a folder to browse it".into());
                    }
                    Err(e) => {
                        self.expanded.remove(&path);
                        self.notice = Some(e.to_string());
                    }
                }
            }
            self.selected = self.selected.min(self.nodes().len().saturating_sub(1));
        } else {
            self.select_file(path);
        }
        self.dirty = true;
    }

    fn select_file(&mut self, path: PathBuf) {
        self.file_path = Some(path.clone());
        self.rows.clear();
        self.scroll = 0;
        self.horizontal = 0;
        self.mode = ViewMode::File;
        self.markdown = true;
        self.layout_dirty = true;
        self.text =
            files_data::read_text(&path).unwrap_or_else(|e| format!("Unable to read file: {e}"));
        self.focus = Focus::File;
    }

    fn enter_dir(&mut self) {
        let Some(node) = self.selected_node() else {
            return;
        };
        if !node.entry.is_dir {
            return;
        }
        let path = node.path.clone();
        self.navigate(path);
    }

    fn up(&mut self) {
        if let Some(parent) = self.current.parent() {
            self.navigate(parent.to_path_buf());
        }
    }

    fn back_project(&mut self) {
        self.navigate(self.project.clone());
    }

    fn navigate(&mut self, path: PathBuf) {
        match files_data::list_dir(&path) {
            Ok(entries) => {
                if !path.starts_with(&self.snapshot.root) {
                    self.git_rx = None; // cancels and reaps a stale request
                    self.snapshot = GitSnapshot {
                        root: path.clone(),
                        changes: BTreeMap::new(),
                    };
                }
                self.current = path;
                self.entries = entries;
                self.selected = 0;
                self.tree_scroll = 0;
                self.expanded.clear();
                self.cache.clear();
                self.file_path = None;
                self.text.clear();
                self.rows.clear();
                self.scroll = 0;
                self.layout_dirty = true;
                self.focus = Focus::Tree;
                self.tree_hidden = false;
                self.changed_only = false;
                self.notice = Some("Read-only · r refresh Git in this directory".into());
            }
            Err(e) => self.notice = Some(e.to_string()),
        }
        self.dirty = true;
    }

    fn toggle_diff(&mut self) {
        if self.file_path.is_none() {
            return;
        }
        self.mode = match self.mode {
            ViewMode::File => ViewMode::Diff,
            ViewMode::Diff => ViewMode::File,
        };
        if self.mode == ViewMode::Diff {
            let path = self.file_path.as_ref().unwrap();
            self.text = files_data::git_diff(path, &self.snapshot.root)
                .unwrap_or_else(|e| format!("Diff unavailable: {e}"));
        } else if let Some(path) = self.file_path.clone() {
            self.text = files_data::read_text(&path).unwrap_or_default();
        }
        self.scroll = 0;
        self.horizontal = 0;
        self.rows.clear();
        self.layout_dirty = true;
        self.dirty = true;
    }

    fn is_markdown(&self) -> bool {
        self.file_path
            .as_ref()
            .and_then(|p| p.extension())
            .and_then(|s| s.to_str())
            .is_some_and(|s| s.eq_ignore_ascii_case("md") || s.eq_ignore_ascii_case("markdown"))
    }

    fn rendered_markdown(&self) -> bool {
        self.mode == ViewMode::File && self.is_markdown() && self.markdown
    }

    fn prepare_layout(&mut self, width: usize) {
        if !self.layout_dirty && self.layout_width == width {
            return;
        }
        // Preserve the source location across resizing or display-mode changes.
        let anchor = self
            .rows
            .get(self.scroll)
            .map(|r| r.source_line)
            .unwrap_or(1);
        let mut document = if self.rendered_markdown() {
            files_markdown::parse(&self.text)
        } else {
            files_layout::source(&self.text)
        };
        if self.mode == ViewMode::File && !self.rendered_markdown() {
            if let Some(path) = &self.file_path {
                files_syntax::highlight(&mut document, path);
            }
        }
        self.rows = files_layout::layout(&document, width, self.wrap);
        self.scroll = self
            .rows
            .iter()
            .position(|r| r.source_line >= anchor)
            .unwrap_or(self.rows.len().saturating_sub(1));
        self.layout_width = width;
        self.layout_dirty = false;
    }
}

/// Run the read-only Files viewer. The terminal is always restored on exit.
pub fn run(project: PathBuf) -> Result<i32> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        anyhow::bail!("files viewer requires an interactive terminal")
    }
    let mut state = State::new(project)?;
    terminal::enable_raw_mode()?;
    let _guard = TerminalGuard;
    let mut out = io::stdout();
    execute!(out, EnterAlternateScreen, Hide, EnableMouseCapture)?;
    loop {
        if let Some(rx) = &state.git_rx {
            match rx.rx.try_recv() {
                Ok(Ok(snapshot)) => {
                    state.snapshot = snapshot;
                    state.git_rx = None;
                    state.notice = Some("Git status ready".into());
                    state.dirty = true;
                }
                Ok(Err(error)) => {
                    state.git_rx = None;
                    state.notice = Some(format!("Git status unavailable: {error}"));
                    state.dirty = true;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    state.git_rx = None;
                    state.notice = Some("Git status unavailable".into());
                    state.dirty = true;
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if state.dirty {
            draw(&mut out, &mut state)?;
            state.dirty = false;
        }
        if !event::poll(Duration::from_millis(250))? {
            continue;
        }
        match event::read()? {
            Event::Resize(_, _) => {
                state.dragging_divider = false;
                state.dirty = true;
            }
            Event::Mouse(mouse) => handle_mouse(&mut state, mouse),
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                if handle_key(&mut state, key) {
                    break Ok(0);
                }
            }
            _ => {}
        }
    }
}

fn start_git_snapshot(project: PathBuf) -> (GitTask, GitSnapshot) {
    let (tx, rx) = mpsc::sync_channel(1);
    let fallback = GitSnapshot {
        root: project.clone(),
        changes: BTreeMap::new(),
    };
    let cancel = crate::consult::CancellationToken::default();
    let stop = cancel.clone();
    let worker = thread::Builder::new()
        .name("pika-files-git".into())
        .spawn(move || {
            let _ = tx.send(
                files_data::git_snapshot_cancellable(&project, &stop).map_err(|e| e.to_string()),
            );
        })
        .ok();
    (GitTask { rx, cancel, worker }, fallback)
}

fn handle_key(state: &mut State, key: KeyEvent) -> bool {
    state.dragging_divider = false;
    if key.modifiers.contains(event::KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return true;
    }
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => return true,
        KeyCode::Tab => {
            state.focus = if state.focus == Focus::Tree {
                Focus::File
            } else {
                Focus::Tree
            };
            if state.focus == Focus::Tree {
                state.tree_hidden = false;
            }
            state.dirty = true;
        }
        KeyCode::Char('t') => {
            let visible = !state.tree_hidden && (!state.narrow || state.focus == Focus::Tree);
            state.tree_hidden = visible;
            state.focus = if visible { Focus::File } else { Focus::Tree };
            state.dirty = true;
        }
        KeyCode::Char('r') => state.refresh(),
        KeyCode::Char('b') => state.back_project(),
        KeyCode::Backspace | KeyCode::Char('u') => state.up(),
        KeyCode::Char('d') => state.toggle_diff(),
        KeyCode::Char('w') => {
            state.wrap = !state.wrap;
            state.horizontal = 0;
            state.layout_dirty = true;
            state.dirty = true;
        }
        KeyCode::Char('m') if state.is_markdown() && state.mode == ViewMode::File => {
            state.markdown = !state.markdown;
            state.horizontal = 0;
            state.layout_dirty = true;
            state.dirty = true;
        }
        KeyCode::Char('c') => {
            state.tree_hidden = false;
            state.focus = Focus::Tree;
            state.changed_only = !state.changed_only;
            state.selected = 0;
            state.tree_scroll = 0;
            state.notice = Some(
                if state.changed_only {
                    "Changed files only"
                } else {
                    "All files"
                }
                .into(),
            );
            state.dirty = true;
        }
        KeyCode::Up | KeyCode::Char('k') => {
            if state.focus == Focus::Tree {
                state.selected = state.selected.saturating_sub(1);
                state.dirty = true
            } else {
                state.scroll = state.scroll.saturating_sub(1);
                state.dirty = true
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if state.focus == Focus::Tree {
                state.selected = (state.selected + 1).min(state.nodes().len().saturating_sub(1));
                state.dirty = true
            } else {
                state.scroll = state
                    .scroll
                    .saturating_add(1)
                    .min(state.rows.len().saturating_sub(1));
                state.dirty = true
            }
        }
        KeyCode::PageDown if state.focus == Focus::File => {
            state.scroll = (state.scroll + state.page_size).min(state.rows.len().saturating_sub(1));
            state.dirty = true;
        }
        KeyCode::PageUp if state.focus == Focus::File => {
            state.scroll = state.scroll.saturating_sub(state.page_size);
            state.dirty = true;
        }
        KeyCode::Home if state.focus == Focus::File => {
            state.scroll = 0;
            state.horizontal = 0;
            state.dirty = true;
        }
        KeyCode::End if state.focus == Focus::File => {
            state.scroll = state.rows.len().saturating_sub(state.page_size);
            state.dirty = true;
        }
        KeyCode::Left if state.focus == Focus::File => {
            state.horizontal = state.horizontal.saturating_sub(4);
            state.dirty = true;
        }
        KeyCode::Right if state.focus == Focus::File => {
            state.horizontal = (state.horizontal + 4).min(256 * 1024);
            state.dirty = true;
        }
        KeyCode::Left => state.up(),
        KeyCode::Right => state.open_selected(),
        KeyCode::Enter => {
            if state.focus == Focus::Tree {
                if state.selected_node().is_some_and(|n| n.entry.is_dir) {
                    state.enter_dir()
                } else {
                    state.open_selected()
                }
            }
        }
        _ => {}
    }
    if state.focus == Focus::Tree {
        state.reveal_tree_selection();
    }
    false
}

fn handle_mouse(state: &mut State, mouse: MouseEvent) {
    let (w, h) = state.viewport;
    if mouse.kind == MouseEventKind::Up(MouseButton::Left) {
        state.dragging_divider = false;
        return;
    }
    if w < 30 || h < 8 {
        return;
    }
    let g = state.geometry();
    if mouse.kind == MouseEventKind::Drag(MouseButton::Left) && state.dragging_divider {
        if g.divider.is_none() {
            state.dragging_divider = false;
            return;
        }
        let width = mouse.column.clamp(18, w.saturating_sub(34));
        if Some(width) != state.preferred_tree_width {
            state.preferred_tree_width = Some(width);
            state.dirty = true;
        }
        return;
    }
    if mouse.column >= w || mouse.row >= h.saturating_sub(2) {
        return;
    }
    if mouse.kind == MouseEventKind::Down(MouseButton::Left) {
        state.dragging_divider = false;
        if g.divider == Some(mouse.column) && mouse.row >= 1 {
            state.dragging_divider = true;
            return;
        }
    }
    let on_tree = g.tree_visible && mouse.column < g.tree_width && mouse.row >= 2;
    let on_file = g.file_visible && mouse.column >= g.file_x && mouse.row >= 3;
    match mouse.kind {
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
            let down = mouse.kind == MouseEventKind::ScrollDown;
            if on_tree {
                let max = state
                    .nodes()
                    .len()
                    .saturating_sub(h.saturating_sub(5) as usize);
                let scroll = if down {
                    state.tree_scroll.saturating_add(3).min(max)
                } else {
                    state.tree_scroll.saturating_sub(3)
                };
                state.dirty |= scroll != state.tree_scroll;
                state.tree_scroll = scroll;
            } else if on_file {
                let max = state.rows.len().saturating_sub(state.page_size);
                let scroll = if down {
                    state.scroll.saturating_add(3).min(max).max(state.scroll)
                } else {
                    state.scroll.saturating_sub(3)
                };
                state.dirty |= scroll != state.scroll;
                state.scroll = scroll;
            }
        }
        MouseEventKind::ScrollLeft | MouseEventKind::ScrollRight if on_file => {
            if state
                .rows
                .iter()
                .skip(state.scroll)
                .take(state.page_size)
                .any(|r| r.horizontal)
            {
                let offset = if mouse.kind == MouseEventKind::ScrollRight {
                    state.horizontal.saturating_add(4).min(256 * 1024)
                } else {
                    state.horizontal.saturating_sub(4)
                };
                state.dirty |= offset != state.horizontal;
                state.horizontal = offset;
            }
        }
        MouseEventKind::Down(MouseButton::Left) if on_tree => {
            let index = state.tree_scroll + mouse.row.saturating_sub(2) as usize;
            if index < state.nodes().len() && mouse.row < h.saturating_sub(3) {
                state.focus = Focus::Tree;
                state.selected = index;
                state.open_selected();
                state.reveal_tree_selection();
            }
        }
        MouseEventKind::Down(MouseButton::Left) if on_file => {
            state.dirty |= state.focus != Focus::File;
            state.focus = Focus::File;
        }
        _ => {}
    }
}

fn draw(out: &mut impl Write, state: &mut State) -> Result<()> {
    let size = terminal::size()?;
    let frame = render(state, size)?;
    out.write_all(&frame)?;
    out.flush()?;
    Ok(())
}

fn render(state: &mut State, (w, h): (u16, u16)) -> Result<Vec<u8>> {
    state.viewport = (w, h);
    let mut frame = Vec::new();
    let color = std::env::var_os("NO_COLOR").is_none();
    queue!(
        frame,
        BeginSynchronizedUpdate,
        Hide,
        SetAttribute(Attribute::Reset),
        ResetColor
    )?;
    // Overwrite rows inside one buffered frame; never expose a cleared screen.
    for y in 0..h {
        queue!(frame, MoveTo(0, y), Clear(ClearType::CurrentLine))?;
    }
    if w < 30 || h < 8 {
        if h > 0 {
            cell(&mut frame, 0, 0, w, "q close · Files · enlarge pane")?;
        }
        queue!(frame, EndSynchronizedUpdate)?;
        return Ok(frame);
    }
    if color {
        queue!(frame, SetForegroundColor(Color::Cyan))?;
    }
    queue!(frame, SetAttribute(Attribute::Bold))?;
    cell(
        &mut frame,
        0,
        0,
        w,
        &format!("Files · {}", state.current.display()),
    )?;
    queue!(frame, SetAttribute(Attribute::Reset), ResetColor)?;
    let narrow = w < 80;
    state.narrow = narrow;
    let Geometry {
        tree_visible,
        file_visible,
        tree_width,
        file_x,
        file_width,
        ..
    } = state.geometry();
    let rendered = state.rendered_markdown();
    let gutter = if rendered { 0 } else { 7 };
    let content_width = file_width.saturating_sub(gutter);
    state.page_size = h.saturating_sub(6).max(1) as usize;
    state.prepare_layout(content_width as usize);
    if tree_visible {
        let label = if state.changed_only {
            "Changed · workspace"
        } else if !narrow {
            "Tree · drag edge"
        } else {
            "Tree"
        };
        cell(&mut frame, 0, 1, tree_width, label)?;
        let nodes = state.nodes();
        let available = h.saturating_sub(5) as usize;
        state.tree_scroll = state.tree_scroll.min(nodes.len().saturating_sub(available));
        let start = state.tree_scroll;
        for (i, node) in nodes.iter().skip(start).take(available).enumerate() {
            let prefix = if node.entry.is_dir {
                if state.expanded.contains(&node.path) {
                    "▾"
                } else {
                    "▸"
                }
            } else if node.entry.is_symlink {
                "↗"
            } else {
                " "
            };
            let row = format!(
                "{}{} {}{}",
                "  ".repeat(node.depth.min(32)),
                prefix,
                node.entry.name,
                status_for(&state.snapshot, &node.path)
            );
            if start + i == state.selected && state.focus == Focus::Tree {
                queue!(frame, SetAttribute(Attribute::Reverse))?;
            }
            cell(&mut frame, 0, 2 + i as u16, tree_width, &row)?;
            queue!(frame, SetAttribute(Attribute::NoReverse))?;
        }
        if nodes.is_empty() {
            cell(
                &mut frame,
                0,
                2,
                tree_width,
                if state.changed_only {
                    "No changes to show"
                } else {
                    "Empty directory"
                },
            )?;
        }
    }
    if !narrow && tree_visible {
        if color {
            queue!(frame, SetForegroundColor(Color::DarkGrey))?;
        }
        for y in 1..h.saturating_sub(2) {
            cell(&mut frame, tree_width, y, 1, "│")?;
        }
        queue!(frame, ResetColor)?;
    }
    if file_visible {
        let mode = if state.mode == ViewMode::Diff {
            "Diff"
        } else if rendered {
            "Rendered"
        } else {
            "Source"
        };
        let tabs = format!(
            "{mode} · w wrap {}{}",
            if state.wrap { "on" } else { "off" },
            if state.is_markdown() && state.mode == ViewMode::File {
                " · m toggle"
            } else {
                ""
            }
        );
        cell(&mut frame, file_x, 1, file_width, &tabs)?;
        let name = state
            .file_path
            .as_deref()
            .and_then(|p| p.strip_prefix(&state.current).ok())
            .or(state.file_path.as_deref());
        cell(
            &mut frame,
            file_x,
            2,
            file_width,
            &name
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "Select a file · Enter".into()),
        )?;
        for (i, row) in state
            .rows
            .iter()
            .skip(state.scroll)
            .take(h.saturating_sub(6) as usize)
            .enumerate()
        {
            let y = 3 + i as u16;
            if !rendered {
                if color {
                    queue!(frame, SetForegroundColor(Color::DarkGrey))?;
                }
                let number = if row.continuation {
                    "       ".into()
                } else {
                    format!("{:>5}  ", row.source_line)
                };
                cell(&mut frame, file_x, y, gutter, &number)?;
                queue!(frame, ResetColor)?;
            }
            let offset = if row.horizontal { state.horizontal } else { 0 };
            let spans = files_layout::viewport(&row.spans, offset, content_width as usize);
            let diff_color = if state.mode == ViewMode::Diff {
                match row.spans.first().and_then(|s| s.text.chars().next()) {
                    Some('+') => Some(Color::Green),
                    Some('-') => Some(Color::Red),
                    _ => None,
                }
            } else {
                None
            };
            queue!(frame, MoveTo(file_x + gutter, y))?;
            for span in spans {
                queue!(frame, SetAttribute(Attribute::Reset), ResetColor)?;
                if span.style.bold || span.style.heading {
                    queue!(frame, SetAttribute(Attribute::Bold))?;
                }
                if span.style.italic {
                    queue!(frame, SetAttribute(Attribute::Italic))?;
                }
                if span.style.link {
                    queue!(frame, SetAttribute(Attribute::Underlined))?;
                }
                if color {
                    let shade = diff_color.unwrap_or(if span.style.heading || span.style.link {
                        Color::Cyan
                    } else if span.style.code {
                        Color::Yellow
                    } else if span.style.quote {
                        Color::DarkGrey
                    } else {
                        syntax_color(span.style.syntax)
                    });
                    queue!(frame, SetForegroundColor(shade))?;
                }
                queue!(frame, Print(span.text))?;
            }
            queue!(frame, SetAttribute(Attribute::Reset), ResetColor)?;
        }
    }
    let notice = state
        .notice
        .as_deref()
        .unwrap_or("Read-only · workspace changes");
    if color {
        queue!(frame, SetForegroundColor(Color::DarkGrey))?;
    }
    cell(&mut frame, 0, h - 2, w, notice)?;
    queue!(frame, ResetColor)?;
    let keys = if state.focus == Focus::File && w >= 80 {
        if state.is_markdown() && state.mode == ViewMode::File {
            "Tab tree · ↑↓ scroll · w wrap · m Markdown · ←→ pan · d diff"
        } else {
            "Tab tree · ↑↓ scroll · w wrap · ←→ pan · d diff · r refresh"
        }
    } else if state.focus == Focus::File && w >= 40 {
        if state.is_markdown() && state.mode == ViewMode::File {
            "Tab · w wrap · m Markdown"
        } else {
            "Tab · w wrap · d diff"
        }
    } else if state.focus == Focus::File {
        if state.is_markdown() && state.mode == ViewMode::File {
            "w/m"
        } else {
            "w wrap"
        }
    } else if w >= 106 {
        "Tab focus · ↑↓ move · Enter open · → expand · u up · b project · c changed · r refresh"
    } else if w >= 60 {
        "Tab · Enter open · u up · b project"
    } else {
        "Enter open"
    };
    let tree_key = if w < 60 {
        "t tree"
    } else if tree_visible {
        "t hide tree"
    } else {
        "t show tree"
    };
    cell(
        &mut frame,
        0,
        h - 1,
        w,
        &format!("q close · {tree_key} · {keys}"),
    )?;
    queue!(frame, EndSynchronizedUpdate)?;
    Ok(frame)
}

fn cell(out: &mut impl Write, x: u16, y: u16, width: u16, text: &str) -> Result<()> {
    queue!(out, MoveTo(x, y), Print(clip(&clean(text), width as usize)))?;
    Ok(())
}

fn syntax_color(ink: files_markdown::SyntaxInk) -> Color {
    use files_markdown::SyntaxInk;
    match ink {
        SyntaxInk::Plain => Color::Reset,
        SyntaxInk::Keyword => Color::Magenta,
        SyntaxInk::String => Color::Green,
        SyntaxInk::Comment => Color::DarkGrey,
        SyntaxInk::Number => Color::Yellow,
        SyntaxInk::Function => Color::Blue,
        SyntaxInk::Type | SyntaxInk::Variable => Color::Cyan,
    }
}

fn status_for(snapshot: &GitSnapshot, path: &Path) -> String {
    snapshot
        .changes
        .get(path)
        .map(|s| format!(" [{}]", clean(s)))
        .unwrap_or_default()
}
fn absolute(path: PathBuf) -> Result<PathBuf> {
    Ok(std::fs::canonicalize(path)?)
}
fn clean(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_control() { '�' } else { c })
        .collect()
}
fn clip(s: &str, width: usize) -> String {
    if UnicodeWidthStr::width(s) <= width {
        return s.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    let target = width.saturating_sub(1);
    let mut used = 0;
    let mut out = String::new();
    for ch in unicode_segmentation::UnicodeSegmentation::graphemes(s, true) {
        let n = UnicodeWidthStr::width(ch);
        if used + n > target {
            break;
        }
        out.push_str(ch);
        used += n;
    }
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn controls_are_removed_from_names() {
        assert_eq!(clean("a\n\tb"), "a��b");
    }
    #[test]
    fn clipping_is_bounded() {
        assert_eq!(clip("abcdef", 4), "abc…");
        for width in 0..20 {
            assert!(clip("文件👨‍👩‍👧‍👦verylong", width).width() <= width);
        }
    }

    #[test]
    fn nested_files_parent_project_and_refresh_do_not_change_process_cwd() {
        let cwd = std::env::current_dir().unwrap();
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join("src")).unwrap();
        std::fs::write(temp.path().join("src/code.rs"), "before").unwrap();
        let mut state = State::new(temp.path().to_path_buf()).unwrap();
        state.open_selected();
        assert_eq!(state.nodes().len(), 2);
        state.selected = 1;
        state.open_selected();
        assert_eq!(state.text, "before");
        std::fs::write(temp.path().join("src/code.rs"), "after").unwrap();
        state.refresh();
        assert_eq!(state.text, "after");
        let project = state.project.clone();
        state.up();
        assert_eq!(state.current, project.parent().unwrap());
        state.back_project();
        assert_eq!(state.current, project);
        state.navigate(temp.path().join("missing"));
        assert_eq!(state.current, project);
        std::os::unix::fs::symlink(project.join("src"), project.join("src-link")).unwrap();
        state.refresh();
        state.selected = state
            .nodes()
            .iter()
            .position(|node| node.entry.name == "src-link")
            .unwrap();
        state.open_selected();
        assert_eq!(state.current, project.join("src"));
        assert_eq!(std::env::current_dir().unwrap(), cwd);
    }

    #[test]
    fn changed_list_and_render_are_bounded_and_inert() {
        let temp = tempfile::tempdir().unwrap();
        let mut state = State::new(temp.path().to_path_buf()).unwrap();
        state.git_rx = None;
        let nested = state.project.join("not-expanded/code.rs");
        state.snapshot.changes.insert(nested.clone(), "M".into());
        state.changed_only = true;
        assert_eq!(state.nodes()[0].path, nested);
        state.text = "\x1b]52;c;do-not-execute\x07\n文件 long code".into();
        state.focus = Focus::File;
        for size in [(120, 32), (60, 20), (32, 8), (10, 2)] {
            let frame = String::from_utf8(render(&mut state, size).unwrap()).unwrap();
            assert!(!frame.contains("\x1b]52"));
            assert!(frame.contains("q close"));
            assert!(frame.ends_with("\x1b[?2026l"));
        }
    }

    #[test]
    fn wrapped_rows_scroll_and_resize_without_changing_source() {
        let temp = tempfile::tempdir().unwrap();
        let mut state = State::new(temp.path().to_path_buf()).unwrap();
        state.git_rx = None;
        state.text = format!("    {}\nlast", "longword ".repeat(100));
        state.focus = Focus::File;
        let original = state.text.clone();
        render(&mut state, (40, 12)).unwrap();
        assert!(state.rows.len() > 20);
        handle_key(&mut state, KeyCode::PageDown.into());
        assert_eq!(state.scroll, 6);
        render(&mut state, (40, 12)).unwrap();
        assert_eq!(
            state.scroll, 6,
            "ordinary render must not reset visual scroll"
        );
        handle_key(&mut state, KeyCode::End.into());
        assert_eq!(state.scroll, state.rows.len() - 6);
        let anchor = state.rows[state.scroll].source_line;
        render(&mut state, (60, 12)).unwrap();
        assert_eq!(state.rows[state.scroll].source_line, anchor);
        handle_key(&mut state, KeyCode::Char('w').into());
        render(&mut state, (60, 12)).unwrap();
        assert!(!state.wrap);
        assert_eq!(state.rows.len(), 2);
        assert_eq!(state.text, original);
    }

    #[test]
    fn markdown_defaults_rendered_and_toggles_without_touching_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("README.MD");
        let text = "# Heading\n\nSome **bold** text\n\n```rust\n    let x = 1;\n```\n";
        std::fs::write(&path, text).unwrap();
        let mut state = State::new(temp.path().to_path_buf()).unwrap();
        state.git_rx = None;
        state.select_file(path.clone());
        let rich = String::from_utf8(render(&mut state, (90, 20)).unwrap()).unwrap();
        assert!(rich.contains("Rendered"));
        assert!(!rich.contains("**bold**"));
        handle_key(&mut state, KeyCode::Char('m').into());
        let raw = String::from_utf8(render(&mut state, (90, 20)).unwrap()).unwrap();
        assert!(raw.contains("Source"));
        assert!(raw.contains("**bold**"));
        handle_key(&mut state, KeyCode::Char('m').into());
        assert!(state.rendered_markdown());
        state.mode = ViewMode::Diff;
        assert!(!state.rendered_markdown());
        assert_eq!(std::fs::read_to_string(path).unwrap(), text);
    }

    #[test]
    fn tree_toggle_gives_full_width_and_preserves_selection_and_source_line() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        std::fs::create_dir(&project).unwrap();
        std::fs::write(project.join("a.rs"), "one\ntwo\nthree\nfour").unwrap();
        let mut state = State::new(project).unwrap();
        state.git_rx = None;
        state.open_selected();
        render(&mut state, (120, 24)).unwrap();
        let split_width = state.layout_width;
        state.scroll = 2;
        let selected = state.selected;
        let path = state.file_path.clone();
        handle_key(&mut state, KeyCode::Char('t').into());
        let full = String::from_utf8(render(&mut state, (120, 24)).unwrap()).unwrap();
        assert!(state.tree_hidden);
        assert_eq!(state.focus, Focus::File);
        assert_eq!(state.layout_width, 113);
        assert!(state.layout_width > split_width);
        assert_eq!(state.rows[state.scroll].source_line, 3);
        assert!(full.contains("t show tree"));
        handle_key(&mut state, KeyCode::Char('t').into());
        render(&mut state, (120, 24)).unwrap();
        assert!(!state.tree_hidden);
        assert_eq!(state.focus, Focus::Tree);
        assert_eq!(state.selected, selected);
        assert_eq!(state.file_path, path);
        assert_eq!(state.rows[state.scroll].source_line, 3);
        handle_key(&mut state, KeyCode::Char('t').into());
        render(&mut state, (60, 24)).unwrap();
        handle_key(&mut state, KeyCode::Char('t').into());
        render(&mut state, (60, 24)).unwrap();
        assert_eq!(state.focus, Focus::Tree);
        handle_key(&mut state, KeyCode::Tab.into());
        render(&mut state, (60, 24)).unwrap();
        handle_key(&mut state, KeyCode::Char('t').into());
        assert_eq!(
            state.focus,
            Focus::Tree,
            "t reveals the auto-hidden narrow tree"
        );
        render(&mut state, (120, 24)).unwrap();
        handle_key(&mut state, KeyCode::Char('t').into());
        handle_key(&mut state, KeyCode::Tab.into());
        assert!(!state.tree_hidden, "Tab must never focus a hidden tree");
        handle_key(&mut state, KeyCode::Char('t').into());
        state.up();
        assert!(!state.tree_hidden, "directory navigation reveals the tree");
    }

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: event::KeyModifiers::NONE,
        }
    }

    #[test]
    fn mouse_wheel_routes_by_hover_without_switching_focus_or_opening_files() {
        let temp = tempfile::tempdir().unwrap();
        for n in 0..40 {
            std::fs::write(temp.path().join(format!("{n:02}.txt")), "safe").unwrap();
        }
        let mut state = State::new(temp.path().to_path_buf()).unwrap();
        state.git_rx = None;
        state.text = (0..100).map(|n| format!("line {n}\n")).collect();
        render(&mut state, (120, 24)).unwrap();
        handle_mouse(&mut state, mouse(MouseEventKind::ScrollDown, 80, 8));
        assert_eq!(state.scroll, 3);
        assert_eq!(state.focus, Focus::Tree);
        assert_eq!(state.tree_scroll, 0);
        handle_mouse(&mut state, mouse(MouseEventKind::ScrollDown, 8, 8));
        assert_eq!(state.tree_scroll, 3);
        assert_eq!(state.scroll, 3);
        assert_eq!(state.selected, 0);
        assert!(state.file_path.is_none());
        render(&mut state, (120, 24)).unwrap();
        assert_eq!(state.tree_scroll, 3, "render must not undo mouse scrolling");
        handle_mouse(&mut state, mouse(MouseEventKind::ScrollUp, 8, 8));
        assert_eq!(state.tree_scroll, 0);
        handle_mouse(&mut state, mouse(MouseEventKind::ScrollUp, 80, 8));
        assert_eq!(state.scroll, 0);
        state.dirty = false;
        handle_mouse(&mut state, mouse(MouseEventKind::Moved, 20, 8));
        handle_mouse(&mut state, mouse(MouseEventKind::ScrollDown, 80, 23));
        assert!(!state.dirty, "hover and footer wheel must not repaint");
        handle_mouse(&mut state, mouse(MouseEventKind::ScrollDown, 8, 8));
        handle_mouse(
            &mut state,
            mouse(MouseEventKind::Down(MouseButton::Left), 8, 2),
        );
        assert_eq!(state.file_path, Some(state.project.join("03.txt")));
        assert_eq!(state.focus, Focus::File);
    }

    #[test]
    fn mouse_divider_drag_is_bounded_persistent_and_cancelled_by_keyboard() {
        let temp = tempfile::tempdir().unwrap();
        let mut state = State::new(temp.path().to_path_buf()).unwrap();
        state.git_rx = None;
        state.text = "first\nsecond\nthird".into();
        render(&mut state, (120, 24)).unwrap();
        state.scroll = 1;
        handle_mouse(
            &mut state,
            mouse(MouseEventKind::Down(MouseButton::Left), 40, 8),
        );
        handle_mouse(
            &mut state,
            mouse(MouseEventKind::Drag(MouseButton::Left), 54, 8),
        );
        render(&mut state, (120, 24)).unwrap();
        assert_eq!(state.geometry().divider, Some(54));
        assert_eq!(state.layout_width, 57);
        assert_eq!(state.rows[state.scroll].source_line, 2);
        handle_mouse(
            &mut state,
            mouse(MouseEventKind::Drag(MouseButton::Left), u16::MAX, 8),
        );
        assert_eq!(state.preferred_tree_width, Some(86));
        handle_mouse(
            &mut state,
            mouse(MouseEventKind::Drag(MouseButton::Left), 0, 8),
        );
        assert_eq!(state.preferred_tree_width, Some(18));
        handle_mouse(
            &mut state,
            mouse(MouseEventKind::Up(MouseButton::Left), 0, u16::MAX),
        );
        handle_mouse(
            &mut state,
            mouse(MouseEventKind::Drag(MouseButton::Left), 60, 8),
        );
        assert_eq!(state.preferred_tree_width, Some(18));
        render(&mut state, (120, 24)).unwrap();
        handle_mouse(
            &mut state,
            mouse(MouseEventKind::Down(MouseButton::Left), 18, 8),
        );
        handle_key(&mut state, KeyCode::Char('t').into());
        handle_mouse(
            &mut state,
            mouse(MouseEventKind::Drag(MouseButton::Left), 60, 8),
        );
        assert_eq!(state.preferred_tree_width, Some(18));
        render(&mut state, (120, 24)).unwrap();
        assert_eq!(state.layout_width, 113);
        handle_key(&mut state, KeyCode::Char('t').into());
        render(&mut state, (120, 24)).unwrap();
        assert_eq!(state.geometry().divider, Some(18));
        render(&mut state, (60, 24)).unwrap();
        assert_eq!(state.geometry().divider, None);
        handle_mouse(
            &mut state,
            mouse(MouseEventKind::Down(MouseButton::Left), 18, 1),
        );
        assert!(!state.dragging_divider);
    }

    #[test]
    fn source_syntax_survives_wrapping_and_keeps_diff_colors_separate() {
        let temp = tempfile::tempdir().unwrap();
        let mut state = State::new(temp.path().to_path_buf()).unwrap();
        state.git_rx = None;
        state.file_path = Some(state.project.join("script.py"));
        state.text = "def example():\n    return \"a long string whose color must survive wrapping across rows\" # note".into();
        state.focus = Focus::File;
        render(&mut state, (40, 16)).unwrap();
        assert!(
            state
                .rows
                .iter()
                .flat_map(|r| &r.spans)
                .any(|s| s.text == "def" && s.style.syntax == files_markdown::SyntaxInk::Keyword)
        );
        let string_rows = state
            .rows
            .iter()
            .filter(|r| {
                r.spans
                    .iter()
                    .any(|s| s.style.syntax == files_markdown::SyntaxInk::String)
            })
            .count();
        assert!(string_rows > 1);
        handle_key(&mut state, KeyCode::Char('w').into());
        render(&mut state, (40, 16)).unwrap();
        assert_eq!(state.rows.len(), 2);
        state.mode = ViewMode::Diff;
        state.layout_dirty = true;
        render(&mut state, (40, 16)).unwrap();
        assert!(
            state
                .rows
                .iter()
                .flat_map(|r| &r.spans)
                .all(|s| s.style.syntax == files_markdown::SyntaxInk::Plain)
        );
    }
}
