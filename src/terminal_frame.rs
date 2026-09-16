//! Rasterize Pika's own drawing commands, not provider terminal output.
//! Unsupported commands fall back to the existing complete-frame presenter.
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Clone, Default, PartialEq)]
struct Cell {
    text: String,
    style: String,
    continuation: bool,
}

pub(crate) fn rows(frame: &[u8], (width, height): (u16, u16)) -> Option<Vec<Vec<u8>>> {
    let (width, height) = (usize::from(width), usize::from(height));
    if width == 0 || height == 0 || width.checked_mul(height)? > 200_000 {
        return None;
    }
    let mut cells = vec![vec![Cell::default(); width]; height];
    let (mut x, mut y) = (0, 0);
    let mut style = String::new();
    let mut input = std::str::from_utf8(frame).ok()?;
    while !input.is_empty() {
        if let Some(csi) = input.strip_prefix("\x1b[") {
            let end = csi.find(|ch: char| ('@'..='~').contains(&ch))?;
            let args = &csi[..end];
            match csi.as_bytes()[end] {
                b'H' => {
                    let (row, column) = args.split_once(';')?;
                    y = row.parse::<usize>().ok()?.checked_sub(1)?;
                    x = column.parse::<usize>().ok()?.checked_sub(1)?;
                }
                b'J' if args == "2" => cells.iter_mut().for_each(|row| row.fill(Cell::default())),
                b'm' if args.bytes().all(|ch| ch.is_ascii_digit() || ch == b';') => {
                    if args == "0" || args.is_empty() {
                        style.clear();
                    } else {
                        style.push_str(&input[..end + 3]);
                        if style.len() > 512 {
                            return None;
                        }
                    }
                }
                b'h' | b'l' if args == "?2026" => {}
                _ => return None,
            }
            input = &input[end + 3..];
            continue;
        }
        let ch = input.chars().next()?;
        match ch {
            '\r' => {
                x = 0;
                input = &input[1..];
            }
            '\n' => {
                y += 1;
                input = &input[1..];
            }
            ch if ch.is_control() => return None,
            _ => {
                let glyph = input.graphemes(true).next()?;
                input = &input[glyph.len()..];
                let span = glyph.width();
                let row = cells.get_mut(y)?;
                if span == 0 {
                    let column = (0..x.min(width)).rev().find(|&i| !row[i].continuation)?;
                    row[column].text.push_str(glyph);
                    continue;
                }
                if x + span > width {
                    return None;
                }
                // Replacing either half of a wide glyph must erase its other half.
                for column in x..x + span {
                    if row[column].continuation && column > 0 {
                        row[column - 1] = Cell::default();
                    }
                    if column + 1 < width && row[column + 1].continuation {
                        row[column + 1] = Cell::default();
                    }
                }
                row[x] = Cell {
                    text: glyph.to_owned(),
                    style: style.clone(),
                    continuation: false,
                };
                for cell in &mut row[x + 1..x + span] {
                    *cell = Cell {
                        continuation: true,
                        ..Cell::default()
                    };
                }
                x += span;
            }
        }
    }
    Some(
        cells
            .into_iter()
            .map(|row| {
                let mut bytes = Vec::new();
                let mut prior = String::new();
                bytes.extend_from_slice(b"\x1b[0m");
                for cell in row {
                    if cell.continuation {
                        continue;
                    }
                    if cell.style != prior {
                        bytes.extend_from_slice(b"\x1b[0m");
                        bytes.extend_from_slice(cell.style.as_bytes());
                        prior = cell.style;
                    }
                    bytes.extend_from_slice(if cell.text.is_empty() {
                        b" "
                    } else {
                        cell.text.as_bytes()
                    });
                }
                bytes.extend_from_slice(b"\x1b[0m");
                bytes
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_preserve_styles_unicode_overlays_and_erase_unused_cells() {
        let result = rows(
            "\x1b[2J\x1b[1;1H\x1b[31m界e\u{301}\x1b[0m\x1b[1;7HX".as_bytes(),
            (8, 2),
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(result[0].clone()).unwrap(),
            "\x1b[0m\x1b[0m\x1b[31m界e\u{301}\x1b[0m   X \x1b[0m"
        );
        assert_eq!(result[1], b"\x1b[0m        \x1b[0m");
        let overwrite = rows("界\x1b[1;2HX".as_bytes(), (3, 1)).unwrap();
        assert_eq!(overwrite[0], b"\x1b[0m X \x1b[0m");
        let emoji = rows("👩‍💻🇺🇸✈️".as_bytes(), (8, 1)).unwrap();
        assert_eq!(
            String::from_utf8(emoji[0].clone()).unwrap(),
            "\x1b[0m👩‍💻🇺🇸✈️  \x1b[0m"
        );
    }

    #[test]
    fn unsupported_programs_and_unbounded_frames_use_complete_frame_fallback() {
        for text in ["\x1b]title\x07", "\x1b[2A", "\t", "too wide"] {
            assert!(rows(text.as_bytes(), (3, 2)).is_none());
        }
        assert!(rows(b"", (u16::MAX, u16::MAX)).is_none());
    }
}
