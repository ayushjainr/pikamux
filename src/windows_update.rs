//! Confirmed Windows updates reuse the embedded, reviewed bootstrap, never a
//! downloaded script. Immutable version directories leave running clients alone.
use anyhow::Result;

#[cfg(not(windows))]
pub fn install(_version: Option<&str>, _reopen: bool) -> Result<i32> {
    anyhow::bail!("Windows client updates require Windows")
}

#[cfg(any(windows, test))]
mod managed {
    use anyhow::{Context, Result, bail};
    use serde::Deserialize;
    use std::{
        fs,
        io::Read,
        path::{Path, PathBuf},
    };

    #[derive(Deserialize)]
    pub struct Receipt {
        schema: u32,
        package: String,
        pub version: String,
        sha256: String,
        pub exe_sha256: Option<String>,
    }

    pub fn plain(path: &Path) -> Result<()> {
        for part in path.ancestors() {
            let meta = fs::symlink_metadata(part)?;
            if meta.file_type().is_symlink() {
                bail!("Client installation contains a link");
            }
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                if meta.file_attributes() & 0x400 != 0 {
                    bail!("Client installation contains a reparse point");
                }
            }
        }
        Ok(())
    }

    fn read(path: &Path, limit: u64) -> Result<Vec<u8>> {
        plain(path)?;
        let file = fs::File::open(path)?;
        let meta = file.metadata()?;
        if !meta.is_file() || meta.len() > limit {
            bail!("Invalid client installation file");
        }
        let mut bytes = Vec::new();
        file.take(limit + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > limit {
            bail!("Client installation file is too large");
        }
        Ok(bytes)
    }

    pub fn stable(value: &str) -> bool {
        let parts = value.split('.').collect::<Vec<_>>();
        parts.len() == 3
            && parts.iter().all(|part| {
                !part.is_empty()
                    && part.bytes().all(|b| b.is_ascii_digit())
                    && part.parse::<u32>().is_ok()
            })
    }

    fn digest(value: &str) -> bool {
        value.len() == 64
            && value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    }

    pub fn active(root: &Path) -> Result<(Receipt, PathBuf)> {
        if read(&root.join(".pika-client-install"), 128)? != b"pika-windows-installer-v1" {
            bail!("Unrecognized client installation");
        }
        let receipt: Receipt = serde_json::from_slice(&read(&root.join("install.json"), 4096)?)?;
        if receipt.schema != 1
            || receipt.package != "pikamux"
            || !stable(&receipt.version)
            || !digest(&receipt.sha256)
            || receipt
                .exe_sha256
                .as_ref()
                .is_some_and(|hash| !digest(hash))
        {
            bail!("Invalid client installation receipt");
        }
        let executable = root
            .join("releases")
            .join(format!("{}-{}", receipt.version, &receipt.sha256[..12]))
            .join("pika.exe");
        plain(&executable)?;
        let metadata = fs::metadata(&executable)?;
        if !metadata.is_file()
            || metadata.len() == 0
            || metadata.len() > crate::update::MAX_EXECUTABLE_BYTES
        {
            bail!("Invalid installed client executable");
        }
        if let Some(expected) = &receipt.exe_sha256 {
            use sha2::{Digest, Sha256};
            let bytes = read(&executable, crate::update::MAX_EXECUTABLE_BYTES)?;
            if format!("{:x}", Sha256::digest(bytes)) != *expected {
                bail!("Installed client bytes changed; update refused");
            }
        }
        Ok((receipt, executable))
    }

    pub fn root_for(executable: &Path) -> Result<PathBuf> {
        let release = executable.parent().context("Missing release directory")?;
        let releases = release.parent().context("Missing releases directory")?;
        if executable.file_name().is_none_or(|name| name != "pika.exe")
            || releases.file_name().is_none_or(|name| name != "releases")
        {
            bail!("This Pika is not in a managed client installation");
        }
        Ok(releases.parent().context("Missing client root")?.to_owned())
    }

    pub fn newer_active(executable: &Path, running_version: &str) -> Result<Option<PathBuf>> {
        let Ok(root) = root_for(executable) else {
            return Ok(None);
        };
        if !root.join(".pika-client-install").exists() {
            return Ok(None);
        }
        let (receipt, active) = active(&root)?;
        if crate::update::compare_versions(&receipt.version, running_version)?.is_gt()
            && receipt.exe_sha256.is_some()
            && fs::canonicalize(executable)? != fs::canonicalize(&active)?
        {
            return Ok(Some(active));
        }
        Ok(None)
    }
}

#[cfg(windows)]
pub fn forward(args: &[std::ffi::OsString]) -> Result<Option<i32>> {
    // Staged candidates are outside releases/, so installer probes never forward.
    let executable = std::env::current_exe()?;
    if let Some(active) = managed::newer_active(&executable, crate::VERSION)? {
        return Ok(Some(
            std::process::Command::new(active)
                .args(args.iter().skip(1))
                .status()?
                .code()
                .unwrap_or(1),
        ));
    }
    Ok(None)
}

#[cfg(windows)]
struct UpdateScript(std::path::PathBuf);

