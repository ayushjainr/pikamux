//! Explicit user feedback, kept in one readable log outside assistant memory.
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{fs::OpenOptions, io::Write, os::unix::fs::OpenOptionsExt, path::Path};

const SKILL: &str = include_str!("../pika-skills/user-feedback/SKILL.md");
const FILENAME: &str = "user_feedback.md";

pub(crate) fn submit(root: &Path, scope: &str, note: Option<&str>, now: i64) -> Result<Value> {
    let path = root.join(FILENAME);
    let Some(note) = note.filter(|note| !note.trim().is_empty()) else {
        let instructions = SKILL
            .split_once("\n---\n")
            .map_or(SKILL, |(_, body)| body.trim());
        return Ok(json!({
            "local_output":format!("{instructions}\n\nFile: {}", path.display()),
            "notice":"Add a note with /feedback TEXT.",
            "feedback_path":path,
        }));
    };
    if note
        .chars()
        .any(|ch| ch.is_control() && !matches!(ch, '\n' | '\t'))
    {
        bail!("Feedback cannot contain terminal control characters.");
    }
    let date = time::OffsetDateTime::from_unix_timestamp(now)?;
    let quoted = note
        .split('\n')
        .map(|line| format!("> {line}\n"))
        .collect::<String>();
    let entry = format!("\n## {date}\n\nScope: {scope}\n\n{quoted}");
    crate::assistant_storage::directory(root)?;
    crate::assistant_storage::file(&path).context("Could not save feedback")?;
    let mut file = OpenOptions::new()
        .append(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
        .context("Could not open user_feedback.md")?;
    file.write_all(entry.as_bytes())
        .context("Could not write feedback")?;
    file.sync_all()
        .context("Could not finish saving feedback")?;
    Ok(json!({
        "feedback_saved":true,
        "feedback_path":path,
        "notice":"Feedback saved to user_feedback.md.",
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn help_does_not_write_and_notes_preserve_existing_feedback() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("profile");
        submit(&root, "pika", None, 0).unwrap();
        assert!(!root.exists());
        let note = "It forgot my preferred name.\n\nPlease remember King in the North 🐺.";
        submit(&root, "pika", Some(note), 1_790_611_200).unwrap();
        let path = root.join(FILENAME);
        let first = fs::read_to_string(&path).unwrap();
        let words = first
            .lines()
            .filter_map(|line| line.strip_prefix("> "))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(words, note);
        assert!(first.contains("2026-09-28"));
        assert!(first.contains("Scope: pika"));
        submit(
            &root,
            "research",
            Some("Make the reply easier to skim."),
            1_790_611_201,
        )
        .unwrap();
        let after = fs::read_to_string(path).unwrap();
        assert!(after.starts_with(&first));
        assert!(after.contains("Scope: research"));
        assert!(after.contains("Make the reply easier to skim."));
        assert_eq!(fs::read_dir(&root).unwrap().count(), 1);
    }

    #[test]
    fn failed_writes_and_control_text_never_report_saved_or_touch_a_link_target() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("profile");
        assert!(submit(&root, "pika", Some("note\u{1b}[2J"), 0).is_err());
        assert!(!root.exists());
        crate::assistant_storage::directory(&root).unwrap();
        let target = temp.path().join("unrelated.md");
        fs::write(&target, "leave this alone").unwrap();
        std::os::unix::fs::symlink(&target, root.join(FILENAME)).unwrap();
        assert!(submit(&root, "pika", Some("Feedback"), 0).is_err());
        assert_eq!(fs::read_to_string(target).unwrap(), "leave this alone");
    }
}
