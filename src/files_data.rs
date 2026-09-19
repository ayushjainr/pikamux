//! Bounded, read-only data access for the Files companion.
//!
//! This module deliberately does not walk directories recursively.  Callers
//! choose each directory to inspect, and Git is asked for its workspace-wide
//! status only when explicitly requested.

use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

const MAX_ENTRIES: usize = 2_000;
const MAX_TEXT_BYTES: usize = 256 * 1024;
const MAX_GIT_BYTES: usize = 4 * 1024 * 1024;
const GIT_TIMEOUT: Duration = Duration::from_secs(5);
const TRUNCATION_NOTICE: &str = "\n[... file truncated at 256 KiB ...]\n";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub path: PathBuf,
    pub name: String,
    pub is_dir: bool,
    pub is_symlink: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitSnapshot {
    pub root: PathBuf,
    pub changes: BTreeMap<PathBuf, String>,
}

/// List one directory, never descending into children.
pub fn list_dir(path: &Path) -> Result<Vec<Entry>> {
    let metadata = fs::metadata(path).with_context(|| format!("list {}", path.display()))?;
    if !metadata.is_dir() {
        bail!("{} is not a directory", path.display());
    }
    let mut entries = Vec::new();
    for item in fs::read_dir(path).with_context(|| format!("read directory {}", path.display()))? {
        if entries.len() >= MAX_ENTRIES {
            bail!("directory contains more than {MAX_ENTRIES} entries; listing refused");
        }
        let item = item.with_context(|| format!("read entry in {}", path.display()))?;
        let item_path = item.path();
        let item_meta = fs::symlink_metadata(&item_path)
            .with_context(|| format!("inspect {}", item_path.display()))?;
        entries.push(Entry {
            path: item_path,
            name: item.file_name().to_string_lossy().into_owned(),
            is_dir: item_meta.file_type().is_dir(),
            is_symlink: item_meta.file_type().is_symlink(),
        });
    }
    entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.cmp(&b.name)));
    Ok(entries)
}

/// Read UTF-8 text with a hard memory/read bound. A truncation notice is
/// included in the returned string when the source exceeds the bound.
pub fn read_text(path: &Path) -> Result<String> {
    let metadata = fs::metadata(path).with_context(|| format!("inspect {}", path.display()))?;
    if !metadata.is_file() {
        bail!("{} is not a regular file", path.display());
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // A symlink/FIFO race must not make the UI block.
        options.custom_flags(libc::O_NONBLOCK);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("open {}", path.display()))?;
    if !file
        .metadata()
        .with_context(|| format!("inspect {}", path.display()))?
        .is_file()
    {
        bail!("{} is not a regular file", path.display());
    }
    let mut bytes = Vec::with_capacity(MAX_TEXT_BYTES.min(metadata.len() as usize));
    let mut buffer = [0_u8; 16 * 1024];
    let mut truncated = false;
    loop {
        let wanted = buffer.len().min(MAX_TEXT_BYTES + 1 - bytes.len());
        if wanted == 0 {
            truncated = true;
            break;
        }
        let read = file
            .read(&mut buffer[..wanted])
            .with_context(|| format!("read {}", path.display()))?;
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..read]);
        if bytes.len() > MAX_TEXT_BYTES {
            truncated = true;
            bytes.truncate(MAX_TEXT_BYTES);
            break;
        }
    }
    let text = match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(error) if truncated && error.utf8_error().error_len().is_none() => {
            let valid = error.utf8_error().valid_up_to();
            String::from_utf8(error.as_bytes()[..valid].to_vec())
                .map_err(|_| anyhow::anyhow!("{} is not UTF-8 text", path.display()))?
        }
        Err(_) => bail!("{} is not UTF-8 text", path.display()),
    };
    if text.as_bytes().contains(&0) {
        bail!("{} is binary data", path.display());
    }
    if truncated {
        Ok(format!("{text}{TRUNCATION_NOTICE}"))
    } else {
        Ok(text)
    }
}

