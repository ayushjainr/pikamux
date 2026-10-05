//! Descriptor-relative private storage for native API credentials.
use anyhow::{Context, Result, ensure};
use std::{
    ffi::CString,
    fs::{self, File},
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{
            ffi::OsStrExt,
            fs::{MetadataExt, OpenOptionsExt},
        },
    },
    path::Path,
};

fn directory(path: &Path) -> Result<File> {
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    private_metadata(&file, true)?;
    unchanged(path, &file)?;
    Ok(file)
}

pub(crate) fn private_directory(path: &Path) -> Result<()> {
    directory(path).map(|_| ())
}

fn private_metadata(file: &File, dir: bool) -> Result<()> {
    let meta = file.metadata()?;
    ensure!(
        meta.uid() == unsafe { libc::geteuid() } && meta.mode() & 0o077 == 0,
        "Native credentials require private ownership and permissions"
    );
    ensure!(
        if dir {
            meta.is_dir()
        } else {
            meta.is_file() && meta.nlink() == 1
        },
        "Native credential storage has unsafe type or links"
    );
    no_acl(file)
}

fn unchanged(path: &Path, file: &File) -> Result<()> {
    let actual = fs::symlink_metadata(path)?;
    let opened = file.metadata()?;
    ensure!(
        actual.dev() == opened.dev()
            && actual.ino() == opened.ino()
            && !actual.file_type().is_symlink(),
        "Native credential path changed"
    );
    Ok(())
}

fn name(path: &Path) -> Result<CString> {
    Ok(CString::new(
        path.file_name()
            .context("Credential filename missing")?
            .as_bytes(),
    )?)
}

fn open_at(parent: &File, name: &CString, flags: i32) -> std::io::Result<File> {
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: openat returned a fresh owned descriptor.
    Ok(unsafe { File::from_raw_fd(fd) })
}

pub(crate) fn open_lock(path: &Path) -> Result<File> {
    let parent_path = path.parent().context("Startup lock parent missing")?;
    let parent = directory(parent_path)?;
    let file = open_at(&parent, &name(path)?, libc::O_RDWR | libc::O_CREAT)?;
    validate_lock(path, &file)?;
    unchanged(parent_path, &parent)?;
    Ok(file)
}

pub(crate) fn validate_lock(path: &Path, file: &File) -> Result<()> {
    private_metadata(file, false)?;
    unchanged(path, file)
}

pub(crate) fn read_private(path: &Path) -> Result<Option<Vec<u8>>> {
    let parent_path = path.parent().context("Credential parent missing")?;
    let parent = directory(parent_path)?;
    let file = match open_at(&parent, &name(path)?, libc::O_RDONLY) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    private_metadata(&file, false)?;
    let mut bytes = Vec::new();
    (&file).take(8193).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 8192, "Native credential exceeds size limit");
    private_metadata(&file, false)?;
    unchanged(path, &file)?;
    unchanged(parent_path, &parent)?;
    Ok(Some(bytes))
}

pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    ensure!(bytes.len() <= 8192, "Native credential exceeds size limit");
    let parent_path = path.parent().context("Credential parent missing")?;
    let parent = directory(parent_path)?;
    let temporary = CString::new(format!(".{}.tmp", uuid::Uuid::new_v4().simple()))?;
    let mut file = open_at(
        &parent,
        &temporary,
        libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
    )?;
    let result = (|| -> Result<()> {
        private_metadata(&file, false)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        private_metadata(&file, false)?;
        unchanged(parent_path, &parent)?;
        publish_at(&parent, &temporary, path)?;
        parent.sync_all()?;
        unchanged(path, &file)?;
        unchanged(parent_path, &parent)?;
        Ok(())
    })();
    // Only the uniquely created temporary name inside our pinned directory.
    unsafe {
        libc::unlinkat(parent.as_raw_fd(), temporary.as_ptr(), 0);
    }
    result
}

