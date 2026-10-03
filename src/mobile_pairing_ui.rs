//! QR rendering owns no terminal lifecycle. Its caller's Screen restores the
//! existing terminal, including an embedded board/setup alternate screen.
use anyhow::Result;
use crossterm::{
    cursor::MoveTo,
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    queue,
    style::{Print, ResetColor},
    terminal::{self, Clear, ClearType},
};
use std::{
    io::{self, Write},
    time::{Duration, Instant},
};

pub(super) fn paired() -> Result<()> {
    let mut output = io::stdout().lock();
    queue!(
        output,
        ResetColor,
        Clear(ClearType::All),
        MoveTo(0, 0),
        Print("Phone key paired. The phone will verify its SSH connection.")
    )?;
    output.flush()?;
    Ok(())
}

pub(super) fn draw(uri: &str, deadline: Instant) -> Result<()> {
    let qr = pairing_qr(uri)?;
    let width = qr.width() + 8;
    let rows = width.div_ceil(2);
    let (columns, height) = terminal::size()?;
    let mut output = io::stdout().lock();
    queue!(
        output,
        ResetColor,
        Clear(ClearType::All),
        MoveTo(0, 0),
        Print("Connect your phone")
    )?;
    if usize::from(columns) < width || usize::from(height) < rows + 4 {
        queue!(
            output,
            MoveTo(0, 2),
            Print(format!(
                "Enlarge this window to {width} columns and {} rows to show the QR.",
                rows + 4
            )),
            MoveTo(0, 4),
            Print("Make the window taller or reduce its text size. Esc goes back.")
        )?;
    } else {
        let left = ((usize::from(columns) - width) / 2) as u16;
        for y in 0..rows {
            queue!(output, MoveTo(left, (y + 2) as u16))?;
            for x in 0..width {
                // Solid cells use backgrounds, not full-block glyphs with font
                // bearings. Mixed cells draw a white half on black. Explicit
                // grayscale bypasses theme palettes and NO_COLOR: these pixels
                // encode data, rather than decorative interface color.
                let cell = match (dark(&qr, x, y * 2), dark(&qr, x, y * 2 + 1)) {
                    (false, false) => "\x1b[48;5;231m ",
                    (true, true) => "\x1b[48;5;16m ",
                    (true, false) => "\x1b[48;5;16m\x1b[38;5;231m▄",
                    (false, true) => "\x1b[48;5;16m\x1b[38;5;231m▀",
                };
                queue!(output, Print(cell))?;
            }
            queue!(output, ResetColor)?;
        }
        queue!(
            output,
            MoveTo(0, (rows + 3) as u16),
            Print(format!(
                "Scan in Pika on your phone · {}s remaining · Esc back",
                deadline.saturating_duration_since(Instant::now()).as_secs()
            ))
        )?;
    }
    output.flush()?;
    Ok(())
}

fn pairing_qr(uri: &str) -> Result<qrcode::QrCode> {
    Ok(qrcode::QrCode::with_error_correction_level(
        uri.as_bytes(),
        qrcode::EcLevel::H,
    )?)
}

fn dark(qr: &qrcode::QrCode, x: usize, y: usize) -> bool {
    x >= 4
        && y >= 4
        && x < qr.width() + 4
        && y < qr.width() + 4
        && qr[(x - 4, y - 4)] == qrcode::Color::Dark
}

pub(super) fn refresh(uri: &str, deadline: Instant, dimensions: &mut (u16, u16)) -> Result<()> {
    let current = terminal::size()?;
    if current != *dimensions {
        draw(uri, deadline)?;
        *dimensions = current;
        return Ok(());
    }
    countdown(uri, deadline)
}

fn countdown(uri: &str, deadline: Instant) -> Result<()> {
    let width = pairing_qr(uri)?.width() + 8;
    let rows = width.div_ceil(2);
    let (columns, height) = terminal::size()?;
    if usize::from(columns) < width || usize::from(height) < rows + 4 {
        return Ok(());
    }
    let mut output = io::stdout().lock();
    queue!(
        output,
        MoveTo(0, (rows + 3) as u16),
        Clear(ClearType::CurrentLine),
        Print(format!(
            "Scan in Pika on your phone · {}s remaining · Esc back",
            deadline.saturating_duration_since(Instant::now()).as_secs()
        ))
    )?;
    output.flush()?;
    Ok(())
}
pub(super) fn cancelled() -> Result<bool> {
    if !event::poll(Duration::ZERO)? {
        return Ok(false);
    }
    Ok(
        matches!(event::read()?,Event::Key(key) if key.kind!=KeyEventKind::Release && (key.code==KeyCode::Esc || key.code==KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))),
    )
}
