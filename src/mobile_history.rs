//! Bounded, connect-free reads of one provider-owned history. No provider is launched.
//! The caller owns watched/node authorization; this module owns exact source identity.
use crate::{config::Config, model::Provider, paths::Paths, providers::Providers};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{Read, Seek, SeekFrom},
    os::unix::fs::MetadataExt,
    path::Path,
};

const WINDOW: u64 = 4 * 1024 * 1024;
const RECORD: usize = 256 * 1024;
const PAGE: usize = 40;

#[path = "mobile_claude_ancestry.rs"]
mod claude_ancestry;
#[path = "mobile_claude_history.rs"]
mod claude_projection;
#[path = "mobile_claude_reader.rs"]
mod claude_reader;

#[derive(Serialize, Deserialize)]
struct Cursor {
    provider: Provider,
    id: String,
    dev: u64,
    inode: u64,
    snapshot: u64,
    before: u64,
    prefix_len: usize,
    prefix_hash: String,
    sql_before: Option<(i64, String)>,
    #[serde(default)]
    displayed_runs: BTreeSet<String>,
    #[serde(default)]
    claude_before: Option<usize>,
    #[serde(default)]
    claude_items_hash: Option<String>,
    #[serde(default)]
    claude_source_hash: Option<String>,
}

/// Read the latest page, or an older snapshot page using its opaque cursor.
pub(crate) fn page(
    paths: &Paths,
    config: &Config,
    provider: Provider,
    id: &str,
    cursor: Option<&str>,
) -> Result<Value> {
    let candidate = exact_candidate(paths, config, provider, id)?;
    let source = Path::new(
        candidate
            .transcript_path
            .as_deref()
            .context("Provider has no exact history source")?,
    );
    let root = match provider {
        Provider::Claude => &paths.claude_home,
        Provider::Opencode => &paths.opencode_data_home,
        Provider::Muse => &paths.muse_data_home,
        Provider::Codex => unreachable!(),
    };
    verify_source(root, source)?;
    let mut file = File::open(source)?;
    let metadata = file.metadata()?;
    let mut state = prepare_cursor(&mut file, provider, id, cursor)?;
    let (items, more) = read_items(paths, &mut file, source, &mut state, cursor.is_none())?;
    verify_finished_source(root, source, &metadata)?;
    let next = if more {
        Some(serde_json::to_string(&state)?)
    } else {
        None
    };
    Ok(history_result(&candidate, items, next))
}

fn read_items(
    paths: &Paths,
    file: &mut File,
    source: &Path,
    state: &mut Cursor,
    initial: bool,
) -> Result<(Vec<Value>, bool)> {
    if state.provider == Provider::Opencode {
        return sql_page(source, &state.id.clone(), state, initial);
    }
    verify_snapshot(file, state)?;
    if state.provider == Provider::Claude {
        return claude_reader::page(paths, file, state, initial);
    }
    jsonl_page(file, state)
}

fn history_result(
    candidate: &crate::model::Candidate,
    items: Vec<Value>,
    next: Option<String>,
) -> Value {
    let turns: Vec<_> = items
        .into_iter()
        .map(|item| json!({"id":item["id"],"items":[item]}))
        .collect();
    json!({"thread":{"id":candidate.session_id,"title":candidate.name,"cwd":candidate.cwd},"turns":{"data":turns,"nextCursor":next,"order":"chronological"},"readOnly":true})
}

fn exact_candidate(
    paths: &Paths,
    config: &Config,
    provider: Provider,
    id: &str,
) -> Result<crate::model::Candidate> {
    ensure!(
        Providers::valid_id(provider, id),
        "Invalid provider identity"
    );
    ensure!(
        provider != Provider::Codex,
        "Codex history uses its running server"
    );
    let identities = BTreeSet::from([id.to_owned()]);
    let candidates = Providers::new(paths, config).tracked(provider, &identities);
    let exact = candidates
        .iter()
        .filter(|c| c.session_id == id)
        .collect::<Vec<_>>();
    ensure!(
        exact.len() == 1,
        "Exact provider history is missing or ambiguous"
    );
    Ok(exact[0].clone())
}

pub(crate) fn source_path(
    paths: &Paths,
    config: &Config,
    provider: Provider,
    id: &str,
) -> Result<std::path::PathBuf> {
    let path = exact_candidate(paths, config, provider, id)?
        .transcript_path
        .context("Provider has no exact history source")?;
    Ok(path.into())
}

