use crate::{
    config::Config,
    consult::{CancellablePipe, CancellationToken, OwnedChild, terminate_child},
    model::{Candidate, Provider, Status},
    paths::Paths,
};
use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags, types::ValueRef};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsStr,
    fs::{self, File},
    io::{BufRead, BufReader, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant, UNIX_EPOCH},
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use walkdir::WalkDir;

pub struct Providers<'a> {
    paths: &'a Paths,
    config: &'a Config,
}

/// Provider-owned durable metadata state for one immutable conversation ID.
///
/// This deliberately avoids transcript contents and process discovery. It is
/// safe to use as the final source-access gate immediately before a private
/// consultation starts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderSourceState {
    Present,
    Archived,
    Deleted,
    Unknown,
}

impl<'a> Providers<'a> {
    pub fn new(paths: &'a Paths, config: &'a Config) -> Self {
        Self { paths, config }
    }

    pub fn browse(&self, provider: Provider) -> Vec<Candidate> {
        self.records(provider, None, false)
    }

    /// Return only conversations whose provider metadata proves that a human
    /// explicitly chose the displayed name. This is deliberately narrower
    /// than [`Self::discover`]: Codex currently persists a title but not its
    /// author, so a DB/index title can support exact lookup and the opt-in
    /// recent browser without being advertised as a personal name during
    /// setup. Existing Pika names remain authoritative in Pika's own store.
    pub fn import_candidates(&self, provider: Provider) -> Vec<Candidate> {
        match provider {
            Provider::Claude => self.records(provider, None, true),
            Provider::Codex | Provider::Opencode => Vec::new(),
        }
    }

    pub fn discover(&self, provider: Provider) -> Vec<Candidate> {
        // Discovery also serves reconciliation, where Codex fork lineage and
        // native rename labels are useful even though their authorship is not
        // proven. Setup must call `import_candidates` instead.
        if provider == Provider::Opencode {
            Vec::new()
        } else {
            self.records(provider, None, true)
        }
    }

    pub fn find(&self, provider: Provider, query: &str) -> Vec<Candidate> {
        match provider {
            // Claude's generated title is orientation, not a stable name.
            // Exact UUID lookup remains available even for an unnamed record.
            Provider::Claude => claude_records(&self.paths.claude_home, Some(query), true, None),
            _ => self.records(provider, Some(query), false),
        }
    }

    fn records(&self, provider: Provider, query: Option<&str>, named_only: bool) -> Vec<Candidate> {
        match provider {
            Provider::Codex => {
                codex_records(&self.paths.codex_home, self.config, query, named_only, None)
            }
            Provider::Claude => claude_records(&self.paths.claude_home, query, named_only, None),
            Provider::Opencode => opencode_records(
                &self.paths.opencode_data_home,
                self.config,
                query,
                named_only,
                None,
            ),
        }
    }

    /// Read watched UUID metadata in one provider pass. A refresh must not
    /// reopen and rescan the same provider store once per board row.
    pub fn tracked(&self, provider: Provider, identities: &BTreeSet<String>) -> Vec<Candidate> {
        if identities.is_empty() {
            return Vec::new();
        }
        match provider {
            Provider::Codex => codex_records(
                &self.paths.codex_home,
                self.config,
                None,
                false,
                Some(identities),
            ),
            Provider::Claude => {
                claude_records(&self.paths.claude_home, None, true, Some(identities))
            }
            Provider::Opencode => {
                let mut records = opencode_records(
                    &self.paths.opencode_data_home,
                    self.config,
                    None,
                    false,
                    Some(identities),
                );
                for record in &mut records {
                    if opencode_placeholder(record.name.as_deref()) {
                        record.name = None;
                    }
                }
                records
            }
        }
    }

    pub fn source_state(
        &self,
        provider: Provider,
        session_id: &str,
        transcript_path: Option<&str>,
    ) -> ProviderSourceState {
        match provider {
            Provider::Codex => {
                codex_source_state(&self.paths.codex_home, session_id, transcript_path)
            }
            Provider::Claude => {
                if transcript_path.map(Path::new).is_some_and(Path::is_file) {
                    ProviderSourceState::Present
                } else {
                    ProviderSourceState::Unknown
                }
            }
            Provider::Opencode => opencode_source_state(&self.paths.opencode_data_home, session_id),
        }
    }

    pub fn new_argv(
        &self,
        provider: Provider,
        name: &str,
        session_id: Option<&str>,
    ) -> Vec<String> {
        let executable = self.config.executable(provider);
        match provider {
            Provider::Codex | Provider::Opencode => vec![executable],
            Provider::Claude => {
                let mut argv = vec![executable, "--name".into(), name.into()];
                if let Some(identity) = session_id {
                    argv.extend(["--session-id".into(), identity.into()]);
                }
                argv
            }
        }
    }

    pub fn resume_argv(&self, provider: Provider, session_id: &str) -> Vec<String> {
        let executable = self.config.executable(provider);
        match provider {
            Provider::Codex => vec![executable, "resume".into(), session_id.into()],
            Provider::Claude => vec![executable, "--resume".into(), session_id.into()],
            Provider::Opencode => vec![executable, "--session".into(), session_id.into()],
        }
    }

    pub fn valid_id(provider: Provider, value: &str) -> bool {
        match provider {
            Provider::Opencode => value.strip_prefix("ses_").is_some_and(|tail| {
                (4..=124).contains(&tail.len())
                    && tail
                        .chars()
                        .all(|character| character.is_ascii_alphanumeric())
            }),
            _ => uuid::Uuid::parse_str(value)
                .is_ok_and(|parsed| parsed.hyphenated().to_string() == value.to_ascii_lowercase()),
        }
    }

