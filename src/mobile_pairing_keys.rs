//! Pairing grants append one restricted public key; no existing credential is
//! copied, replaced, or removed. A changed target is an unknown outcome.
use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::STANDARD};
use fs2::FileExt;
use std::{
    fs,
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

pub(super) fn public_key(value: &str) -> Result<String> {
    if value.len() > 100 || value.chars().any(char::is_control) {
        bail!("A canonical Ed25519 public key is required");
    }
    let fields: Vec<_> = value.split(' ').collect();
    if fields.len() != 2 || fields[0] != "ssh-ed25519" {
        bail!("Only a dedicated Ed25519 public key can be paired");
    }
    let bytes = STANDARD
        .decode(fields[1])
        .context("Invalid SSH public key encoding")?;
    if bytes.len() != 51
        || !bytes.starts_with(b"\0\0\0\x0bssh-ed25519\0\0\0\x20")
        || STANDARD.encode(&bytes) != fields[1]
    {
        bail!("Invalid Ed25519 SSH wire key");
    }
    Ok(value.to_owned())
}

pub(super) fn verified_host_key(
    path: &Path,
    address: std::net::IpAddr,
    port: u16,
) -> Result<String> {
    let anchor = read_host_anchor(path)?;
    let observed = scan_host(address, port)?;
    if !observed
        .lines()
        .filter(|line| !line.starts_with('#'))
        .any(|line| {
            let fields: Vec<_> = line.split_whitespace().collect();
            fields.len() == 3 && format!("{} {}", fields[1], fields[2]) == anchor
        })
    {
        bail!(
            "The SSH service does not present the selected public host key. Check its port and OpenSSH setup; no pairing was started."
        );
    }
    Ok(anchor)
}
fn read_host_anchor(path: &Path) -> Result<String> {
    let metadata =
        fs::symlink_metadata(path).context("The selected SSH public host key is unavailable")?;
    trusted_ancestors(path.parent().context("SSH host key has no parent")?)?;
    crate::mobile_pairing_acl::validate(path)?;
    trusted_host_file(&metadata)?;
    let mut text = String::new();
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    let opened = file.metadata()?;
    if opened.dev() != metadata.dev() || opened.ino() != metadata.ino() {
        bail!("SSH host key changed during identity validation");
    }
    file.take(1024).read_to_string(&mut text)?;
    let fields: Vec<_> = text.split_whitespace().collect();
    if fields.len() < 2 {
        bail!("Invalid SSH public host key");
    }
    public_key(&format!("{} {}", fields[0], fields[1]))
}
fn trusted_host_file(metadata: &fs::Metadata) -> Result<()> {
    if !metadata.is_file()
        || metadata.nlink() != 1
        || metadata.mode() & 0o022 != 0
        || metadata.uid() != 0 && metadata.uid() != unsafe { libc::geteuid() }
    {
        bail!("SSH public host key must be a trusted regular file");
    }
    Ok(())
}

fn scan_host(address: std::net::IpAddr, port: u16) -> Result<String> {
    let mut child = Command::new("/usr/bin/ssh-keyscan")
        .args([
            "-T",
            "3",
            "-p",
            &port.to_string(),
            "-t",
            "ed25519",
            &address.to_string(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let deadline = Instant::now() + Duration::from_secs(5);
    while child.try_wait()?.is_none() {
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("SSH identity check timed out");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let mut output = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .take(8192)
        .read_to_string(&mut output)?;
    Ok(output)
}

pub(super) struct Grant {
    path: PathBuf,
    executable: PathBuf,
}
impl Grant {
    pub(super) fn prepare(path: PathBuf, executable: PathBuf) -> Result<Self> {
        if !path.is_absolute() {
            bail!("SSH authorization path must be absolute");
        }
        if has_traversal(&path) {
            bail!("SSH authorization path must not contain traversal components");
        }
        let parent = path
            .parent()
            .context("SSH authorization path has no parent")?;
        ssh_directory(parent)?;
        match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                ssh_file(&metadata)?;
                crate::mobile_pairing_acl::validate(&path)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        Ok(Self { path, executable })
    }
    pub(super) fn authorize(
        &self,
        key: &str,
        deadline: Instant,
        cancel: &AtomicBool,
        interactive: bool,
    ) -> Result<()> {
        let key = public_key(key)?;
        let (mut file, generation) = self.locked_file(deadline, cancel, interactive)?;
        let before = read_authorizations(&mut file, &generation)?;
        let line = self.grant_line(&key)?;
        if existing_grant(&before, &line, &key)? {
            return Ok(());
        }
        enrollment_live(deadline, cancel, interactive)?;
        self.append_grant(&mut file, &generation, &before, &line, || {
            enrollment_live(deadline, cancel, interactive)
        })
    }
    fn locked_file(
        &self,
        deadline: Instant,
        cancel: &AtomicBool,
        interactive: bool,
    ) -> Result<(fs::File, fs::Metadata)> {
        ssh_directory(self.path.parent().unwrap())?;
        let file = fs::OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&self.path)?;
        lock_before_deadline(&file, deadline, cancel, interactive)?;
        let generation = file.metadata()?;
        let path_generation = fs::symlink_metadata(&self.path)?;
        ssh_file(&generation)?;
        ssh_file(&path_generation)?;
        crate::mobile_pairing_acl::validate(&self.path)?;
        same_file(&generation, &path_generation)?;
        Ok((file, generation))
    }
    fn grant_line(&self, key: &str) -> Result<String> {
        let command = format!(
            "{} _mobile",
            shell_words::quote(
                self.executable
                    .to_str()
                    .context("Pika executable path must be UTF-8")?
            )
        );
        let escaped = command.replace('\\', "\\\\").replace('"', "\\\"");
        Ok(format!("restrict,command=\"{escaped}\" {key} pika-phone\n"))
    }
    fn append_grant(
        &self,
        file: &mut fs::File,
        generation: &fs::Metadata,
        before: &str,
        line: &str,
        live: impl Fn() -> Result<()>,
    ) -> Result<()> {
        let prefix = if before.is_empty() || before.ends_with('\n') {
            ""
        } else {
            "\n"
        };
        let current = fs::symlink_metadata(&self.path)?;
        ssh_file(&current)?;
        crate::mobile_pairing_acl::validate(&self.path)?;
        same_file(generation, &current)?;
        live()?;
        file.write_all(format!("{prefix}{line}").as_bytes())?;
        file.sync_all()?;
        let after = fs::symlink_metadata(&self.path)?;
        same_file(generation, &after)
            .context("SSH authorization outcome is unknown; use the same phone key to check SSH")?;
        Ok(())
    }
}

fn lock_before_deadline(
    file: &fs::File,
    deadline: Instant,
    cancel: &AtomicBool,
    interactive: bool,
) -> Result<()> {
    loop {
        enrollment_live(deadline, cancel, interactive)?;
        match file.try_lock_exclusive() {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20))
            }
            Err(error) => return Err(error.into()),
        }
    }
}
fn same_file(expected: &fs::Metadata, actual: &fs::Metadata) -> Result<()> {
    if expected.dev() != actual.dev() || expected.ino() != actual.ino() {
        bail!("SSH authorization file changed during pairing");
    }
    Ok(())
}
fn read_authorizations(file: &mut fs::File, metadata: &fs::Metadata) -> Result<String> {
    if metadata.len() > 1024 * 1024 {
        bail!("SSH authorization file is too large to safely inspect");
    }
    let mut before = String::new();
    file.take(1024 * 1024 + 1).read_to_string(&mut before)?;
    if before.len() > 1024 * 1024 {
        bail!("SSH authorization file grew beyond the inspection bound");
    }
    Ok(before)
}
fn existing_grant(before: &str, line: &str, key: &str) -> Result<bool> {
    if before.lines().any(|existing| existing == line.trim_end()) {
        return Ok(true);
    }
    let encoded = key.split(' ').nth(1).context("Canonical key missing")?;
    if before
        .lines()
        .any(|existing| existing.split_whitespace().any(|field| field == encoded))
    {
        bail!("This key is already authorized differently; no existing grant was changed");
    }
    Ok(false)
}

fn enrollment_live(deadline: Instant, cancel: &AtomicBool, interactive: bool) -> Result<()> {
    if interactive && crate::mobile_pairing_ui::cancelled()? {
        cancel.store(true, Ordering::Relaxed);
    }
    if cancel.load(Ordering::Relaxed) || Instant::now() >= deadline {
        bail!("Pairing cancelled or expired before authorization");
    }
    Ok(())
}
fn ssh_file(metadata: &fs::Metadata) -> Result<()> {
    if !metadata.is_file()
        || metadata.nlink() != 1
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o022 != 0
    {
        bail!("SSH authorization must be an owned, non-writable-by-others regular file");
    }
    Ok(())
}
fn ssh_directory(path: &Path) -> Result<()> {
    if !path.exists() {
        fs::DirBuilder::new().mode(0o700).create(path)?;
    }
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o022 != 0
    {
        bail!("SSH directory must be owned and not writable by others");
    }
    let home = directories::BaseDirs::new().context("Cannot determine SSH home")?;
    grant_ancestors(path, home.home_dir())?;
    Ok(())
}

fn trusted_ancestors(path: &Path) -> Result<()> {
    validate_ancestors(path, None)
}

fn grant_ancestors(path: &Path, home: &Path) -> Result<()> {
    // Lexical traversal must not escape the account-home boundary. Custom
    // targets outside HOME retain the stronger host-anchor ancestor policy.
    if has_traversal(path) {
        bail!("SSH authorization path must not contain traversal components");
    }
    let boundary =
        (home.is_absolute() && !has_traversal(home) && path.starts_with(home)).then_some(home);
    validate_ancestors(path, boundary)
}

fn has_traversal(path: &Path) -> bool {
    path.components().any(|part| {
        matches!(
            part,
            std::path::Component::ParentDir | std::path::Component::CurDir
        )
    })
}

fn ancestor_owner_trusted(owner: u32, current: u32, above_home: bool) -> bool {
    above_home || owner == 0 || owner == current
}

fn validate_ancestors(path: &Path, home: Option<&Path>) -> Result<()> {
    for ancestor in path.ancestors() {
        crate::mobile_pairing_acl::validate(ancestor)
            .with_context(|| format!("Cannot validate SSH ancestor ACL {}", ancestor.display()))?;
        let metadata = fs::symlink_metadata(ancestor)?;
        let owner = metadata.uid();
        let above_home = home.is_some_and(|home| ancestor != home && home.starts_with(ancestor));
        // A sticky, root-owned temporary parent cannot replace an owned child.
        let sticky_root = owner == 0 && metadata.mode() & 0o1000 != 0;
        if !metadata.is_dir()
            || !ancestor_owner_trusted(owner, unsafe { libc::geteuid() }, above_home)
            || home == Some(ancestor) && owner != unsafe { libc::geteuid() }
            || metadata.mode() & 0o022 != 0 && !sticky_root
        {
            bail!("SSH path has an untrusted or writable ancestor");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    fn key() -> String {
        let mut wire = b"\0\0\0\x0bssh-ed25519\0\0\0\x20".to_vec();
        wire.extend([7; 32]);
        format!("ssh-ed25519 {}", STANDARD.encode(wire))
    }
    #[test]
    fn canonical_key_rejects_options_comments_and_other_algorithms() {
        let key = key();
        assert_eq!(public_key(&key).unwrap(), key);
        for bad in [
            format!("{key} comment"),
            format!("{key}\n"),
            format!("restrict {key}"),
            key.replace("ssh-ed25519", "ssh-rsa"),
        ] {
            assert!(public_key(&bad).is_err());
        }
    }
    #[test]
    fn foreign_owner_exception_is_only_above_account_home() {
        assert!(ancestor_owner_trusted(1234, 5678, true));
        assert!(!ancestor_owner_trusted(1234, 5678, false));
        assert!(ancestor_owner_trusted(0, 5678, false));
        assert!(ancestor_owner_trusted(5678, 5678, false));
    }
    #[test]
    fn home_boundary_keeps_path_and_write_safety() {
        let root = tempfile::tempdir().unwrap();
        let base = root.path().canonicalize().unwrap();
        let home = base.join("home");
        let ssh = home.join(".ssh");
        fs::create_dir(&home).unwrap();
        fs::create_dir(&ssh).unwrap();
        assert!(grant_ancestors(&ssh, &home).is_ok());
        assert!(grant_ancestors(&ssh.join("../.ssh"), &home).is_err());
        fs::set_permissions(&home, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(grant_ancestors(&ssh, &home).is_err());
        fs::set_permissions(&home, fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(&base, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(grant_ancestors(&ssh, &home).is_err());
        fs::set_permissions(&base, fs::Permissions::from_mode(0o755)).unwrap();
        let alias = base.join("alias");
        std::os::unix::fs::symlink(&home, &alias).unwrap();
        assert!(grant_ancestors(&alias.join(".ssh"), &alias).is_err());
        assert!(grant_ancestors(&ssh, &base.join("elsewhere")).is_ok());
    }
    #[test]
    fn preserves_public_ssh_modes_and_unrelated_bytes() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().canonicalize().unwrap().join("ssh");
        fs::create_dir(&dir).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        let path = dir.join("authorized_keys");
        fs::write(&path, b"# original sentinel\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let grant = Grant::prepare(path.clone(), "/absolute/pika".into()).unwrap();
        let cancel = AtomicBool::new(false);
        grant
            .authorize(
                &key(),
                Instant::now() + Duration::from_secs(1),
                &cancel,
                false,
            )
            .unwrap();
        let bytes = fs::read_to_string(&path).unwrap();
        assert!(
            bytes.starts_with("# original sentinel\nrestrict,command=\"/absolute/pika _mobile\" ")
        );
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o644);
        assert_eq!(fs::metadata(&dir).unwrap().mode() & 0o777, 0o755);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).unwrap();
        assert!(Grant::prepare(path, "/absolute/pika".into()).is_err());
    }
    #[test]
    fn locked_or_cancelled_claim_never_appends() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().canonicalize().unwrap().join("authorized_keys");
        fs::write(&path, b"# unchanged\n").unwrap();
        let grant = Grant::prepare(path.clone(), "/absolute/pika".into()).unwrap();
        let lock = fs::File::open(&path).unwrap();
        lock.lock_exclusive().unwrap();
        assert!(
            grant
                .authorize(
                    &key(),
                    Instant::now() + Duration::from_millis(40),
                    &AtomicBool::new(false),
                    false
                )
                .is_err()
        );
        assert_eq!(fs::read(&path).unwrap(), b"# unchanged\n");
        drop(lock);
        assert!(
            grant
                .authorize(
                    &key(),
                    Instant::now() + Duration::from_secs(1),
                    &AtomicBool::new(true),
                    false
                )
                .is_err()
        );
        assert_eq!(fs::read(&path).unwrap(), b"# unchanged\n");
    }
}
