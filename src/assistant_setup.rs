//! Human-facing setup and summaries; reuse the existing assistant owners.
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    process::Command,
};

pub(super) fn choose(
    root: &Path,
    snapshot: &Value,
    scope: &str,
) -> Result<Option<crate::assistant_startup::Selection>> {
    let ui = crate::onboarding::Screen::embedded();
    if snapshot["can_reconnect"] == true {
        return reconnect(&ui, root, snapshot, scope);
    }
    if !super::needs_connection(snapshot) {
        let choice = ui.choice(
            "Pika settings",
            &summary(snapshot),
            &["Back to conversation", "Technical details"],
        )?;
        if choice == Some(1) {
            ui.details("Technical details", &details(snapshot))?;
        }
        return Ok(None);
    }
    connect(&ui, root, snapshot, scope)
}

fn reconnect(
    ui: &crate::onboarding::Screen,
    root: &Path,
    snapshot: &Value,
    scope: &str,
) -> Result<Option<crate::assistant_startup::Selection>> {
    let choice = ui.choice("Reconnect Pika", "Codex couldn't start. Your draft and saved memories are still here.\n\nReconnecting won't send a message.", &["Reconnect", "Technical details", "Back to conversation"])?;
    if choice == Some(1) {
        ui.details("Technical details", &details(snapshot))?;
    }
    if choice != Some(0) {
        return Ok(None);
    }
    if let Some(saved) = crate::assistant_startup::load(&crate::assistant_startup::path()?)?
        && saved.profile_root == root
        && saved.scope == scope
        && snapshot["profile_id"] == saved.profile_id
        && saved.executable.is_file()
    {
        return Ok(Some(saved));
    }
    connect(ui, root, snapshot, scope)
}

fn connect(
    ui: &crate::onboarding::Screen,
    root: &Path,
    snapshot: &Value,
    scope: &str,
) -> Result<Option<crate::assistant_startup::Selection>> {
    let choice = ui.choice(
        "Pika settings",
        "Connect Pika to start talking. Your saved memories stay on this machine.",
        &[
            "Connect with Codex",
            "Connection details",
            "Back to conversation",
        ],
    )?;
    if choice == Some(1) {
        ui.details("Connection details", &details(snapshot))?;
        return Ok(None);
    }
    if choice != Some(0) {
        return Ok(None);
    }
    let executable = executable()?;
    if ui.choice("Connect Pika", "Pika uses Codex (Luna) to reply, using your account's quota. Messages and relevant Pika memories are sent to Codex.\n\nRemember this connection for future visits, with no artificial message cap. Background work and access to project conversations are not enabled by this step.", &["Connect and remember", "Not now"])? != Some(0) {
        return Ok(None);
    }
    if !ensure_sign_in(ui, &executable, &root.join("provider-home"))? {
        return Ok(None);
    }
    Ok(Some(crate::assistant_startup::Selection {
        profile_root: root.to_path_buf(),
        profile_id: snapshot["profile_id"]
            .as_str()
            .context("Pika's saved identity is unavailable")?
            .into(),
        scope: scope.into(),
        executable,
        max_calls: crate::assistant_policy::NO_CALL_LIMIT,
    }))
}

fn summary(snapshot: &Value) -> String {
    let connection = match snapshot["state"].as_str() {
        Some("ready") => "Connected to Codex · Luna",
        Some("working") => "Pika is replying",
        Some("starting" | "recovering") => "Connecting to Codex…",
        _ => "Connection needs attention. Your saved memories are still here.",
    };
    format!(
        "{connection}\n\nMemory · {}\nBoard access · {}\n{}",
        snapshot["scope"].as_str().unwrap_or("personal"),
        if snapshot["control"]["board_shared"] == true {
            "shared with Pika"
        } else {
            "not shared"
        },
        super::background_label(&snapshot["control"])
    )
}

fn ensure_sign_in(ui: &crate::onboarding::Screen, executable: &Path, home: &Path) -> Result<bool> {
    crate::assistant_storage::directory(home)?;
    let auth = home.join("auth.json");
    if auth.try_exists()? {
        crate::assistant_storage::existing_file(&auth)?;
    } else {
        if ui.choice("Sign in to Codex", "Pika needs its own sign-in. Follow Codex's sign-in instructions, then you'll return here. Your existing coding sessions won't change.", &["Sign in", "Not now"])? != Some(0) {
            return Ok(false);
        }
        sign_in(executable, home)?;
    }
    Ok(true)
}

