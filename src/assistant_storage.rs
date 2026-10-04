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
        // SQLite may unlink a journal or WAL as its last connection closes.
        // Validate directly instead of probing existence before another stat;
        // only an absent optional sidecar is harmless, never an unsafe one.
        match check(sibling, false) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            result => result?,
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
        check_ancestors(path, account_home()?.as_deref())?;
    }
    Ok(())
}

#[cfg(unix)]
fn account_home() -> io::Result<Option<std::path::PathBuf>> {
    use std::{ffi::CStr, os::unix::ffi::OsStrExt};
    // Provider children replace HOME with their private provider profile. It is
    // not an account trust boundary; use the effective account's native record.
    let mut bytes = vec![0u8; 16_384];
    loop {
        let mut record = std::mem::MaybeUninit::<libc::passwd>::uninit();
        let mut result = std::ptr::null_mut();
        // SAFETY: writable native record and live buffer, pointers consumed only
        // after a successful lookup and before that buffer is dropped.
        let status = unsafe {
            libc::getpwuid_r(
                libc::geteuid(),
                record.as_mut_ptr(),
                bytes.as_mut_ptr().cast(),
                bytes.len(),
                &mut result,
            )
        };
        if status == libc::ERANGE && bytes.len() < 1_048_576 {
            bytes.resize(bytes.len() * 2, 0);
            continue;
        }
        if status != 0 {
            return Err(io::Error::from_raw_os_error(status));
        }
        if result.is_null() {
            return Ok(None); // No boundary means the original strict policy.
        }
        let record = unsafe { record.assume_init() };
        if record.pw_dir.is_null() {
            return Ok(None);
        }
        let home = Path::new(std::ffi::OsStr::from_bytes(
            unsafe { CStr::from_ptr(record.pw_dir) }.to_bytes(),
        ));
        return Ok((home.is_absolute()
            && !home
                .components()
                .any(|component| matches!(component, std::path::Component::ParentDir)))
        .then(|| home.to_owned()));
    }
}

#[cfg(unix)]
fn ancestor_trusted(owner: u32, mode: u32, current: u32, above_home: bool, at_home: bool) -> bool {
    let root_sticky = owner == 0 && mode & 0o1000 != 0;
    (above_home || owner == 0 || owner == current)
        && (!at_home || owner == current)
        && (mode & 0o022 == 0 || root_sticky)
}

