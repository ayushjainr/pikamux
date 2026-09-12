//! Provider accounting extracted from durable, structured metadata only.
//!
//! Pika never reads message text for this surface. Codex exposes cumulative
//! counters in its JSONL rollout, Claude exposes per-message counters, and
//! OpenCode exposes cumulative counters and provider-reported cost in SQLite.
//! Monetary values derived from Pika's rate table are API-equivalent estimates,
//! never subscription bills.

use crate::{
    model::{Provider, Session},
    paths::Paths,
    store::{Store, UsageCacheRecord},
};
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OpenFlags, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeSet,
    fs::{self, File},
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

pub const PRICING_AS_OF: &str = "2026-08-12";
const MAX_CODEX_TAIL_BYTES: u64 = 8 * 1024 * 1024;
const MAX_CLAUDE_INITIAL_BYTES: u64 = 128 * 1024 * 1024;
const MAX_CLAUDE_INCREMENT_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CostBasis {
    /// Estimated from public API list prices on [`PRICING_AS_OF`].
    ApiEquivalent,
    /// Reported directly by the provider's durable local state.
    ProviderReported,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct UsageMetrics {
    pub model: Option<String>,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cached_input_tokens: i64,
    pub cache_write_tokens: i64,
    pub total_tokens: i64,
    pub estimated_cost_usd: Option<f64>,
    pub cost_basis: Option<CostBasis>,
    pub pricing_as_of: Option<&'static str>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct UsageReport {
    pub hydrated: usize,
    pub unavailable: usize,
    pub errors: Vec<String>,
}

pub fn format_tokens(value: Option<i64>) -> String {
    match value {
        None => "—".to_owned(),
        Some(value) if value < 1_000 => value.to_string(),
        Some(value) if value < 1_000_000 => format!("{:.1}k", value as f64 / 1_000.0),
        Some(value) if value < 1_000_000_000 => {
            format!("{:.2}m", value as f64 / 1_000_000.0)
        }
        Some(value) => format!("{:.2}b", value as f64 / 1_000_000_000.0),
    }
}

pub fn format_cost(value: Option<f64>) -> String {
    match value {
        None => "—".to_owned(),
        Some(value) if value < 0.01 => format!("~${value:.3}"),
        Some(value) => format!("~${value:.2}"),
    }
}

/// Hydrate sessions independently: corrupt or unavailable accounting for one
/// provider never blocks the operational inventory.
pub fn hydrate_sessions(paths: &Paths, store: &Store, sessions: &mut [Session]) -> UsageReport {
    let mut report = UsageReport::default();
    for session in sessions {
        match usage_for_session(paths, store, session) {
            Ok(Some(usage)) => {
                session.model = usage.model.or_else(|| session.model.clone());
                session.input_tokens = Some(usage.input_tokens);
                session.output_tokens = Some(usage.output_tokens);
                session.cached_input_tokens = Some(usage.cached_input_tokens);
                session.cache_write_tokens = Some(usage.cache_write_tokens);
                session.total_tokens = Some(usage.total_tokens);
                session.estimated_cost_usd = usage.estimated_cost_usd;
                report.hydrated += 1;
            }
            Ok(None) => report.unavailable += 1,
            Err(error) => report.errors.push(format!(
                "{}:{}: {error}",
                session.provider, session.session_id
            )),
        }
    }
    report
}

pub fn usage_for_session(
    paths: &Paths,
    store: &Store,
    session: &Session,
) -> Result<Option<UsageMetrics>> {
    match session.provider {
        Provider::Codex => codex_usage(store, session),
        Provider::Claude => claude_usage(store, session),
        Provider::Opencode => opencode_usage(&paths.opencode_data_home, session),
    }
}

fn codex_usage(store: &Store, session: &Session) -> Result<Option<UsageMetrics>> {
    let Some(path) = session.transcript_path.as_deref().map(Path::new) else {
        return Ok(None);
    };
    let fingerprint = fingerprint(path)?;
    if let Some(cached) = store.get_cached_usage(
        Provider::Codex,
        &session.session_id,
        &path.to_string_lossy(),
        fingerprint.mtime_ns,
        fingerprint.size,
    )? {
        return Ok(Some(from_cache(cached, CostBasis::ApiEquivalent, true)));
    }

    let lines = reverse_tail(path, MAX_CODEX_TAIL_BYTES)?;
    let mut counters = None;
    let mut discovered_model = None;
    for line in lines {
        if !line.contains("token_usage") && !line.contains("\"model\"") {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let payload = value.get("payload").and_then(Value::as_object);
        if discovered_model.is_none() {
            discovered_model = payload
                .and_then(|item| item.get("model"))
                .and_then(Value::as_str)
                .map(str::to_owned);
        }
        if counters.is_none() {
            counters = payload
                .and_then(|item| item.get("info"))
                .and_then(Value::as_object)
                .and_then(|item| item.get("total_token_usage"))
                .and_then(Value::as_object)
                .map(|usage| {
                    (
                        token(usage.get("input_tokens")),
                        token(usage.get("output_tokens")),
                        token(usage.get("cached_input_tokens")),
                        token(usage.get("total_tokens")),
                    )
                });
        }
        if counters.is_some() && (session.model.is_some() || discovered_model.is_some()) {
            break;
        }
    }
    let Some((input, output, cached, total)) = counters else {
        return Ok(None);
    };
    let mut usage = UsageMetrics {
        model: session.model.clone().or(discovered_model),
        input_tokens: input,
        output_tokens: output,
        cached_input_tokens: cached,
        cache_write_tokens: 0,
        total_tokens: total,
        ..UsageMetrics::default()
    };
    attach_api_estimate(&mut usage, true);
    store.put_cached_usage(&to_cache(
        Provider::Codex,
        &session.session_id,
        path,
        fingerprint,
        &usage,
    ))?;
    Ok(Some(usage))
}

fn claude_usage(store: &Store, session: &Session) -> Result<Option<UsageMetrics>> {
    let Some(path) = session.transcript_path.as_deref().map(Path::new) else {
        return Ok(None);
    };
    let current = fingerprint(path)?;
    let source = path.to_string_lossy();
    if let Some(cached) = store.get_cached_usage(
        Provider::Claude,
        &session.session_id,
        &source,
        current.mtime_ns,
        current.size,
    )? {
        return Ok(Some(from_cache(cached, CostBasis::ApiEquivalent, false)));
    }

    let prior = store.get_latest_cached_usage(Provider::Claude, &session.session_id, &source)?;
    let can_increment = prior
        .as_ref()
        .is_some_and(|value| value.source_size >= 0 && value.source_size < current.size);
    let (start, limit, mut usage) = if can_increment {
        let cached = prior.expect("checked above");
        (
            cached.source_size as u64,
            MAX_CLAUDE_INCREMENT_BYTES,
            from_cache(cached, CostBasis::ApiEquivalent, false),
        )
    } else {
        (
            0,
            MAX_CLAUDE_INITIAL_BYTES,
            UsageMetrics {
                model: session.model.clone(),
                ..UsageMetrics::default()
            },
        )
    };

    let mut found = false;
    let parsed_end = scan_forward_json(path, start, limit, b"\"usage\"", |value| {
        let Some(message) = value.get("message").and_then(Value::as_object) else {
            return Ok(());
        };
        let Some(tokens) = message.get("usage").and_then(Value::as_object) else {
            return Ok(());
        };
        found = true;
        if let Some(model) = message.get("model").and_then(Value::as_str) {
            usage.model = Some(model.to_owned());
        }
        checked_add(&mut usage.input_tokens, token(tokens.get("input_tokens")))?;
        checked_add(&mut usage.output_tokens, token(tokens.get("output_tokens")))?;
        checked_add(
            &mut usage.cached_input_tokens,
            token(tokens.get("cache_read_input_tokens")),
        )?;
        checked_add(
            &mut usage.cache_write_tokens,
            token(tokens.get("cache_creation_input_tokens")),
        )?;
        Ok(())
    })?;
    if !found && start == 0 {
        return Ok(None);
    }
    usage.total_tokens = [
        usage.input_tokens,
        usage.output_tokens,
        usage.cached_input_tokens,
        usage.cache_write_tokens,
    ]
    .into_iter()
    .try_fold(0_i64, |sum, value| sum.checked_add(value))
    .context("Claude token total exceeds supported range")?;
    attach_api_estimate(&mut usage, false);
    let parsed = Fingerprint {
        mtime_ns: current.mtime_ns,
        size: parsed_end as i64,
    };
    store.put_cached_usage(&to_cache(
        Provider::Claude,
        &session.session_id,
        path,
        parsed,
        &usage,
    ))?;
    Ok(Some(usage))
}

fn opencode_usage(home: &Path, session: &Session) -> Result<Option<UsageMetrics>> {
    let database = home.join("opencode.db");
    if !database.is_file() {
        return Ok(None);
    }
    let uri = format!("file:{}?mode=ro", database.to_string_lossy());
    let db = Connection::open_with_flags(
        uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let columns = table_columns(&db, "session")?;
    if !columns.contains("id") || !columns.contains("parent_id") {
        return Ok(None);
    }
    let active = if columns.contains("time_archived") {
        " AND time_archived IS NULL"
    } else {
        ""
    };
    let child_active = if columns.contains("time_archived") {
        " WHERE child.time_archived IS NULL"
    } else {
        ""
    };
    let sum = |column: &str| {
        if columns.contains(column) {
            format!("SUM(COALESCE(CAST(s.{column} AS INTEGER),0))")
        } else {
            "0".to_owned()
        }
    };
    let cost = if columns.contains("cost") {
        "SUM(COALESCE(CAST(s.cost AS REAL),0))".to_owned()
    } else {
        "NULL".to_owned()
    };
    let model_column = if columns.contains("model") {
        "s.model"
    } else {
        "NULL"
    };
    // Keep ?2 present even for older schemas without a model column so the
    // prepared statement has one stable binding shape.
    let model = format!("MAX(CASE WHEN s.id=?2 THEN {model_column} END)");
    let sql = format!(
        "WITH RECURSIVE tree(id) AS (\
         SELECT id FROM session WHERE id=?1{active} \
         UNION SELECT child.id FROM session AS child JOIN tree ON child.parent_id=tree.id{child_active}\
         ) SELECT COUNT(*),{model},{cost},{},{},{},{},{} \
         FROM session AS s JOIN tree ON s.id=tree.id",
        sum("tokens_input"),
        sum("tokens_output"),
        sum("tokens_reasoning"),
        sum("tokens_cache_read"),
        sum("tokens_cache_write"),
    );
    let thread = session.provider_thread_id();
    let row = db.query_row(&sql, params![thread, thread], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, Option<f64>>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, i64>(4)?,
            row.get::<_, i64>(5)?,
            row.get::<_, i64>(6)?,
            row.get::<_, i64>(7)?,
        ))
    })?;
    if row.0 == 0 {
        return Ok(None);
    }
    if [row.3, row.4, row.5, row.6, row.7]
        .into_iter()
        .any(|value| value < 0)
    {
        bail!("OpenCode returned a negative token counter")
    }
    if row.2.is_some_and(|cost| !cost.is_finite() || cost < 0.0) {
        bail!("OpenCode returned an invalid provider cost")
    }
    let total = [row.3, row.4, row.5, row.6, row.7]
        .into_iter()
        .try_fold(0_i64, |sum, value| sum.checked_add(value))
        .context("OpenCode token total exceeds supported range")?;
    Ok(Some(UsageMetrics {
        model: row
            .1
            .as_deref()
            .and_then(opencode_model)
            .or_else(|| session.model.clone()),
        input_tokens: row.3,
        output_tokens: row.4,
        cached_input_tokens: row.6,
        cache_write_tokens: row.7,
        total_tokens: total,
        estimated_cost_usd: row.2,
        cost_basis: row.2.map(|_| CostBasis::ProviderReported),
        pricing_as_of: None,
    }))
}

