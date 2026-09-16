//! Shared terminal presentation for setup. No provider, config or network work.
use anyhow::Result;
use crossterm::{
    cursor::{Hide, MoveTo, Show},
    event::{self, DisableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute, queue,
    style::{Attribute, Color, Print, ResetColor, SetAttribute, SetForegroundColor},
    terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen},
};
use std::io::{self, IsTerminal, Write};
use unicode_width::UnicodeWidthChar;

pub(crate) struct Screen {
    active: bool,
    color: bool,
}

impl Screen {
    pub(crate) fn new(enabled: bool) -> Result<Self> {
        let active = enabled && supported();
        let screen = Self {
            active,
            color: std::env::var_os("NO_COLOR").is_none(),
        };
        if active {
            terminal::enable_raw_mode()?;
            // Construct the guard first so partial initialization is restored.
            execute!(
                io::stdout(),
                DisableMouseCapture,
                EnterAlternateScreen,
                Hide
            )?;
        }
        Ok(screen)
    }

    pub(crate) fn active(&self) -> bool {
        self.active
    }

    pub(crate) fn progress(&self, title: &str, body: &str) -> Result<()> {
        if self.active {
            self.paint(title, body, &[], 0, &[], false, "Working…")?;
        }
        Ok(())
    }

    /// Empty selection is deliberate; Enter never implicitly selects every row.
    pub(crate) fn select(
        &self,
        title: &str,
        body: &str,
        choices: &[String],
        multiple: bool,
    ) -> Result<Option<Vec<usize>>> {
        let mut focused = 0;
        let mut selected = vec![false; choices.len()];
        loop {
            let footer: String = if multiple {
                "Space select · Enter next · d details · Esc skip".into()
            } else {
                "↑↓ move · Enter choose · d details · Esc back".into()
            };
            self.paint(title, body, choices, focused, &selected, multiple, &footer)?;
            match event::read()? {
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    if key.code == KeyCode::Esc
                        || (key.code == KeyCode::Char('c')
                            && key.modifiers.contains(KeyModifiers::CONTROL))
                    {
                        return Ok(None);
                    }
                    if !fits(terminal::size()?) {
                        continue;
                    }
                    match key.code {
                        KeyCode::Char('d') => self.details(
                            title,
                            &format!(
                                "{body}\n\n{}",
                                choices.get(focused).map(String::as_str).unwrap_or("")
                            ),
                        )?,
                        KeyCode::Up | KeyCode::Char('k') => focused = focused.saturating_sub(1),
                        KeyCode::Down | KeyCode::Char('j') => {
                            focused = (focused + 1).min(choices.len().saturating_sub(1));
                        }
                        KeyCode::Home => focused = 0,
                        KeyCode::End => focused = choices.len().saturating_sub(1),
                        KeyCode::Char(' ') if multiple && !choices.is_empty() => {
                            selected[focused] = !selected[focused];
                        }
                        KeyCode::Enter => {
                            return Ok(Some(if multiple {
                                selected
                                    .iter()
                                    .enumerate()
                                    .filter_map(|(i, yes)| yes.then_some(i))
                                    .collect()
                            } else if choices.is_empty() {
                                Vec::new()
                            } else {
                                vec![focused]
                            }));
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
    }

    pub(crate) fn choice(
        &self,
        title: &str,
        body: &str,
        choices: &[&str],
    ) -> Result<Option<usize>> {
        Ok(self
            .select(
                title,
                body,
                &choices.iter().map(|s| (*s).into()).collect::<Vec<_>>(),
                false,
            )?
            .and_then(|v| v.first().copied()))
    }

    pub(crate) fn details(&self, title: &str, text: &str) -> Result<()> {
        let mut offset = 0_usize;
        loop {
            let (width, height) = terminal::size()?;
            let lines = wrap(text, width.saturating_sub(6).max(1) as usize);
            let page = height.saturating_sub(7).max(1) as usize;
            offset = offset.min(lines.len().saturating_sub(page));
            let body = lines
                .iter()
                .skip(offset)
                .take(page)
                .cloned()
                .collect::<Vec<_>>()
                .join("\n");
            self.paint(
                title,
                &body,
                &[],
                0,
                &[],
                false,
                "↑↓ scroll · PgUp/PgDn page · Enter/Esc back",
            )?;
            match event::read()? {
                Event::Key(key) if key.kind != KeyEventKind::Release => match key.code {
                    KeyCode::Enter | KeyCode::Esc => return Ok(()),
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        return Ok(());
                    }
                    KeyCode::Up => offset = offset.saturating_sub(1),
                    KeyCode::Down => offset += 1,
                    KeyCode::PageUp => offset = offset.saturating_sub(page),
                    KeyCode::PageDown => offset += page,
                    KeyCode::Home => offset = 0,
                    KeyCode::End => offset = lines.len(),
                    _ => {}
                },
                _ => {}
            }
        }
    }

    pub(crate) fn input(&self, title: &str, body: &str) -> Result<Option<String>> {
        let mut value = String::new();
        loop {
            self.paint(
                title,
                &format!("{body}\n\n> {value}▏"),
                &[],
                0,
                &[],
                false,
                "Enter continue · Esc cancel",
            )?;
            match event::read()? {
                Event::Key(key) if key.kind != KeyEventKind::Release => match key.code {
                    KeyCode::Esc => return Ok(None),
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        return Ok(None);
                    }
                    KeyCode::Enter if !value.trim().is_empty() && fits(terminal::size()?) => {
                        return Ok(Some(value.trim().into()));
                    }
                    KeyCode::Backspace => {
                        value.pop();
                    }
                    KeyCode::Char(c)
                        if !c.is_control()
                            && !key.modifiers.contains(KeyModifiers::CONTROL)
                            && value.len() < 1024 =>
                    {
                        value.push(c)
                    }
                    _ => {}
                },
                _ => {}
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn paint(
        &self,
        title: &str,
        body: &str,
        choices: &[String],
        focused: usize,
        selected: &[bool],
        multiple: bool,
        footer: &str,
    ) -> Result<()> {
        let (width, height) = terminal::size()?;
        let frame = render(
            width, height, title, body, choices, focused, selected, multiple, footer, self.color,
        )?;
        let mut out = io::stdout().lock();
        out.write_all(&frame)?;
        out.flush()?;
        Ok(())
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        if self.active {
            let _ = execute!(
                io::stdout(),
                ResetColor,
                SetAttribute(Attribute::Reset),
                DisableMouseCapture,
                Show,
                LeaveAlternateScreen
            );
            let _ = terminal::disable_raw_mode();
        }
    }
}

pub(crate) fn supported() -> bool {
    io::stdin().is_terminal()
        && io::stdout().is_terminal()
        && std::env::var("TERM").as_deref() != Ok("dumb")
}

fn fits((width, height): (u16, u16)) -> bool {
    width >= 60 && height >= 20
}

/// Provider names, host aliases and diagnostic payloads are never terminal programs.
fn clean(value: &str) -> String {
    value
        .chars()
        .filter(|c| {
            !c.is_control() && !matches!(*c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        })
        .collect()
}

fn clip(value: &str, width: usize) -> String {
    let mut used = 0;
    clean(value)
        .chars()
        .take_while(|c| {
            used += c.width().unwrap_or(0);
            used <= width
        })
        .collect()
}

fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut result = Vec::new();
    for line in text.lines() {
        let mut current = String::new();
        let mut used = 0;
        for c in clean(line).chars() {
            let n = c.width().unwrap_or(0);
            if used + n > width && !current.is_empty() {
                result.push(std::mem::take(&mut current));
                used = 0;
            }
            current.push(c);
            used += n;
        }
        result.push(current);
    }
    result
}

#[allow(clippy::too_many_arguments)]
fn render(
    width: u16,
    height: u16,
    title: &str,
    body: &str,
    choices: &[String],
    focused: usize,
    selected: &[bool],
    multiple: bool,
    footer: &str,
    color: bool,
) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    queue!(
        out,
        terminal::BeginSynchronizedUpdate,
        MoveTo(0, 0),
        Clear(ClearType::All)
    )?;
    let available = width.saturating_sub(4) as usize;
    let mut line = |row: u16, text: &str, role: Color, highlight: bool| -> Result<()> {
        queue!(out, MoveTo(2.min(width.saturating_sub(1)), row))?;
        if color {
            queue!(out, SetForegroundColor(role))?;
        }
        if row == 1 {
            queue!(out, SetAttribute(Attribute::Bold))?;
        }
        if highlight {
            queue!(out, SetAttribute(Attribute::Reverse))?;
        }
        queue!(
            out,
            Print(clip(text, available)),
            SetAttribute(Attribute::Reset)
        )?;
        if color {
            queue!(out, ResetColor)?;
        }
        Ok(())
    };
    if !fits((width, height)) {
        line(
            0,
            "PIKA · Enlarge to 60 × 20 to see setup",
            Color::Red,
            false,
        )?;
        if height > 1 {
            line(1, "Esc back", Color::Reset, false)?;
        }
    } else {
        line(1, "PIKA", Color::Red, false)?;
        line(3, title, Color::Cyan, false)?;
        let body_lines = wrap(body, available);
        let body_budget = if choices.is_empty() {
            height.saturating_sub(7)
        } else {
            (height / 2).saturating_sub(3).max(2)
        } as usize;
        let count = body_lines.len().min(body_budget);
        for (i, text) in body_lines.iter().take(count).enumerate() {
            line(5 + i as u16, text, Color::Reset, false)?;
        }
        let start = 6 + count as u16;
        let slots = height.saturating_sub(start + 3) as usize;
        let first = focused.saturating_sub(slots.saturating_sub(1));
        for (i, text) in choices.iter().enumerate().skip(first).take(slots) {
            let mark = if multiple {
                if selected[i] { "[x] " } else { "[ ] " }
            } else {
                ""
            };
            let arrow = if i == focused { "› " } else { "  " };
            line(
                start + (i - first) as u16,
                &format!("{arrow}{mark}{text}"),
                Color::Cyan,
                i == focused,
            )?;
        }
        line(height - 2, footer, Color::DarkGrey, false)?;
    }
    queue!(out, terminal::EndSynchronizedUpdate)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setup_frame_is_bounded_and_uses_board_palette() {
        let choices = (0..200).map(|i| format!("thread {i}")).collect::<Vec<_>>();
        let bytes = render(
            80,
            24,
            "Choose your work",
            "Only selected conversations appear on your board.",
            &choices,
            190,
            &[false; 200],
            true,
            "Space select · Enter continue · Esc skip",
            true,
        )
        .unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("thread 190"));
        assert!(!text.contains("thread 0"));
        assert!(text.contains("PIKA"));
        assert!(text.len() < 2500);
        assert!(text.contains("\x1b[38;5;9m"));
    }

    #[test]
    fn no_color_still_marks_selection_and_never_sets_background() {
        let text = String::from_utf8(
            render(
                80,
                24,
                "Your board",
                "Welcome",
                &["Open board".into()],
                0,
                &[false],
                false,
                "Esc back",
                false,
            )
            .unwrap(),
        )
        .unwrap();
        assert!(text.contains("› Open board"));
        assert!(!text.contains("38;"));
        assert!(!text.contains("48;"));
    }

    #[test]
    fn tiny_terminal_and_untrusted_labels_are_safe() {
        let text =
            String::from_utf8(render(35, 6, "x", "y", &[], 0, &[], false, "z", false).unwrap())
                .unwrap();
        assert!(text.contains("Enlarge"));
        assert!(text.contains("Esc back"));
        assert!(!clean("bad\x1b]52;payload\x07\u{202e}").contains('\x1b'));
        assert_eq!(clip("界界a", 4), "界界");
        assert_eq!(wrap("abcdef", 3), vec!["abc", "def"]);
    }
}
