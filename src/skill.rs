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
    let destination = target.join("SKILL.md");
    reject_symlink_components(&destination)?;
    fs::create_dir_all(target)
        .with_context(|| format!("cannot create skill directory {}", target.display()))?;
    sync_directory(target)?;
    if let Some(parent) = target.parent() {
        sync_directory(parent)?;
    }
    reject_symlink_components(&destination)?;
    if fs::read_to_string(&destination).ok().as_deref() == Some(AGENT_CONVO_SKILL) {
        return Ok(SkillInstallReceipt {
            path: destination,
            changed: false,
            backup: None,
        });
    }
    let backup = if destination.exists() {
        reject_symlink_components(&destination)?;
        let path = target.join(format!("SKILL-{}.pika-backup", Uuid::new_v4()));
        fs::copy(&destination, &path)
            .with_context(|| format!("cannot back up existing skill {}", destination.display()))?;
        fs::File::open(&path)?.sync_all()?;
        sync_directory(target)?;
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
    reject_symlink_components(&destination)?;
    fs::rename(&temporary, &destination)?;
    sync_directory(target)?;
    Ok(SkillInstallReceipt {
        path: destination,
        changed: true,
        backup,
    })
}

fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        fs::File::open(path)
            .with_context(|| format!("cannot open skill directory {}", path.display()))?
            .sync_all()
            .with_context(|| format!("cannot sync skill directory {}", path.display()))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn reject_symlink_components(path: &Path) -> Result<()> {
    for component in path.ancestors() {
        match fs::symlink_metadata(component) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                bail!(
                    "skill destination crosses externally managed symlink {}",
                    component.display()
                )
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("cannot inspect {}", component.display()));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    use std::os::unix::fs::symlink;

    #[test]
    fn install_is_idempotent_and_backs_up_other_content() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        fs::create_dir(root.join("agent-convo")).unwrap();
        fs::write(root.join("agent-convo/SKILL.md"), "old").unwrap();
        let first = install(&root.join("agent-convo")).unwrap();
        assert!(first.changed);
        assert!(first.backup.unwrap().is_file());
        let second = install(&root.join("agent-convo")).unwrap();
        assert!(!second.changed);
    }

    #[cfg(unix)]
    #[test]
    fn install_rejects_symlinked_target_or_ancestor_without_touching_external_files() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();

        let external_target = root.join("external-target");
        fs::create_dir(&external_target).unwrap();
        fs::write(external_target.join("SKILL.md"), "externally managed\n").unwrap();
        let linked_target = root.join("linked-target");
        symlink(&external_target, &linked_target).unwrap();

        let error = install(&linked_target).unwrap_err();
        assert!(error.to_string().contains("symlink"));
        assert_eq!(
            fs::read_to_string(external_target.join("SKILL.md")).unwrap(),
            "externally managed\n"
        );
        assert_eq!(fs::read_dir(&external_target).unwrap().count(), 1);

        let external_ancestor = root.join("external-ancestor");
        let external_skill = external_ancestor.join("skills/agent-convo");
        fs::create_dir_all(&external_skill).unwrap();
        fs::write(external_skill.join("SKILL.md"), "ancestor managed\n").unwrap();
        let linked_ancestor = root.join("linked-ancestor");
        symlink(&external_ancestor, &linked_ancestor).unwrap();

        let error = install(&linked_ancestor.join("skills/agent-convo")).unwrap_err();
        assert!(error.to_string().contains("symlink"));
        assert_eq!(
            fs::read_to_string(external_skill.join("SKILL.md")).unwrap(),
            "ancestor managed\n"
        );
        assert_eq!(fs::read_dir(&external_skill).unwrap().count(), 1);
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
