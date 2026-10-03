//! Darwin ACLs can grant writes independently of mode bits. Permit harmless
//! read/deny entries (including macOS's default home-directory deny-delete ACL).
use std::{io, path::Path};

#[cfg(not(target_os = "macos"))]
pub(super) fn validate(_path: &Path) -> io::Result<()> {
    // Supported non-Darwin hosts use POSIX ACL masks: effective named-user/group
    // writes are reflected in group mode bits, checked by the caller.
    Ok(())
}

#[cfg(target_os = "macos")]
pub(super) fn validate(path: &Path) -> io::Result<()> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    unsafe extern "C" {
        fn filesec_init() -> *mut libc::c_void;
        fn filesec_free(security: *mut libc::c_void);
        fn filesec_query_property(
            security: *mut libc::c_void,
            property: libc::c_int,
            present: *mut libc::c_int,
        ) -> libc::c_int;
        #[cfg_attr(target_arch = "x86_64", link_name = "lstatx_np$INODE64")]
        fn lstatx_np(
            path: *const libc::c_char,
            metadata: *mut libc::stat,
            security: *mut libc::c_void,
        ) -> libc::c_int;
        fn acl_get_link_np(path: *const libc::c_char, kind: libc::c_int) -> *mut libc::c_void;
        fn acl_valid(acl: *mut libc::c_void) -> libc::c_int;
        fn acl_get_entry(
            acl: *mut libc::c_void,
            index: libc::c_int,
            entry: *mut *mut libc::c_void,
        ) -> libc::c_int;
        fn acl_get_tag_type(entry: *mut libc::c_void, tag: *mut libc::c_int) -> libc::c_int;
        fn acl_get_permset_mask_np(entry: *mut libc::c_void, mask: *mut u64) -> libc::c_int;
        fn acl_free(acl: *mut libc::c_void) -> libc::c_int;
    }
    struct Acl(*mut libc::c_void);
    impl Drop for Acl {
        fn drop(&mut self) {
            // SAFETY: this guard owns the live allocation returned by Darwin.
            unsafe {
                acl_free(self.0);
            }
        }
    }
    let path = CString::new(path.as_os_str().as_bytes())?;
    // acl_get_link_np returns ENOENT for both an absent ACL and a missing path.
    // Establish positive absence with Darwin's filesec API rather than treating
    // arbitrary lookup errors as an empty ACL.
    let security = unsafe { filesec_init() };
    if security.is_null() {
        return Err(io::Error::last_os_error());
    }
    let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
    let mut present = -1;
    // SAFETY: live filesec allocation and writable native outputs. FILESEC_ACL=5.
    let result = unsafe {
        if lstatx_np(path.as_ptr(), metadata.as_mut_ptr(), security) != 0
            || filesec_query_property(security, 5, &mut present) != 0
        {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    };
    unsafe {
        filesec_free(security);
    }
    result?;
    if present == 0 {
        return Ok(());
    }
    // SAFETY: NUL-terminated path, public ACL_TYPE_EXTENDED constant. This API
    // reads the link itself; caller separately rejects links and checks owner.
    let raw = unsafe { acl_get_link_np(path.as_ptr(), 0x100) };
    if raw.is_null() {
        return Err(io::Error::last_os_error());
    }
    let acl = Acl(raw);
    // SAFETY: the ACL is live for this entire function.
    if unsafe { acl_valid(acl.0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let mut index = 0; // Darwin ACL_FIRST_ENTRY=0, ACL_NEXT_ENTRY=-1.
    loop {
        let mut entry = std::ptr::null_mut();
        // SAFETY: valid ACL, public index, writable pointer to borrowed entry.
        if unsafe { acl_get_entry(acl.0, index, &mut entry) } != 0 {
            let error = io::Error::last_os_error();
            // Darwin reports EINVAL at the end of a previously validated ACL.
            return if error.raw_os_error() == Some(libc::EINVAL) {
                Ok(())
            } else {
                Err(error)
            };
        }
        index = -1;
        let (mut tag, mut mask) = (0, 0u64);
        // SAFETY: entry belongs to the live ACL; both outputs have native types.
        if unsafe {
            acl_get_tag_type(entry, &mut tag) != 0 || acl_get_permset_mask_np(entry, &mut mask) != 0
        } {
            return Err(io::Error::last_os_error());
        }
        const MUTATION: u64 = (1 << 2)
            | (1 << 4)
            | (1 << 5)
            | (1 << 6)
            | (1 << 8)
            | (1 << 10)
            | (1 << 12)
            | (1 << 13);
        if !matches!(tag, 1 | 2) || (tag == 1 && mask & MUTATION != 0) {
            return Err(io::Error::other(
                "An access-control rule permits changes to this SSH path; no phone key was added",
            ));
        }
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    #[test]
    fn actual_darwin_acl_allows_default_deny_but_rejects_write_grant() {
        struct RemoveTestAcl(std::path::PathBuf);
        impl Drop for RemoveTestAcl {
            fn drop(&mut self) {
                let _ = std::process::Command::new("/bin/chmod")
                    .arg("-N")
                    .arg(&self.0)
                    .status();
            }
        }
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("authorized_keys");
        std::fs::write(&path, "existing public key\n").unwrap();
        let _cleanup = RemoveTestAcl(path.clone());
        super::validate(&path).unwrap();
        for (rule, accepted) in [
            ("everyone deny delete", true),
            ("everyone allow write", false),
        ] {
            assert!(
                std::process::Command::new("/bin/chmod")
                    .args(["+a", rule])
                    .arg(&path)
                    .status()
                    .unwrap()
                    .success()
            );
            assert_eq!(super::validate(&path).is_ok(), accepted);
        }
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            "existing public key\n"
        );
    }
}