#[derive(Clone, Copy)]
struct Fingerprint {
    mtime_ns: i64,
    size: i64,
}

fn fingerprint(path: &Path) -> Result<Fingerprint> {
    let metadata = fs::metadata(path)
        .with_context(|| format!("cannot inspect usage source {}", path.display()))?;
    let modified = metadata
        .modified()
        .unwrap_or(SystemTime::UNIX_EPOCH)
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    Ok(Fingerprint {
        mtime_ns: i64::try_from(modified).unwrap_or(i64::MAX),
        size: i64::try_from(metadata.len()).unwrap_or(i64::MAX),
    })
}

fn scan_forward_json<F>(
    path: &Path,
    start: u64,
    limit: u64,
    needle: &[u8],
    mut visit: F,
) -> Result<u64>
where
    F: FnMut(&Value) -> Result<()>,
{
    let size = fs::metadata(path)?.len();
    if start > size {
        bail!("usage checkpoint is past end of source")
    }
    if size.saturating_sub(start) > limit {
        bail!(
            "usage source requires scanning {} bytes; bounded limit is {limit}",
            size - start
        )
    }
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(start))?;
    let mut reader = BufReader::new(file);
    let mut position = start;
    let mut parsed_end = start;
    loop {
        let mut bytes = Vec::new();
        let read = reader.read_until(b'\n', &mut bytes)?;
        if read == 0 {
            break;
        }
        position += read as u64;
        let terminated = bytes.last() == Some(&b'\n');
        let without_newline = bytes.strip_suffix(b"\n").unwrap_or(&bytes);
        let trimmed = without_newline
            .strip_suffix(b"\r")
            .unwrap_or(without_newline);
        if terminated && !trimmed.windows(needle.len()).any(|window| window == needle) {
            parsed_end = position;
            continue;
        }
        let parsed = serde_json::from_slice::<Value>(trimmed);
        if let Ok(value) = parsed {
            visit(&value)?;
            parsed_end = position;
        } else if terminated {
            // A malformed newline-terminated record cannot become valid later.
            parsed_end = position;
        }
    }
    Ok(parsed_end)
}

