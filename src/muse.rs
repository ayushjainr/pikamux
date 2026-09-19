//! Bounded, metadata-only discovery for native Muse session logs.
//!
//! Muse's JSONL stream is an append-only event log.  This adapter deliberately
//! reads only small head/tail windows and accepts the durable metadata event;
//! it never interprets prompts, titles, lifecycle, or subagent streams.

use crate::{
    model::{Candidate, Provider},
    providers::ProviderSourceState,
};
use serde_json::Value;
use std::{
    collections::BTreeSet,
    fs,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    time::{Duration, Instant, UNIX_EPOCH},
};
use uuid::Uuid;

const MAX_FILES: usize = 2_000;
const MAX_DIRS: usize = 4_000;
const MAX_FILE_BYTES: u64 = 256 * 1024;
const MAX_LINE_BYTES: usize = 128 * 1024;
const MAX_SCAN_TIME: Duration = Duration::from_millis(250);
const MAX_SCAN_BYTES: u64 = 32 * 1024 * 1024;

/// Discover root Muse sessions, excluding nested `subagent` directories.
pub(crate) fn records(
    home: &Path,
    query: Option<&str>,
    identities: Option<&BTreeSet<String>>,
) -> Vec<Candidate> {
    let deadline = Instant::now() + MAX_SCAN_TIME;
    let sessions = home.join("sessions");
    if !regular_dir(&sessions) {
        return Vec::new();
    }
    let mut output = Vec::new();
    let mut dirs = 0usize;
    let mut files = 0usize;
    let mut byte_budget = MAX_SCAN_BYTES;
    for year in read_dirs(&sessions, &deadline, &mut dirs) {
        for month in read_dirs(&year, &deadline, &mut dirs) {
            for day in read_dirs(&month, &deadline, &mut dirs) {
                if Instant::now() >= deadline {
                    break;
                }
                let Ok(entries) = fs::read_dir(&day) else {
                    continue;
                };
                for entry in entries.flatten() {
                    if Instant::now() >= deadline || dirs >= MAX_DIRS || files >= MAX_FILES {
                        break;
                    }
                    dirs += 1;
                    let path = entry.path();
                    if !regular_dir(&path) {
                        continue;
                    }
                    let Some(id) = path.file_name().and_then(|v| v.to_str()) else {
                        continue;
                    };
                    if Uuid::parse_str(id).is_err() || query.is_some_and(|needle| needle != id) {
                        continue;
                    }
                    if identities.is_some_and(|wanted| !wanted.contains(id)) {
                        continue;
                    }
                    let log = path.join("session.jsonl");
                    if !regular_file(&log) {
                        continue;
                    }
                    files += 1;
                    if let Some(candidate) = read_candidate(&log, id, &deadline, &mut byte_budget) {
                        output.push(candidate);
                    }
                }
            }
        }
    }
    output.sort_by(|a, b| {
        b.updated_at
            .total_cmp(&a.updated_at)
            .then(a.session_id.cmp(&b.session_id))
    });
    output
}

/// Return Present only when the exact UUID/path has a valid Muse metadata event.
/// Missing or malformed data is Unknown: a bounded log scan cannot prove deletion.
pub(crate) fn source_state(home: &Path, id: &str) -> ProviderSourceState {
    if Uuid::parse_str(id).is_err() {
        return ProviderSourceState::Unknown;
    }
    records(home, Some(id), None)
        .into_iter()
        .next()
        .map_or(ProviderSourceState::Unknown, |_| {
            ProviderSourceState::Present
        })
}

fn read_dirs(root: &Path, deadline: &Instant, dirs: &mut usize) -> Vec<PathBuf> {
    if *dirs >= MAX_DIRS || Instant::now() >= *deadline {
        return Vec::new();
    }
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut paths = Vec::new();
    for entry in entries.flatten() {
        if *dirs >= MAX_DIRS || Instant::now() >= *deadline {
            break;
        }
        *dirs += 1;
        let path = entry.path();
        if regular_dir(&path) {
            paths.push(path);
        }
    }
    // Prefer recent date directories when the bounded scan cannot visit all history.
    paths.sort_by(|a, b| b.cmp(a));
    paths
}

