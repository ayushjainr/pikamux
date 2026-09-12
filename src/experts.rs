//! Durable expert cards and metadata-only discovery.
//!
//! This module never starts a provider or reads transcript contents. A transcript
//! fingerprint is metadata used only to describe whether the published current
//! work still matches the provider source.

use crate::model::{ExpertProfile, Provider, Session, Status};
use crate::store::{Store, StoredExpertProfile};
use anyhow::{Context, Result, bail};
use regex::Regex;
use rusqlite::{Connection, OpenFlags};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_SCOPE: usize = 600;
const MAX_CURRENT_STATE: usize = 600;
const MAX_TOPIC: usize = 80;
const MAX_ARTIFACT: usize = 500;
const MAX_ITEMS: usize = 12;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TranscriptFingerprint {
    pub checkpoint: i64,
    pub size: i64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum CardStatus {
    Current,
    Stale,
    Missing,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CardState {
    pub status: CardStatus,
    pub detail: String,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Freshness {
    pub scope_updated_at: Option<f64>,
    pub scope_age_seconds: Option<f64>,
    pub scope_status: &'static str,
    pub current_state_updated_at: Option<f64>,
    pub current_state_age_seconds: Option<f64>,
    pub current_state_status: CardStatus,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ExpertMatch {
    pub provider: Provider,
    pub session_id: String,
    pub name: String,
    pub project: Option<String>,
    pub branch: Option<String>,
    pub status: Status,
    pub live: bool,
    pub summary: String,
    pub scope: String,
    pub current_state: String,
    pub topics: Vec<String>,
    pub artifacts: Vec<String>,
    pub profile_updated_at: f64,
    pub profile_source: String,
    pub card_status: CardStatus,
    pub score: i64,
    pub matched_on: Vec<String>,
    pub watched: bool,
    pub discoverable: bool,
    pub availability: String,
    #[serde(flatten)]
    pub freshness: Freshness,
}

/// Capability produced after the lifecycle layer proves the calling pane.
///
/// The constructor validates the immutable identity tuple. It intentionally does
/// not inspect processes: the process generation proof belongs to `core`, while
/// this module makes it impossible to accidentally publish a different session.
#[derive(Clone, Debug)]
pub struct PublisherProof {
    provider: Provider,
    workstream_id: String,
    provider_thread_id: String,
}

impl PublisherProof {
    pub fn from_verified_identity(
        session: &Session,
        provider: Provider,
        workstream_id: &str,
        provider_thread_id: &str,
    ) -> Result<Self> {
        if session.provider != provider
            || session.session_id != workstream_id
            || session.provider_thread_id() != provider_thread_id
        {
            bail!("expert publication identity does not match the exact calling conversation");
        }
        Ok(Self {
            provider,
            workstream_id: workstream_id.to_owned(),
            provider_thread_id: provider_thread_id.to_owned(),
        })
    }

    fn proves(&self, session: &Session) -> bool {
        self.provider == session.provider
            && self.workstream_id == session.session_id
            && self.provider_thread_id == session.provider_thread_id()
    }
}

#[derive(Clone, Debug, Default)]
pub struct PublishInput {
    pub scope: String,
    pub current_state: String,
    pub topics: Vec<String>,
    pub artifacts: Vec<String>,
    pub source: String,
}

pub fn publish(
    store: &Store,
    session: &Session,
    proof: &PublisherProof,
    input: PublishInput,
) -> Result<StoredExpertProfile> {
    if !proof.proves(session) {
        bail!("cannot publish an expert card for a conversation other than the proven caller");
    }
    let profile = make_profile(session, input)?;
    let fingerprint = transcript_fingerprint(session)?;
    store.put_expert_profile(&StoredExpertProfile {
        profile,
        transcript_mtime_ns: fingerprint.map(|value| value.checkpoint),
        transcript_size: fingerprint.map(|value| value.size),
        current_state_mtime_ns: fingerprint.map(|value| value.checkpoint),
        current_state_size: fingerprint.map(|value| value.size),
    })
}

pub fn publish_current_work(
    store: &Store,
    session: &Session,
    proof: &PublisherProof,
    current_state: &str,
) -> Result<StoredExpertProfile> {
    if !proof.proves(session) {
        bail!("cannot update expert work for a conversation other than the proven caller");
    }
    let clean = clean(current_state, "current state", MAX_CURRENT_STATE)?;
    let Some(mut existing) =
        store.get_stored_expert_profile(session.provider, &session.session_id)?
    else {
        bail!("publish an expert profile before a current-work update");
    };
    if existing.profile.current_state == clean {
        return Ok(existing);
    }
    existing.profile.current_state = clean;
    existing.profile.updated_at = now();
    let fingerprint = transcript_fingerprint(session)?;
    existing.transcript_mtime_ns = fingerprint.map(|value| value.checkpoint);
    existing.transcript_size = fingerprint.map(|value| value.size);
    store.put_expert_profile(&existing)
}

pub fn make_profile(session: &Session, input: PublishInput) -> Result<ExpertProfile> {
    let scope = clean(&input.scope, "scope", MAX_SCOPE)?;
    let current_state = if input.current_state.trim().is_empty() {
        String::new()
    } else {
        clean(&input.current_state, "current state", MAX_CURRENT_STATE)?
    };
    let topics = clean_many(&input.topics, "topic", MAX_TOPIC, MAX_ITEMS, true)?;
    let artifacts = clean_many(&input.artifacts, "artifact", MAX_ARTIFACT, MAX_ITEMS, false)?;
    Ok(ExpertProfile {
        provider: session.provider,
        session_id: session.session_id.clone(),
        summary: scope,
        current_state,
        topics,
        artifacts,
        source: if input.source.trim().is_empty() {
            "self".to_owned()
        } else {
            clean(&input.source, "source", 40)?
        },
        updated_at: now(),
        scope_updated_at: 0.0,
        current_state_updated_at: 0.0,
    })
}

pub fn rank_experts(
    profiles: &[StoredExpertProfile],
    sessions: &[Session],
    query: &str,
    untracked: &BTreeSet<(Provider, String)>,
) -> Vec<ExpertMatch> {
    let sessions: BTreeMap<(Provider, String), &Session> = sessions
        .iter()
        .map(|session| ((session.provider, session.session_id.clone()), session))
        .collect();
    let raw_query = query
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    let query_tokens = tokens(&raw_query)
        .into_iter()
        .filter(|token| !ignored_terms().contains(token.as_str()))
        .collect::<Vec<_>>();
    let terms = query_tokens.iter().cloned().collect::<BTreeSet<_>>();
    if !raw_query.is_empty() && terms.is_empty() {
        return Vec::new();
    }

    let mut matches = Vec::new();
    for stored in profiles {
        let profile = &stored.profile;
        let Some(session) = sessions.get(&(profile.provider, profile.session_id.clone())) else {
            continue;
        };
        let fields = [
            ("topic", profile.topics.join(" "), 10_i64),
            ("scope", profile.summary.clone(), 6),
            ("now", profile.current_state.clone(), 5),
            ("name", session.name.clone().unwrap_or_default(), 4),
            (
                "project",
                [session.cwd.as_deref(), session.branch.as_deref()]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join(" "),
                3,
            ),
            ("artifact", profile.artifacts.join(" "), 2),
        ];
        let mut score = 0;
        let mut matched_on = Vec::new();
        let mut matched_terms = BTreeSet::new();
        for (label, value, weight) in fields {
            let field_tokens = tokens(&value);
            let field_terms = field_tokens.iter().cloned().collect::<BTreeSet<_>>();
            let field_matches = terms
                .intersection(&field_terms)
                .cloned()
                .collect::<Vec<_>>();
            if !field_matches.is_empty() {
                score += i64::try_from(field_matches.len()).unwrap_or(i64::MAX) * weight;
                matched_on.push(label.to_owned());
                matched_terms.extend(field_matches);
            }
            if !query_tokens.is_empty() && contains_tokens(&field_tokens, &query_tokens) {
                score += weight * 2;
            }
        }
        if !raw_query.is_empty() && matched_terms != terms {
            continue;
        }
        let fingerprint = transcript_fingerprint(session).ok().flatten();
        matches.push(ExpertMatch {
            provider: session.provider,
            session_id: session.session_id.clone(),
            name: session.display_name(),
            project: session.cwd.clone(),
            branch: session.branch.clone(),
            status: session.status,
            live: session.live,
            summary: profile.summary.clone(),
            scope: profile.summary.clone(),
            current_state: profile.current_state.clone(),
            topics: profile.topics.clone(),
            artifacts: profile.artifacts.clone(),
            profile_updated_at: profile.updated_at,
            profile_source: profile.source.clone(),
            card_status: card_state_from_fingerprint(stored, fingerprint).status,
            score,
            matched_on,
            watched: !untracked.contains(&(session.provider, session.session_id.clone())),
            discoverable: true,
            availability: expert_availability(session, fingerprint).to_owned(),
            freshness: profile_freshness_from_fingerprint(stored, fingerprint, now()),
        });
    }
    matches.sort_by(|left, right| {
        right
            .score
            .cmp(&left.score)
            .then_with(|| right.live.cmp(&left.live))
            .then_with(|| right.profile_updated_at.total_cmp(&left.profile_updated_at))
            .then_with(|| right.session_id.cmp(&left.session_id))
    });
    matches
}

pub fn card_state(session: &Session, profile: Option<&StoredExpertProfile>) -> CardState {
    let fingerprint = transcript_fingerprint(session).ok().flatten();
    match profile {
        None if fingerprint.is_none() => CardState {
            status: CardStatus::Unknown,
            detail: "durable transcript unavailable".to_owned(),
        },
        None => CardState {
            status: CardStatus::Missing,
            detail: "not interviewed yet".to_owned(),
        },
        Some(profile) => card_state_from_fingerprint(profile, fingerprint),
    }
}

fn card_state_from_fingerprint(
    profile: &StoredExpertProfile,
    fingerprint: Option<TranscriptFingerprint>,
) -> CardState {
    let Some(fingerprint) = fingerprint else {
        return CardState {
            status: CardStatus::Unknown,
            detail: "durable transcript unavailable".to_owned(),
        };
    };
    if profile.profile.current_state.is_empty() {
        return CardState {
            status: CardStatus::Stale,
            detail: "legacy card lacks a current-state snapshot".to_owned(),
        };
    }
    let saved = (profile.transcript_mtime_ns, profile.transcript_size);
    if saved == (Some(fingerprint.checkpoint), Some(fingerprint.size)) {
        CardState {
            status: CardStatus::Current,
            detail: "matches transcript".to_owned(),
        }
    } else {
        CardState {
            status: CardStatus::Stale,
            detail: "conversation changed".to_owned(),
        }
    }
}

pub fn profile_freshness(
    session: &Session,
    profile: Option<&StoredExpertProfile>,
    at: f64,
) -> Freshness {
    let fingerprint = transcript_fingerprint(session).ok().flatten();
    profile.map_or(
        Freshness {
            scope_updated_at: None,
            scope_age_seconds: None,
            scope_status: "MISSING",
            current_state_updated_at: None,
            current_state_age_seconds: None,
            current_state_status: CardStatus::Missing,
        },
        |stored| profile_freshness_from_fingerprint(stored, fingerprint, at),
    )
}

fn profile_freshness_from_fingerprint(
    profile: &StoredExpertProfile,
    fingerprint: Option<TranscriptFingerprint>,
    at: f64,
) -> Freshness {
    let scope_at =
        nonzero(profile.profile.scope_updated_at).or_else(|| nonzero(profile.profile.updated_at));
    let work_at = if profile.profile.current_state.is_empty() {
        None
    } else {
        nonzero(profile.profile.current_state_updated_at)
            .or_else(|| nonzero(profile.profile.updated_at))
    };
    let saved = if profile.profile.current_state_updated_at > 0.0 {
        (profile.current_state_mtime_ns, profile.current_state_size)
    } else {
        (profile.transcript_mtime_ns, profile.transcript_size)
    };
    let current_state_status = if profile.profile.current_state.is_empty() {
        CardStatus::Missing
    } else if let Some(fingerprint) = fingerprint {
        if saved == (Some(fingerprint.checkpoint), Some(fingerprint.size)) {
            CardStatus::Current
        } else {
            CardStatus::Stale
        }
    } else {
        CardStatus::Unknown
    };
    Freshness {
        scope_updated_at: scope_at,
        scope_age_seconds: scope_at.map(|value| (at - value).max(0.0)),
        scope_status: "PUBLISHED",
        current_state_updated_at: work_at,
        current_state_age_seconds: work_at.map(|value| (at - value).max(0.0)),
        current_state_status,
    }
}

pub fn transcript_fingerprint(session: &Session) -> Result<Option<TranscriptFingerprint>> {
    let Some(path) = session.transcript_path.as_deref() else {
        return Ok(None);
    };
    if session.provider == Provider::Opencode {
        return opencode_fingerprint(Path::new(path), session.provider_thread_id());
    }
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("cannot stat {path}")),
    };
    let modified = metadata
        .modified()
        .context("provider transcript has no modification time")?
        .duration_since(UNIX_EPOCH)
        .context("provider transcript predates the Unix epoch")?;
    Ok(Some(TranscriptFingerprint {
        checkpoint: i64::try_from(modified.as_nanos()).context("transcript timestamp overflow")?,
        size: i64::try_from(metadata.len()).context("transcript size overflow")?,
    }))
}

fn opencode_fingerprint(path: &Path, session_id: &str) -> Result<Option<TranscriptFingerprint>> {
    if !path.is_file() {
        return Ok(None);
    }
    let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("cannot open OpenCode source {}", path.display()))?;
    let mut statement = db.prepare(
        "WITH RECURSIVE tree(id) AS (\
         SELECT id FROM session WHERE id=?1 \
         UNION ALL SELECT s.id FROM session s JOIN tree t ON s.parent_id=t.id \
         WHERE s.time_archived IS NULL) \
         SELECT COALESCE((SELECT MAX(time_updated) FROM session WHERE id IN tree),0), \
         COALESCE((SELECT COUNT(*) FROM message WHERE session_id IN tree),0)*1000000 + \
         COALESCE((SELECT COUNT(*) FROM part WHERE session_id IN tree),0)",
    )?;
    let (checkpoint, size): (i64, i64) =
        statement.query_row([session_id], |row| Ok((row.get(0)?, row.get(1)?)))?;
    Ok((checkpoint != 0).then_some(TranscriptFingerprint { checkpoint, size }))
}

pub fn expert_availability(
    session: &Session,
    fingerprint: Option<TranscriptFingerprint>,
) -> &'static str {
    if session
        .transcript_path
        .as_deref()
        .is_some_and(is_archived_path)
    {
        return "archived";
    }
    if matches!(session.status, Status::Error | Status::OpenTwice) {
        return "requires-reconciliation";
    }
    if fingerprint.is_none() {
        return "source-unavailable";
    }
    "source-available"
}