#[cfg(windows)]
impl UpdateScript {
    fn create() -> Result<Self> {
        use std::io::Write;
        let directory = std::env::temp_dir().join(format!("pika-update-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory)?;
        let script = Self(directory.join("update.ps1"));
        let mut file = std::fs::File::create_new(&script.0)?;
        file.write_all(include_bytes!("../install.ps1"))?;
        file.sync_all()?;
        Ok(script)
    }
}

#[cfg(windows)]
impl Drop for UpdateScript {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
        if let Some(directory) = self.0.parent() {
            let _ = std::fs::remove_dir(directory);
        }
    }
}

#[cfg(windows)]
pub fn install(version: Option<&str>, reopen: bool) -> Result<i32> {
    use anyhow::{Context, bail};
    use std::{fs, path::PathBuf, process::Command};
    let executable = std::env::current_exe()?;
    let root = managed::root_for(&executable)?;
    let (before, active) = managed::active(&root)?;
    let expected_root =
        PathBuf::from(std::env::var_os("LOCALAPPDATA").context("LOCALAPPDATA is unavailable")?)
            .join("Pika/Client");
    if fs::canonicalize(&root)? != fs::canonicalize(expected_root)?
        || fs::canonicalize(&active)? != fs::canonicalize(&executable)?
        || before.version != crate::VERSION
    {
        bail!("Update requires the active managed Pika client; close this copy and reopen Pika");
    }
    if version.is_some_and(|value| !managed::stable(value)) {
        bail!("Invalid stable update version");
    }
    let script = UpdateScript::create()?;
    let powershell =
        PathBuf::from(std::env::var_os("SystemRoot").context("SystemRoot is unavailable")?)
            .join("System32/WindowsPowerShell/v1.0/powershell.exe");
    let mut command = Command::new(powershell);
    command
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
        ])
        .arg(&script.0);
    if let Some(version) = version {
        command.arg("-PikaInstallVersion").arg(version);
    }
    println!("Updating Pika on this machine. Running agents stay running.");
    let status = command
        .status()
        .context("Could not start the Windows updater")?;
    if !status.success() {
        bail!("Update did not complete. Reopen Pika to retry; running agents were not stopped");
    }
    let (after, active) = managed::active(&root)?;
    if after.exe_sha256.is_none() || version.is_some_and(|v| v != after.version) {
        bail!("Update receipt did not match the requested release; board not restarted");
    }
    drop(script);
    if reopen {
        return Ok(Command::new(active).status()?.code().unwrap_or(1));
    }
    println!("Pika {} is ready.", after.version);
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::managed::*;
    use std::fs;
    fn fixture() -> (tempfile::TempDir, std::path::PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        // Canonicalize /tmp on macOS: production rejects symlink ancestors.
        let root = temp.path().canonicalize().unwrap();
        fs::write(
            root.join(".pika-client-install"),
            "pika-windows-installer-v1",
        )
        .unwrap();
        let exe = root.join("releases/9.0.0-aaaaaaaaaaaa/pika.exe");
        fs::create_dir_all(exe.parent().unwrap()).unwrap();
        fs::write(&exe, b"fixture executable").unwrap();
        let hash = crate::update::sha256_file(&exe).unwrap();
        fs::write(root.join("install.json"), serde_json::json!({"schema":1,"package":"pikamux","version":"9.0.0","sha256":"a".repeat(64),"exe_sha256":hash}).to_string()).unwrap();
        (temp, exe)
    }
    #[test]
    fn active_receipt_binds_executable_and_refuses_tampering() {
        let (_temp, exe) = fixture();
        let root = root_for(&exe).unwrap();
        assert_eq!(active(&root).unwrap().1, exe);
        fs::write(&exe, b"tampered").unwrap();
        assert!(active(&root).is_err());
    }
    #[test]
    fn old_path_forwards_only_to_verified_newer_release() {
        let (_temp, exe) = fixture();
        let old = root_for(&exe)
            .unwrap()
            .join("releases/8.0.0-bbbbbbbbbbbb/pika.exe");
        fs::create_dir_all(old.parent().unwrap()).unwrap();
        fs::write(&old, b"old").unwrap();
        assert_eq!(newer_active(&old, "8.0.0").unwrap(), Some(exe.clone()));
        assert_eq!(newer_active(&exe, "9.0.0").unwrap(), None);
        assert_eq!(newer_active(&old, "10.0.0").unwrap(), None);
    }
    #[test]
    fn malformed_versions_cannot_supply_paths_or_powershell_arguments() {
        for value in [
            "../1.2.3",
            "1.2.3;exit",
            "1.2",
            "1.2.3-rc.1",
            "1.2.3\n",
            "9999999999999.1.1",
        ] {
            assert!(!stable(value));
        }
        assert!(stable("0.6.11"));
    }

    #[test]
    fn legacy_receipt_is_readable_but_cannot_authorize_forwarding() {
        let (_temp, exe) = fixture();
        let root = root_for(&exe).unwrap();
        let receipt_path = root.join("install.json");
        let mut receipt: serde_json::Value =
            serde_json::from_slice(&fs::read(&receipt_path).unwrap()).unwrap();
        receipt.as_object_mut().unwrap().remove("exe_sha256");
        fs::write(&receipt_path, receipt.to_string()).unwrap();
        assert!(active(&root).unwrap().0.exe_sha256.is_none());
        assert_eq!(newer_active(&exe, "8.0.0").unwrap(), None);
        receipt["version"] = "../../elsewhere".into();
        fs::write(&receipt_path, receipt.to_string()).unwrap();
        assert!(active(&root).is_err());
    }

    #[test]
    fn oversized_receipt_and_foreign_marker_are_rejected() {
        let (_temp, exe) = fixture();
        let root = root_for(&exe).unwrap();
        fs::write(root.join("install.json"), vec![b' '; 4097]).unwrap();
        assert!(active(&root).is_err());
        fs::write(root.join(".pika-client-install"), "foreign").unwrap();
        assert!(active(&root).is_err());
    }
}