fn read_candidate(
    path: &Path,
    id: &str,
    deadline: &Instant,
    byte_budget: &mut u64,
) -> Option<Candidate> {
    let bytes = head_tail(path, deadline, byte_budget)?;
    let updated_at = fs::metadata(path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0.0, |d| d.as_secs_f64());
    let mut candidate = None;
    for line in bytes
        .split(|b| *b == b'\n')
        .filter(|line| line.len() <= MAX_LINE_BYTES)
    {
        let Ok(value) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        if value.get("schema_version").and_then(Value::as_u64) != Some(1)
            || value.pointer("/stream/kind").and_then(Value::as_str) != Some("session")
            || value.pointer("/stream/id").and_then(Value::as_str) != Some(id)
            || value.get("payload_type").and_then(Value::as_str) != Some("runtime.session.metadata")
        {
            continue;
        }
        let Some(record) = value.pointer("/payload/record") else {
            continue;
        };
        let workspace = record
            .get("workspace_root")
            .and_then(Value::as_str)
            .filter(|v| {
                v.len() <= 8_192 && Path::new(v).is_absolute() && !v.chars().any(char::is_control)
            })
            .map(str::to_owned);
        let provider_id = record
            .get("provider_id")
            .and_then(Value::as_str)
            .filter(|v| !v.trim().is_empty());
        if provider_id.is_none() || workspace.is_none() {
            continue;
        }
        candidate = Some(Candidate {
            provider: Provider::Muse,
            session_id: id.to_owned(),
            name: None,
            cwd: workspace,
            branch: None,
            transcript_path: Some(path.to_string_lossy().into_owned()),
            model: record
                .get("model_id")
                .and_then(Value::as_str)
                .filter(|v| !v.is_empty() && v.len() <= 256 && !v.chars().any(char::is_control))
                .map(str::to_owned),
            updated_at,
            live: false,
            pid: None,
            source: "muse-metadata".into(),
            parent_session_id: None,
            created_at: value
                .get("recorded_at")
                .and_then(Value::as_f64)
                .map_or(updated_at, |micros| micros / 1_000_000.0),
            lifecycle_status: None,
        });
    }
    candidate
}

fn head_tail(path: &Path, deadline: &Instant, byte_budget: &mut u64) -> Option<Vec<u8>> {
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let mut file = options.open(path).ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() {
        return None;
    }
    let len = metadata.len();
    let window = MAX_FILE_BYTES.min(len).min(*byte_budget);
    if window == 0 {
        return None;
    }
    *byte_budget -= window;
    if len <= MAX_FILE_BYTES {
        let mut bytes = Vec::with_capacity(window as usize);
        file.take(window).read_to_end(&mut bytes).ok()?;
        return (Instant::now() < *deadline).then_some(bytes);
    }
    let head = window / 2;
    let tail = window - head;
    let mut bytes = Vec::with_capacity(window as usize);
    (&mut file).take(head).read_to_end(&mut bytes).ok()?;
    // Never join a truncated head record to an unrelated tail record.
    bytes.truncate(
        bytes
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |pos| pos + 1),
    );
    if tail > 0 && len > head {
        file.seek(SeekFrom::Start(len - tail)).ok()?;
        let mut suffix = Vec::new();
        (&mut file).take(tail).read_to_end(&mut suffix).ok()?;
        bytes.push(b'\n');
        if let Some(pos) = suffix.iter().position(|b| *b == b'\n') {
            bytes.extend_from_slice(&suffix[pos + 1..]);
        }
    }
    if Instant::now() >= *deadline {
        return None;
    }
    Some(bytes)
}

