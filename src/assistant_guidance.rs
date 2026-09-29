//! Reversible, source-linked guidance. Learned wording is advisory, never a grant
//! or human instruction. Legacy presentation variants remain readable on disk.
use crate::assistant_context::SourceVersion;
use crate::assistant_memory::{MemoryError, NewRecord, Origin, Record, RecordKind, Scope, Store};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DetailLevel {
    Brief,
    Balanced,
    Detailed,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum StructureStyle {
    Prose,
    Bullets,
    NumberedSteps,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LeadWith {
    Answer,
    Recommendation,
    Evidence,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum QuestionStyle {
    OneAtATime,
    Grouped,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Adaptation {
    Guidance {
        topic: String,
        instruction: String,
        /// The ordinary turn/reflection interprets lasting versus one-off intent.
        /// This is a hypothesis, not a native proof of semantic correctness.
        lasting: bool,
        #[serde(default)]
        when: Option<String>,
    },
    Detail {
        level: DetailLevel,
    },
    Structure {
        style: StructureStyle,
    },
    Lead {
        with: LeadWith,
    },
    Questions {
        style: QuestionStyle,
    },
}
impl Adaptation {
    fn rendered(&self) -> String {
        match self {
            Self::Guidance {
                instruction,
                when: Some(condition),
                ..
            } => format!("When {condition}: {instruction}"),
            _ => self.instruction().to_owned(),
        }
    }
    fn key(&self) -> &str {
        match self {
            Self::Guidance { topic, .. } => topic,
            Self::Detail { .. } => "detail",
            Self::Structure { .. } => "structure",
            Self::Lead { .. } => "lead",
            Self::Questions { .. } => "questions",
        }
    }
    fn instruction(&self) -> &str {
        match self {
            Self::Guidance { instruction, .. } => instruction,
            Self::Detail {
                level: DetailLevel::Brief,
            } => "Prefer concise explanations while retaining material uncertainty and caveats.",
            Self::Detail {
                level: DetailLevel::Balanced,
            } => "Balance concise conclusions with enough explanation to support them.",
            Self::Detail {
                level: DetailLevel::Detailed,
            } => "Prefer detailed explanations when relevant to the question.",
            Self::Structure {
                style: StructureStyle::Prose,
            } => "Prefer short prose paragraphs when suitable.",
            Self::Structure {
                style: StructureStyle::Bullets,
            } => "Prefer bullets when presenting several related points.",
            Self::Structure {
                style: StructureStyle::NumberedSteps,
            } => "Prefer numbered steps for sequential instructions.",
            Self::Lead {
                with: LeadWith::Answer,
            } => "Lead with the answer when the evidence supports one.",
            Self::Lead {
                with: LeadWith::Recommendation,
            } => "Lead with a recommendation when one is warranted; retain the user's choice.",
            Self::Lead {
                with: LeadWith::Evidence,
            } => "Lead with the key evidence before the conclusion when useful.",
            Self::Questions {
                style: QuestionStyle::OneAtATime,
            } => "When clarification is necessary, prefer one focused question at a time.",
            Self::Questions {
                style: QuestionStyle::Grouped,
            } => "When clarification is necessary, group closely related questions where useful.",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Applicability {
    ScopeWide,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GuidanceSpec {
    pub adaptation: Adaptation,
    pub sources: Vec<SourceVersion>,
    pub applicability: Applicability,
    pub reason: String,
}

pub(crate) fn validate_sources(
    store: &Store,
    scope: &Scope,
    sources: &[SourceVersion],
) -> Result<(), MemoryError> {
    if sources.is_empty() || sources.len() > 64 {
        return Err(MemoryError::Invalid(
            "guidance needs 1–64 exact sources".into(),
        ));
    }
    let mut seen = BTreeSet::new();
    for source in sources {
        if !seen.insert(&source.id) {
            return Err(MemoryError::Invalid("duplicate guidance source".into()));
        }
        let record = store
            .get_active(&source.id)?
            .ok_or_else(|| MemoryError::Invalid("guidance source is no longer active".into()))?;
        if !record.scope.permits(scope)
            || store.source_version(&source.id)? != Some(source.revision)
        {
            return Err(MemoryError::Invalid(
                "guidance source version or scope changed".into(),
            ));
        }
    }
    Ok(())
}

pub(crate) fn validate_sources_in_tx(
    connection: &rusqlite::Connection,
    profile: &str,
    sources: &[SourceVersion],
) -> Result<(), MemoryError> {
    for source in sources {
        let live: bool = connection.query_row("SELECT EXISTS(SELECT 1 FROM memory_records m JOIN memory_revisions v ON v.record_id=m.id WHERE m.id=? AND m.profile_id=? AND v.revision=? AND NOT EXISTS(SELECT 1 FROM memory_records n WHERE n.profile_id=m.profile_id AND n.supersedes=m.id))", params![source.id,profile,source.revision], |r|r.get(0))?;
        if !live {
            return Err(MemoryError::Invalid(
                "guidance source revision is no longer active".into(),
            ));
        }
    }
    Ok(())
}

fn validate_adaptation(adaptation: &Adaptation) -> Result<(), MemoryError> {
    if let Adaptation::Guidance {
        topic,
        instruction,
        when,
        ..
    } = adaptation
    {
        if topic.trim().is_empty()
            || topic.len() > 128
            || topic.chars().any(char::is_control)
            || instruction.trim().is_empty()
            || instruction.len() > 2048
            || when
                .as_ref()
                .is_some_and(|v| v.trim().is_empty() || v.len() > 1024)
        {
            return Err(MemoryError::Invalid(
                "guidance needs a bounded topic, instruction and optional condition".into(),
            ));
        }
    }
    Ok(())
}

pub(crate) fn validate_guidance(
    store: &Store,
    scope: &Scope,
    spec: &GuidanceSpec,
    timestamp: i64,
) -> Result<NewRecord, MemoryError> {
    validate_sources(store, scope, &spec.sources)?;
    let (explicit, human_evidence, ambiguous) = evidence_support(store, spec)?;
    validate_adaptation(&spec.adaptation)?;
    if spec.reason.trim().is_empty() || spec.reason.len() > 2048 {
        return Err(MemoryError::Invalid(
            "bounded guidance reason and applicability required".into(),
        ));
    }
    let active = !ambiguous && human_evidence;
    Ok(NewRecord {
        kind: if active { RecordKind::InferredPreference } else { RecordKind::Proposal },
        origin: Origin::Worker, scope: scope.clone(), body: spec.adaptation.rendered(),
        provenance: serde_json::json!({"type":if active {"active_guidance_v1"} else {"guidance_proposal_v1"},"explicit_source_backed":explicit,"spec":spec}).to_string(),
        timestamp, supersedes: None, dependencies: spec.sources.iter().map(|s| s.id.clone()).collect(),
        decision_state: if active { None } else { Some(crate::assistant_memory::DecisionState::Proposed) }, protected_policy: false,
    })
}

fn evidence_support(store: &Store, spec: &GuidanceSpec) -> Result<(bool, bool, bool), MemoryError> {
    let mut remaining: Vec<String> = spec.sources.iter().map(|s| s.id.clone()).collect();
    let mut checked = BTreeSet::new();
    let mut explicit = false;
    let mut human_evidence = false;
    let mut ambiguous = matches!(spec.adaptation, Adaptation::Guidance { lasting: false, .. });
    while let Some(id) = remaining.pop() {
        if !checked.insert(id.clone()) {
            continue;
        }
        if checked.len() > 64 {
            return Err(MemoryError::Invalid(
                "guidance evidence ancestry exceeds bound".into(),
            ));
        }
        let record = store.get_active(&id)?.ok_or_else(|| {
            MemoryError::Invalid("guidance evidence disappeared or was superseded".into())
        })?;
        if record.origin == Origin::Human {
            human_evidence = true;
            if transient_or_quoted(&record.body) {
                ambiguous = true;
            }
            explicit |= matches!(
                record.kind,
                RecordKind::UserInstruction | RecordKind::Correction
            );
        }
        // A correction retains its predecessor as historical rationale, not as
        // another active vote for the old preference.
        remaining.extend(
            record
                .dependencies
                .into_iter()
                .filter(|id| Some(id) != record.supersedes.as_ref()),
        );
    }
    Ok((explicit, human_evidence, ambiguous))
}

// A rejection-only guard, never a grammar that promotes text to human authority.
// Ambiguous cases remain proposals; omission here is not proof of enduring intent.
fn transient_or_quoted(body: &str) -> bool {
    let lower = body.to_lowercase();
    [
        "today",
        "this time",
        "for this answer",
        "for this turn",
        "next reply",
        "next response",
        "this response",
        "this reply",
        "just once",
        "for example",
        "hypothetically",
        "suppose ",
        "say i tell you",
        "what if ",
        "imagine ",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
        || lower.trim_start().starts_with("if ")
        || lower.trim_start().starts_with("for ")
        || body.lines().any(|line| line.trim_start().starts_with('>'))
        || body.contains("```")
        || body.contains('"')
}

fn spec(record: &Record) -> Option<GuidanceSpec> {
    if record.kind != RecordKind::InferredPreference || record.origin != Origin::Worker {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(&record.provenance).ok()?;
    if value["type"] != "active_guidance_v1" {
        return None;
    }
    let spec: GuidanceSpec = serde_json::from_value(value["spec"].clone()).ok()?;
    (record.body == spec.adaptation.rendered()
        && record.dependencies
            == spec
                .sources
                .iter()
                .map(|s| s.id.clone())
                .collect::<Vec<_>>())
    .then_some(spec)
}

fn schema(store: &Store) -> Result<(), MemoryError> {
    store.connection.execute_batch("CREATE TABLE IF NOT EXISTS guidance_controls(record_id TEXT PRIMARY KEY, enabled INTEGER NOT NULL CHECK(enabled IN (0,1))); CREATE TABLE IF NOT EXISTS guidance_selection(scope_json TEXT NOT NULL,adaptation_key TEXT NOT NULL,record_id TEXT NOT NULL,PRIMARY KEY(scope_json,adaptation_key))")?;
    Ok(())
}

/// Explicit human control. Re-enabling never revives missing/revoked sources.
pub(crate) fn set_enabled(
    store: &mut Store,
    id: &str,
    enabled: bool,
    _timestamp: i64,
) -> Result<(), MemoryError> {
    schema(store)?;
    let (record, guidance) = controllable_guidance(store, id)?;
    let epoch = store.forget_epoch()?;
    let tx = store
        .connection
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    check_control_generation(&tx, id, epoch)?;
    validate_sources_in_tx(&tx, &store.profile_id, &guidance.sources)?;
    write_control(&tx, &record, &guidance, enabled)?;
    tx.commit()?;
    Ok(())
}

fn controllable_guidance(store: &Store, id: &str) -> Result<(Record, GuidanceSpec), MemoryError> {
    let record = store
        .get_active(id)?
        .ok_or_else(|| MemoryError::Invalid("guidance is unavailable".into()))?;
    let guidance = spec(&record)
        .ok_or_else(|| MemoryError::Invalid("not validated interaction guidance".into()))?;
    if validate_guidance(store, &record.scope, &guidance, record.timestamp)?.kind
        != RecordKind::InferredPreference
    {
        return Err(MemoryError::Invalid(
            "guidance no longer has supported standing evidence".into(),
        ));
    }
    Ok((record, guidance))
}

fn check_control_generation(
    tx: &rusqlite::Connection,
    id: &str,
    epoch: u64,
) -> Result<(), MemoryError> {
    let live: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM memory_records WHERE id=?)",
        [id],
        |row| row.get(0),
    )?;
    let current: Option<String> = tx
        .query_row(
            "SELECT value FROM memory_meta WHERE key='forget_epoch'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    if !live || current.as_deref().unwrap_or("0") != epoch.to_string() {
        return Err(MemoryError::Invalid(
            "guidance changed during control".into(),
        ));
    }
    Ok(())
}
fn write_control(
    tx: &rusqlite::Connection,
    record: &Record,
    guidance: &GuidanceSpec,
    enabled: bool,
) -> Result<(), MemoryError> {
    let id = &record.id;
    tx.execute("INSERT INTO guidance_controls(record_id,enabled) VALUES(?,?) ON CONFLICT(record_id) DO UPDATE SET enabled=excluded.enabled", params![id,enabled])?;
    tx.execute("INSERT INTO guidance_selection(scope_json,adaptation_key,record_id) VALUES(?,?,?) ON CONFLICT(scope_json,adaptation_key) DO UPDATE SET record_id=excluded.record_id", params![serde_json::to_string(&record.scope)?,guidance.adaptation.key(),id])?;
    Ok(())
}

fn guidance_candidates(store: &Store, scope: &Scope) -> Result<Vec<Record>, MemoryError> {
    // Explicit-backed adaptations have their own bounded SQL selection before
    // inferred recency. A burst of newer reflections cannot evict user intent.
    let mut query=store.connection.prepare("SELECT id FROM memory_records m WHERE profile_id=? AND kind='\"InferredPreference\"' AND origin='\"Worker\"' AND (project IS NULL OR project=?) AND (provider IS NULL OR provider=?) AND (conversation IS NULL OR conversation=?) AND (node IS NULL OR node=?) AND CASE WHEN json_valid(provenance) THEN json_extract(provenance,'$.explicit_source_backed') END=1 AND NOT EXISTS(SELECT 1 FROM memory_records n WHERE n.profile_id=m.profile_id AND n.supersedes=m.id) ORDER BY timestamp DESC,id DESC LIMIT 64")?;
    let ids = query
        .query_map(
            params![
                store.profile_id,
                scope.project,
                scope.provider,
                scope.conversation,
                scope.node
            ],
            |r| r.get::<_, String>(0),
        )?
        .collect::<Result<Vec<_>, _>>()?;
    let mut result = Vec::new();
    let mut seen = BTreeSet::new();
    for id in ids {
        if let Some(record) = store.get(&id)? {
            seen.insert(id);
            result.push(record);
        }
    }
    for record in store
        .active_by_kind(scope, RecordKind::InferredPreference, None, 64)?
        .records
    {
        if seen.insert(record.id.clone()) {
            result.push(record);
        }
    }
    Ok(result)
}

fn control_state(
    store: &Store,
    record: &Record,
    guidance: &GuidanceSpec,
    has_controls: bool,
) -> Result<Option<bool>, MemoryError> {
    if !has_controls {
        return Ok(Some(true));
    }
    let selected: Option<String> = store
        .connection
        .query_row(
            "SELECT record_id FROM guidance_selection WHERE scope_json=? AND adaptation_key=?",
            params![
                serde_json::to_string(&record.scope)?,
                guidance.adaptation.key()
            ],
            |r| r.get(0),
        )
        .optional()?;
    if selected.as_deref().is_some_and(|id| id != record.id) {
        return Ok(None);
    }
    let enabled: Option<bool> = store
        .connection
        .query_row(
            "SELECT enabled FROM guidance_controls WHERE record_id=?",
            [&record.id],
            |r| r.get(0),
        )
        .optional()?;
    Ok(Some(enabled.unwrap_or(true)))
}

fn applicable_candidate(
    store: &Store,
    record: &Record,
    has_controls: bool,
) -> Result<Option<(String, bool)>, MemoryError> {
    let Some(guidance) = spec(record) else {
        return Ok(None);
    };
    if !validate_guidance(store, &record.scope, &guidance, record.timestamp)
        .is_ok_and(|r| r.kind == RecordKind::InferredPreference)
    {
        return Ok(None);
    }
    Ok(control_state(store, record, &guidance, has_controls)?
        .map(|enabled| (guidance.adaptation.key().to_owned(), enabled)))
}

/// Advisory guidance may coexist with unrelated explicit instructions. The
/// original human instructions always take precedence in the context contract.
pub(crate) fn applicable_guidance(
    store: &Store,
    scope: &Scope,
    limit: usize,
) -> Result<Vec<Record>, MemoryError> {
    if limit == 0 {
        return Ok(vec![]);
    }
    let has_controls: bool = store.connection.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='guidance_controls')", [], |r|r.get(0))?;
    let mut seen = BTreeSet::new();
    let mut result = Vec::new();
    for record in guidance_candidates(store, scope)? {
        let Some((key, enabled)) = applicable_candidate(store, &record, has_controls)? else {
            continue;
        };
        // A disabled newest adaptation does not resurrect an older adaptation.
        if !seen.insert(key) || !enabled {
            continue;
        }
        result.push(record);
        if result.len() >= limit.min(16) {
            break;
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_guidance_retains_conditions_and_reaches_fresh_context_without_new_authority() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("private/memory.sqlite");
        let scope = Scope {
            project: Some("synthetic".into()),
            ..Scope::default()
        };
        let mut store = Store::open(&path).unwrap();
        let human = store.append(NewRecord {
            kind: RecordKind::Finding, origin: Origin::Human, scope: scope.clone(),
            body: "When we review designs, challenge my premise before agreeing; don't just flatter me.".into(),
            provenance: "submitted_user_message".into(), timestamp: 1,
            supersedes: None, dependencies: vec![], decision_state: None, protected_policy: false,
        }).unwrap();
        let mut spec = GuidanceSpec {
            adaptation: Adaptation::Guidance {
                topic: "design-discussion".into(),
                instruction: "Challenge the premise and explain a credible alternative without reflexive agreement.".into(),
                lasting: true, when: Some("reviewing designs".into()),
            },
            sources: vec![SourceVersion { id: human.id.clone(), revision: store.source_version(&human.id).unwrap().unwrap() }],
            applicability: Applicability::ScopeWide, reason: "A direct request for our ongoing design discussions, not a personality diagnosis.".into(),
        };
        let learned = validate_guidance(&store, &scope, &spec, 2).unwrap();
        assert_eq!(learned.kind, RecordKind::InferredPreference);
        assert_eq!(learned.origin, Origin::Worker);
        assert!(!learned.protected_policy);
        let learned = store.append(learned).unwrap();
        drop(store);
        let mut store = Store::open(&path).unwrap();
        let package = crate::assistant_context::build(
            &store,
            &scope,
            &["architecture".into()],
            &[],
            32 * 1024,
        )
        .unwrap();
        let recalled = package.records.iter().find(|r| r.id == learned.id).unwrap();
        assert!(recalled.body.starts_with("When reviewing designs:"));
        assert!(package.methods.is_empty());
        assert!(
            store
                .active_by_kind(&scope, RecordKind::UserInstruction, None, 10)
                .unwrap()
                .records
                .is_empty(),
            "model interpretation must never become human authority"
        );
        if let Adaptation::Guidance { lasting, .. } = &mut spec.adaptation {
            *lasting = false;
        }
        assert_eq!(
            validate_guidance(&store, &scope, &spec, 3).unwrap().kind,
            RecordKind::Proposal
        );
        set_enabled(&mut store, &learned.id, false, 4).unwrap();
        assert!(applicable_guidance(&store, &scope, 16).unwrap().is_empty());
        set_enabled(&mut store, &learned.id, true, 5).unwrap();
        store.forget(&human.id).unwrap();
        assert!(applicable_guidance(&store, &scope, 16).unwrap().is_empty());
        assert!(set_enabled(&mut store, &learned.id, true, 6).is_err());
    }

    fn source(store: &mut Store, scope: &Scope) -> Record {
        store
            .append(NewRecord {
                kind: RecordKind::UserInstruction,
                origin: Origin::Human,
                scope: scope.clone(),
                body: "I prefer concise answers".into(),
                provenance: "synthetic human input".into(),
                timestamp: 1,
                supersedes: None,
                dependencies: vec![],
                decision_state: None,
                protected_policy: false,
            })
            .unwrap()
    }

    #[test]
    fn hypothetical_nickname_cannot_become_active_guidance() {
        let temp = tempfile::tempdir().unwrap();
        let mut store = Store::open(temp.path().join("private/memory.sqlite")).unwrap();
        let scope = Scope::default();
        let proposed = store
            .append(NewRecord {
                kind: RecordKind::Finding,
                origin: Origin::Human,
                scope: scope.clone(),
                body:
                    "Say I tell you to start calling me King in the North: how will you remember?"
                        .into(),
                provenance: "synthetic human question".into(),
                timestamp: 1,
                supersedes: None,
                dependencies: vec![],
                decision_state: None,
                protected_policy: false,
            })
            .unwrap();
        let spec = |record: &Record, store: &Store| GuidanceSpec {
            adaptation: Adaptation::Guidance {
                topic: "form-of-address".into(),
                instruction: "Call the user King in the North.".into(),
                lasting: true,
                when: None,
            },
            sources: vec![SourceVersion {
                id: record.id.clone(),
                revision: store.source_version(&record.id).unwrap().unwrap(),
            }],
            applicability: Applicability::ScopeWide,
            reason: "Model proposes a lasting form of address.".into(),
        };
        assert_eq!(
            validate_guidance(&store, &scope, &spec(&proposed, &store), 2)
                .unwrap()
                .kind,
            RecordKind::Proposal
        );
        let explicit = store
            .append(NewRecord {
                kind: RecordKind::Finding,
                origin: Origin::Human,
                scope: scope.clone(),
                body: "From now on, call me King in the North.".into(),
                provenance: "synthetic human instruction".into(),
                timestamp: 3,
                supersedes: None,
                dependencies: vec![],
                decision_state: None,
                protected_policy: false,
            })
            .unwrap();
        assert_eq!(
            validate_guidance(&store, &scope, &spec(&explicit, &store), 4)
                .unwrap()
                .kind,
            RecordKind::InferredPreference
        );
    }

    #[test]
    fn lengthy_advice_cannot_crowd_out_explicit_instructions() {
        let temp = tempfile::tempdir().unwrap();
        let mut store = Store::open(temp.path().join("private/memory.sqlite")).unwrap();
        let scope = Scope::default();
        let human = source(&mut store, &scope);
        for i in 0..16 {
            let spec = GuidanceSpec {
                adaptation: Adaptation::Guidance {
                    topic: format!("lesson-{i}"),
                    instruction: "Context-dependent advice. ".repeat(70),
                    lasting: true,
                    when: None,
                },
                sources: vec![SourceVersion {
                    id: human.id.clone(),
                    revision: store.source_version(&human.id).unwrap().unwrap(),
                }],
                applicability: Applicability::ScopeWide,
                reason: "synthetic stress fixture".into(),
            };
            let record = validate_guidance(&store, &scope, &spec, i + 2).unwrap();
            store.append(record).unwrap();
        }
        let package = crate::assistant_context::build(&store, &scope, &[], &[], 6000).unwrap();
        assert!(package.records.iter().any(|r| r.id == human.id));
        assert!(
            package
                .records
                .iter()
                .filter(|r| r.kind == RecordKind::InferredPreference)
                .count()
                < 16
        );
        assert!(serde_json::to_vec(&package).unwrap().len() <= 6000);
    }
    fn guidance(
        store: &mut Store,
        scope: &Scope,
        source: &Record,
        level: DetailLevel,
        time: i64,
    ) -> Record {
        let spec = GuidanceSpec {
            adaptation: Adaptation::Detail { level },
            sources: vec![SourceVersion {
                id: source.id.clone(),
                revision: store.source_version(&source.id).unwrap().unwrap(),
            }],
            applicability: Applicability::ScopeWide,
            reason: "The source reports difficulty with excessive detail; tentative and reversible"
                .into(),
        };
        let record = validate_guidance(store, scope, &spec, time).unwrap();
        store.append(record).unwrap()
    }
    #[test]
    fn guidance_is_canonical_reversible_and_survives_restart_without_archive() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("private/memory.sqlite");
        let scope = Scope {
            project: Some("synthetic".into()),
            ..Scope::default()
        };
        let mut store = Store::open(&path).unwrap();
        let source = source(&mut store, &scope);
        let older = guidance(&mut store, &scope, &source, DetailLevel::Brief, 2);
        let newer = guidance(&mut store, &scope, &source, DetailLevel::Brief, 3);
        assert_eq!(
            applicable_guidance(&store, &scope, 8).unwrap()[0].id,
            newer.id
        );
        set_enabled(&mut store, &newer.id, false, 4).unwrap();
        assert!(applicable_guidance(&store, &scope, 8).unwrap().is_empty());
        set_enabled(&mut store, &older.id, true, 5).unwrap();
        drop(store);
        let mut store = Store::open(&path).unwrap();
        let fresh = applicable_guidance(&store, &scope, 8).unwrap();
        assert_eq!(fresh[0].id, older.id);
        assert_eq!(fresh[0].origin, Origin::Worker);
        assert!(!fresh[0].body.contains("Long explanations"));
        store.forget(&source.id).unwrap();
        assert!(applicable_guidance(&store, &scope, 8).unwrap().is_empty());
        assert!(set_enabled(&mut store, &older.id, true, 6).is_err());
        drop(store);
        assert!(
            applicable_guidance(&Store::open(path).unwrap(), &scope, 8)
                .unwrap()
                .is_empty()
        );
    }
    #[test]
    fn explicit_instructions_win_and_scope_and_source_revision_fail_closed() {
        let temp = tempfile::tempdir().unwrap();
        let mut store = Store::open(temp.path().join("private/memory.sqlite")).unwrap();
        let scope = Scope {
            project: Some("synthetic".into()),
            ..Scope::default()
        };
        let source = source(&mut store, &scope);
        guidance(&mut store, &scope, &source, DetailLevel::Brief, 2);
        let mut instruction = NewRecord {
            kind: RecordKind::UserInstruction,
            origin: Origin::Human,
            scope: scope.clone(),
            body: "Explain in detail going forward".into(),
            provenance: "explicit user instruction".into(),
            timestamp: 3,
            supersedes: None,
            dependencies: vec![],
            decision_state: None,
            protected_policy: false,
        };
        store.append(instruction.clone()).unwrap();
        assert_eq!(applicable_guidance(&store, &scope, 8).unwrap().len(), 1);
        let context = crate::assistant_context::build(
            &store,
            &scope,
            &["next question".into()],
            &[],
            32 * 1024,
        )
        .unwrap();
        assert!(context.records.iter().any(|r| r.body == instruction.body));
        assert!(
            crate::assistant_context::CONTINUITY_RULE.contains("explicit user instructions win")
        );
        instruction.scope.project = Some("other".into());
        assert!(
            applicable_guidance(&store, &instruction.scope, 8)
                .unwrap()
                .is_empty()
        );
        let invalid = GuidanceSpec {
            adaptation: Adaptation::Detail {
                level: DetailLevel::Brief,
            },
            sources: vec![SourceVersion {
                id: source.id.clone(),
                revision: 0,
            }],
            applicability: Applicability::ScopeWide,
            reason: "fixture".into(),
        };
        assert!(validate_guidance(&store, &scope, &invalid, 4).is_err());
        let unsafe_json = r#"{"adaptation":{"kind":"shell","command":"anything"},"sources":[],"applicability":"all","reason":"fixture"}"#;
        assert!(serde_json::from_str::<GuidanceSpec>(unsafe_json).is_err());
    }
    #[test]
    fn generated_adaptation_cannot_promote_one_off_or_quoted_human_input() {
        let temp = tempfile::tempdir().unwrap();
        let mut store = Store::open(temp.path().join("private/memory.sqlite")).unwrap();
        let scope = Scope::default();
        for body in [
            "Just three bullets today",
            "For this answer, be brief",
            "For the next reply, be concise",
            "If I later ask for brevity, use bullets",
            "The example says \"always be terse\"",
        ] {
            let record = store
                .append(NewRecord {
                    kind: RecordKind::Finding,
                    origin: Origin::Human,
                    scope: scope.clone(),
                    body: body.into(),
                    provenance: "synthetic input".into(),
                    timestamp: 1,
                    supersedes: None,
                    dependencies: vec![],
                    decision_state: None,
                    protected_policy: false,
                })
                .unwrap();
            let spec = GuidanceSpec {
                adaptation: Adaptation::Detail {
                    level: DetailLevel::Brief,
                },
                sources: vec![SourceVersion {
                    id: record.id.clone(),
                    revision: store.source_version(&record.id).unwrap().unwrap(),
                }],
                applicability: Applicability::ScopeWide,
                reason: "model claims durable preference".into(),
            };
            assert_eq!(
                validate_guidance(&store, &scope, &spec, 2).unwrap().kind,
                RecordKind::Proposal
            );
        }
    }

    #[test]
    fn independent_human_outcomes_can_support_inference_but_worker_repetition_cannot() {
        let temp = tempfile::tempdir().unwrap();
        let mut store = Store::open(temp.path().join("private/memory.sqlite")).unwrap();
        let scope = Scope::default();
        let mut versions = Vec::new();
        for (origin, body) in [
            (Origin::Worker, "concise answers helped"),
            (Origin::Worker, "concise answers worked well"),
            (Origin::Human, "concise answers helped"),
            (Origin::Human, "concise answers worked well"),
        ] {
            let record = store
                .append(NewRecord {
                    kind: RecordKind::Finding,
                    origin,
                    scope: scope.clone(),
                    body: body.into(),
                    provenance: "synthetic outcome".into(),
                    timestamp: 1,
                    supersedes: None,
                    dependencies: vec![],
                    decision_state: None,
                    protected_policy: false,
                })
                .unwrap();
            versions.push(SourceVersion {
                id: record.id.clone(),
                revision: store.source_version(&record.id).unwrap().unwrap(),
            });
        }
        let mut spec = GuidanceSpec {
            adaptation: Adaptation::Detail {
                level: DetailLevel::Brief,
            },
            sources: versions[..2].to_vec(),
            applicability: Applicability::ScopeWide,
            reason: "Tentative repeated-outcome adaptation".into(),
        };
        assert_eq!(
            validate_guidance(&store, &scope, &spec, 2).unwrap().kind,
            RecordKind::Proposal
        );
        spec.sources = versions[2..].to_vec();
        assert_eq!(
            validate_guidance(&store, &scope, &spec, 2).unwrap().kind,
            RecordKind::InferredPreference
        );
        let invalid = serde_json::json!({"adaptation":{"kind":"detail","level":"brief"},"sources":spec.sources,"applicability":"only when writing invoices","reason":"fixture"});
        assert!(serde_json::from_value::<GuidanceSpec>(invalid).is_err());
    }

    #[test]
    fn ordinary_turn_preserves_supported_lasting_correction_for_fresh_context() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("private/memory.sqlite");
        let mut store = Store::open(&path).unwrap();
        let scope = Scope::default();
        let human = source(&mut store, &scope);
        let version = SourceVersion {
            id: human.id.clone(),
            revision: store.source_version(&human.id).unwrap().unwrap(),
        };
        let spec = GuidanceSpec {
            adaptation: Adaptation::Detail {
                level: DetailLevel::Brief,
            },
            sources: vec![version.clone()],
            applicability: Applicability::ScopeWide,
            reason: "Supported direct lasting preference; original human input remains attributed"
                .into(),
        };
        let epoch = store.forget_epoch().unwrap();
        let records = crate::assistant_continuity::commit_turn(
            &mut store,
            "lasting-style",
            NewRecord {
                kind: RecordKind::Finding,
                origin: Origin::Worker,
                scope: scope.clone(),
                body: "I will keep answers concise.".into(),
                provenance: "ordinary synthetic response".into(),
                timestamp: 2,
                supersedes: None,
                dependencies: vec![human.id.clone()],
                decision_state: None,
                protected_policy: false,
            },
            &[crate::assistant_continuity::LearningCandidate::Guidance { spec }],
            &[version],
            epoch,
        )
        .unwrap();
        let id = records
            .iter()
            .find(|r| r.kind == RecordKind::InferredPreference)
            .unwrap()
            .id
            .clone();
        drop(store);
        let store = Store::open(path).unwrap();
        let package = crate::assistant_context::build(
            &store,
            &scope,
            &["new unrelated question".into()],
            &[],
            32 * 1024,
        )
        .unwrap();
        let current = package.records.iter().find(|r| r.id == id).unwrap();
        assert_eq!(current.origin, Origin::Worker);
        assert!(current.dependencies.contains(&human.id));
        assert_eq!(store.get(&human.id).unwrap().unwrap().origin, Origin::Human);
        assert!(current.body.starts_with("Prefer concise explanations"));
    }

    #[test]
    fn old_explicit_adaptation_cannot_be_evicted_by_newer_inferred_records() {
        let temp = tempfile::tempdir().unwrap();
        let mut store = Store::open(temp.path().join("private/memory.sqlite")).unwrap();
        let scope = Scope::default();
        let human = source(&mut store, &scope);
        let explicit = guidance(&mut store, &scope, &human, DetailLevel::Brief, 2);
        let mut sources = Vec::new();
        for body in ["detailed answers helped", "detailed answers worked well"] {
            let outcome = store
                .append(NewRecord {
                    kind: RecordKind::Finding,
                    origin: Origin::Human,
                    scope: scope.clone(),
                    body: body.into(),
                    provenance: "distinct synthetic human outcome".into(),
                    timestamp: 3,
                    supersedes: None,
                    dependencies: vec![],
                    decision_state: None,
                    protected_policy: false,
                })
                .unwrap();
            sources.push(SourceVersion {
                id: outcome.id.clone(),
                revision: store.source_version(&outcome.id).unwrap().unwrap(),
            });
        }
        let spec = GuidanceSpec {
            adaptation: Adaptation::Detail {
                level: DetailLevel::Detailed,
            },
            sources,
            applicability: Applicability::ScopeWide,
            reason: "Repeated outcome inference must never displace explicit user intent".into(),
        };
        for timestamp in 10..76 {
            let update = validate_guidance(&store, &scope, &spec, timestamp).unwrap();
            store.append(update).unwrap();
        }
        let applicable = applicable_guidance(&store, &scope, 16).unwrap();
        assert_eq!(applicable.len(), 1);
        assert_eq!(applicable[0].id, explicit.id);
    }
}
