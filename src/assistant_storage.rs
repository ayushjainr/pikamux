//! One private-file boundary shared by assistant stores. Not a sandbox for same-UID code.
use std::{fs, io, path::Path};

pub(crate) fn database(path: &Path) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("Assistant database needs a private parent"))?;
    directory(parent)?;
    file(path)?;
    check_sidecars(path)
}

/// Validate existing state without creating a directory, database, or sidecar.
/// Readers must additionally open SQLite with READ_ONLY (never CREATE).
pub(crate) fn existing_database(path: &Path) -> io::Result<()> {
    existing_file(path)?;
    check_sidecars(path)
}

pub(crate) fn existing_file(path: &Path) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("Assistant database needs a private parent"))?;
    check_directory(parent)?;
    check(path, false)
}

fn check_sidecars(path: &Path) -> io::Result<()> {
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut sibling = path.as_os_str().to_owned();
        sibling.push(suffix);
        let sibling = Path::new(&sibling);
        if fs::symlink_metadata(sibling).is_ok() {
            check(sibling, false)?;
        }
    }
    Ok(())
}

pub(crate) fn directory(path: &Path) -> io::Result<()> {
    private_directory(path)?;
    check_directory(path)
}

/// A private coordination directory inside the existing board state boundary.
/// This does not establish the ancestor trust required by assistant authority.
pub(crate) fn private_directory(path: &Path) -> io::Result<()> {
    if fs::symlink_metadata(path).is_err() {
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(path)?;
    }
    check(path, true)
}

fn check_directory(path: &Path) -> io::Result<()> {
    check(path, true)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // A private leaf is insufficient when another user can rename an
        // ancestor. Canonical paths permit platform aliases such as /var while
        // checking the actual parents; root-owned sticky /tmp is safe here.
        for ancestor in path.canonicalize()?.ancestors().skip(1) {
            let metadata = fs::metadata(ancestor)?;
            let root_sticky = metadata.uid() == 0 && metadata.mode() & 0o1000 != 0;
            if !metadata.is_dir()
                || (metadata.uid() != 0 && metadata.uid() != unsafe { libc::geteuid() })
                || (metadata.mode() & 0o022 != 0 && !root_sticky)
            {
                return Err(io::Error::other(format!(
                    "Assistant state ancestor {} is not trusted (owner UID {}, mode {:04o}); use a private profile under trusted ancestors",
                    ancestor.display(),
                    metadata.uid(),
                    metadata.mode() & 0o7777
                )));
            }
        }
    }
    Ok(())
}

pub(crate) fn file(path: &Path) -> io::Result<()> {
    if fs::symlink_metadata(path).is_err() {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        match options.open(path) {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    check(path, false)
}

fn check(path: &Path, directory: bool) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink()
        || (directory && !metadata.is_dir())
        || (!directory && !metadata.is_file())
    {
        return Err(io::Error::other(
            "Assistant state must use regular private files/directories, not symlinks",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if metadata.uid() != unsafe { libc::geteuid() }
            || metadata.permissions().mode() & 0o077 != 0
            || (!directory && metadata.nlink() != 1)
        {
            return Err(io::Error::other(
                "Assistant state must be owner-only and cannot be hard-linked",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn existing_database_validation_never_creates_missing_state() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("absent/state.sqlite");
        assert!(existing_database(&path).is_err());
        assert!(!path.parent().unwrap().exists());
        database(&path).unwrap();
        existing_database(&path).unwrap();
        fs::remove_file(&path).unwrap();
        assert!(existing_database(&path).is_err());
        assert!(!path.exists());
    }
    #[cfg(unix)]
    #[test]
    fn rejects_database_and_sidecar_links_without_modifying_targets() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state/db");
        database(&path).unwrap();
        let target = tmp.path().join("other");
        fs::write(&target, "untouched").unwrap();
        std::os::unix::fs::symlink(&target, path.with_file_name("db-wal")).unwrap();
        assert!(database(&path).is_err());
        assert_eq!(fs::read_to_string(target).unwrap(), "untouched");
    }
}