    /// Set Codex's provider-native title through its documented app-server
    /// protocol. Failure is non-destructive and callers retain a durable retry
    /// marker; this never edits Codex's SQLite state directly.
    pub fn set_codex_native_name(&self, session_id: &str, name: &str) -> bool {
        self.set_codex_native_name_with_timeout(session_id, name, Duration::from_millis(3500))
    }

    fn set_codex_native_name_with_timeout(
        &self,
        session_id: &str,
        name: &str,
        timeout: Duration,
    ) -> bool {
        let deadline = Instant::now() + timeout;
        // Bound input before JSON escaping or copying it into the I/O worker.
        if name.len() > 64 * 1024
            || name.trim().is_empty()
            || !Self::valid_id(Provider::Codex, session_id)
        {
            return false;
        }
        let executable = self.config.executable(Provider::Codex);
        let mut command = Command::new(executable);
        command
            .args(["app-server", "--stdio"])
            .env("CODEX_HOME", &self.paths.codex_home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = match OwnedChild::spawn(&mut command) {
            Ok(child) => child,
            Err(_) => return false,
        };
        let stop = CancellationToken::default();
        let named = (|| {
            let input = CancellablePipe::new(child.stdin.take()?, stop.clone()).ok()?;
            let output = CancellablePipe::new(child.stdout.take()?, stop.clone()).ok()?;
            let session_id = session_id.to_owned();
            let name = name.to_owned();
            let (sender, receiver) = mpsc::sync_channel(1);
            std::thread::spawn(move || {
                let result = codex_name_protocol(input, output, &session_id, &name);
                let _ = sender.send(result.is_ok_and(|named| named));
            });
            receiver
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .ok()
        })()
        .unwrap_or(false);
        stop.cancel();
        // The unreaped leader pins its PGID until owned descendants are killed.
        // Never signal the caller's group or wait for a foreign pipe holder.
        let cleaned = terminate_child(&mut child).is_ok();
        named && cleaned
    }
}

fn codex_name_protocol(
    mut input: impl Write,
    output: impl Read,
    session_id: &str,
    name: &str,
) -> Result<bool> {
    const MAX_FRAME_BYTES: usize = 64 * 1024;
    const MAX_OUTPUT_BYTES: usize = 1024 * 1024;
    let write = |input: &mut dyn Write, value: &Value| -> Result<()> {
        serde_json::to_writer(&mut *input, value)?;
        input.write_all(b"\n")?;
        input.flush()?;
        Ok(())
    };
    write(
        &mut input,
        &serde_json::json!({
            "method":"initialize", "id":1,
            "params":{"clientInfo":{"name":"pikamux","version":crate::VERSION},
                      "capabilities":{"experimentalApi":true}}
        }),
    )?;
    let mut output = BufReader::new(output);
    let mut total_bytes = 0;
    let mut sent_name = false;
    loop {
        let mut line = Vec::new();
        // Take wraps the buffered reader, so even an unterminated JSON frame
        // cannot grow a read_until allocation beyond the explicit frame bound.
        let read = output
            .by_ref()
            .take((MAX_FRAME_BYTES + 1) as u64)
            .read_until(b'\n', &mut line)?;
        total_bytes += read;
        if read == 0 || line.len() > MAX_FRAME_BYTES || total_bytes > MAX_OUTPUT_BYTES {
            return Ok(false);
        }
        let Ok(response) = serde_json::from_slice::<Value>(&line) else {
            continue;
        };
        if response.get("id").and_then(Value::as_i64) == Some(1) && !sent_name {
            if response.get("error").is_some() || response.get("result").is_none() {
                return Ok(false);
            }
            write(&mut input, &serde_json::json!({"method":"initialized"}))?;
            write(
                &mut input,
                &serde_json::json!({
                    "method":"thread/name/set", "id":2,
                    "params":{"threadId":session_id,"name":name}
                }),
            )?;
            sent_name = true;
        } else if response.get("id").and_then(Value::as_i64) == Some(2) && sent_name {
            return Ok(response.get("result").is_some() && response.get("error").is_none());
        }
    }
}

fn codex_records(
    home: &Path,
    config: &Config,
    query: Option<&str>,
    named_only: bool,
    identities: Option<&BTreeSet<String>>,
) -> Vec<Candidate> {
    let index_records = codex_index_records(home);
    let Some(database) = newest_matching(home, "state_", ".sqlite") else {
        return filter_codex_index(
            index_records,
            query,
            named_only,
            identities,
            &BTreeSet::new(),
        );
    };
    let Ok(db) = readonly(&database) else {
        return Vec::new();
    };
    let Ok(columns) = columns(&db, "threads") else {
        return Vec::new();
    };
    if !columns.contains("id") {
        return Vec::new();
    }
    let archived = if columns.contains("archived") {
        "COALESCE(archived,0)=0"
    } else {
        "1=1"
    };
    let fields = select_fields(
        &columns,
        &[
            "id",
            "name",
            "cwd",
            "git_branch",
            "rollout_path",
            "model",
            "created_at",
            "created_at_ms",
            "updated_at",
            "updated_at_ms",
        ],
    );
    let mut conditions = vec![archived.to_owned()];
    let index_matches = query
        .map(|needle| {
            index_records
                .iter()
                .filter(|candidate| {
                    candidate.session_id == needle
                        || candidate
                            .name
                            .as_deref()
                            .is_some_and(|name| name.eq_ignore_ascii_case(needle))
                })
                .map(|candidate| candidate.session_id.clone())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let mut parameters = query.into_iter().map(str::to_owned).collect::<Vec<_>>();
    if query.is_some() {
        let marks = (2..2 + index_matches.len())
            .map(|index| format!("?{index}"))
            .collect::<Vec<_>>();
        let indexed = if marks.is_empty() {
            String::new()
        } else {
            format!(" OR id IN ({})", marks.join(","))
        };
        let native_name = if columns.contains("name") {
            " OR lower(name)=lower(?1)"
        } else {
            ""
        };
        conditions.push(format!("(id=?1{native_name}{indexed})"));
        parameters.extend(index_matches);
    }
    if let Some(identities) = identities {
        let first = parameters.len() + 1;
        let marks = (first..first + identities.len())
            .map(|index| format!("?{index}"))
            .collect::<Vec<_>>()
            .join(",");
        conditions.push(format!("id IN ({marks})"));
        parameters.extend(identities.iter().cloned());
    }
    let sql = format!(
        "SELECT {fields} FROM threads WHERE {}",
        conditions.join(" AND ")
    );
    let Ok(mut statement) = db.prepare(&sql) else {
        return Vec::new();
    };
    let mapper = |row: &rusqlite::Row<'_>| -> rusqlite::Result<Candidate> {
        let session_id: String = row.get(0)?;
        let transcript: Option<String> = row.get(4)?;
        let metadata = transcript
            .as_deref()
            .and_then(read_first_json)
            .unwrap_or(Value::Null);
        let created_ms = timestamp_sql(row.get_ref(7)?);
        let created = timestamp_sql(row.get_ref(6)?);
        let updated_ms = timestamp_sql(row.get_ref(9)?);
        let updated = timestamp_sql(row.get_ref(8)?);
        Ok(Candidate {
            provider: Provider::Codex,
            session_id,
            name: row.get(1)?,
            cwd: row.get(2)?,
            branch: row.get(3)?,
            transcript_path: transcript.clone(),
            model: row.get(5)?,
            created_at: if created_ms != 0.0 {
                created_ms
            } else {
                created
            },
            updated_at: if updated_ms != 0.0 {
                updated_ms
            } else {
                updated
            },
            live: false,
            pid: None,
            source: "codex-state".into(),
            parent_session_id: metadata
                .pointer("/payload/forked_from_id")
                .and_then(Value::as_str)
                .map(str::to_owned),
            lifecycle_status: transcript.as_deref().and_then(codex_lifecycle),
        })
    };
    let rows = statement.query_map(rusqlite::params_from_iter(parameters), mapper);
    let Ok(rows) = rows else {
        return Vec::new();
    };
    let archived_ids = if columns.contains("archived") {
        db.prepare("SELECT id FROM threads WHERE COALESCE(archived,0) != 0")
            .and_then(|mut statement| {
                statement
                    .query_map([], |row| row.get::<_, String>(0))?
                    .collect::<rusqlite::Result<BTreeSet<_>>>()
            })
            .unwrap_or_default()
    } else {
        BTreeSet::new()
    };
    let database_rows = rows.flatten().collect::<Vec<_>>();
    let worker_ids = database_rows
        .iter()
        .filter(|candidate| codex_worker(candidate, config))
        .map(|candidate| candidate.session_id.clone())
        .collect::<BTreeSet<_>>();
    let mut output = database_rows
        .into_iter()
        .filter(|candidate| !worker_ids.contains(&candidate.session_id))
        .collect::<Vec<_>>();
    let mut positions = output
        .iter()
        .enumerate()
        .map(|(index, candidate)| (candidate.session_id.clone(), index))
        .collect::<BTreeMap<_, _>>();
    for indexed in filter_codex_index(index_records, query, named_only, identities, &archived_ids) {
        if let Some(position) = positions.get(&indexed.session_id).copied() {
            output[position].name = indexed.name;
            output[position].updated_at = output[position].updated_at.max(indexed.updated_at);
        } else if !worker_ids.contains(&indexed.session_id) {
            positions.insert(indexed.session_id.clone(), output.len());
            output.push(indexed);
        }
    }
    if named_only {
        output.retain(|candidate| {
            candidate
                .name
                .as_deref()
                .is_some_and(|name| !name.trim().is_empty())
        });
    }
    output.sort_by(|a, b| b.updated_at.total_cmp(&a.updated_at));
    output
}

fn codex_index_records(home: &Path) -> Vec<Candidate> {
    let Ok(file) = File::open(home.join("session_index.jsonl")) else {
        return Vec::new();
    };
    let mut records = BTreeMap::<String, Candidate>::new();
    for line in BufReader::new(file.take(16 * 1024 * 1024))
        .lines()
        .map_while(Result::ok)
        .take(100_000)
    {
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(identity) = value
            .get("id")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
        else {
            continue;
        };
        let Some(name) = value
            .get("thread_name")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
        else {
            continue;
        };
        let updated_at = timestamp(value.get("updated_at"));
        records.insert(
            identity.to_owned(),
            Candidate {
                provider: Provider::Codex,
                session_id: identity.to_owned(),
                name: Some(name.to_owned()),
                cwd: None,
                branch: None,
                transcript_path: None,
                model: None,
                updated_at,
                live: false,
                pid: None,
                source: "codex-index".into(),
                parent_session_id: None,
                created_at: 0.0,
                lifecycle_status: None,
            },
        );
    }
    records.into_values().collect()
}

fn filter_codex_index(
    records: Vec<Candidate>,
    query: Option<&str>,
    named_only: bool,
    identities: Option<&BTreeSet<String>>,
    archived_ids: &BTreeSet<String>,
) -> Vec<Candidate> {
    records
        .into_iter()
        .filter(|candidate| !archived_ids.contains(&candidate.session_id))
        .filter(|candidate| {
            !named_only
                || candidate
                    .name
                    .as_deref()
                    .is_some_and(|name| !name.trim().is_empty())
        })
        .filter(|candidate| identities.is_none_or(|wanted| wanted.contains(&candidate.session_id)))
        .filter(|candidate| {
            query.is_none_or(|needle| {
                candidate.session_id == needle
                    || candidate
                        .name
                        .as_deref()
                        .is_some_and(|name| name.eq_ignore_ascii_case(needle))
            })
        })
        .collect()
}

fn codex_worker(candidate: &Candidate, config: &Config) -> bool {
    let Some(path) = candidate.transcript_path.as_deref() else {
        return false;
    };
    let Some(value) = read_first_json(path) else {
        return false;
    };
    if value.get("type").and_then(Value::as_str) != Some("session_meta") {
        return false;
    }
    let payload = value.get("payload").and_then(Value::as_object);
    let source = payload.and_then(|item| item.get("source"));
    if source.and_then(Value::as_str) == Some("exec") {
        return true;
    }
    if payload
        .and_then(|item| item.get("thread_source"))
        .and_then(Value::as_str)
        == Some("subagent")
        || source
            .and_then(Value::as_object)
            .is_some_and(|item| item.contains_key("subagent"))
    {
        return true;
    }
    payload
        .and_then(|item| item.get("originator"))
        .and_then(Value::as_str)
        .is_some_and(|origin| {
            origin.eq_ignore_ascii_case("codex_exec")
                || config
                    .codex_worker_originators
                    .iter()
                    .any(|item| item.eq_ignore_ascii_case(origin))
        })
}

fn codex_lifecycle(path: &str) -> Option<Status> {
    reverse_lines(Path::new(path)).into_iter().find_map(|line| {
        if !line.contains("\"event_msg\"") {
            return None;
        }
        let value: Value = serde_json::from_str(&line).ok()?;
        match value.pointer("/payload/type").and_then(Value::as_str) {
            Some("task_started") => Some(Status::Working),
            Some("task_complete") => Some(Status::Ready),
            _ => None,
        }
    })
}

fn claude_records(
    home: &Path,
    query: Option<&str>,
    explicit_only: bool,
    identities: Option<&BTreeSet<String>>,
) -> Vec<Candidate> {
    // Inventory once: the live registry and historical lookup share these
    // exact paths instead of walking the entire projects tree for every row.
    let mut transcripts: Vec<_> = WalkDir::new(home.join("projects"))
        .min_depth(2)
        .max_depth(2)
        .into_iter()
        .flatten()
        .filter(|entry| {
            entry.file_type().is_file() && entry.path().extension() == Some(OsStr::new("jsonl"))
        })
        .map(|entry| entry.into_path())
        .collect();
    let transcript_paths = transcripts
        .iter()
        .filter_map(|path| Some((path.file_stem()?.to_str()?.to_owned(), path.clone())))
        .collect::<BTreeMap<_, _>>();
    let mut records: BTreeMap<String, Candidate> = BTreeMap::new();
    for path in fs::read_dir(home.join("sessions"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension() == Some(OsStr::new("json")))
    {
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        if value.get("kind").and_then(Value::as_str) != Some("interactive") {
            continue;
        }
        let Some(identity) = value
            .get("sessionId")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
        else {
            continue;
        };
        if identities.is_some_and(|wanted| !wanted.contains(identity)) {
            continue;
        }
        let visible_name = value.get("name").and_then(Value::as_str);
        let transcript = transcript_paths.get(identity).cloned();
        if transcript
            .as_ref()
            .is_some_and(|path| claude_worker(path, identity))
        {
            continue;
        }
        let name = if value.get("nameSource").and_then(Value::as_str) == Some("derived") {
            None
        } else {
            visible_name.map(str::to_owned)
        };
        records.insert(
            identity.into(),
            Candidate {
                provider: Provider::Claude,
                session_id: identity.into(),
                name,
                cwd: value.get("cwd").and_then(Value::as_str).map(str::to_owned),
                branch: None,
                transcript_path: transcript.map(|path| path.to_string_lossy().into_owned()),
                model: value
                    .get("model")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                updated_at: timestamp(value.get("updatedAt").or_else(|| value.get("startedAt"))),
                created_at: timestamp(value.get("startedAt")),
                live: false,
                pid: None,
                source: if value.get("nameSource").and_then(Value::as_str) == Some("custom") {
                    "claude-live-custom".into()
                } else {
                    "claude-live".into()
                },
                parent_session_id: None,
                lifecycle_status: None,
            },
        );
    }

    transcripts.sort_by_key(|path| std::cmp::Reverse(modified(path).to_bits()));
    for (index, path) in transcripts.into_iter().enumerate() {
        let Some(identity) = path.file_stem().and_then(OsStr::to_str).map(str::to_owned) else {
            continue;
        };
        if identities.is_some_and(|wanted| !wanted.contains(&identity)) {
            continue;
        }
        let exact_identity = query == Some(identity.as_str())
            || identities.is_some_and(|wanted| wanted.contains(&identity));
        // A browsing budget must never make an explicitly requested or
        // already watched immutable identity disappear.
        if index >= 1000 && !exact_identity {
            continue;
        }
        let title = transcript_title(&path, explicit_only);
        if query.is_some_and(|needle| {
            identity != needle
                && !title
                    .as_deref()
                    .is_some_and(|name| name.eq_ignore_ascii_case(needle))
        }) {
            continue;
        }
        if claude_worker(&path, &identity) || (explicit_only && title.is_none() && !exact_identity)
        {
            continue;
        }
        records
            .entry(identity.clone())
            .and_modify(|existing| {
                if title.is_some() {
                    existing.name = title.clone();
                }
                existing.transcript_path = Some(path.to_string_lossy().into_owned());
                existing.updated_at = existing.updated_at.max(modified(&path));
                existing.source = if explicit_only {
                    "claude-live+explicit-history".into()
                } else {
                    "claude-live+history".into()
                };
            })
            .or_insert(Candidate {
                provider: Provider::Claude,
                session_id: identity,
                name: title,
                cwd: None,
                branch: None,
                transcript_path: Some(path.to_string_lossy().into_owned()),
                model: None,
                updated_at: modified(&path),
                live: false,
                pid: None,
                source: "claude-history".into(),
                parent_session_id: None,
                created_at: 0.0,
                lifecycle_status: None,
            });
    }
    let mut output: Vec<_> = records.into_values().collect();
    if explicit_only {
        output.retain(|candidate| {
            let exact_identity = query == Some(candidate.session_id.as_str())
                || identities.is_some_and(|wanted| wanted.contains(&candidate.session_id));
            let explicit_name = candidate
                .name
                .as_deref()
                .is_some_and(|name| !name.trim().is_empty())
                && matches!(
                    candidate.source.as_str(),
                    "claude-live-custom" | "claude-live+explicit-history" | "claude-history"
                );
            exact_identity || explicit_name
        });
    }
    if let Some(needle) = query {
        output.retain(|candidate| {
            candidate.session_id == needle
                || candidate
                    .name
                    .as_deref()
                    .is_some_and(|name| name.eq_ignore_ascii_case(needle))
        });
    }
    output.sort_by(|a, b| b.updated_at.total_cmp(&a.updated_at));
    output
}

fn transcript_title(path: &Path, explicit_only: bool) -> Option<String> {
    const MAX_BYTES: u64 = 2 * 1024 * 1024;
    const MAX_LINES: usize = 16_384;
    const MAX_LINE_BYTES: usize = 64 * 1024;
    let deadline = Instant::now() + Duration::from_millis(100);
    let mut file = File::open(path).ok()?;
    // Provider title events are append-only; inspect a bounded recent window.
    // An unavailable title is not authority to rename an existing Pika row.
    let offset = file.metadata().ok()?.len().saturating_sub(MAX_BYTES);
    file.seek(SeekFrom::Start(offset)).ok()?;
    let mut bytes = Vec::new();
    file.take(MAX_BYTES).read_to_end(&mut bytes).ok()?;
    // Skip the first incomplete record when the window starts mid-file.
    let start = if offset > 0 {
        bytes
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(bytes.len(), |index| index + 1)
    } else {
        0
    };
    let lines = bytes[start..].split(|byte| *byte == b'\n');
    let mut explicit = None;
    let mut generated = None;
    for line in lines.rev().take(MAX_LINES) {
        if Instant::now() >= deadline {
            break;
        }
        if line.len() > MAX_LINE_BYTES {
            continue;
        }
        let Ok(value) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        match value.get("type").and_then(Value::as_str) {
            Some("custom-title" | "session-title") => {
                explicit = ["customTitle", "title", "sessionTitle", "name"]
                    .iter()
                    .find_map(|key| value.get(*key).and_then(Value::as_str).map(str::to_owned));
                if explicit.is_some() {
                    break;
                }
            }
            Some("ai-title") if !explicit_only && generated.is_none() => {
                generated = value
                    .get("aiTitle")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
            }
            _ if value.get("sessionTitle").and_then(Value::as_str).is_some() => {
                explicit = value
                    .get("sessionTitle")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                break;
            }
            _ => {}
        }
    }
    explicit.or(generated)
}

fn claude_worker(path: &Path, identity: &str) -> bool {
    let Ok(file) = File::open(path) else {
        return false;
    };
    let mut bytes = Vec::new();
    if file.take(1024 * 1024).read_to_end(&mut bytes).is_err() {
        return false;
    }
    bytes
        .split(|byte| *byte == b'\n')
        .take(128)
        .filter(|line| line.len() <= 64 * 1024)
        .find_map(|line| {
            let value = serde_json::from_slice::<Value>(line).ok()?;
            if value.get("sessionId").and_then(Value::as_str) != Some(identity)
                || value.get("isSidechain").and_then(Value::as_bool) != Some(false)
            {
                return None;
            }
            value
                .get("entrypoint")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .is_some_and(|entrypoint| entrypoint == "sdk-cli")
}

fn opencode_records(
    home: &Path,
    config: &Config,
    query: Option<&str>,
    named_only: bool,
    identities: Option<&BTreeSet<String>>,
) -> Vec<Candidate> {
    let database = home.join("opencode.db");
    let Ok(db) = readonly(&database) else {
        return Vec::new();
    };
    let Ok(columns) = columns(&db, "session") else {
        return Vec::new();
    };
    if !["id", "title", "directory", "parent_id"]
        .iter()
        .all(|item| columns.contains(*item))
    {
        return Vec::new();
    }
    let fields = select_fields(
        &columns,
        &[
            "id",
            "title",
            "directory",
            "parent_id",
            "time_created",
            "time_updated",
            "model",
        ],
    );
    let mut conditions = vec!["parent_id IS NULL".to_owned()];
    if columns.contains("time_archived") {
        conditions.push("time_archived IS NULL".into());
    }
    if named_only {
        conditions.push("title IS NOT NULL AND trim(title) != ''".into());
    }
    if query.is_some() {
        conditions.push("(id=?1 OR lower(title)=lower(?1))".into());
    }
    let mut parameters = query.into_iter().map(str::to_owned).collect::<Vec<_>>();
    if let Some(identities) = identities {
        let first = parameters.len() + 1;
        let marks = (first..first + identities.len())
            .map(|index| format!("?{index}"))
            .collect::<Vec<_>>()
            .join(",");
        conditions.push(format!("id IN ({marks})"));
        parameters.extend(identities.iter().cloned());
    }
    let sql = format!(
        "SELECT {fields} FROM session WHERE {}",
        conditions.join(" AND ")
    );
    let Ok(mut statement) = db.prepare(&sql) else {
        return Vec::new();
    };
    let has_archived = columns.contains("time_archived");
    let mapper = |row: &rusqlite::Row<'_>| -> rusqlite::Result<Candidate> {
        let model: Option<String> = row.get(6)?;
        let session_id: String = row.get(0)?;
        Ok(Candidate {
            provider: Provider::Opencode,
            session_id: session_id.clone(),
            name: row.get(1)?,
            cwd: row.get(2)?,
            branch: None,
            transcript_path: Some(database.to_string_lossy().into_owned()),
            model: model.and_then(|value| opencode_model(&value)),
            updated_at: opencode_tree_updated(&db, &session_id, has_archived)
                .unwrap_or_else(|| timestamp_sql(row.get_ref(5).unwrap_or(ValueRef::Null))),
            created_at: timestamp_sql(row.get_ref(4)?),
            live: false,
            pid: None,
            source: "opencode-state".into(),
            parent_session_id: None,
            lifecycle_status: opencode_lifecycle(&db, &session_id, has_archived),
        })
    };
    let rows = statement.query_map(rusqlite::params_from_iter(parameters), mapper);
    let Ok(rows) = rows else {
        return Vec::new();
    };
    let mut output: Vec<_> = rows
        .flatten()
        .filter(|item| !named_only || !opencode_placeholder(item.name.as_deref()))
        .filter(|item| !opencode_automation(item, config))
        .collect();
    output.sort_by(|a, b| b.updated_at.total_cmp(&a.updated_at));
    output
}

fn opencode_tree_ids(db: &Connection, session_id: &str, has_archived: bool) -> Vec<String> {
    let archive_clause = if has_archived {
        " AND time_archived IS NULL"
    } else {
        ""
    };
    let sql = format!(
        "WITH RECURSIVE tree(id) AS (\
         SELECT id FROM session WHERE id=?1{archive_clause} \
         UNION ALL \
         SELECT child.id FROM session AS child JOIN tree ON child.parent_id=tree.id \
         WHERE 1=1{archive_clause}\
         ) SELECT id FROM tree"
    );
    db.prepare(&sql)
        .and_then(|mut statement| {
            statement
                .query_map([session_id], |row| row.get::<_, String>(0))
                .map(|rows| rows.flatten().collect())
        })
        .unwrap_or_default()
}

fn opencode_tree_updated(db: &Connection, session_id: &str, has_archived: bool) -> Option<f64> {
    let identities = opencode_tree_ids(db, session_id, has_archived);
    if identities.is_empty() {
        return None;
    }
    let marks = std::iter::repeat_n("?", identities.len())
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!("SELECT MAX(time_updated) FROM session WHERE id IN ({marks})");
    let mut statement = db.prepare(&sql).ok()?;
    statement
        .query_row(rusqlite::params_from_iter(identities), |row| {
            Ok(timestamp_sql(row.get_ref(0)?))
        })
        .ok()
}

fn opencode_lifecycle(db: &Connection, session_id: &str, has_archived: bool) -> Option<Status> {
    if !columns(db, "message").ok().is_some_and(|value| {
        ["id", "session_id", "time_created", "data"]
            .iter()
            .all(|column| value.contains(*column))
    }) {
        return None;
    }
    let mut completed = false;
    for identity in opencode_tree_ids(db, session_id, has_archived) {
        let data: Option<String> = db
            .query_row(
                "SELECT data FROM message WHERE session_id=?1 \
                 ORDER BY time_created DESC, id DESC LIMIT 1",
                [&identity],
                |row| row.get(0),
            )
            .ok();
        let Some(value) = data.and_then(|raw| serde_json::from_str::<Value>(&raw).ok()) else {
            continue;
        };
        let role = value.get("role").and_then(Value::as_str);
        let turn_completed = value
            .pointer("/time/completed")
            .is_some_and(|value| !value.is_null() && value.as_bool() != Some(false));
        if role == Some("user") || (role == Some("assistant") && !turn_completed) {
            return Some(Status::Working);
        }
        completed |= role == Some("assistant") && turn_completed;
    }
    completed.then_some(Status::Ready)
}

fn opencode_placeholder(value: Option<&str>) -> bool {
    value.is_some_and(|title| {
        title.starts_with("New session - ") || (title.contains(" (fork #") && title.ends_with(')'))
    })
}

fn opencode_automation(item: &Candidate, config: &Config) -> bool {
    item.cwd.as_deref().is_some_and(|cwd| {
        Path::new(cwd)
            .components()
            .any(|part| part.as_os_str() == "opencode-runtime")
    }) && item.name.as_deref().is_some_and(|name| {
        config
            .opencode_worker_title_prefixes
            .iter()
            .any(|prefix| name.to_lowercase().starts_with(&prefix.to_lowercase()))
    })
}

fn opencode_model(raw: &str) -> Option<String> {
    let Ok(value) = serde_json::from_str::<Value>(raw) else {
        return Some(raw.into());
    };
    let model = value
        .get("id")
        .or_else(|| value.get("modelID"))
        .and_then(Value::as_str)?;
    let provider = value
        .get("providerID")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty());
    let variant = value
        .get("variant")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty());
    let label = provider.map_or_else(
        || model.to_owned(),
        |provider| format!("{provider}/{model}"),
    );
    Some(variant.map_or(label.clone(), |variant| format!("{label}[{variant}]")))
}

fn codex_source_state(
    home: &Path,
    session_id: &str,
    transcript_path: Option<&str>,
) -> ProviderSourceState {
    let transcript_exists = transcript_path.map(Path::new).is_some_and(Path::is_file);
    let Some(database) = newest_matching(home, "state_", ".sqlite") else {
        return if transcript_exists {
            ProviderSourceState::Present
        } else {
            ProviderSourceState::Unknown
        };
    };
    let Ok(db) = readonly(&database) else {
        return ProviderSourceState::Unknown;
    };
    let Ok(columns) = columns(&db, "threads") else {
        return ProviderSourceState::Unknown;
    };
    if !columns.contains("id") {
        return ProviderSourceState::Unknown;
    }
    let archived = if columns.contains("archived") {
        db.query_row(
            "SELECT COALESCE(archived,0) FROM threads WHERE id=?1 LIMIT 1",
            [session_id],
            |row| row.get::<_, i64>(0),
        )
        .ok()
    } else {
        db.query_row(
            "SELECT 0 FROM threads WHERE id=?1 LIMIT 1",
            [session_id],
            |row| row.get::<_, i64>(0),
        )
        .ok()
    };
    match archived {
        Some(value) if value != 0 => ProviderSourceState::Archived,
        Some(_) if transcript_exists => ProviderSourceState::Present,
        Some(_) => ProviderSourceState::Unknown,
        // Older Codex stores and migrated conversations can retain a valid
        // rollout without a row in the newest state DB.
        None if transcript_exists => ProviderSourceState::Present,
        None => ProviderSourceState::Deleted,
    }
}

fn opencode_source_state(home: &Path, session_id: &str) -> ProviderSourceState {
    let database = home.join("opencode.db");
    let Ok(db) = readonly(&database) else {
        return ProviderSourceState::Unknown;
    };
    let Ok(columns) = columns(&db, "session") else {
        return ProviderSourceState::Unknown;
    };
    if !columns.contains("id") {
        return ProviderSourceState::Unknown;
    }
    let archived = if columns.contains("time_archived") {
        db.query_row(
            "SELECT time_archived IS NOT NULL FROM session WHERE id=?1 LIMIT 1",
            [session_id],
            |row| row.get::<_, bool>(0),
        )
        .ok()
    } else {
        db.query_row(
            "SELECT 0 FROM session WHERE id=?1 LIMIT 1",
            [session_id],
            |row| row.get::<_, bool>(0),
        )
        .ok()
    };
    match archived {
        Some(true) => ProviderSourceState::Archived,
        Some(false) => ProviderSourceState::Present,
        None => ProviderSourceState::Deleted,
    }
}

fn readonly(path: &Path) -> Result<Connection> {
    let uri = format!("file:{}?mode=ro", path.to_string_lossy());
    Connection::open_with_flags(
        uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("cannot open {}", path.display()))
}

fn columns(db: &Connection, table: &str) -> Result<BTreeSet<String>> {
    let mut statement = db.prepare(&format!("PRAGMA table_info({table})"))?;
    Ok(statement
        .query_map([], |row| row.get(1))?
        .flatten()
        .collect())
}

fn select_fields(columns: &BTreeSet<String>, wanted: &[&str]) -> String {
    wanted
        .iter()
        .map(|field| {
            if columns.contains(*field) {
                (*field).to_owned()
            } else {
                format!("NULL AS {field}")
            }
        })
        .collect::<Vec<_>>()
        .join(",")
}

fn newest_matching(root: &Path, prefix: &str, suffix: &str) -> Option<PathBuf> {
    let mut paths: Vec<_> = fs::read_dir(root)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(prefix) && name.ends_with(suffix))
        })
        .collect();
    paths.sort_by_key(|path| std::cmp::Reverse(modified(path).to_bits()));
    paths.into_iter().next()
}

