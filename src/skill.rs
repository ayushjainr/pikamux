use crate::{paths::Paths, setup::FileChange};
use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};
use uuid::Uuid;

pub const AGENT_CONVO_SKILL: &str = include_str!("../assets/agent-convo/SKILL.md");

#[derive(Clone, Debug, Serialize)]
pub struct SkillInstallReceipt {
    pub path: PathBuf,
    pub changed: bool,
    pub backup: Option<PathBuf>,
}

pub fn default_target(paths: &Paths) -> PathBuf {
    paths.codex_home.join("skills/agent-convo")
}

/// Return the bundled skill as a previewable setup change. The destination is
/// not created until the full setup batch is applied.
pub fn proposed_change(paths: &Paths) -> Result<FileChange> {
    let path = default_target(paths).join("SKILL.md");
    let before = match fs::read_to_string(&path) {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => {
            return Err(error).with_context(|| format!("cannot read {}", path.display()));
        }
    };
    Ok(FileChange {
        path,
        before,
        after: AGENT_CONVO_SKILL.to_owned(),
        notice: Some("bundled agent-to-agent consultation skill".into()),
    })
}

pub fn install(target: &Path) -> Result<SkillInstallReceipt> {
    if target.as_os_str().is_empty() {
        bail!("skill destination cannot be empty");
    }
    fs::create_dir_all(target)
        .with_context(|| format!("cannot create skill directory {}", target.display()))?;
    let destination = target.join("SKILL.md");
    if fs::read_to_string(&destination).ok().as_deref() == Some(AGENT_CONVO_SKILL) {
        return Ok(SkillInstallReceipt {
            path: destination,
            changed: false,
            backup: None,
        });
    }
    let backup = if destination.exists() {
        let path = target.join(format!("SKILL-{}.pika-backup", Uuid::new_v4()));
        fs::copy(&destination, &path)
            .with_context(|| format!("cannot back up existing skill {}", destination.display()))?;
        Some(path)
    } else {
        None
    };
    let temporary = target.join(format!(".pika-skill-{}.tmp", Uuid::new_v4()));
    let mut options = fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    file.write_all(AGENT_CONVO_SKILL.as_bytes())?;
    file.sync_all()?;
    fs::rename(&temporary, &destination)?;
    Ok(SkillInstallReceipt {
        path: destination,
        changed: true,
        backup,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_is_idempotent_and_backs_up_other_content() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("agent-convo")).unwrap();
        fs::write(root.path().join("agent-convo/SKILL.md"), "old").unwrap();
        let first = install(&root.path().join("agent-convo")).unwrap();
        assert!(first.changed);
        assert!(first.backup.unwrap().is_file());
        let second = install(&root.path().join("agent-convo")).unwrap();
        assert!(!second.changed);
    }

    #[test]
    fn setup_change_is_previewable_without_creating_the_destination() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths {
            config_dir: root.path().join("config/pika"),
            state_dir: root.path().join("state/pika"),
            config: root.path().join("config/pika/config.json"),
            database: root.path().join("state/pika/pika.db"),
            codex_home: root.path().join("codex"),
            claude_home: root.path().join("claude"),
            opencode_data_home: root.path().join("opencode-data"),
            opencode_config_home: root.path().join("opencode-config"),
        };
        let change = proposed_change(&paths).unwrap();
        assert!(change.changed());
        assert_eq!(change.after, AGENT_CONVO_SKILL);
        assert!(!change.path.exists());
    }
}