fn executable() -> Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let config = crate::config::Config::load(&crate::paths::Paths::discover()?)?;
    let name = config.executable(crate::model::Provider::Codex);
    let candidate = Path::new(&name);
    let candidates: Vec<_> = if candidate.components().count() > 1 {
        vec![candidate.to_path_buf()]
    } else {
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .map(|dir| dir.join(&name))
            .collect()
    };
    candidates
        .into_iter()
        .find(|path| {
            path.metadata()
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
        .map(|path| path.canonicalize())
        .transpose()?
        .context("Codex isn't installed on this machine. Install Codex, then choose Connect again")
}

fn sign_in(executable: &Path, home: &Path) -> Result<()> {
    use crossterm::{
        cursor::{Hide, Show},
        event::{DisableMouseCapture, EnableMouseCapture},
        execute,
        terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
    };
    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            let _ = terminal::enable_raw_mode();
            let _ = execute!(
                std::io::stdout(),
                EnterAlternateScreen,
                EnableMouseCapture,
                Hide
            );
        }
    }
    let _restore = Restore;
    execute!(
        std::io::stdout(),
        DisableMouseCapture,
        Show,
        LeaveAlternateScreen
    )?;
    terminal::disable_raw_mode()?;
    let mut command = Command::new(executable);
    command.env_clear().args(["login", "--device-auth"]);
    for key in [
        "HOME",
        "PATH",
        "TERM",
        "LANG",
        "HTTPS_PROXY",
        "HTTP_PROXY",
        "NO_PROXY",
    ] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    let status = command
        .env("CODEX_HOME", home)
        .status()
        .context("Couldn't open Codex sign-in")?;
    if !status.success() {
        bail!("Sign-in didn't finish. Choose Connect to try again");
    }
    Ok(())
}

pub(super) fn details(snapshot: &Value) -> String {
    let mut text = format!(
        "Connection: {}\nScope: {}\n{}\nBoard access: {}\n\n{}",
        snapshot["state"].as_str().unwrap_or("not connected"),
        snapshot["scope"].as_str().unwrap_or("personal"),
        super::background_label(&snapshot["control"]),
        if snapshot["control"]["board_shared"] == true {
            "selected conversations"
        } else {
            "not shared with the assistant"
        },
        snapshot["error"].as_str().unwrap_or("")
    );
    for key in [
        "view_notice",
        "author",
        "method_control",
        "recovery",
        "investigation",
        "cleanup_notice",
    ] {
        if !snapshot[key].is_null() {
            text.push_str(&format!(
                "\n\n{key}: {}",
                serde_json::to_string_pretty(&snapshot[key]).unwrap_or_default()
            ));
        }
    }
    text
}

pub(super) fn brief_text(brief: &Value) -> String {
    let mut lines = Vec::new();
    for (key, label) in [
        ("updates", "Since your last visit"),
        ("decisions", "Decisions"),
        ("commitments", "To follow up"),
        ("uncertainty", "Open questions"),
        ("instructions", "Saved preferences and instructions"),
    ] {
        if let Some(rows) = brief[key].as_array().filter(|rows| !rows.is_empty()) {
            lines.push(label.to_owned());
            for row in rows {
                let sanitized =
                    crate::fleet::sanitize_terminal_lines(row["text"].as_str().unwrap_or(""));
                let text = if sanitized.trim().is_empty() {
                    "Saved note has no printable text; /brief for details."
                } else {
                    &sanitized
                };
                lines.push(format!(
                    "  {}",
                    crate::assistant_briefing::readable_decision(text)
                ));
            }
            lines.push(String::new());
        }
    }
    if !lines.is_empty() {
        lines.push("From your saved notes. /brief for full details.".into());
        if brief["presentation_limited"] == true {
            lines.push("Some notes are shortened in this view.".into());
        }
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn summary_preserves_every_update_before_a_page_can_be_acknowledged() {
        let updates: Vec<_> = (0..12)
            .map(|i| json!({"id":format!("id-{i}"),"text":format!("Update {i} end")}))
            .collect();
        let text = brief_text(&json!({"updates":updates,"presentation_limited":true}));
        for i in 0..12 {
            assert!(text.contains(&format!("Update {i} end")));
        }
        assert!(text.contains("shortened"));
        assert!(!text.contains("id-"));
        assert!(brief_text(&json!({"updates":[],"coverage":"engineering detail"})).is_empty());
    }

    #[test]
    fn summary_keeps_unprintable_notes_visible_without_terminal_commands() {
        let text = brief_text(&json!({"updates":[{"text":"\u{1b}]52;c;secret\u{7}"}]}));
        assert!(text.contains("no printable text"));
        assert!(!text.contains('\u{1b}'));
        assert!(!text.contains("secret"));
    }
}