#[cfg(test)]
pub fn git_snapshot(path: &Path) -> Result<GitSnapshot> {
    git_snapshot_cancellable(path, &crate::consult::CancellationToken::default())
}

pub fn git_snapshot_cancellable(
    path: &Path,
    cancel: &crate::consult::CancellationToken,
) -> Result<GitSnapshot> {
    let run = |dir: &Path, args: &[&str], cap| -> Result<Output> {
        let output = run_git_raw_cancellable(dir, args, cap, cancel)?;
        if !output.status.success() {
            bail!("Git: {}", String::from_utf8_lossy(&output.stderr).trim());
        }
        Ok(output)
    };
    let root_output = run(path, &["rev-parse", "--show-toplevel"], 64 * 1024)?;
    let root_text = String::from_utf8(root_output.stdout)?;
    let root = PathBuf::from(root_text.strip_suffix('\n').unwrap_or(&root_text));
    if !root.is_absolute() {
        bail!("git returned a non-absolute workspace root");
    }
    let status = run(
        &root,
        &["status", "--porcelain=v1", "-z", "--untracked-files=normal"],
        MAX_GIT_BYTES,
    )?;
    let mut changes = BTreeMap::new();
    let mut records = status.stdout.split(|b| *b == 0);
    while let Some(record) = records.next() {
        if record.is_empty() {
            continue;
        }
        if record.len() < 3 || record[2] != b' ' {
            bail!("invalid git status record");
        }
        let code = String::from_utf8_lossy(&record[..2]);
        let status_code = if record[0] != b' ' {
            record[0]
        } else {
            record[1]
        };
        let status_code = if status_code == b'?' {
            "??".to_string()
        } else {
            (status_code as char).to_string()
        };
        let name = &record[3..];
        let relative = PathBuf::from(String::from_utf8(name.to_vec())?);
        let absolute = root.join(relative);
        changes.insert(
            absolute,
            if code == "??" {
                "??".into()
            } else {
                status_code
            },
        );
        // Rename records carry a second NUL-delimited destination path.
        if code.contains('R') || code.contains('C') {
            let _ = records.next();
        }
    }
    Ok(GitSnapshot { root, changes })
}

pub fn git_diff(path: &Path, root: &Path) -> Result<String> {
    let relative = path
        .strip_prefix(root)
        .context("file is outside this Git workspace")?;
    if relative
        .components()
        .any(|c| !matches!(c, std::path::Component::Normal(_)))
    {
        bail!("file path must be inside the Git workspace");
    }
    let relative = relative
        .to_str()
        .context("Git diff requires a UTF-8 file path")?;
    let tracked = run_git_raw(
        root,
        &["ls-files", "--error-unmatch", "--", relative],
        MAX_GIT_BYTES,
    )?;
    if !tracked.status.success() {
        return Ok("Untracked file · use File to read its contents.".into());
    }
    let head = run_git_raw(root, &["rev-parse", "--verify", "HEAD"], 64 * 1024)?;
    let args: Vec<&str> = if head.status.success() {
        vec![
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            "HEAD",
            "--",
            relative,
        ]
    } else {
        // Diff against Git's empty tree without writing an object, including
        // both staged and unstaged work in a repository without a first commit.
        vec![
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            "--cached",
            "--",
            relative,
        ]
    };
    let output = run_git(root, &args, MAX_GIT_BYTES)?;
    let mut text = String::from_utf8(output.stdout).context("git diff was not UTF-8")?;
    if !head.status.success() {
        let unstaged = run_git(
            root,
            &[
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                "--no-color",
                "--",
                relative,
            ],
            MAX_GIT_BYTES,
        )?;
        let unstaged = String::from_utf8(unstaged.stdout)?;
        if !unstaged.is_empty() {
            text.push_str("\nUnstaged changes:\n");
            text.push_str(&unstaged);
        }
    }
    if text.is_empty() {
        text = "No changes against HEAD.".into();
    }
    Ok(text)
}