fn publish_at(parent: &File, temporary: &CString, path: &Path) -> Result<()> {
    let destination = name(path)?;
    let rc = unsafe {
        libc::renameat(
            parent.as_raw_fd(),
            temporary.as_ptr(),
            parent.as_raw_fd(),
            destination.as_ptr(),
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn no_acl(file: &File) -> Result<()> {
    unsafe extern "C" {
        fn filesec_init() -> *mut libc::c_void;
        fn filesec_free(sec: *mut libc::c_void);
        fn filesec_query_property(sec: *mut libc::c_void, property: i32, present: *mut i32) -> i32;
        #[cfg_attr(target_arch = "x86_64", link_name = "fstatx_np$INODE64")]
        fn fstatx_np(fd: i32, metadata: *mut libc::stat, sec: *mut libc::c_void) -> i32;
    }
    let sec = unsafe { filesec_init() };
    ensure!(!sec.is_null(), "Cannot inspect credential ACL");
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    let mut present = -1;
    let success = unsafe {
        fstatx_np(file.as_raw_fd(), stat.as_mut_ptr(), sec) == 0
            && filesec_query_property(sec, 5, &mut present) == 0
    };
    unsafe {
        filesec_free(sec);
    }
    ensure!(
        success && present == 0,
        "Native credential storage must not have an extended ACL"
    );
    Ok(())
}

#[cfg(target_os = "linux")]
fn no_acl(file: &File) -> Result<()> {
    for attribute in [c"system.posix_acl_access", c"system.posix_acl_default"] {
        let result = unsafe {
            libc::fgetxattr(
                file.as_raw_fd(),
                attribute.as_ptr(),
                std::ptr::null_mut(),
                0,
            )
        };
        let error = std::io::Error::last_os_error().raw_os_error();
        ensure!(
            result < 0 && matches!(error, Some(libc::ENODATA | libc::ENOTSUP)),
            "Native credential storage ACL is present or unverifiable"
        );
    }
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn no_acl(_file: &File) -> Result<()> {
    anyhow::bail!("Native credential ACL inspection unavailable")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn private_store_rejects_hardlinked_secret() {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let path = root.path().join("credential.json");
        write_private(&path, b"private-test-value").unwrap();
        assert_eq!(read_private(&path).unwrap().unwrap(), b"private-test-value");
        fs::hard_link(&path, root.path().join("alias")).unwrap();
        assert!(read_private(&path).is_err());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn private_store_rejects_actual_foreign_read_acl() {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let path = root.path().join("credential.json");
        write_private(&path, b"disposable-test-value").unwrap();
        let status = std::process::Command::new("/bin/chmod")
            .args(["+a", "everyone allow read"])
            .arg(&path)
            .status()
            .unwrap();
        assert!(status.success());
        assert!(read_private(&path).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn private_store_rejects_actual_posix_acl_despite_private_mode() {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let path = root.path().join("credential.json");
        write_private(&path, b"disposable-test-value").unwrap();
        let file = File::open(&path).unwrap();
        // Linux UAPI POSIX_ACL_XATTR_VERSION=2, little-endian tag/perm/id entries.
        // The named grant is masked off: mode remains 0600, but the extended ACL
        // must still be rejected independently of the ordinary permission bits.
        let foreign = unsafe { libc::geteuid() }.wrapping_add(1);
        let mut acl = 2_u32.to_le_bytes().to_vec();
        for (tag, permission, uid) in [
            (1_u16, 6_u16, u32::MAX),
            (2, 4, foreign),
            (4, 0, u32::MAX),
            (16, 0, u32::MAX),
            (32, 0, u32::MAX),
        ] {
            acl.extend_from_slice(&tag.to_le_bytes());
            acl.extend_from_slice(&permission.to_le_bytes());
            acl.extend_from_slice(&uid.to_le_bytes());
        }
        let result = unsafe {
            libc::fsetxattr(
                file.as_raw_fd(),
                c"system.posix_acl_access".as_ptr(),
                acl.as_ptr().cast(),
                acl.len(),
                0,
            )
        };
        assert_eq!(result, 0, "{}", std::io::Error::last_os_error());
        assert_eq!(file.metadata().unwrap().mode() & 0o777, 0o600);
        assert!(read_private(&path).is_err());
    }
}