/// Only a separately certified live client may treat this registry-only state as
/// an empty conversation. Saved-history readers still require an actual source.
pub(crate) fn claude_registry_without_history(
    paths: &Paths,
    config: &Config,
    id: &str,
) -> Result<bool> {
    let candidate = exact_candidate(paths, config, Provider::Claude, id)?;
    Ok(candidate.transcript_path.is_none()
        && matches!(
            candidate.source.as_str(),
            "claude-live" | "claude-live-custom"
        ))
}

fn prepare_cursor(
    file: &mut File,
    provider: Provider,
    id: &str,
    cursor: Option<&str>,
) -> Result<Cursor> {
    let metadata = file.metadata()?;
    if let Some(encoded) = cursor {
        ensure!(encoded.len() <= 4096, "History cursor exceeds its bound");
        let state: Cursor = serde_json::from_str(encoded).context("Invalid history cursor")?;
        ensure!(
            state.provider == provider && state.id == id,
            "History cursor belongs to another identity"
        );
        ensure!(
            state.dev == metadata.dev() && state.inode == metadata.ino(),
            "History source was replaced; reopen it"
        );
        ensure!(
            state.prefix_len <= 4096 && state.before <= state.snapshot,
            "Invalid history boundary"
        );
        Ok(state)
    } else {
        let prefix_len = metadata.len().min(4096) as usize;
        Ok(Cursor {
            provider,
            id: id.into(),
            dev: metadata.dev(),
            inode: metadata.ino(),
            snapshot: metadata.len(),
            before: metadata.len(),
            prefix_len,
            prefix_hash: prefix_hash(file, prefix_len)?,
            sql_before: None,
            displayed_runs: BTreeSet::new(),
            claude_before: None,
            claude_items_hash: None,
            claude_source_hash: None,
        })
    }
}

fn verify_snapshot(file: &mut File, state: &Cursor) -> Result<()> {
    ensure!(
        file.metadata()?.len() >= state.snapshot,
        "History was truncated; reopen it"
    );
    ensure!(
        prefix_hash(file, state.prefix_len)? == state.prefix_hash,
        "History prefix changed; reopen it"
    );
    Ok(())
}

fn verify_finished_source(root: &Path, source: &Path, metadata: &fs::Metadata) -> Result<()> {
    let current = fs::symlink_metadata(source)?;
    verify_source(root, source)?;
    ensure!(
        current.is_file() && current.dev() == metadata.dev() && current.ino() == metadata.ino(),
        "History source changed during read"
    );
    Ok(())
}

fn verify_source(root: &Path, source: &Path) -> Result<()> {
    ensure!(
        root.is_absolute() && source.is_absolute(),
        "History path is not absolute"
    );
    ensure!(
        !source
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir)),
        "Unsafe history path"
    );
    ensure!(
        source.starts_with(root),
        "History source is outside its provider root"
    );
    let mut current = Some(source);
    while let Some(path) = current {
        let meta = fs::symlink_metadata(path)?;
        ensure!(
            !meta.file_type().is_symlink(),
            "History path contains a symlink"
        );
        if path == source {
            ensure!(meta.is_file(), "History source is not a regular file");
        } else {
            ensure!(meta.is_dir(), "History ancestor is not a directory");
        }
        current = path.parent();
    }
    Ok(())
}