fn regular_file(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_file())
}
fn regular_dir(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_dir())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{create_dir_all, write};

    fn fixture(id: &str, stream_id: &str) -> (tempfile::TempDir, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_owned();
        let dir = root.join("sessions/2026/09/19").join(id);
        create_dir_all(&dir).unwrap();
        let line = serde_json::json!({
            "schema_version": 1,
            "stream": {"kind": "session", "id": stream_id},
            "recorded_at": 2_000_000,
            "payload_type": "runtime.session.metadata",
            "payload": {"record": {"workspace_root": "/tmp/work", "provider_id": "echo", "model_id": "m"}}
        }).to_string() + "\n";
        write(dir.join("session.jsonl"), line).unwrap();
        (temp, root)
    }

    #[test]
    fn invalid_workspace_metadata_is_rejected() {
        let id = "01a0bb09-7f64-78e1-8038-cfaf2b41a2aa";
        let (_temp, root) = fixture(id, id);
        let log = root
            .join("sessions/2026/09/19")
            .join(id)
            .join("session.jsonl");
        let mut value: Value = serde_json::from_str(&fs::read_to_string(&log).unwrap()).unwrap();
        for path in [
            "relative/path".to_owned(),
            "/tmp/escape\nline".to_owned(),
            format!("/{}", "x".repeat(8192)),
        ] {
            value["payload"]["record"]["workspace_root"] = Value::String(path);
            write(&log, serde_json::to_vec(&value).unwrap()).unwrap();
            assert!(records(&root, None, None).is_empty());
        }
    }

    #[test]
    fn identity_mismatch_is_rejected() {
        let id = "01a0bb09-7f64-78e1-8038-cfaf2b41a2aa";
        let (_temp, root) = fixture(id, "01a0bb09-7f64-78e1-8038-cfaf2b41a2ab");
        assert!(records(&root, None, None).is_empty());
    }

    #[test]
    fn metadata_helpers_and_controls() {
        let id = "01a0bb09-7f64-78e1-8038-cfaf2b41a2aa";
        let (_temp, root) = fixture(id, id);
        let found = records(&root, Some(id), None);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].provider, Provider::Muse);
        assert_eq!(found[0].model.as_deref(), Some("m"));
        let wanted = BTreeSet::from([id.to_owned()]);
        assert_eq!(records(&root, None, Some(&wanted)).len(), 1);
        assert_eq!(source_state(&root, id), ProviderSourceState::Present);
        assert_eq!(source_state(&root, "bad"), ProviderSourceState::Unknown);
    }

    #[test]
    fn subagents_and_symlinks_are_ignored() {
        let id = "01a0bb09-7f64-78e1-8038-cfaf2b41a2aa";
        let (_temp, root) = fixture(id, id);
        let parent = root.join("sessions/2026/09/19").join(id);
        let child = parent.join("subagent/550531b8-ee1c-48c6-a277-7c70c4300641");
        create_dir_all(&child).unwrap();
        write(child.join("session.jsonl"), b"bad").unwrap();
        assert_eq!(records(&root, None, None).len(), 1);
    }

    #[test]
    fn symlink_and_nested_helper_are_ignored() {
        let id = "01a0bb09-7f64-78e1-8038-cfaf2b41a2aa";
        let (_temp, root) = fixture(id, id);
        let day = root.join("sessions/2026/09/19");
        let nested = day
            .join("subagent")
            .join("550531b8-ee1c-48c6-a277-7c70c4300641");
        create_dir_all(&nested).unwrap();
        write(nested.join("session.jsonl"), b"bad").unwrap();
        let link = day.join("01a0bb09-7f64-78e1-8038-cfaf2b41a2ab");
        #[cfg(unix)]
        std::os::unix::fs::symlink(day.join(id), link).unwrap();
        assert_eq!(records(&root, None, None).len(), 1);
    }

    #[test]
    fn small_file_and_late_metadata_are_supported() {
        let id = "01a0bb09-7f64-78e1-8038-cfaf2b41a2aa";
        let (_temp, root) = fixture(id, id);
        let log = root
            .join("sessions/2026/09/19")
            .join(id)
            .join("session.jsonl");
        let line = br#"{"schema_version":1,"stream":{"kind":"session","id":"01a0bb09-7f64-78e1-8038-cfaf2b41a2aa"},"payload_type":"runtime.session.metadata","payload":{"record":{"workspace_root":"/tmp/new","provider_id":"echo","model_id":"new"}}}"#;
        write(&log, [b"garbage\n".as_slice(), line, b"\n"].concat()).unwrap();
        let found = records(&root, None, None);
        assert_eq!(found[0].cwd.as_deref(), Some("/tmp/new"));
        assert_eq!(found[0].model.as_deref(), Some("new"));
    }

    #[test]
    fn oversized_lines_are_skipped_without_unbounded_reads() {
        let id = "01a0bb09-7f64-78e1-8038-cfaf2b41a2aa";
        let (_temp, root) = fixture(id, id);
        let log = root
            .join("sessions/2026/09/19")
            .join(id)
            .join("session.jsonl");
        let mut bytes = vec![b'x'; MAX_LINE_BYTES + 1];
        bytes.extend_from_slice(b"\n");
        write(&log, bytes).unwrap();
        assert!(records(&root, None, None).is_empty());
    }
}
