//! Small, conservative syntax colouring for the read-only Files view.
//!
//! This is intentionally a lexer rather than a parser.  It only colours tokens
//! when they are outside strings and comments, and leaves unknown languages
//! untouched.

use std::path::Path;

use crate::files_markdown::{Line, Span, Style, SyntaxInk};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Lang {
    Python,
    R,
    Shell,
    Rust,
    Javascript,
    Sql,
    Json,
    Toml,
    Yaml,
    PowerShell,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum State {
    Plain,
    BlockComment { powershell: bool },
    Quote { ch: u8, triple: bool },
    RawString { hashes: u8 },
}

fn language(path: &Path, first: Option<&str>) -> Option<Lang> {
    let ext = path
        .extension()
        .and_then(|x| x.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let l = match ext.as_str() {
        "py" | "pyw" => Lang::Python,
        "r" | "R" => Lang::R,
        "sh" | "bash" | "zsh" => Lang::Shell,
        "rs" => Lang::Rust,
        "js" | "ts" | "jsx" | "tsx" | "mjs" | "cjs" | "mts" | "cts" => Lang::Javascript,
        "sql" => Lang::Sql,
        "json" => Lang::Json,
        "toml" => Lang::Toml,
        "yaml" | "yml" => Lang::Yaml,
        "ps1" | "psm1" => Lang::PowerShell,
        _ => {
            if !ext.is_empty() {
                return None;
            }
            let s = first?.trim_start();
            if !s.starts_with("#!") {
                return None;
            }
            let interpreter = s[2..].split_whitespace().take(8).find_map(|w| {
                let name = w.rsplit('/').next().unwrap_or(w);
                (name != "env" && !name.starts_with('-') && !name.contains('=')).then_some(name)
            })?;
            if interpreter.starts_with("python") {
                Lang::Python
            } else if matches!(interpreter, "sh" | "bash" | "zsh" | "dash" | "ksh") {
                Lang::Shell
            } else if matches!(interpreter, "node" | "deno" | "bun") {
                Lang::Javascript
            } else if interpreter == "Rscript" {
                Lang::R
            } else if matches!(interpreter, "pwsh" | "powershell") {
                Lang::PowerShell
            } else {
                return None;
            }
        }
    };
    Some(l)
}

fn is_word(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'$'
}
fn ident_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_' || c == b'$'
}

fn keyword(lang: Lang, word: &str) -> bool {
    let set = match lang {
        Lang::Python => {
            "and as assert async await break case class continue def del elif else except finally for from global if import in is lambda match nonlocal not or pass raise return try while with yield True False None self print"
        }
        Lang::R => {
            "if else repeat while function for in next break TRUE FALSE NULL NA NaN Inf library require return"
        }
        Lang::Shell => {
            "if then else elif fi for while in do done case esac function select time coproc return export local readonly unset source alias true false"
        }
        Lang::Rust => {
            "as break const continue crate else enum extern false fn for if impl in let loop match mod move mut pub ref return self Self static struct super trait true type unsafe use where while async await dyn"
        }
        Lang::Javascript => {
            "as async await break case catch class const continue debugger default delete do else export extends false finally for from function get if import in instanceof let new null of return set static super switch this throw true try typeof var void while with yield interface type enum implements package private protected public declare abstract"
        }
        Lang::Sql => {
            "select from where and or not insert into values update set delete create alter drop table view index join inner left right full outer on as group by having order asc desc limit offset union all distinct null is like exists case when then else end begin commit rollback"
        }
        Lang::Json => "true false null",
        Lang::Toml => "true false",
        Lang::Yaml => "true false null yes no on off",
        Lang::PowerShell => {
            "function param begin process end if else elseif foreach for while do until switch break continue return class enum using namespace try catch finally throw trap filter in where"
        }
    };
    set.split_whitespace().any(|x| x == word)
}

fn type_word(lang: Lang, word: &str) -> bool {
    matches!(
        lang,
        Lang::Rust | Lang::Javascript | Lang::Python | Lang::PowerShell
    ) && (word.chars().next().is_some_and(char::is_uppercase)
        || matches!(
            word,
            "str"
                | "String"
                | "bool"
                | "int"
                | "float"
                | "usize"
                | "isize"
                | "i32"
                | "i64"
                | "u32"
                | "u64"
                | "number"
                | "boolean"
        ))
}

fn comment_start(lang: Lang, b: &[u8], i: usize) -> usize {
    if matches!(
        lang,
        Lang::Python | Lang::R | Lang::Shell | Lang::Toml | Lang::Yaml | Lang::PowerShell
    ) && b[i] == b'#'
        && (!matches!(lang, Lang::Shell | Lang::Yaml | Lang::PowerShell)
            || i == 0
            || b[i - 1].is_ascii_whitespace())
    {
        return 1;
    }
    if matches!(lang, Lang::Rust | Lang::Javascript) && b.get(i..i + 2) == Some(b"//") {
        return 2;
    }
    if lang == Lang::Sql && b.get(i..i + 2) == Some(b"--") {
        return 2;
    }
    0
}

fn push(out: &mut Vec<Span>, text: &str, mut style: Style, ink: SyntaxInk) {
    style.syntax = ink;
    if text.is_empty() {
        return;
    }
    if let Some(last) = out.last_mut() {
        if last.style == style {
            last.text.push_str(text);
            return;
        }
    }
    out.push(Span {
        text: text.to_owned(),
        style,
    });
}

fn flush_plain(text: &str, plain: &mut usize, end: usize, out: &mut Vec<Span>) {
    if end > *plain {
        push(out, &text[*plain..end], Style::default(), SyntaxInk::Plain);
    }
}

fn rust_character(text: &str) -> bool {
    let mut chars = text[1..].chars();
    match chars.next() {
        Some('\\') => match chars.next() {
            Some('u') => {
                if chars.next() != Some('{') {
                    return false;
                }
                if !chars.by_ref().take(8).any(|c| c == '}') {
                    return false;
                }
            }
            Some('x') => {
                if !chars.by_ref().take(2).all(|c| c.is_ascii_hexdigit()) {
                    return false;
                }
            }
            Some(_) => {}
            None => return false,
        },
        Some('\'') | None => return false,
        Some(_) => {}
    }
    chars.next() == Some('\'')
}

fn scan(text: &str, lang: Lang, mut state: State) -> (Vec<Span>, State) {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    let mut plain = 0;
    while i < b.len() {
        if let State::BlockComment { powershell } = state {
            let close = if powershell { "#>" } else { "*/" };
            let end = if let Some(n) = text[i..].find(close) {
                i + n + 2
            } else {
                b.len()
            };
            push(
                &mut out,
                &text[i..end],
                Style::default(),
                SyntaxInk::Comment,
            );
            i = end;
            if end >= 2 && &b[end - 2..end] == close.as_bytes() {
                state = State::Plain;
            }
            plain = i;
            continue;
        }
        if let State::RawString { hashes } = state {
            let mut end = b.len();
            let mut closed = false;
            let mut n = i;
            while n < b.len() {
                if b[n] == b'"'
                    && b.get(n + 1..n + 1 + hashes as usize)
                        .is_some_and(|xs| xs.iter().all(|&x| x == b'#'))
                {
                    end = n + 1 + hashes as usize;
                    closed = true;
                    break;
                }
                n += 1;
            }
            push(&mut out, &text[i..end], Style::default(), SyntaxInk::String);
            i = end;
            plain = i;
            if closed {
                state = State::Plain;
            }
            continue;
        }
        if let State::Quote { ch, triple } = state {
            let mut end = i;
            let mut closed = false;
            while end < b.len() {
                let escape = if lang == Lang::PowerShell {
                    b'`'
                } else {
                    b'\\'
                };
                let literal = lang == Lang::Sql
                    || (matches!(lang, Lang::Shell | Lang::Toml | Lang::PowerShell) && ch == b'\'');
                if !literal && b[end] == escape {
                    end += 1;
                    if end < b.len() {
                        end += text[end..].chars().next().map(char::len_utf8).unwrap_or(1);
                    }
                    continue;
                }
                if triple && b.get(end..end + 3) == Some(&[ch, ch, ch]) {
                    end += 3;
                    closed = true;
                    break;
                }
                if !triple && b[end] == ch {
                    end += 1;
                    closed = true;
                    break;
                }
                end += 1;
            }
            push(&mut out, &text[i..end], Style::default(), SyntaxInk::String);
            i = end;
            plain = i;
            if closed {
                state = State::Plain;
            }
            continue;
        }
        let c = b[i];
        let cs = comment_start(lang, b, i);
        if cs != 0 {
            flush_plain(text, &mut plain, i, &mut out);
            push(&mut out, &text[i..], Style::default(), SyntaxInk::Comment);
            return (out, State::Plain);
        }
        if ((lang == Lang::Rust || lang == Lang::Javascript || lang == Lang::Sql)
            && b.get(i..i + 2) == Some(b"/*"))
            || (lang == Lang::PowerShell && b.get(i..i + 2) == Some(b"<#"))
        {
            flush_plain(text, &mut plain, i, &mut out);
            push(
                &mut out,
                &text[i..i + 2],
                Style::default(),
                SyntaxInk::Comment,
            );
            i += 2;
            plain = i;
            state = State::BlockComment {
                powershell: lang == Lang::PowerShell,
            };
            continue;
        }
        if lang == Lang::Rust && (c == b'r' || b.get(i..i + 2) == Some(b"br")) {
            let start = i + usize::from(c == b'b');
            let mut n = start + 1;
            while n < b.len() && b[n] == b'#' && n - start <= 255 {
                n += 1;
            }
            if b.get(n) == Some(&b'"') {
                let hashes = (n - start - 1) as u8;
                flush_plain(text, &mut plain, i, &mut out);
                let end = n + 1;
                push(&mut out, &text[i..end], Style::default(), SyntaxInk::String);
                i = end;
                plain = i;
                state = State::RawString { hashes };
                continue;
            }
        }
        if c == b'"' || c == b'\'' || (c == b'`' && lang == Lang::Javascript) {
            let triple =
                (lang == Lang::Python || lang == Lang::Toml) && b.get(i..i + 3) == Some(&[c, c, c]);
            if lang == Lang::Rust && c == b'\'' && !rust_character(&text[i..]) {
                i += 1;
                continue;
            }
            flush_plain(text, &mut plain, i, &mut out);
            let n = if triple { 3 } else { 1 };
            push(
                &mut out,
                &text[i..i + n],
                Style::default(),
                SyntaxInk::String,
            );
            i += n;
            plain = i;
            state = State::Quote { ch: c, triple };
            continue;
        }
        if c.is_ascii_digit() {
            flush_plain(text, &mut plain, i, &mut out);
            let mut n = i + 1;
            while n < b.len() && (b[n].is_ascii_alphanumeric() || b[n] == b'.' || b[n] == b'_') {
                n += 1;
            }
            push(&mut out, &text[i..n], Style::default(), SyntaxInk::Number);
            i = n;
            plain = i;
            continue;
        }
        if ident_start(c) {
            let mut n = i + 1;
            while n < b.len() && is_word(b[n]) {
                n += 1;
            }
            let w = &text[i..n];
            let kw = if matches!(lang, Lang::Sql | Lang::PowerShell) {
                keyword(lang, &w.to_ascii_lowercase())
            } else {
                keyword(lang, w)
            };
            let mut ink = if kw {
                SyntaxInk::Keyword
            } else if type_word(lang, w) {
                SyntaxInk::Type
            } else {
                SyntaxInk::Plain
            };
            if (lang == Lang::Shell || lang == Lang::PowerShell) && c == b'$' {
                ink = SyntaxInk::Variable;
            }
            let fn_ = b
                .get(n..)
                .is_some_and(|rest| rest.iter().find(|c| !c.is_ascii_whitespace()) == Some(&b'('));
            let ink = if fn_ && ink == SyntaxInk::Plain {
                SyntaxInk::Function
            } else {
                ink
            };
            flush_plain(text, &mut plain, i, &mut out);
            push(&mut out, w, Style::default(), ink);
            i = n;
            plain = i;
            continue;
        }
        let width = text[i..].chars().next().map(char::len_utf8).unwrap_or(1);
        i += width;
    }
    flush_plain(text, &mut plain, b.len(), &mut out);
    (out, state)
}

pub(crate) fn highlight(lines: &mut [Line], path: &Path) {
    let first = lines
        .first()
        .and_then(|l| l.spans.first())
        .map(|s| s.text.as_str());
    let Some(lang) = language(path, first) else {
        return;
    };
    let mut state = State::Plain;
    for line in lines {
        let mut spans = Vec::new();
        for span in std::mem::take(&mut line.spans) {
            let (mut colored, next) = scan(&span.text, lang, state);
            for x in &mut colored {
                x.style.bold |= span.style.bold;
                x.style.italic |= span.style.italic;
                x.style.code |= span.style.code;
                x.style.quote |= span.style.quote;
                x.style.heading |= span.style.heading;
                x.style.link |= span.style.link;
            }
            spans.extend(colored);
            state = next;
        }
        line.spans = spans;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::files_layout;

    fn colored(text: &str, path: &str) -> Vec<Line> {
        let mut lines = files_layout::source(text);
        let original: Vec<_> = lines.iter().map(plain).collect();
        highlight(&mut lines, Path::new(path));
        assert_eq!(lines.iter().map(plain).collect::<Vec<_>>(), original);
        lines
    }
    fn plain(line: &Line) -> String {
        line.spans.iter().map(|s| s.text.as_str()).collect()
    }
    fn has(line: &Line, text: &str, ink: SyntaxInk) -> bool {
        line.spans
            .iter()
            .any(|s| s.text.contains(text) && s.style.syntax == ink)
    }

    #[test]
    fn languages_color_tokens_without_changing_the_text() {
        for (path, text, keyword) in [
            ("a.py", "def answer(): return \"hello\" # note", "def"),
            ("a.R", "f <- function(x) TRUE# note", "function"),
            ("a.sh", "if true; then echo \"hello\"; fi", "if"),
            ("a.rs", "fn answer() -> i32 { 42 } // comment", "fn"),
            ("a.ts", "const message = `hello`; run(message);", "const"),
            (
                "a.sql",
                "SELECT 'hello' FROM table_name; -- comment",
                "SELECT",
            ),
            ("a.json", "{\"active\": true, \"count\": 42}", "true"),
            ("a.toml", "active = true # setting", "true"),
            ("a.yaml", "active: true # setting", "true"),
            ("a.ps1", "FUNCTION demo { return $count }", "FUNCTION"),
        ] {
            let lines = colored(text, path);
            assert!(has(&lines[0], keyword, SyntaxInk::Keyword), "{path}");
        }
    }

    #[test]
    fn strings_comments_and_multiline_state_do_not_leak() {
        let py = colored(
            "value = \"\"\"for\nreturn\n\"\"\"\ndef f(): pass#done",
            "a.py",
        );
        assert!(has(&py[1], "return", SyntaxInk::String));
        assert!(has(&py[3], "def", SyntaxInk::Keyword));
        assert!(has(&py[3], "#done", SyntaxInk::Comment));
        let js = colored(
            "/* return\nconst */ let x = `hi\nhello`;\nconst z = 3",
            "a.js",
        );
        assert!(has(&js[1], "const", SyntaxInk::Comment));
        assert!(has(&js[1], "let", SyntaxInk::Keyword));
        assert!(has(&js[2], "hello", SyntaxInk::String));
        assert!(has(&js[3], "const", SyntaxInk::Keyword));
        let escaped = colored("x = \"escaped \\\"\nstill string\"\nreturn 1", "a.py");
        assert!(has(&escaped[1], "still string", SyntaxInk::String));
        assert!(has(&escaped[2], "return", SyntaxInk::Keyword));
    }

    #[test]
    fn rust_raw_strings_characters_and_lifetimes() {
        let lines = colored(
            "let x = r#\"fn # text\"#\nlet y = br##\"raw\"##\nfn f<'a>(s: &'a str) { let c = '界'; let n = '\\n'; }",
            "a.rs",
        );
        assert!(has(&lines[0], "fn # text", SyntaxInk::String));
        assert!(has(&lines[1], "let", SyntaxInk::Keyword));
        assert!(has(&lines[2], "fn", SyntaxInk::Keyword));
        assert!(has(&lines[2], "'界'", SyntaxInk::String));
        assert!(has(&lines[2], "'\\n'", SyntaxInk::String));
        assert!(
            !lines[2]
                .spans
                .iter()
                .any(|s| s.style.syntax == SyntaxInk::String && s.text.contains("'a"))
        );
    }

    #[test]
    fn shell_parameters_yaml_hashes_and_powershell_escapes() {
        let sh = colored(
            "echo ${#items[@]} $# foo#bar # actual comment\nif true; then echo 'backslash\\'; fi",
            "a.sh",
        );
        assert!(has(&sh[0], "# actual comment", SyntaxInk::Comment));
        assert!(
            !sh[0]
                .spans
                .iter()
                .any(|s| s.style.syntax == SyntaxInk::Comment && s.text.contains("items"))
        );
        assert!(has(&sh[1], "fi", SyntaxInk::Keyword));
        let yaml = colored("url: foo#bar # comment", "a.yml");
        assert!(has(&yaml[0], "# comment", SyntaxInk::Comment));
        assert!(
            !yaml[0]
                .spans
                .iter()
                .any(|s| s.style.syntax == SyntaxInk::Comment && s.text.contains("foo"))
        );
        let ps = colored(
            "<# function\nreturn #>\n$path = \"C:\\folder\\\"\nRETURN $path",
            "a.ps1",
        );
        assert!(has(&ps[1], "return", SyntaxInk::Comment));
        assert!(has(&ps[3], "RETURN", SyntaxInk::Keyword));
        assert!(has(&ps[3], "$path", SyntaxInk::Variable));
    }

    #[test]
    fn shebang_is_a_hint_only_for_extensionless_scripts() {
        for first in [
            "#!/bin/sh",
            "#!/usr/bin/env sh",
            "#!/usr/bin/env -S bash -e",
        ] {
            let lines = colored(&format!("{first}\nif true; then :; fi"), "run");
            assert!(has(&lines[1], "if", SyntaxInk::Keyword));
        }
        for path in ["unknown.txt", "README.md", "no-extension"] {
            let text = if path == "no-extension" {
                "#!/usr/bin/ruby\ndef f; end"
            } else {
                "#!/bin/sh\nif true; then :; fi"
            };
            let lines = colored(text, path);
            assert!(
                lines
                    .iter()
                    .flat_map(|l| &l.spans)
                    .all(|s| s.style.syntax == SyntaxInk::Plain)
            );
        }
    }

    #[test]
    fn unicode_and_control_bytes_remain_inert_and_exact() {
        for path in ["a.py", "a.rs", "a.js", "a.ps1"] {
            let lines = colored("\"\\界 e\u{301} 👨‍👩‍👧‍👦\"\n// \x1b]52;c;payload\x07\n42", path);
            assert!(
                !lines
                    .iter()
                    .flat_map(|l| &l.spans)
                    .any(|s| s.text.chars().any(char::is_control))
            );
        }
    }
}