fn prefix_hash(file: &mut File, length: usize) -> Result<String> {
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = vec![0; length];
    file.read_exact(&mut bytes)?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn item(id: &str, user: bool, text: String) -> Result<Value> {
    ensure!(
        !id.is_empty() && id.len() <= 512,
        "Provider message has no bounded stable identity"
    );
    ensure!(
        text.len() <= RECORD,
        "Provider message exceeds history bounds"
    );
    Ok(json!({"id":id,"type":if user {"userMessage"} else {"agentMessage"},"text":text}))
}

fn text_parts(content: &Value) -> Option<String> {
    if let Some(text) = content.as_str() {
        return Some(text.to_owned());
    }
    let parts = content.as_array()?;
    let text = parts
        .iter()
        .filter(|part| part["type"] == "text")
        .filter_map(|part| part["text"].as_str())
        .collect::<String>();
    (!text.is_empty()).then_some(text)
}

fn jsonl_item(value: &Value, provider: Provider, identity: &str) -> Result<Option<Value>> {
    if provider == Provider::Claude {
        if value["sessionId"].as_str() != Some(identity)
            || value["isSidechain"] == true
            || value["isMeta"] == true
        {
            return Ok(None);
        }
        let role = value["type"].as_str();
        if !matches!(role, Some("user" | "assistant")) || value["message"]["role"].as_str() != role
        {
            return Ok(None);
        }
        let Some(text) = text_parts(&value["message"]["content"]) else {
            return Ok(None);
        };
        return Ok(Some(item(
            value["uuid"]
                .as_str()
                .context("Claude message UUID missing")?,
            role == Some("user"),
            text,
        )?));
    }
    if value["schema_version"] != 1
        || value["stream"]["kind"] != "session"
        || value["stream"]["id"].as_str() != Some(identity)
        || value["payload_type"] != "runtime.session"
    {
        return Ok(None);
    }
    let event = &value["payload"]["event"];
    muse_item(value, event)
}

fn muse_item(value: &Value, event: &Value) -> Result<Option<Value>> {
    let (user, text) = match event["kind"].as_str() {
        Some("started") => (true, event["prompt"].as_str()),
        Some("user_prompt_display") => (true, event["text"].as_str()),
        Some("assistant_message_committed") => (false, event["text"].as_str()),
        Some("inbox_item_queued") if event["source"]["source"] == "user_steer" => (
            true,
            event["payload"]["prompt"]
                .as_str()
                .or_else(|| event["body"].as_str()),
        ),
        _ => return Ok(None),
    };
    let Some(text) = text else {
        return Ok(None);
    };
    let id = if matches!(
        event["kind"].as_str(),
        Some("started" | "user_prompt_display")
    ) {
        value["payload"]["run_id"]
            .as_str()
            .map(|id| format!("muse-user-{id}"))
    } else {
        None
    };
    Ok(Some(item(
        id.as_deref()
            .or_else(|| value["id"].as_str())
            .context("Muse event ID missing")?,
        user,
        text.to_owned(),
    )?))
}

fn page_item(value: &Value, state: &mut Cursor) -> Result<Option<Value>> {
    let entry = jsonl_item(value, state.provider, &state.id)?;
    if entry.is_none() || state.provider != Provider::Muse {
        return Ok(entry);
    }
    let event = &value["payload"]["event"];
    match event["kind"].as_str() {
        Some("user_prompt_display") => {
            let run = value["payload"]["run_id"]
                .as_str()
                .context("Muse display record has no exact run association")?;
            ensure!(
                run.len() <= 128 && state.displayed_runs.len() < 40,
                "Muse display association exceeds its bound"
            );
            if !state.displayed_runs.insert(run.into()) {
                return Ok(None);
            }
        }
        Some("started") => {
            if let Some(run) = value["payload"]["run_id"].as_str() {
                if state.displayed_runs.remove(run) {
                    return Ok(None);
                }
            }
        }
        _ => {}
    }
    Ok(entry)
}

fn jsonl_page(file: &mut File, state: &mut Cursor) -> Result<(Vec<Value>, bool)> {
    jsonl_page_projected(file, state, &BTreeMap::new())
}

fn jsonl_page_projected(
    file: &mut File,
    state: &mut Cursor,
    projection: &BTreeMap<String, Vec<Value>>,
) -> Result<(Vec<Value>, bool)> {
    let end = state.before;
    let start = end.saturating_sub(WINDOW);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = vec![0; (end - start) as usize];
    file.read_exact(&mut bytes)?;
    let first = if start > 0 {
        bytes
            .iter()
            .position(|b| *b == b'\n')
            .map_or(bytes.len(), |i| i + 1)
    } else {
        0
    };
    ensure!(
        first < bytes.len() || start == 0,
        "History record exceeds read window"
    );
    let mut positions = Vec::new();
    let mut offset = first;
    for line in bytes[first..].split_inclusive(|b| *b == b'\n') {
        if !line.ends_with(b"\n") {
            break;
        } // Uncommitted append, not a durable record.
        positions.push((offset, &line[..line.len() - 1]));
        offset += line.len();
    }
    let mut items = Vec::new();
    state.before = start + first as u64;
    for (offset, line) in positions.into_iter().rev() {
        state.before = start + offset as u64;
        ensure!(
            line.len() <= RECORD,
            "Provider record exceeds history bounds"
        );
        if line.is_empty() {
            continue;
        }
        let value: Value =
            serde_json::from_slice(line).context("Malformed durable history record")?;
        if let Some(entries) = value["uuid"].as_str().and_then(|id| projection.get(id)) {
            items.extend(entries.iter().rev().cloned());
        } else if let Some(entry) = page_item(&value, state)? {
            items.push(entry);
        }
        if items.len() >= PAGE {
            break;
        }
    }
    items.reverse();
    Ok((items, state.before > 0))
}

fn sql_page(
    source: &Path,
    id: &str,
    state: &mut Cursor,
    initial: bool,
) -> Result<(Vec<Value>, bool)> {
    let db = sql_connection(source, id)?;
    if initial {
        state.snapshot = db.query_row(
            "SELECT COALESCE(MAX(rowid),0) FROM message WHERE session_id=?",
            [id],
            |r| r.get::<_, u64>(0),
        )?;
        state.before = state.snapshot;
    }
    sql_messages(&db, id, state)
}

fn sql_connection(source: &Path, id: &str) -> Result<Connection> {
    verify_sql_sidecars(source)?;
    let db = Connection::open_with_flags(
        source,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )?;
    db.busy_timeout(std::time::Duration::from_millis(100))?;
    let budget = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    db.progress_handler(
        10_000,
        Some(move || budget.fetch_add(1, std::sync::atomic::Ordering::Relaxed) >= 100),
    );
    ensure!(
        db.query_row(
            "SELECT 1 FROM session WHERE id=? AND parent_id IS NULL",
            [id],
            |_| Ok(())
        )
        .optional()?
        .is_some(),
        "Exact root OpenCode session is unavailable"
    );
    verify_sql_sidecars(source)?;
    Ok(db)
}

fn verify_sql_sidecars(source: &Path) -> Result<()> {
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut path = source.as_os_str().to_os_string();
        path.push(suffix);
        match fs::symlink_metadata(Path::new(&path)) {
            Ok(metadata) => ensure!(
                metadata.is_file() && !metadata.file_type().is_symlink(),
                "OpenCode sidecar is not a regular file"
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn sql_messages(db: &Connection, id: &str, state: &mut Cursor) -> Result<(Vec<Value>, bool)> {
    let (before_time, before_id) = state
        .sql_before
        .clone()
        .unwrap_or((i64::MAX, String::new()));
    let mut query = db.prepare("SELECT id,time_created,CASE WHEN length(CAST(data AS BLOB))<=262144 THEN json_extract(data,'$.role') END,length(CAST(data AS BLOB)) FROM message WHERE session_id=?1 AND rowid<=?2 AND (time_created<?3 OR (time_created=?3 AND id<?4)) ORDER BY time_created DESC,id DESC LIMIT 41")?;
    let rows = query
        .query_map(params![id, state.snapshot, before_time, before_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, usize>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let more = rows.len() > PAGE;
    let mut items = Vec::new();
    let mut retained = 0usize;
    for (message_id, time, role, length) in rows.into_iter().take(PAGE) {
        ensure!(length <= RECORD, "OpenCode message exceeds history bounds");
        state.sql_before = Some((time, message_id.clone()));
        let Some(role @ ("user" | "assistant")) = role.as_deref() else {
            continue;
        };
        let text = sql_text(db, id, &message_id)?;
        retained += text.len();
        ensure!(
            retained <= WINDOW as usize,
            "History page exceeds its byte bound"
        );
        if !text.is_empty() {
            items.push(item(&message_id, role == "user", text)?);
        }
    }
    items.reverse();
    Ok((items, more))
}

fn sql_text(db: &Connection, id: &str, message_id: &str) -> Result<String> {
    let mut parts = db.prepare("SELECT CASE WHEN length(CAST(data AS BLOB))<=262144 THEN data END,length(CAST(data AS BLOB)) FROM part WHERE session_id=? AND message_id=? ORDER BY time_created,id LIMIT 257")?;
    let pieces = parts.query_map(params![id, message_id], |r| {
        Ok((r.get::<_, Option<String>>(0)?, r.get::<_, usize>(1)?))
    })?;
    let mut text = String::new();
    for (index, piece) in pieces.enumerate() {
        ensure!(index < 256, "OpenCode message has too many parts");
        let (part, length) = piece?;
        ensure!(length <= RECORD, "OpenCode part exceeds history bounds");
        let part: Value = serde_json::from_str(&part.context("OpenCode part unavailable")?)?;
        if part["type"] == "text" {
            if let Some(value) = part["text"].as_str() {
                text.push_str(value);
            }
        }
        ensure!(text.len() <= RECORD, "OpenCode text exceeds history bounds");
    }
    Ok(text)
}

#[cfg(test)]
#[path = "mobile_history_tests.rs"]
mod tests;