fn modified(path: &Path) -> f64 {
    fs::metadata(path)
        .and_then(|value| value.modified())
        .ok()
        .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
        .map_or(0.0, |value| value.as_secs_f64())
}

fn timestamp(value: Option<&Value>) -> f64 {
    match value {
        Some(Value::Number(number)) => normalize_timestamp(number.as_f64().unwrap_or(0.0)),
        Some(Value::String(value)) => value
            .parse::<f64>()
            .ok()
            .map(normalize_timestamp)
            .or_else(|| {
                OffsetDateTime::parse(value, &Rfc3339)
                    .ok()
                    .map(|date| date.unix_timestamp() as f64)
            })
            .unwrap_or(0.0),
        _ => 0.0,
    }
}

fn timestamp_sql(value: ValueRef<'_>) -> f64 {
    match value {
        ValueRef::Integer(value) => normalize_timestamp(value as f64),
        ValueRef::Real(value) => normalize_timestamp(value),
        ValueRef::Text(value) => timestamp(Some(&Value::String(
            String::from_utf8_lossy(value).into_owned(),
        ))),
        _ => 0.0,
    }
}

fn normalize_timestamp(value: f64) -> f64 {
    if value > 10_000_000_000.0 {
        value / 1000.0
    } else {
        value
    }
}

