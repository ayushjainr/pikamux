//! Display-only, Unicode-cell-aware layout for the bounded Files preview.
use crate::files_markdown::{Line, Span, Style};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

const MAX_ROWS: usize = 40_000;

#[derive(Debug)]
pub(crate) struct Row {
    pub spans: Vec<Span>,
    pub source_line: usize,
    pub continuation: bool,
    pub horizontal: bool,
}

pub(crate) fn source(text: &str) -> Vec<Line> {
    let mut input = text.lines();
    let mut lines: Vec<_> = input
        .by_ref()
        .take(MAX_ROWS)
        .enumerate()
        .map(|(i, text)| {
            let text = sanitize(text);
            let indent = text.chars().take_while(|c| *c == ' ').count();
            Line {
                spans: vec![Span {
                    text,
                    style: Style::default(),
                }],
                source_line: i + 1,
                literal: false,
                indent,
            }
        })
        .collect();
    if input.next().is_some() {
        lines.pop(); // Reserve the final bounded row for an honest limit notice.
        lines.push(Line {
            spans: vec![Span {
                text: "Preview line limit reached · remaining lines omitted".into(),
                style: Style::default(),
            }],
            source_line: MAX_ROWS,
            literal: true,
            indent: 0,
        });
    }
    lines
}

pub(crate) fn sanitize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if c == '\t' {
            out.push_str("    ");
        } else {
            out.push(if c.is_control() { '�' } else { c });
        }
    }
    out
}

pub(crate) fn layout(lines: &[Line], width: usize, wrap: bool) -> Vec<Row> {
    let width = width.max(1);
    let mut rows = Vec::new();
    for (line_index, line) in lines.iter().enumerate() {
        let mut incomplete_line = false;
        if !wrap || line.literal {
            rows.push(Row {
                spans: line.spans.clone(),
                source_line: line.source_line,
                continuation: false,
                horizontal: true,
            });
        } else {
            // Keep styles attached to graphemes; never split a combining sequence.
            let units: Vec<_> = line
                .spans
                .iter()
                .flat_map(|span| {
                    span.text
                        .graphemes(true)
                        .map(move |g| (g, span.style, g.width()))
                })
                .collect();
            let indent = line.indent.min(width / 3);
            let mut start = 0;
            let mut continuation = false;
            loop {
                let padding = if continuation { indent } else { 0 };
                let available = width - padding;
                let mut end = start;
                let mut used = 0;
                let mut boundary = None;
                while end < units.len() && used + units[end].2 <= available {
                    used += units[end].2;
                    end += 1;
                    if units[end - 1].0.chars().all(char::is_whitespace) {
                        boundary = Some(end);
                    }
                }
                if end < units.len() {
                    if let Some(word_end) = boundary.filter(|b| *b > start + padding) {
                        end = word_end;
                    }
                }
                let mut spans = Vec::new();
                if padding > 0 {
                    push(&mut spans, &" ".repeat(padding), Style::default());
                }
                if end == start && end < units.len() {
                    // A two-cell grapheme cannot fit a one-cell viewport.
                    push(&mut spans, "�", units[end].1);
                    end += 1;
                } else {
                    for (text, style, _) in &units[start..end] {
                        push(&mut spans, text, *style);
                    }
                }
                rows.push(Row {
                    spans,
                    source_line: line.source_line,
                    continuation,
                    horizontal: false,
                });
                if rows.len() >= MAX_ROWS || end >= units.len() {
                    incomplete_line = end < units.len();
                    break;
                }
                start = end;
                continuation = true;
            }
        }
        if rows.len() >= MAX_ROWS {
            if incomplete_line || line_index + 1 < lines.len() {
                rows.pop();
                rows.push(Row {
                    spans: vec![Span {
                        text: "Preview limit reached · remaining rows omitted".into(),
                        style: Style::default(),
                    }],
                    source_line: line.source_line,
                    continuation: true,
                    horizontal: true,
                });
            }
            break;
        }
    }
    rows
}

fn push(spans: &mut Vec<Span>, text: &str, style: Style) {
    if let Some(last) = spans.last_mut().filter(|s| s.style == style) {
        last.text.push_str(text);
    } else {
        spans.push(Span {
            text: text.into(),
            style,
        });
    }
}

/// Slice a row by terminal cells, retaining grapheme boundaries and style.
pub(crate) fn viewport(spans: &[Span], offset: usize, width: usize) -> Vec<Span> {
    let mut out = Vec::new();
    let mut skipped = 0;
    let mut used = 0;
    for span in spans {
        for g in span.text.graphemes(true) {
            let n = g.width();
            if skipped < offset {
                skipped += n;
                continue;
            }
            if used + n > width {
                // A neutral marker indicates more content, not a file mutation.
                if used < width {
                    push(&mut out, "…", Style::default());
                }
                return out;
            }
            push(&mut out, g, span.style);
            used += n;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    fn plain(row: &Row) -> String {
        row.spans.iter().map(|s| s.text.as_str()).collect()
    }
    #[test]
    fn wraps_words_retains_indentation_and_original_numbers() {
        let rows = layout(&source("    alpha beta gamma delta\nend"), 15, true);
        assert!(rows.len() > 2);
        assert_eq!(rows[0].source_line, 1);
        assert!(!rows[0].continuation);
        assert!(rows[1].continuation);
        assert!(plain(&rows[1]).starts_with("    "));
        assert_eq!(rows.last().unwrap().source_line, 2);
        assert!(rows.iter().all(|r| plain(r).width() <= 15));
    }
    #[test]
    fn unicode_long_words_and_controls_are_safe() {
        for width in 1..25 {
            let rows = layout(
                &source("👨‍👩‍👧‍👦文件e\u{301}abcdefghijk\x1b]52;X\x07"),
                width,
                true,
            );
            assert!(rows.iter().all(|r| plain(r).width() <= width));
            assert!(!rows.iter().any(|r| plain(r).contains('\x1b')));
        }
    }
    #[test]
    fn source_wrap_is_lossless_except_added_continuation_indent() {
        let input = "alpha beta gamma  delta superlongword文件👨‍👩‍👧‍👦";
        let rows = layout(&source(input), 12, true);
        assert_eq!(rows.iter().map(plain).collect::<String>(), input);
        let unwrapped = layout(&source(input), 12, false);
        assert_eq!(unwrapped.len(), 1);
        assert!(unwrapped[0].horizontal);
    }
    #[test]
    fn literal_blocks_never_reflow() {
        let mut lines = source("    a   b   c   d");
        lines[0].literal = true;
        assert_eq!(layout(&lines, 8, true).len(), 1);
        let slice = viewport(&lines[0].spans, 4, 5);
        assert!(
            slice
                .iter()
                .map(|s| s.text.as_str())
                .collect::<String>()
                .starts_with("a   b")
        );
    }

    #[test]
    fn layout_limit_is_hard_and_notice_only_when_content_is_omitted() {
        let exact = source(&"x\n".repeat(MAX_ROWS));
        assert_eq!(exact.len(), MAX_ROWS);
        let rows = layout(&exact, 20, true);
        assert_eq!(rows.len(), MAX_ROWS);
        assert_eq!(plain(rows.last().unwrap()), "x");
        let over = source(&"x\n".repeat(MAX_ROWS + 10));
        assert_eq!(over.len(), MAX_ROWS);
        assert!(over.last().unwrap().spans[0].text.contains("limit reached"));
        let wrapped = layout(&source(&"word ".repeat(MAX_ROWS)), 4, true);
        assert_eq!(wrapped.len(), MAX_ROWS);
        assert!(plain(wrapped.last().unwrap()).contains("limit reached"));
    }
}