fn reverse_tail(path: &Path, limit: u64) -> Result<Vec<String>> {
    let mut file = File::open(path)?;
    let size = file.metadata()?.len();
    let start = size.saturating_sub(limit);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::with_capacity(usize::try_from(size - start).unwrap_or(0));
    file.take(limit).read_to_end(&mut bytes)?;
    if start > 0 {
        if let Some(index) = bytes.iter().position(|byte| *byte == b'\n') {
            bytes.drain(..=index);
        } else {
            return Ok(Vec::new());
        }
    }
    Ok(bytes
        .split(|byte| *byte == b'\n')
        .rev()
        .filter(|line| !line.is_empty())
        .map(|line| String::from_utf8_lossy(line).into_owned())
        .collect())
}

fn checked_add(target: &mut i64, value: i64) -> Result<()> {
    *target = target
        .checked_add(value)
        .context("provider token counter exceeds supported range")?;
    Ok(())
}

fn token(value: Option<&Value>) -> i64 {
    value
        .and_then(|value| {
            value
                .as_i64()
                .filter(|number| *number >= 0)
                .or_else(|| value.as_u64().and_then(|number| i64::try_from(number).ok()))
        })
        .unwrap_or(0)
}

#[derive(Clone, Copy)]
struct Rate {
    input: f64,
    output: f64,
    cached_input: Option<f64>,
    cache_write: Option<f64>,
}