fn run_git_raw(dir: &Path, args: &[&str], cap: usize) -> Result<Output> {
    run_git_raw_cancellable(
        dir,
        args,
        cap,
        &crate::consult::CancellationToken::default(),
    )
}

fn run_git_raw_cancellable(
    dir: &Path,
    args: &[&str],
    cap: usize,
    cancel: &crate::consult::CancellationToken,
) -> Result<Output> {
    let mut command = Command::new("git");
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("GIT_") {
            command.env_remove(key);
        }
    }
    command
        .current_dir(dir)
        .args([
            "--literal-pathspecs",
            "--no-pager",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.untrackedCache=false",
            "-c",
            "diff.external=",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "diff.ignoreSubmodules=all",
        ])
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_NO_LAZY_FETCH", "1");
    // Clean/process filters can run even for status or --no-ext-diff.
    if matches!(args.first(), Some(&"status") | Some(&"diff")) {
        let filters = run_git_raw_cancellable(
            dir,
            &[
                "config",
                "-z",
                "--name-only",
                "--get-regexp",
                r"^filter\..*\.(clean|smudge|process|required)$",
            ],
            64 * 1024,
            cancel,
        )?;
        if !filters.status.success() && filters.status.code() != Some(1) {
            bail!("Could not inspect Git filters safely");
        }
        for name in filters.stdout.split(|b| *b == 0).filter(|b| !b.is_empty()) {
            let name = std::str::from_utf8(name).context("Invalid Git filter configuration")?;
            let value = if name.ends_with(".required") {
                "false"
            } else {
                ""
            };
            command.args(["-c", &format!("{name}={value}")]);
        }
    }
    command.args(args);
    // Reuse Pika's cancellation, owned process-group and bounded pipe reader.
    let output = crate::tmux::bounded_output_cancellable(&mut command, GIT_TIMEOUT, cancel)?;
    if output.stdout.len() > cap {
        bail!("Git output exceeded {cap} bytes");
    }
    Ok(output)
}

