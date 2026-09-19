//! A small, deliberately conservative Markdown projection for the Files pane.
//!
//! This is presentation parsing, not a CommonMark implementation: anything we
//! do not understand is retained as text and no resulting text is executable.

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) enum SyntaxInk {
    #[default]
    Plain,
    Keyword,
    String,
    Comment,
    Number,
    Function,
    Type,
    Variable,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct Style {
    pub bold: bool,
    pub italic: bool,
    pub code: bool,
    pub quote: bool,
    pub heading: bool,
    pub link: bool,
    pub syntax: SyntaxInk,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Span {
    pub text: String,
    pub style: Style,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Line {
    pub spans: Vec<Span>,
    pub source_line: usize,
    pub literal: bool,
    pub indent: usize,
}

const MAX_LINE: usize = 256 * 1024;

fn clean(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut col = 0usize;
    for c in input.chars() {
        match c {
            '\t' => {
                let n = 4 - (col % 4);
                out.extend(std::iter::repeat_n(' ', n));
                col += n;
            }
            c if !c.is_control() => {
                out.push(c);
                col += UnicodeWidthChar::width(c).unwrap_or(0);
            }
            _ => {
                out.push('\u{fffd}');
                col += 1;
            }
        }
    }
    out
}

fn push_span(spans: &mut Vec<Span>, text: String, style: Style) {
    if text.is_empty() {
        return;
    }
    if let Some(last) = spans.last_mut() {
        if last.style == style {
            last.text.push_str(&text);
            return;
        }
    }
    spans.push(Span { text, style });
}

fn inline(text: &str, base: Style) -> Vec<Span> {
    let s = clean(text);
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut plain = String::new();
    let flush = |out: &mut Vec<Span>, plain: &mut String| {
        if !plain.is_empty() {
            push_span(out, std::mem::take(plain), base);
        }
    };
    let mut i = 0;
    while i < b.len() {
        // Keep look-ahead bounded: malformed markup must not turn a long
        // source line into repeated whole-line scans.
        let mut look_end = (i + 4096).min(s.len());
        while look_end > i && !s.is_char_boundary(look_end) {
            look_end -= 1;
        }
        // Code spans are intentionally displayed, never interpreted.
        if b[i] == b'`' {
            if let Some(end) = s[i + 1..look_end].find('`') {
                flush(&mut out, &mut plain);
                let mut st = base;
                st.code = true;
                push_span(&mut out, s[i + 1..i + 1 + end].to_owned(), st);
                i += end + 2;
                continue;
            }
        }
        // Links and images become readable labels, with their destinations inert.
        let image = b[i] == b'!' && i + 1 < b.len() && b[i + 1] == b'[';
        if b[i] == b'[' || image {
            let open = if image { i + 1 } else { i };
            if let Some(close_rel) = s[open + 1..look_end].find(']') {
                let close = open + 1 + close_rel;
                if close + 2 <= look_end && close + 1 < b.len() && b[close + 1] == b'(' {
                    if let Some(paren_rel) = s[close + 2..look_end].find(')') {
                        flush(&mut out, &mut plain);
                        let mut st = base;
                        st.link = true;
                        let label = if image {
                            format!("Image: {}", &s[open + 1..close])
                        } else {
                            s[open + 1..close].to_owned()
                        };
                        push_span(&mut out, label, st);
                        // Keep the destination as an inert, readable path;
                        // callers must never turn this into a terminal link.
                        push_span(
                            &mut out,
                            format!(" ({})", &s[close + 2..close + 2 + paren_rel]),
                            st,
                        );
                        i = close + 2 + paren_rel + 1;
                        continue;
                    }
                }
            }
        }
        let marker = if i + 1 < b.len()
            && ((b[i] == b'*' && b[i + 1] == b'*') || (b[i] == b'_' && b[i + 1] == b'_'))
        {
            2
        } else if b[i] == b'*' || b[i] == b'_' {
            1
        } else {
            0
        };
        // Intraword underscores are identifiers, not emphasis delimiters.
        let intraword = b[i] == b'_'
            && s[..i]
                .chars()
                .next_back()
                .is_some_and(char::is_alphanumeric);
        if marker != 0 && !intraword {
            let ch = b[i];
            if let Some(end_rel) = s[i + marker..look_end].find(if marker == 2 {
                if ch == b'*' { "**" } else { "__" }
            } else if ch == b'*' {
                "*"
            } else {
                "_"
            }) {
                flush(&mut out, &mut plain);
                let mut st = base;
                if marker == 2 {
                    st.bold = true;
                } else {
                    st.italic = true;
                }
                push_span(&mut out, s[i + marker..i + marker + end_rel].to_owned(), st);
                i += marker + end_rel + marker;
                continue;
            }
        }
        let c = s[i..].chars().next().unwrap();
        plain.push(c);
        i += c.len_utf8();
    }
    flush(&mut out, &mut plain);
    out
}

fn is_table_row(s: &str) -> bool {
    s.contains('|') && s.trim().len() >= 3
}
fn table_cells(s: &str) -> Vec<String> {
    let t = s.trim();
    let t = t.strip_prefix('|').unwrap_or(t);
    let t = t.strip_suffix('|').unwrap_or(t);
    t.split('|').map(|x| clean(x.trim())).collect()
}
fn separator_cell(s: &str) -> bool {
    let t = s.trim();
    t.len() >= 3 && t.chars().all(|c| c == '-' || c == ':' || c == ' ')
}

fn table_lines(raw: &[&str], start: usize) -> (Vec<Line>, usize) {
    if start + 1 >= raw.len() || !is_table_row(raw[start + 1]) {
        return (Vec::new(), start);
    }
    let first = table_cells(raw[start]);
    let second = table_cells(raw[start + 1]);
    if first.is_empty()
        || first.len() > 32
        || second.len() != first.len()
        || !second.iter().all(|c| separator_cell(c))
    {
        return (Vec::new(), start);
    }
    let mut rows = vec![first];
    let mut i = start + 1;
    let mut widths = vec![0; rows[0].len()];
    rows.push(second);
    i += 1;
    while i < raw.len() && is_table_row(raw[i]) && rows.len() < 4096 {
        let cells = table_cells(raw[i]);
        if cells.len() != widths.len() {
            break;
        }
        rows.push(cells);
        i += 1;
    }
    for row in &rows {
        for (n, c) in row.iter().enumerate() {
            widths[n] = widths[n].max(UnicodeWidthStr::width(c.as_str()));
        }
    }
    if widths.iter().sum::<usize>() + widths.len().saturating_sub(1) * 3 > 512 {
        // Consume the entire inspected block once: rescanning every candidate
        // separator in an oversized table would be quadratic.
        return (
            raw[start..i]
                .iter()
                .enumerate()
                .map(|(n, line)| Line {
                    spans: vec![Span {
                        text: clean(line),
                        style: Style::default(),
                    }],
                    source_line: start + n + 1,
                    literal: true,
                    indent: 0,
                })
                .collect(),
            i,
        );
    }
    let mut result = Vec::new();
    for (row_no, cells) in rows.into_iter().enumerate() {
        let mut text = String::new();
        for (n, width) in widths.iter().enumerate() {
            if n > 0 {
                text.push_str(" | ");
            }
            let cell = cells.get(n).map(String::as_str).unwrap_or("");
            if row_no == 1 {
                text.push_str(&"-".repeat((*width).max(3)));
            } else {
                text.push_str(cell);
                text.extend(std::iter::repeat_n(
                    ' ',
                    width.saturating_sub(UnicodeWidthStr::width(cell)),
                ));
            }
        }
        result.push(Line {
            spans: vec![Span {
                text,
                style: Style {
                    bold: row_no == 0,
                    ..Style::default()
                },
            }],
            source_line: start + row_no + 1,
            literal: true,
            indent: 0,
        });
    }
    (result, i)
}

pub(crate) fn parse(text: &str) -> Vec<Line> {
    let mut raw: Vec<&str> = text
        .lines()
        .take(4097)
        .map(|l| {
            if l.len() <= MAX_LINE {
                return l;
            }
            let mut end = MAX_LINE;
            while end > 0 && !l.is_char_boundary(end) {
                end -= 1;
            }
            &l[..end]
        })
        .collect();
    let truncated = raw.len() > 4096;
    if truncated {
        raw.truncate(4096);
    }
    let mut out = Vec::new();
    let mut i = 0;
    let mut fence: Option<(u8, usize)> = None;
    while i < raw.len() {
        let source_line = i + 1;
        let line = raw[i];
        let ts = line.trim_start();
        let mark = ts
            .as_bytes()
            .first()
            .copied()
            .filter(|c| *c == b'`' || *c == b'~');
        if let Some(ch) = mark {
            let n = ts.bytes().take_while(|c| *c == ch).count();
            if let Some((fc, fn_)) = fence {
                if ch == fc && n >= fn_ && ts[n..].trim().is_empty() {
                    fence = None;
                    i += 1;
                    continue;
                }
            } else if n >= 3 {
                fence = Some((ch, n));
                let lang = ts[n..].trim();
                let st = Style {
                    code: true,
                    ..Style::default()
                };
                out.push(Line {
                    spans: vec![Span {
                        text: if lang.is_empty() {
                            "Code".into()
                        } else {
                            clean(lang)
                        },
                        style: st,
                    }],
                    source_line,
                    literal: true,
                    indent: 0,
                });
                i += 1;
                continue;
            }
        }
        if fence.is_some() {
            let st = Style {
                code: true,
                ..Style::default()
            };
            out.push(Line {
                spans: vec![Span {
                    text: clean(line),
                    style: st,
                }],
                source_line,
                literal: true,
                indent: 0,
            });
            i += 1;
            continue;
        }
        if is_table_row(line) {
            let (lines, next) = table_lines(&raw, i);
            if next > i {
                out.extend(lines);
                i = next;
                continue;
            }
        }
        let cleaned = clean(line);
        let trimmed = cleaned.trim_start();
        let leading_bytes = cleaned.len() - trimmed.len();
        let leading = &cleaned[..leading_bytes];
        let mut style = Style::default();
        let mut body = trimmed;
        if body.starts_with('>') {
            style.quote = true;
            body = body[1..].trim_start();
        }
        let mut prefix = leading.to_owned();
        if style.quote {
            prefix.push_str("│ ");
        }
        if body.starts_with('#') {
            let n = body.bytes().take_while(|&c| c == b'#').count();
            if n <= 6 && body.as_bytes().get(n) == Some(&b' ') {
                style.heading = true;
                body = body[n..].trim_start();
            }
        }
        if body.starts_with("- ") || body.starts_with("* ") || body.starts_with("+ ") {
            prefix.push_str(&body[..2]);
            body = &body[2..];
        } else if body.as_bytes().first().is_some_and(|c| c.is_ascii_digit()) {
            let n = body.bytes().take_while(|c| c.is_ascii_digit()).count();
            if body.as_bytes().get(n) == Some(&b'.') && body.as_bytes().get(n + 1) == Some(&b' ') {
                prefix.push_str(&body[..n + 2]);
                body = &body[n + 2..];
            }
        }
        let indent = prefix.width();
        let mut spans = inline(body, style);
        if !prefix.is_empty() {
            let mut p = vec![Span {
                text: prefix,
                style,
            }];
            p.append(&mut spans);
            spans = p;
        }
        out.push(Line {
            spans,
            source_line,
            literal: false,
            indent,
        });
        i += 1;
    }
    if truncated {
        out.push(Line {
            spans: vec![Span {
                text: "[output truncated]".into(),
                style: Style::default(),
            }],
            source_line: raw.len() + 1,
            literal: true,
            indent: 0,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mixed_blocks() {
        let x = parse("# Hi\n> **bold** and `code` [go](x)\n- item\n```rust\na\u{1b}[31mx\n```");
        assert!(x[0].spans[0].style.heading);
        assert!(x[1].spans.iter().any(|s| s.style.bold));
        assert!(x[4].literal && x[4].spans[0].style.code);
    }
    #[test]
    fn unicode_table() {
        let x = parse("| 名 | ok |\n| --- | --- |\n| 界 | yes |");
        assert!(x[0].literal);
        assert_eq!(
            UnicodeWidthStr::width(x[0].spans[0].text.as_str()),
            UnicodeWidthStr::width(x[2].spans[0].text.as_str())
        );
    }
    #[test]
    fn hostile_controls_are_inert() {
        let x = parse("a\u{1b}[2J\t b [x](javascript:alert(1))\u{009b}2J");
        let s: String = x
            .iter()
            .flat_map(|l| &l.spans)
            .map(|s| s.text.as_str())
            .collect();
        assert!(!s.chars().any(char::is_control));
        assert!(s.contains("javascript:"), "destination remains inert text");
    }
    #[test]
    fn long_input_is_bounded() {
        let x = parse(&"x\n".repeat(20_000));
        assert_eq!(x.len(), 4097);
        assert_eq!(x.last().unwrap().spans[0].text, "[output truncated]");
    }

    fn plain(line: &Line) -> String {
        line.spans.iter().map(|s| s.text.as_str()).collect()
    }

    #[test]
    fn preserves_lists_quotes_images_and_identifiers() {
        let x = parse(
            "- top\n  * child\n12. numbered\n> quote\n![Diagram](assets/image.png)\nfoo_bar_baz",
        );
        assert_eq!(plain(&x[0]), "- top");
        assert_eq!(plain(&x[1]), "  * child");
        assert_eq!(x[1].indent, 4);
        assert_eq!(plain(&x[2]), "12. numbered");
        assert_eq!(plain(&x[3]), "│ quote");
        assert_eq!(plain(&x[4]), "Image: Diagram (assets/image.png)");
        assert_eq!(plain(&x[5]), "foo_bar_baz");
    }

    #[test]
    fn code_fences_match_type_length_and_literal_content() {
        let x = parse("````rust\n  **literal**\n```\n~~~\n````not a close\n````\n# Heading");
        assert!(x[..5].iter().all(|l| l.literal));
        assert_eq!(plain(&x[1]), "  **literal**");
        assert_eq!(plain(&x[2]), "```");
        assert!(x.last().unwrap().spans[0].style.heading);
    }

    #[test]
    fn lookahead_at_multibyte_boundaries_never_panics() {
        for padding in 4084..4100 {
            let s = format!(
                "[{}](path) **{}** `{} `",
                "界".repeat(padding),
                "a".repeat(padding),
                "👨‍👩‍👧‍👦".repeat(300)
            );
            let x = parse(&s);
            assert!(!x.is_empty());
        }
    }

    #[test]
    fn wide_tables_fall_back_without_padding_amplification() {
        let s = format!(
            "| {} |\n| --- |\n{}",
            "x".repeat(1000),
            "| --- |\n".repeat(1000)
        );
        let x = parse(&s);
        let bytes: usize = x.iter().map(|l| plain(l).len()).sum();
        assert!(bytes <= s.len());
        assert!(plain(&x[0]).contains(&"x".repeat(1000)));
        let invalid = "not | a | table\n".repeat(4000);
        assert_eq!(parse(&invalid).len(), 4000);
    }
}
