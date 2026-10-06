//! Launch-only native compatibility probe; unknown runtimes retain ordinary launch.
use crate::{consult::CancellationToken, fleet::run_bounded_command_cancellable};
use anyhow::{Context, Result};
use std::{
    fs,
    os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

struct Scratch(PathBuf, u64, u64);
impl Scratch {
    fn create() -> Result<Self> {
        let path =
            std::env::temp_dir().join(format!("pika-opencode-version-{}", uuid::Uuid::new_v4()));
        fs::DirBuilder::new().mode(0o700).create(&path)?;
        let metadata = fs::symlink_metadata(&path)?;
        Ok(Self(path, metadata.dev(), metadata.ino()))
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        if fs::symlink_metadata(&self.0).is_ok_and(|metadata| {
            metadata.is_dir() && metadata.dev() == self.1 && metadata.ino() == self.2
        }) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}

pub(super) fn verified(executable: &str) -> bool {
    probe(executable, Duration::from_secs(2)).unwrap_or(false)
}

fn resolve(executable: &str) -> Result<PathBuf> {
    if Path::new(executable).components().count() > 1 {
        return fs::canonicalize(executable).context("Native executable unavailable");
    }
    let path = std::env::var_os("PATH").context("Native executable search unavailable")?;
    std::env::split_paths(&path)
        .map(|directory| directory.join(executable))
        .find(|candidate| {
            fs::metadata(candidate).is_ok_and(|metadata| {
                metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
            })
        })
        .map(fs::canonicalize)
        .context("Native executable unavailable")?
        .context("Native executable resolution failed")
}

fn probe(executable: &str, timeout: Duration) -> Result<bool> {
    let executable = resolve(executable)?;
    let scratch = Scratch::create()?;
    let mut command = Command::new(executable);
    command.env_clear().current_dir(&scratch.0).arg("--version");
    private_environment(&mut command, &scratch.0)?;
    let output = run_bounded_command_cancellable(
        &mut command,
        None,
        timeout,
        128,
        128,
        &CancellationToken::default(),
    )?;
    Ok(output.status.success()
        && matches!(
            std::str::from_utf8(&output.stdout)?.trim(),
            "1.18.31" | "1.18.33"
        ))
}

fn private_environment(command: &mut Command, root: &Path) -> Result<()> {
    for (key, folder) in [
        ("HOME", "home"),
        ("XDG_CONFIG_HOME", "config"),
        ("XDG_DATA_HOME", "data"),
        ("XDG_STATE_HOME", "state"),
        ("XDG_CACHE_HOME", "cache"),
        ("TMPDIR", "tmp"),
        ("OPENCODE_CONFIG_DIR", "config/opencode"),
        ("OPENCODE_DATA_HOME", "data/opencode"),
    ] {
        let directory = root.join(folder);
        fs::create_dir_all(&directory)?;
        command.env(key, directory);
    }
    command
        .env("PATH", "/usr/bin:/bin")
        .env("OPENCODE_DISABLE_MODELS_FETCH", "1")
        .env("OPENCODE_DISABLE_AUTOUPDATE", "1")
        .env("OPENCODE_DISABLE_DEFAULT_PLUGINS", "1")
        .env("OPENCODE_DISABLE_PROJECT_CONFIG", "1")
        .env("OPENCODE_DISABLE_EXTERNAL_SKILLS", "1")
        .env("OPENCODE_DISABLE_LSP_DOWNLOAD", "1");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn script(root: &Path, body: &str) -> PathBuf {
        let path = root.join("native");
        fs::write(
            &path,
            format!("#!/bin/sh\n[ \"$1\" = --version ] || exit 99\n{body}\n"),
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    #[test]
    fn only_verified_versions_admit_shared_launch_and_probe_is_private() {
        for (version, accepted) in [
            ("1.18.31", true),
            ("1.18.33", true),
            ("1.18.32", false),
            ("opencode 1.18.33", false),
            ("1.18.33 extra", false),
        ] {
            let root = tempfile::tempdir().unwrap();
            let path = script(
                root.path(),
                &format!(
                    "[ \"$HOME\" != '{}' ] || exit 98\n[ -z \"$OPENCODE_CONFIG_CONTENT\" ] || exit 97\nprintf '%s\\n' '{version}'",
                    root.path().display()
                ),
            );
            assert_eq!(
                probe(path.to_str().unwrap(), Duration::from_secs(1)).unwrap(),
                accepted
            );
        }
    }

    #[test]
    fn stalled_oversized_or_failed_probe_never_admits_shared_launch() {
        for body in [
            "sleep 5",
            "printf '1.18.33\\n'; exit 1",
            "head -c 1024 /dev/zero",
        ] {
            let root = tempfile::tempdir().unwrap();
            let path = script(root.path(), body);
            let start = std::time::Instant::now();
            assert!(!probe(path.to_str().unwrap(), Duration::from_millis(150)).unwrap_or(false));
            assert!(start.elapsed() < Duration::from_secs(2));
        }
    }
}