fn read_first_json(path: impl AsRef<Path>) -> Option<Value> {
    let line = BufReader::new(File::open(path).ok()?)
        .lines()
        .next()?
        .ok()?;
    serde_json::from_str(&line).ok()
}

fn reverse_lines(path: &Path) -> Vec<String> {
    const MAX_TAIL_BYTES: u64 = 8 * 1024 * 1024;
    let Ok(mut file) = File::open(path) else {
        return Vec::new();
    };
    let Ok(mut position) = file.seek(SeekFrom::End(0)) else {
        return Vec::new();
    };
    let mut remainder = Vec::new();
    let mut output = Vec::new();
    let mut scanned = 0_u64;
    while position > 0 && scanned < MAX_TAIL_BYTES {
        let size = usize::try_from(
            position
                .min(65_536)
                .min(MAX_TAIL_BYTES.saturating_sub(scanned)),
        )
        .unwrap_or(65_536);
        position -= size as u64;
        scanned += size as u64;
        if file.seek(SeekFrom::Start(position)).is_err() {
            break;
        }
        let mut chunk = vec![0; size];
        if file.read_exact(&mut chunk).is_err() {
            break;
        }
        chunk.extend_from_slice(&remainder);
        let mut lines: Vec<_> = chunk
            .split(|byte| *byte == b'\n')
            .map(<[u8]>::to_vec)
            .collect();
        remainder = lines.remove(0);
        output.extend(
            lines
                .into_iter()
                .rev()
                .filter(|line| !line.is_empty())
                .map(|line| String::from_utf8_lossy(&line).into_owned()),
        );
    }
    // `remainder` is a complete first line only when we reached the beginning;
    // otherwise it is a truncated JSON fragment and must not be interpreted.
    if position == 0 && !remainder.is_empty() {
        output.push(String::from_utf8_lossy(&remainder).into_owned());
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_provider_timestamps() {
        assert_eq!(
            timestamp(Some(&Value::String("2026-08-12T10:30:00Z".into()))),
            1_786_530_600.0
        );
    }

    #[test]
    fn recognizes_only_native_opencode_placeholders() {
        assert!(opencode_placeholder(Some("New session - 2026-01-01")));
        assert!(opencode_placeholder(Some("Research (fork #2)")));
        assert!(!opencode_placeholder(Some("oc_research")));
    }

    #[test]
    fn validates_provider_ids() {
        assert!(Providers::valid_id(
            Provider::Codex,
            "11111111-1111-4111-8111-111111111111"
        ));
        assert!(Providers::valid_id(Provider::Opencode, "ses_abcdef12"));
        assert!(!Providers::valid_id(Provider::Opencode, "ses_bad-name"));
    }

    #[test]
    fn native_name_protocol_bounds_unterminated_frames_and_total_output() {
        assert!(!codex_name_protocol(Vec::new(), std::io::repeat(b'x'), "id", "name").unwrap());
        let notification = format!(
            "{{\"method\":\"notice\",\"params\":\"{}\"}}\n",
            "x".repeat(1024)
        );
        let mut output = std::io::Cursor::new(notification.repeat(2048));
        assert!(!codex_name_protocol(Vec::new(), &mut output, "id", "name").unwrap());
        assert!(output.position() <= 1024 * 1024 + 64 * 1024);
    }

    #[test]
    fn native_name_protocol_rejects_initialization_error_without_a_name_request() {
        let mut input = Vec::new();
        assert!(
            !codex_name_protocol(&mut input, &b"{\"id\":1,\"error\":{}}\n"[..], "id", "name")
                .unwrap()
        );
        assert!(
            !String::from_utf8(input)
                .unwrap()
                .contains("thread/name/set")
        );
    }

    #[cfg(unix)]
    fn native_name_fixture(temp: &tempfile::TempDir, body: &str) -> (Paths, Config) {
        use std::os::unix::fs::PermissionsExt;
        let executable = temp.path().join("codex-naming-fixture");
        fs::write(&executable, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let paths = Paths {
            config_dir: temp.path().join("config"),
            state_dir: temp.path().join("state"),
            config: temp.path().join("config/config.json"),
            database: temp.path().join("state/pika.db"),
            codex_home: temp.path().join("codex-home"),
            claude_home: temp.path().join("claude-home"),
            opencode_data_home: temp.path().join("opencode-data"),
            opencode_config_home: temp.path().join("opencode-config"),
        };
        let mut config = Config::default();
        config
            .provider_executables
            .insert("codex".into(), executable.to_string_lossy().into_owned());
        (paths, config)
    }

    #[cfg(unix)]
    #[test]
    fn native_name_deadline_bounds_blocked_writes_and_kills_descendants() {
        let temp = tempfile::tempdir().unwrap();
        let pid_file = temp.path().join("descendant.pid");
        let (paths, config) = native_name_fixture(
            &temp,
            &format!(
                "sleep 30 &\nprintf '%s' \"$!\" > {}\nprintf '%s\\n' '{{\"id\":1,\"result\":{{}}}}'\nwait",
                shell_words::quote(&pid_file.to_string_lossy())
            ),
        );
        let started = Instant::now();
        assert!(
            !Providers::new(&paths, &config).set_codex_native_name_with_timeout(
                "11111111-1111-4111-8111-111111111111",
                &"x".repeat(64 * 1024),
                Duration::from_secs(1)
            )
        );
        assert!(started.elapsed() < Duration::from_secs(2));
        let pid: i32 = fs::read_to_string(pid_file).unwrap().parse().unwrap();
        for _ in 0..100 {
            // Signal zero only inspects our fixture child; no user process is signalled.
            if unsafe { libc::kill(pid, 0) } == -1 {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("owned naming descendant survived cleanup");
    }

    #[cfg(unix)]
    #[test]
    fn native_name_stdout_flood_is_rejected_before_deadline() {
        let temp = tempfile::tempdir().unwrap();
        let (paths, config) = native_name_fixture(&temp, "exec /usr/bin/yes flood");
        let started = Instant::now();
        assert!(
            !Providers::new(&paths, &config)
                .set_codex_native_name("11111111-1111-4111-8111-111111111111", "name")
        );
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[cfg(unix)]
    #[test]
    fn codex_native_name_uses_app_server_instead_of_editing_state() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let executable = temp.path().join("codex");
        let log = temp.path().join("requests.jsonl");
        std::fs::write(
            &executable,
            format!(
                "#!/bin/sh\nIFS= read -r a; printf '%s\\n' \"$a\" >> '{}'; printf '%s\\n' '{{\"id\":1,\"result\":{{}}}}'; IFS= read -r b; printf '%s\\n' \"$b\" >> '{}'; IFS= read -r c; printf '%s\\n' \"$c\" >> '{}'; printf '%s\\n' '{{\"id\":2,\"result\":{{}}}}'\n",
                log.display(),
                log.display(),
                log.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let paths = Paths {
            config_dir: temp.path().join("config"),
            state_dir: temp.path().join("state"),
            config: temp.path().join("config/config.json"),
            database: temp.path().join("state/pika.db"),
            codex_home: temp.path().join("codex-home"),
            claude_home: temp.path().join("claude-home"),
            opencode_data_home: temp.path().join("opencode-data"),
            opencode_config_home: temp.path().join("opencode-config"),
        };
        let mut config = Config::default();
        config
            .provider_executables
            .insert("codex".into(), executable.to_string_lossy().into_owned());
        let id = "11111111-1111-4111-8111-111111111111";
        assert!(Providers::new(&paths, &config).set_codex_native_name(id, "research_pipeline"));
        let requests = std::fs::read_to_string(log).unwrap();
        assert!(requests.contains("thread/name/set"));
        assert!(requests.contains(id));
        assert!(requests.contains("research_pipeline"));
    }
}