fn is_archived_path(value: &str) -> bool {
    Path::new(value).components().any(|component| {
        matches!(component, Component::Normal(name) if name == "archived_sessions" || name == "sessions_archived")
    })
}

fn clean(value: &str, label: &str, limit: usize) -> Result<String> {
    let value = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if value.is_empty() {
        bail!("expert {label} cannot be empty");
    }
    if value.chars().count() > limit {
        bail!("expert {label} must be {limit} characters or fewer");
    }
    if value.chars().any(char::is_control) {
        bail!("expert {label} contains control characters");
    }
    Ok(value)
}

fn clean_many(
    values: &[String],
    label: &str,
    limit: usize,
    maximum: usize,
    required: bool,
) -> Result<Vec<String>> {
    let mut seen = BTreeSet::new();
    let mut result = Vec::new();
    for value in values {
        let value = clean(value, label, limit)?;
        if seen.insert(value.to_lowercase()) {
            result.push(value);
        }
    }
    if required && result.is_empty() {
        bail!("publish at least one expert {label}");
    }
    if result.len() > maximum {
        bail!("publish at most {maximum} expert {label}s");
    }
    Ok(result)
}

fn tokens(value: &str) -> Vec<String> {
    static TOKEN: OnceLock<Regex> = OnceLock::new();
    TOKEN
        .get_or_init(|| Regex::new(r"[^\W_]+").expect("static token regex"))
        .find_iter(value)
        .map(|item| item.as_str().to_lowercase())
        .collect()
}

fn ignored_terms() -> &'static BTreeSet<&'static str> {
    static TERMS: OnceLock<BTreeSet<&'static str>> = OnceLock::new();
    TERMS.get_or_init(|| {
        [
            "a", "an", "and", "for", "in", "of", "on", "or", "the", "to", "with",
        ]
        .into_iter()
        .collect()
    })
}

fn contains_tokens(haystack: &[String], needle: &[String]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

fn nonzero(value: f64) -> Option<f64> {
    (value != 0.0).then_some(value)
}

fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}
