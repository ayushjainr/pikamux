//! Secret-free, bounded setup previews.

use crate::setup::FileChange;

const MAX_LINES: usize = 500;
const MAX_LINE_CHARS: usize = 240;
const SENSITIVE: &[&str] = &[
    "password",
    "secret",
    "token",
    "api_key",
    "authorization",
    "credential",
    "private_key",
];

pub fn sanitized_unified_diff(change: &FileChange) -> String {
    let before = change.before.lines().collect::<Vec<_>>();
    let after = change.after.lines().collect::<Vec<_>>();
    let path = safe_line(&change.path.display().to_string(), false);
    let mut output = format!("--- {path}\n+++ {path}\n");
    if before.len() > MAX_LINES || after.len() > MAX_LINES {
        let purpose = safe_line(
            change.notice.as_deref().unwrap_or("Pika integration"),
            false,
        );
        output.push_str(&format!(
            "@@ bounded preview @@\n-existing file · {} lines · contents withheld\n+{purpose} · {} lines\n",
            before.len(),
            after.len(),
        ));
        return output;
    }

    // The matrix is capped at 500×500. Unchanged settings never enter output.
    let mut lcs = vec![vec![0_u16; after.len() + 1]; before.len() + 1];
    for left in (0..before.len()).rev() {
        for right in (0..after.len()).rev() {
            lcs[left][right] = if before[left] == after[right] {
                lcs[left + 1][right + 1] + 1
            } else {
                lcs[left + 1][right].max(lcs[left][right + 1])
            };
        }
    }
    output.push_str("@@ changed lines only · unchanged content omitted @@\n");
    let (mut left, mut right) = (0, 0);
    while left < before.len() || right < after.len() {
        if left < before.len() && right < after.len() && before[left] == after[right] {
            left += 1;
            right += 1;
        } else if right < after.len()
            && (left == before.len() || lcs[left][right + 1] >= lcs[left + 1][right])
        {
            output.push('+');
            output.push_str(&safe_line(after[right], true));
            output.push('\n');
            right += 1;
        } else {
            output.push('-');
            output.push_str(&safe_line(before[left], true));
            output.push('\n');
            left += 1;
        }
    }
    output
}

fn safe_line(value: &str, redact_sensitive: bool) -> String {
    let lower = value.to_ascii_lowercase();
    if redact_sensitive && SENSITIVE.iter().any(|needle| lower.contains(needle)) {
        return "[redacted sensitive line]".into();
    }
    let mut safe = value
        .chars()
        .filter(|character| !character.is_control() || *character == '\t')
        .take(MAX_LINE_CHARS + 1)
        .collect::<String>();
    if safe.chars().count() > MAX_LINE_CHARS {
        safe = safe.chars().take(MAX_LINE_CHARS).collect();
        safe.push('…');
    }
    safe
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn hook_addition_is_visible_while_unchanged_secret_is_absent() {
        let change = FileChange {
            path: PathBuf::from("/tmp/settings.json"),
            before: "{\n  \"api_key\": \"super-secret\",\n  \"hooks\": []\n}\n".into(),
            after: "{\n  \"api_key\": \"super-secret\",\n  \"hooks\": [\n    \"pika hook codex SessionStart\"\n  ]\n}\n".into(),
            notice: Some("Codex lifecycle hooks".into()),
        };
        let preview = sanitized_unified_diff(&change);
        assert!(preview.starts_with("--- /tmp/settings.json\n+++ /tmp/settings.json\n@@"));
        assert!(preview.contains("+    \"pika hook codex SessionStart\""));
        assert!(!preview.contains("super-secret"));
        assert!(!preview.contains("api_key"));
    }

    #[test]
    fn changed_secret_lines_are_fully_redacted_and_controls_are_removed() {
        let change = FileChange {
            path: PathBuf::from("/tmp/settings.json"),
            before: "password=old\n".into(),
            after: "password=new\npika=true\u{1b}\n".into(),
            notice: None,
        };
        let preview = sanitized_unified_diff(&change);
        assert_eq!(preview.matches("[redacted sensitive line]").count(), 2);
        assert!(!preview.contains("password"));
        assert!(!preview.contains('\u{1b}'));
        assert!(preview.contains("+pika=true"));
    }
}