fn rate(model: Option<&str>) -> Option<Rate> {
    let normalized = model?.to_ascii_lowercase();
    const RATES: [(&str, Rate); 10] = [
        (
            "claude-haiku-4-5",
            Rate {
                input: 1.0,
                output: 5.0,
                cached_input: Some(0.1),
                cache_write: Some(1.25),
            },
        ),
        (
            "claude-sonnet-5",
            Rate {
                input: 2.0,
                output: 10.0,
                cached_input: Some(0.2),
                cache_write: Some(2.5),
            },
        ),
        (
            "claude-opus-4-6",
            Rate {
                input: 5.0,
                output: 25.0,
                cached_input: Some(0.5),
                cache_write: Some(6.25),
            },
        ),
        (
            "claude-opus-4-7",
            Rate {
                input: 5.0,
                output: 25.0,
                cached_input: Some(0.5),
                cache_write: Some(6.25),
            },
        ),
        (
            "claude-opus-4-8",
            Rate {
                input: 5.0,
                output: 25.0,
                cached_input: Some(0.5),
                cache_write: Some(6.25),
            },
        ),
        (
            "claude-opus-5",
            Rate {
                input: 5.0,
                output: 25.0,
                cached_input: Some(0.5),
                cache_write: Some(6.25),
            },
        ),
        (
            "gpt-5.4-mini",
            Rate {
                input: 0.75,
                output: 4.5,
                cached_input: Some(0.075),
                cache_write: None,
            },
        ),
        (
            "gpt-5.4-nano",
            Rate {
                input: 0.2,
                output: 1.25,
                cached_input: Some(0.02),
                cache_write: None,
            },
        ),
        (
            "gpt-5.4",
            Rate {
                input: 2.5,
                output: 15.0,
                cached_input: Some(0.25),
                cache_write: None,
            },
        ),
        (
            "gpt-5.5",
            Rate {
                input: 12.5,
                output: 75.0,
                cached_input: Some(1.25),
                cache_write: None,
            },
        ),
    ];
    RATES
        .iter()
        .find(|(name, _)| normalized == *name || normalized.starts_with(&format!("{name}-")))
        .map(|(_, rate)| *rate)
}