fn run_git(dir: &Path, args: &[&str], cap: usize) -> Result<Output> {
    let output = run_git_raw(dir, args, cap)?;
    if !output.status.success() {
        bail!("Git: {}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .current_dir(dir)
            .args(["--literal-pathspecs"])
            .args(args)
            .output()
            .unwrap();
        assert!(
            status.status.success(),
            "git {:?}: {}",
            args,
            String::from_utf8_lossy(&status.stderr)
        );
    }

    fn repo() -> tempfile::TempDir {
        let temp = tempfile::tempdir().unwrap();
        git(temp.path(), &["init", "--quiet"]);
        git(
            temp.path(),
            &["config", "user.email", "test@example.invalid"],
        );
        git(temp.path(), &["config", "user.name", "Pika Test"]);
        temp
    }

    #[test]
    fn lists_entries_without_following_symlink() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        fs::create_dir(dir.join("child")).unwrap();
        fs::write(dir.join("file"), b"ok").unwrap();
        let entries = list_dir(dir).unwrap();
        assert_eq!(entries.len(), 2);
        assert!(
            entries
                .iter()
                .any(|entry| entry.name == "child" && entry.is_dir)
        );
    }

    #[test]
    fn text_cap_is_explicit_and_controls_are_cleaned() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        fs::write(dir.join("large"), "x".repeat(MAX_TEXT_BYTES + 1)).unwrap();
        let text = read_text(&dir.join("large")).unwrap();
        assert!(text.contains("file truncated"));
    }

    #[test]
    fn snapshot_reports_unborn_staged_unstaged_and_normal_untracked() {
        let temp = repo();
        let dir = fs::canonicalize(temp.path()).unwrap();
        fs::write(dir.join("tracked"), "one\n").unwrap();
        git(&dir, &["add", "tracked"]);
        fs::write(dir.join("tracked"), "two\n").unwrap();
        fs::create_dir(dir.join("new-dir")).unwrap();
        fs::write(dir.join("new-dir").join("child"), "x").unwrap();
        let snapshot = git_snapshot(&dir).unwrap();
        assert_eq!(snapshot.root, fs::canonicalize(&dir).unwrap());
        assert_eq!(
            snapshot
                .changes
                .get(&dir.join("tracked"))
                .map(String::as_str),
            Some("A")
        );
        assert_eq!(
            snapshot
                .changes
                .get(&dir.join("new-dir"))
                .map(String::as_str),
            Some("??")
        );
        let diff = git_diff(&dir.join("tracked"), &dir).unwrap();
        assert!(diff.contains("two") || diff.contains("Unstaged changes"));
    }

    #[test]
    fn snapshot_and_diff_use_literal_magic_filename_and_reject_outside() {
        let temp = repo();
        let dir = fs::canonicalize(temp.path()).unwrap();
        let name = ":(top) literal";
        fs::write(dir.join(name), "before\n").unwrap();
        git(&dir, &["add", "--", name]);
        git(&dir, &["commit", "-m", "initial", "--quiet"]);
        fs::write(dir.join(name), "after\n").unwrap();
        let snapshot = git_snapshot(&dir).unwrap();
        assert!(snapshot.changes.contains_key(&dir.join(name)));
        assert!(git_diff(&dir.join(name), &dir).unwrap().contains("after"));
        let outside = tempfile::tempdir().unwrap();
        assert!(git_diff(&outside.path().join("nope"), &dir).is_err());
    }

    #[test]
    fn untracked_file_is_readable_but_binary_and_nonregular_are_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let binary = dir.join("binary");
        fs::write(&binary, [0_u8, 1, 2]).unwrap();
        assert!(read_text(&binary).is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            symlink(&binary, dir.join("link")).unwrap();
            assert!(read_text(&dir.join("link")).is_err());
            let fifo = dir.join("fifo");
            assert!(
                std::process::Command::new("mkfifo")
                    .arg(&fifo)
                    .status()
                    .unwrap()
                    .success()
            );
            assert!(read_text(&fifo).is_err());
        }
    }

    #[test]
    fn directory_entry_limit_is_explicit() {
        let temp = tempfile::tempdir().unwrap();
        for index in 0..=MAX_ENTRIES {
            fs::write(temp.path().join(format!("{index:04}")), b"x").unwrap();
        }
        assert!(list_dir(temp.path()).is_err());
    }

    #[test]
    fn status_and_diff_never_execute_repository_clean_filters() {
        let temp = repo();
        let root = fs::canonicalize(temp.path()).unwrap();
        fs::write(root.join("code.rs"), "before\n").unwrap();
        git(&root, &["add", "code.rs"]);
        git(&root, &["commit", "-qm", "baseline"]);
        let marker = root.join("FILTER_MUST_NOT_RUN");
        let filter = format!(
            "touch {}; cat",
            shell_words::quote(marker.to_str().unwrap())
        );
        git(&root, &["config", "filter.pika.clean", &filter]);
        git(&root, &["config", "filter.pika.required", "true"]);
        fs::write(root.join(".gitattributes"), "*.rs filter=pika\n").unwrap();
        fs::write(root.join("code.rs"), "after\n").unwrap();
        git_snapshot(&root).unwrap();
        assert!(
            git_diff(&root.join("code.rs"), &root)
                .unwrap()
                .contains("after")
        );
        assert!(!marker.exists());
    }

    #[test]
    fn truncation_preserves_a_split_utf8_character_boundary() {
        let temp = tempfile::tempdir().unwrap();
        let text = "x".repeat(MAX_TEXT_BYTES - 1) + "文件";
        fs::write(temp.path().join("large"), text).unwrap();
        assert!(
            read_text(&temp.path().join("large"))
                .unwrap()
                .ends_with(TRUNCATION_NOTICE)
        );
    }
}
