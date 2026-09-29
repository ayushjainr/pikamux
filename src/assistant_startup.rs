//! One local startup selection shared by the board and `pika pika`.
//! References an existing authority in place; never copies memory or credentials.
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Selection {
    pub profile_root: PathBuf,
    pub profile_id: String,
    pub scope: String,
    pub executable: PathBuf,
    pub max_calls: u64,
}

impl Selection {
    fn validate(&self) -> Result<()> {
        crate::assistant::scope(&self.scope)?;
        if !self.executable.is_absolute()
            || (!(1..=100).contains(&self.max_calls)
                && self.max_calls != crate::assistant_policy::NO_CALL_LIMIT)
        {
            bail!("Assistant startup requires an absolute provider and an explicit allowance");
        }
        crate::assistant_host::verify_existing_profile(&self.profile_root, &self.profile_id)
    }
}

pub(crate) fn path() -> Result<PathBuf> {
    Ok(crate::paths::Paths::discover()?
        .state_dir
        .join("assistant-startup/selection.json"))
}

pub(crate) fn load(path: &Path) -> Result<Option<Selection>> {
    if !path.try_exists()? && std::fs::symlink_metadata(path).is_err() {
        return Ok(None);
    }
    crate::assistant_storage::existing_file(path)?;
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(16385)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 16384 {
        bail!("Assistant startup selection is too large");
    }
    let selection: Selection = serde_json::from_slice(&bytes)
        .context("Invalid assistant startup selection; no replacement profile was opened")?;
    selection.validate()?;
    Ok(Some(selection))
}

pub(crate) fn save(path: &Path, selection: &Selection) -> Result<()> {
    selection.validate()?;
    let parent = path.parent().context("Startup selection has no parent")?;
    crate::assistant_storage::directory(parent)?;
    if std::fs::symlink_metadata(path).is_ok() {
        crate::assistant_storage::existing_file(path)?;
    }
    use std::os::unix::fs::OpenOptionsExt;
    let temporary = parent.join(format!(".assistant-startup-{}", uuid::Uuid::new_v4()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)?;
    let result = (|| -> Result<()> {
        file.write_all(&serde_json::to_vec(selection)?)?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)?;
        std::fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

/// A bound profile uses this installation's shared board producer, not a
/// projection accidentally inferred from its storage directory's parent.
pub(crate) fn owns(root: &Path) -> Result<bool> {
    let paths = crate::paths::Paths::discover()?;
    // Invalid defaults must not block an explicitly selected offline authority.
    // Normal startup reports the error when resolving its selection; an
    // independent profile simply receives no ambient-board ownership here.
    let selection = match load(&paths.state_dir.join("assistant-startup/selection.json")) {
        Ok(selection) => selection,
        Err(_) => return Ok(false),
    };
    let selected = selection
        .map(|selection| selection.profile_root)
        .unwrap_or_else(|| paths.state_dir.join("assistant"));
    Ok(root.canonicalize()? == selected.canonicalize().unwrap_or(selected))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn selection_preserves_profile_and_rejects_missing_or_replaced_authority() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("profile");
        let store = crate::assistant_memory::Store::open(root.join("memory.sqlite")).unwrap();
        let selected = Selection {
            profile_root: root.clone(),
            profile_id: store.profile_id().into(),
            scope: "pika".into(),
            executable: "/fake/codex".into(),
            max_calls: 17,
        };
        let path = tmp.path().join("state/assistant-startup.json");
        assert!(load(&path).unwrap().is_none());
        save(&path, &selected).unwrap();
        let restored = load(&path).unwrap().unwrap();
        assert_eq!(restored.profile_id, store.profile_id());
        assert_eq!(restored.profile_root, root);
        assert_eq!(restored.scope, "pika");
        assert_eq!(restored.max_calls, 17);
        let mut wrong = selected;
        wrong.profile_id = uuid::Uuid::new_v4().to_string();
        assert!(save(&path, &wrong).is_err());
        assert_eq!(load(&path).unwrap().unwrap().profile_id, store.profile_id());
        drop(store);
        std::fs::rename(&root, tmp.path().join("moved")).unwrap();
        assert!(load(&path).is_err());
        assert!(!root.exists());
    }
    #[test]
    fn selection_refuses_links_and_malformed_data() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("selection");
        let target = tmp.path().join("target");
        std::fs::write(&target, "unchanged").unwrap();
        std::os::unix::fs::symlink(&target, &path).unwrap();
        assert!(load(&path).is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "unchanged");
        std::fs::remove_file(&path).unwrap();
        crate::assistant_storage::file(&path).unwrap();
        std::fs::write(&path, "not json").unwrap();
        assert!(load(&path).is_err());
    }
}