#[cfg(unix)]
fn check_ancestors(path: &Path, home: Option<&Path>) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    let path = absolute.as_path();
    // A private leaf is insufficient when another user can rename an
    // ancestor. Canonical paths permit platform aliases such as /var while
    // checking the actual parents; root-owned sticky /tmp is safe here.
    let canonical = path.canonicalize()?;
    let boundary = home
        .filter(|home| path.starts_with(home))
        .map(Path::canonicalize)
        .transpose()?;
    // Preserve Darwin's system aliases, not arbitrary profile aliases.
    // Canonicalization alone must not let a private profile escape its
    // ownership boundary through a link, including for external profiles.
    for ancestor in path.ancestors() {
        if fs::symlink_metadata(ancestor)?.file_type().is_symlink() && !platform_alias(ancestor) {
            return Err(io::Error::other(
                "Assistant state must not use symlink ancestors",
            ));
        }
    }
    let boundary = boundary
        .as_deref()
        .filter(|home| canonical.starts_with(home));
    for ancestor in canonical.ancestors().skip(1) {
        let metadata = fs::metadata(ancestor)?;
        let above_home =
            boundary.is_some_and(|home| ancestor != home && home.starts_with(ancestor));
        crate::mobile_pairing_acl::validate(ancestor).map_err(|error| {
            io::Error::other(format!(
                "Assistant state ancestor {} has an unsafe ACL: {error}",
                ancestor.display()
            ))
        })?;
        if !metadata.is_dir()
            || !ancestor_trusted(
                metadata.uid(),
                metadata.mode(),
                unsafe { libc::geteuid() },
                above_home,
                boundary == Some(ancestor),
            )
        {
            return Err(io::Error::other(format!(
                "Assistant state ancestor {} is not trusted (owner UID {}, mode {:04o}); use a private profile under trusted ancestors",
                ancestor.display(),
                metadata.uid(),
                metadata.mode() & 0o7777
            )));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn platform_alias(path: &Path) -> bool {
    cfg!(target_os = "macos") && matches!(path.to_str(), Some("/var" | "/tmp" | "/etc"))
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
        crate::mobile_pairing_acl::validate(path).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "Assistant state {} has an unsafe ACL: {error}",
                    path.display()
                ),
            )
        })?;
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        // A concurrent unlink can leave the stat snapshot with no links.
        // This is absence, not a hard-link bypass: mandatory files still fail.
        if !directory && metadata.nlink() == 0 {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "Assistant state file was unlinked during validation",
            ));
        }
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
    #[cfg(unix)]
    #[test]
    fn administrator_owner_exception_is_only_above_home_and_never_writable() {
        assert!(ancestor_trusted(1234, 0o755, 5678, true, false));
        assert!(!ancestor_trusted(1234, 0o755, 5678, false, false));
        assert!(!ancestor_trusted(1234, 0o755, 5678, false, true));
        assert!(!ancestor_trusted(1234, 0o775, 5678, true, false));
        assert!(!ancestor_trusted(5678, 0o777, 5678, false, false));
        assert!(ancestor_trusted(0, 0o1777, 5678, true, false));
        assert!(!ancestor_trusted(0, 0o755, 5678, false, true));
    }

    #[cfg(unix)]
    #[test]
    fn home_boundary_preserves_data_and_rejects_writes_and_links() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let home = base.join("home");
        let state = home.join(".local/state/profile");
        private_directory(&state).unwrap();
        let db = state.join("memory.sqlite");
        file(&db).unwrap();
        fs::write(&db, b"existing identity and history").unwrap();
        check_ancestors(&state, Some(&home)).unwrap();
        fs::set_permissions(home.join(".local/state"), fs::Permissions::from_mode(0o775)).unwrap();
        assert!(check_ancestors(&state, Some(&home)).is_err());
        fs::set_permissions(home.join(".local/state"), fs::Permissions::from_mode(0o700)).unwrap();
        let alias = home.join("alias");
        symlink(home.join(".local"), &alias).unwrap();
        assert!(check_ancestors(&alias.join("state/profile"), Some(&home)).is_err());
        let external = base.join("external/profile");
        private_directory(&external).unwrap();
        check_ancestors(&external, Some(&home)).unwrap();
        let external_alias = base.join("external-alias");
        symlink(base.join("external"), &external_alias).unwrap();
        assert!(check_ancestors(&external_alias.join("profile"), Some(&home)).is_err());
        fs::set_permissions(base.join("external"), fs::Permissions::from_mode(0o775)).unwrap();
        assert!(check_ancestors(&external, Some(&home)).is_err());
        assert_eq!(fs::read(&db).unwrap(), b"existing identity and history");
    }
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

    #[cfg(unix)]
    #[test]
    fn transient_sidecars_may_disappear_but_unsafe_files_still_fail_closed() {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt, symlink};
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("state/db");
        database(&path).unwrap();
        let journal = path.with_file_name("db-journal");
        let barrier = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            let churn = scope.spawn(|| {
                barrier.wait();
                for _ in 0..1000 {
                    fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .mode(0o600)
                        .open(&journal)
                        .unwrap();
                    fs::remove_file(&journal).unwrap();
                }
            });
            barrier.wait();
            for _ in 0..1000 {
                check_sidecars(&path).unwrap();
            }
            churn.join().unwrap();
        });
        assert!(!journal.exists());
        check_sidecars(&path).unwrap();
        file(&journal).unwrap();
        fs::set_permissions(&journal, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(check_sidecars(&path).is_err());
        fs::set_permissions(&journal, fs::Permissions::from_mode(0o600)).unwrap();
        let alias = tmp.path().join("journal-alias");
        fs::hard_link(&journal, &alias).unwrap();
        assert!(check_sidecars(&path).is_err());
        fs::remove_file(&journal).unwrap();
        symlink(tmp.path().join("missing-target"), &journal).unwrap();
        assert!(check_sidecars(&path).is_err());
        assert!(existing_file(&tmp.path().join("missing-database")).is_err());
    }
}