fn attach_api_estimate(usage: &mut UsageMetrics, cached_in_input: bool) {
    let Some(rate) = rate(usage.model.as_deref()) else {
        usage.estimated_cost_usd = None;
        usage.cost_basis = None;
        usage.pricing_as_of = None;
        return;
    };
    let cached = if cached_in_input {
        usage.cached_input_tokens.min(usage.input_tokens)
    } else {
        usage.cached_input_tokens
    };
    let uncached = if cached_in_input {
        usage.input_tokens.saturating_sub(cached)
    } else {
        usage.input_tokens
    };
    usage.estimated_cost_usd = Some(
        (uncached as f64 * rate.input
            + cached as f64 * rate.cached_input.unwrap_or(rate.input)
            + usage.output_tokens as f64 * rate.output
            + usage.cache_write_tokens as f64 * rate.cache_write.unwrap_or(rate.input))
            / 1_000_000.0,
    );
    usage.cost_basis = Some(CostBasis::ApiEquivalent);
    usage.pricing_as_of = Some(PRICING_AS_OF);
}

fn to_cache(
    provider: Provider,
    session_id: &str,
    path: &Path,
    fingerprint: Fingerprint,
    usage: &UsageMetrics,
) -> UsageCacheRecord {
    UsageCacheRecord {
        provider,
        session_id: session_id.to_owned(),
        source_path: path.to_string_lossy().into_owned(),
        source_mtime_ns: fingerprint.mtime_ns,
        source_size: fingerprint.size,
        model: usage.model.clone(),
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cached_input_tokens: usage.cached_input_tokens,
        cache_write_tokens: usage.cache_write_tokens,
        total_tokens: usage.total_tokens,
        estimated_cost_usd: usage.estimated_cost_usd,
        updated_at: now(),
    }
}

fn from_cache(value: UsageCacheRecord, basis: CostBasis, cached_in_input: bool) -> UsageMetrics {
    let cached_cost = value.estimated_cost_usd;
    let mut usage = UsageMetrics {
        model: value.model,
        input_tokens: value.input_tokens,
        output_tokens: value.output_tokens,
        cached_input_tokens: value.cached_input_tokens,
        cache_write_tokens: value.cache_write_tokens,
        total_tokens: value.total_tokens,
        ..UsageMetrics::default()
    };
    match basis {
        // Reprice cached counters so a changed dated rate table never leaves a
        // stale number attached to otherwise-valid transcript evidence.
        CostBasis::ApiEquivalent => attach_api_estimate(&mut usage, cached_in_input),
        CostBasis::ProviderReported => {
            usage.estimated_cost_usd = cached_cost;
            usage.cost_basis = cached_cost.map(|_| CostBasis::ProviderReported);
        }
    }
    usage
}

fn table_columns(db: &Connection, table: &str) -> Result<BTreeSet<String>> {
    let mut statement = db.prepare(&format!("PRAGMA table_info({table})"))?;
    Ok(statement
        .query_map([], |row| row.get(1))?
        .flatten()
        .collect())
}

fn opencode_model(raw: &str) -> Option<String> {
    let Ok(value) = serde_json::from_str::<Value>(raw) else {
        return Some(raw.to_owned());
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

fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}
