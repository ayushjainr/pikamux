//! Local, bounded briefing projection. Only the authority host refreshes or
//! acknowledges it; board readers open existing state without IPC or providers.
use crate::assistant_briefing::{self, Brief, Entry};
use crate::assistant_memory::{
    DecisionState, MemoryError, Origin, RecordKind, RevisionPage, RevisionRecord, Scope, Store,
};
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

const CACHE_BYTES: usize = 512 * 1024;
const CUE_BYTES: usize = 4096;
const FRESH_SECONDS: i64 = 5;
const PAGE_RECORDS: usize = 32;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ServiceState {
    Idle,
    Investigating,
    Paused,
    Unavailable,
    Unknown,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct BriefCursor {
    pub profile: String,
    pub scope: Scope,
    pub revision: u64,
    pub epoch: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct Cue {
    pub profile: String,
    pub scope: Scope,
    pub revision: u64,
    pub asof: i64,
    pub state: String,
    pub unread: bool,
    pub unread_summary: String,
    pub notice: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Presentation {
    pub cue: Cue,
    pub cursor: BriefCursor,
    pub brief: Value,
    pub has_more: bool,
}

struct Material {
    brief: Value,
    epoch: u64,
    through: u64,
    has_more: bool,
    updates: usize,
    pending_decisions: usize,
    answer_available: bool,
}

fn scope_key(scope: &Scope) -> Result<String> {
    let project = scope
        .project
        .as_deref()
        .context("Choose an exact project scope")?;
    if project.trim().is_empty() || project.len() > 256 || project.chars().any(char::is_control) {
        bail!("Invalid briefing scope");
    }
    Ok(serde_json::to_string(scope)?)
}

fn open_writer(root: &Path) -> Result<Connection> {
    let path = root.join("owner.sqlite");
    crate::assistant_storage::database(&path)?;
    let db = Connection::open(path)?;
    db.busy_timeout(std::time::Duration::from_millis(15))?;
    db.execute_batch("CREATE TABLE IF NOT EXISTS assistant_briefing_cache(profile TEXT NOT NULL,scope TEXT NOT NULL,revision INTEGER NOT NULL,epoch INTEGER NOT NULL,asof INTEGER NOT NULL,material_hash TEXT NOT NULL,cue TEXT NOT NULL,body TEXT NOT NULL,PRIMARY KEY(profile,scope));
        CREATE TABLE IF NOT EXISTS assistant_briefing_cursors(profile TEXT NOT NULL,scope TEXT NOT NULL,epoch INTEGER NOT NULL,revision INTEGER NOT NULL,through_revision INTEGER NOT NULL,PRIMARY KEY(profile,scope));
        CREATE TABLE IF NOT EXISTS assistant_briefing_pages(profile TEXT NOT NULL,scope TEXT NOT NULL,revision INTEGER NOT NULL,epoch INTEGER NOT NULL,through_revision INTEGER NOT NULL,PRIMARY KEY(profile,scope,revision));")?;
    Ok(db)
}

fn existing_reader(root: &Path) -> Result<Option<Connection>> {
    let path = root.join("owner.sqlite");
    if !path.try_exists()? {
        return Ok(None);
    }
    crate::assistant_storage::existing_database(&path)?;
    let db = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    db.busy_timeout(std::time::Duration::from_millis(15))?;
    let exists:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='assistant_briefing_cache')",[],|r|r.get(0))?;
    Ok(exists.then_some(db))
}

/// Authority-host operation only. Timestamp-only heartbeats renew `asof` but
/// cannot create a new briefing revision or an unread source event.
pub(crate) fn refresh(
    root: &Path,
    memory: &mut Store,
    scope: &Scope,
    state: ServiceState,
    now: i64,
) -> Result<Presentation> {
    Publisher::open(root)?.refresh(memory, scope, state, now)
}

/// One host-owned writer, shared by all presentation consumers. Schema setup
/// happens once; refresh never waits for the memory writer and bounds the
/// owner-journal wait to 15ms. A busy refresh leaves the previous cache intact.
pub(crate) struct Publisher {
    root: PathBuf,
    db: Connection,
}

impl Publisher {
    pub(crate) fn open(root: &Path) -> Result<Self> {
        Ok(Self {
            root: root.to_path_buf(),
            db: open_writer(root)?,
        })
    }

    pub(crate) fn refresh(
        &mut self,
        memory: &mut Store,
        scope: &Scope,
        state: ServiceState,
        now: i64,
    ) -> Result<Presentation> {
        if memory.path() != self.root.join("memory.sqlite") || now < 0 {
            bail!("Invalid briefing authority or timestamp");
        }
        let key = scope_key(scope)?;
        let material = memory.try_read_snapshot(|store| {
            let epoch = store.forget_epoch()?;
            let after = acknowledged_through(&self.db, store.profile_id(), &key, epoch)?;
            collect_material(store, scope, after)
        })?;
        let mut view = make_presentation(memory.profile_id(), scope, state, now, &material);
        let (material_hash, cue, body) = prepare_view(&self.db, &mut view, &key, &material)?;
        memory.try_publish_at_epoch(material.epoch, || {
            save_view(
                &mut self.db,
                &view,
                &key,
                &material_hash,
                &cue,
                &body,
                material.through,
            )
        })?;
        Ok(view)
    }
}

fn prepare_view(
    db: &Connection,
    view: &mut Presentation,
    key: &str,
    material: &Material,
) -> Result<(String, String, String)> {
    let material_hash = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&(
            &view.brief,
            &view.has_more,
            &view.cue.state,
            &view.cue.unread_summary,
            material.epoch,
            material.through
        ))?)
    );
    let previous:Option<(u64,String)>=db.query_row("SELECT revision,material_hash FROM assistant_briefing_cache WHERE profile=? AND scope=?",params![view.cue.profile,key],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
    let revision = next_revision(previous, &material_hash)?;
    view.cue.revision = revision;
    view.cursor.revision = revision;
    let body = serde_json::to_string(&view)?;
    let cue = serde_json::to_string(&view.cue)?;
    if body.len() > CACHE_BYTES || cue.len() > CUE_BYTES {
        bail!("Briefing cache exceeds its bounded size");
    }
    Ok((material_hash, cue, body))
}

fn next_revision(previous: Option<(u64, String)>, hash: &str) -> Result<u64> {
    match previous {
        Some((revision, old)) if old == hash => Ok(revision),
        Some((revision, _)) => revision
            .checked_add(1)
            .context("Briefing revision exhausted"),
        None => Ok(1),
    }
}

fn acknowledged_through(
    db: &Connection,
    profile: &str,
    key: &str,
    epoch: u64,
) -> rusqlite::Result<u64> {
    Ok(db.query_row("SELECT through_revision FROM assistant_briefing_cursors WHERE profile=? AND scope=? AND epoch=?",params![profile,key,epoch],|r|r.get(0)).optional()?.unwrap_or(0))
}

fn collect_material(memory: &Store, scope: &Scope, after: u64) -> Result<Material, MemoryError> {
    let epoch = memory.forget_epoch()?;
    let page = memory.revision_page(scope, after, None, PAGE_RECORDS, true)?;
    let mut brief = assistant_briefing::load(memory, scope, i64::MAX)?;
    brief.changes = change_entries(memory, &page)?;
    let pending_decisions = brief
        .decisions
        .iter()
        .filter(|e| {
            matches!(
                e.decision_state,
                Some(DecisionState::Proposed | DecisionState::Unresolved | DecisionState::Deferred)
            )
        })
        .count();
    let answer_available = page.records.iter().any(|e| {
        e.record.origin == Origin::Worker
            && matches!(e.record.kind, RecordKind::Finding | RecordKind::Briefing)
    });
    Ok(Material {
        brief: compact_brief(brief, &page),
        epoch,
        through: page.next,
        has_more: page.has_more,
        updates: page.records.len(),
        pending_decisions,
        answer_available,
    })
}

fn change_entries(memory: &Store, page: &RevisionPage) -> Result<Vec<Entry>, MemoryError> {
    let mut changes = Vec::new();
    let mut ancestry = 128;
    for item in &page.records {
        let include = match item.record.kind {
            RecordKind::Finding | RecordKind::Briefing => true,
            RecordKind::Correction => {
                assistant_briefing::semantic_kind(memory, &item.record, &mut ancestry)?
                    != Some(RecordKind::UserInstruction)
            }
            _ => false,
        };
        if include {
            changes.push(Entry::from(&item.record));
        }
    }
    Ok(changes)
}

fn compact_brief(brief: Brief, page: &RevisionPage) -> Value {
    let mut limited = false;
    // The acknowledgement cursor covers exactly this page. Reserve a visible
    // preview for every update before allocating optional standing context.
    let updates = update_previews(page, &mut limited);
    let mut budget = (256 * 1024usize).saturating_sub(updates.to_string().len());
    let mut result = json!({"coverage":brief.coverage,"coverage_details":brief.coverage_details,"ignored_drafts":brief.ignored_drafts});
    result["updates"] = updates;
    for (name, entries) in [
        ("changes", brief.changes),
        ("instructions", brief.instructions),
        ("decisions", brief.decisions),
        ("uncertainty", brief.uncertainty),
        ("commitments", brief.commitments),
    ] {
        result[name] = compact_entries(entries, &mut budget, &mut limited);
    }
    result["presentation_limited"] = json!(limited);
    result["snapshot_through"] = json!(page.through);
    result["notice"] = json!(
        "Saved scoped evidence, not current project verification. Text is a bounded preview; inspect exact IDs for full records. Acknowledging this briefing does not mark any source task read."
    );
    result
}

fn update_previews(page: &RevisionPage, limited: &mut bool) -> Value {
    let updates = page
        .records
        .iter()
        .map(|item| {
            let text: String = item.record.body.chars().take(256).collect();
            let preview_limited = text.len() < item.record.body.len();
            *limited |= preview_limited;
            json!({"id":item.record.id,"revision":item.revision,"kind":item.record.kind,
            "origin":item.record.origin,"recorded_at":item.record.timestamp,"text":text,
            "preview_limited":preview_limited})
        })
        .collect();
    Value::Array(updates)
}

fn compact_entries(entries: Vec<Entry>, budget: &mut usize, limited: &mut bool) -> Value {
    *limited |= entries.len() > 64;
    let mut output = Vec::new();
    for mut entry in entries.into_iter().take(64) {
        if entry.text.len() > 1536 {
            entry.text = entry.text.chars().take(384).collect::<String>() + "…";
            *limited = true;
        }
        let value = json!(entry);
        let cost = value.to_string().len();
        if cost > *budget {
            *limited = true;
            continue;
        }
        *budget -= cost;
        output.push(value);
    }
    Value::Array(output)
}

fn make_presentation(
    profile: &str,
    scope: &Scope,
    state: ServiceState,
    now: i64,
    material: &Material,
) -> Presentation {
    let cue=Cue { profile:profile.into(),scope:scope.clone(),revision:0,asof:now,
        state:cue_state(state,material).into(),unread:material.updates>0,
        unread_summary:format!("{} new saved updates; {} decisions awaiting you{}",material.updates,material.pending_decisions,if material.has_more { "; more after acknowledgement" } else { "" }),
        notice:"Dated assistant cache; source task unread state is unchanged. This is not proof of live project state.".into() };
    Presentation {
        cue,
        cursor: BriefCursor {
            profile: profile.into(),
            scope: scope.clone(),
            revision: 0,
            epoch: material.epoch,
        },
        brief: material.brief.clone(),
        has_more: material.has_more,
    }
}

fn cue_state(state: ServiceState, material: &Material) -> &'static str {
    match state {
        ServiceState::Investigating => "investigating",
        ServiceState::Paused => "paused",
        ServiceState::Unavailable | ServiceState::Unknown => "unavailable",
        ServiceState::Idle if material.pending_decisions > 0 => "awaiting_decision",
        ServiceState::Idle if material.answer_available => "answer_available",
        ServiceState::Idle if material.updates > 0 => "updated",
        ServiceState::Idle => "no_new_state",
    }
}

fn save_view(
    db: &mut Connection,
    view: &Presentation,
    key: &str,
    hash: &str,
    cue: &str,
    body: &str,
    through: u64,
) -> rusqlite::Result<()> {
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    tx.execute(
        "DELETE FROM assistant_briefing_cache WHERE profile!=?",
        [&view.cursor.profile],
    )?;
    tx.execute("INSERT INTO assistant_briefing_cache VALUES(?,?,?,?,?,?,?,?) ON CONFLICT(profile,scope) DO UPDATE SET revision=excluded.revision,epoch=excluded.epoch,asof=excluded.asof,material_hash=excluded.material_hash,cue=excluded.cue,body=excluded.body",params![view.cursor.profile,key,view.cursor.revision,view.cursor.epoch,view.cue.asof,hash,cue,body])?;
    tx.execute(
        "INSERT OR IGNORE INTO assistant_briefing_pages VALUES(?,?,?,?,?)",
        params![
            view.cursor.profile,
            key,
            view.cursor.revision,
            view.cursor.epoch,
            through
        ],
    )?;
    tx.execute(
        "DELETE FROM assistant_briefing_pages WHERE profile=? AND scope=? AND revision<?",
        params![
            view.cursor.profile,
            key,
            view.cursor.revision.saturating_sub(32)
        ],
    )?;
    tx.commit()
}

/// Explicit user acknowledgement of one emitted briefing, never a source-read
/// acknowledgement. Old displayed pages cannot acknowledge newer raced arrivals.
pub(crate) fn acknowledge(root: &Path, memory: &mut Store, cursor: &BriefCursor) -> Result<()> {
    if cursor.profile != memory.profile_id() || memory.path() != root.join("memory.sqlite") {
        bail!("Briefing profile does not match this authority");
    }
    let key = scope_key(&cursor.scope)?;
    let db = open_writer(root)?;
    let through:Option<u64>=db.query_row("SELECT through_revision FROM assistant_briefing_pages WHERE profile=? AND scope=? AND revision=? AND epoch=?",params![cursor.profile,key,cursor.revision,cursor.epoch],|r|r.get(0)).optional()?;
    let through = through
        .context("That exact briefing is no longer available; refresh before acknowledging")?;
    memory.publish_at_epoch(cursor.epoch,|| {
        db.execute("INSERT INTO assistant_briefing_cursors VALUES(?,?,?,?,?) ON CONFLICT(profile,scope) DO UPDATE SET revision=CASE WHEN epoch=excluded.epoch THEN MAX(revision,excluded.revision) ELSE excluded.revision END,through_revision=CASE WHEN epoch=excluded.epoch THEN MAX(through_revision,excluded.through_revision) ELSE excluded.through_revision END,epoch=excluded.epoch",params![cursor.profile,key,cursor.epoch,cursor.revision,through])?;
        Ok(())
    })?;
    Ok(())
}

fn cached_field(root: &Path, scope: &Scope, field: &str, bound: usize) -> Result<Option<String>> {
    let key = scope_key(scope)?;
    let Some(db) = existing_reader(root)? else {
        return Ok(None);
    };
    // field is a module-owned SQL identifier, never input from a model/view.
    let saved:Option<(String,u64,String)>=db.query_row(&format!("SELECT profile,epoch,{field} FROM assistant_briefing_cache WHERE scope=? AND length(CAST({field} AS BLOB))<=? ORDER BY asof DESC LIMIT 1"),params![key,bound],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
    let Some((profile, epoch, body)) = saved else {
        return Ok(None);
    };
    // Retention spans databases. A crash after committing forget but before
    // deleting this cache must not expose the old saved briefing on a board.
    if existing_memory_generation(root)? != Some((profile, epoch)) {
        return Ok(None);
    }
    Ok(Some(body))
}

fn existing_memory_generation(root: &Path) -> Result<Option<(String, u64)>> {
    let path = root.join("memory.sqlite");
    if !path.try_exists()? {
        return Ok(None);
    }
    crate::assistant_storage::existing_database(&path)?;
    let db = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    db.busy_timeout(std::time::Duration::ZERO)?;
    let profile: String = db.query_row(
        "SELECT value FROM memory_meta WHERE key='profile_id'",
        [],
        |r| r.get(0),
    )?;
    let epoch: Option<String> = db
        .query_row(
            "SELECT value FROM memory_meta WHERE key='forget_epoch'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    Ok(Some((
        profile,
        epoch.map(|value| value.parse()).transpose()?.unwrap_or(0),
    )))
}

pub(crate) fn read_cue(root: &Path, scope: &Scope, now: i64) -> Result<Option<Cue>> {
    let Some(body) = cached_field(root, scope, "cue", CUE_BYTES)? else {
        return Ok(None);
    };
    let mut cue: Cue = serde_json::from_str(&body)?;
    if cue.scope != *scope {
        bail!("Cached briefing scope mismatch");
    }
    mark_freshness(&mut cue, now);
    Ok(Some(cue))
}

pub(crate) fn read_cached(root: &Path, scope: &Scope, now: i64) -> Result<Option<Presentation>> {
    let Some(body) = cached_field(root, scope, "body", CACHE_BYTES)? else {
        return Ok(None);
    };
    let mut view: Presentation = serde_json::from_str(&body)?;
    if view.cue.scope != *scope
        || view.cursor.scope != *scope
        || view.cue.profile != view.cursor.profile
        || view.cue.revision != view.cursor.revision
    {
        bail!("Cached briefing identity mismatch");
    }
    mark_freshness(&mut view.cue, now);
    Ok(Some(view))
}

fn mark_freshness(cue: &mut Cue, now: i64) {
    if now < cue.asof || now.saturating_sub(cue.asof) > FRESH_SECONDS {
        cue.state = "unavailable".into();
        cue.notice="Assistant host freshness is unverified. Showing a dated cached cue only; no host, provider, or investigation was launched.".into();
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct MemoryCursor {
    pub profile: String,
    pub scope: Scope,
    pub epoch: u64,
    pub through: u64,
    pub after: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct MemoryPage {
    pub profile: String,
    pub scope: Scope,
    pub epoch: u64,
    pub through: u64,
    pub records: Vec<RevisionRecord>,
    pub next: Option<MemoryCursor>,
}

/// Explicit, scoped inspection/export. Returns data to the caller, never a
/// filename or filesystem effect. Cursors freeze the archive revision ceiling.
pub(crate) fn memory_page(
    memory: &Store,
    scope: &Scope,
    cursor: Option<&MemoryCursor>,
    limit: usize,
) -> Result<MemoryPage> {
    scope_key(scope)?;
    let epoch = memory.forget_epoch()?;
    if let Some(cursor) = cursor {
        if cursor.profile != memory.profile_id() || cursor.scope != *scope || cursor.epoch != epoch
        {
            bail!("Memory cursor identity or forgetting generation changed");
        }
    }
    let page = memory.read_snapshot(|store| {
        if store.forget_epoch()? != epoch {
            return Err(MemoryError::Invalid(
                "Memory changed through forgetting".into(),
            ));
        }
        store.revision_page(
            scope,
            cursor.map_or(0, |c| c.after),
            cursor.map(|c| c.through),
            limit,
            false,
        )
    })?;
    bound_memory_export(memory.profile_id(), scope, epoch, page)
}

fn bound_memory_export(
    profile: &str,
    scope: &Scope,
    epoch: u64,
    mut page: RevisionPage,
) -> Result<MemoryPage> {
    while serde_json::to_vec(&page.records)?.len() > 768 * 1024 {
        if page.records.len() == 1 {
            bail!(
                "Record {} exceeds a single bounded page; export it with memory_record_chunk",
                page.records[0].record.id
            );
        }
        page.records.pop();
        page.has_more = true;
        page.next = page.records.last().context("Empty memory page")?.revision;
    }
    let next = page.has_more.then(|| MemoryCursor {
        profile: profile.into(),
        scope: scope.clone(),
        epoch,
        through: page.through,
        after: page.next,
    });
    Ok(MemoryPage {
        profile: profile.into(),
        scope: scope.clone(),
        epoch,
        through: page.through,
        records: page.records,
        next,
    })
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct RecordChunkCursor {
    pub profile: String,
    pub scope: Scope,
    pub epoch: u64,
    pub id: String,
    pub hash: String,
    pub offset: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct RecordChunk {
    pub profile: String,
    pub scope: Scope,
    pub id: String,
    pub total_bytes: usize,
    pub data: String,
    pub next: Option<RecordChunkCursor>,
}

pub(crate) fn memory_record_chunk(
    memory: &Store,
    scope: &Scope,
    id: &str,
    cursor: Option<&RecordChunkCursor>,
) -> Result<RecordChunk> {
    scope_key(scope)?;
    let (record, epoch) = memory.read_snapshot(|store| {
        Ok((
            store
                .get(id)?
                .ok_or_else(|| MemoryError::NotFound(id.into()))?,
            store.forget_epoch()?,
        ))
    })?;
    if !record.scope.permits(scope) {
        bail!("Memory record is outside the selected scope");
    }
    let body = serde_json::to_string(&record)?;
    let hash = format!("{:x}", Sha256::digest(body.as_bytes()));
    let offset = validate_chunk_cursor(memory.profile_id(), scope, id, epoch, &hash, cursor)?;
    if offset > body.len() || !body.is_char_boundary(offset) {
        bail!("Invalid memory chunk offset");
    }
    let mut end = offset.saturating_add(64 * 1024).min(body.len());
    while !body.is_char_boundary(end) {
        end -= 1;
    }
    let next = (end < body.len()).then(|| RecordChunkCursor {
        profile: memory.profile_id().into(),
        scope: scope.clone(),
        epoch,
        id: id.into(),
        hash,
        offset: end,
    });
    Ok(RecordChunk {
        profile: memory.profile_id().into(),
        scope: scope.clone(),
        id: id.into(),
        total_bytes: body.len(),
        data: body[offset..end].into(),
        next,
    })
}

fn validate_chunk_cursor(
    profile: &str,
    scope: &Scope,
    id: &str,
    epoch: u64,
    hash: &str,
    cursor: Option<&RecordChunkCursor>,
) -> Result<usize> {
    let Some(cursor) = cursor else {
        return Ok(0);
    };
    if cursor.profile != profile
        || cursor.scope != *scope
        || cursor.id != id
        || cursor.epoch != epoch
        || cursor.hash != hash
    {
        bail!("Memory chunk identity, contents, or forgetting generation changed");
    }
    Ok(cursor.offset)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assistant_memory::{NewRecord, Record};
    use std::collections::BTreeSet;

    struct Fixture {
        _temp: tempfile::TempDir,
        root: std::path::PathBuf,
        memory: Store,
        scope: Scope,
    }
    impl Fixture {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().join("assistant");
            let memory = Store::open(root.join("memory.sqlite")).unwrap();
            Self {
                _temp: temp,
                root,
                memory,
                scope: Scope {
                    project: Some("alpha".into()),
                    ..Scope::default()
                },
            }
        }
        fn add(&mut self, kind: RecordKind, body: &str, time: i64) -> Record {
            self.memory
                .append(NewRecord {
                    kind,
                    origin: if kind == RecordKind::Finding {
                        Origin::Worker
                    } else {
                        Origin::Human
                    },
                    scope: self.scope.clone(),
                    body: body.into(),
                    provenance: "synthetic presentation test".into(),
                    timestamp: time,
                    supersedes: None,
                    dependencies: vec![],
                    decision_state: (kind == RecordKind::Decision)
                        .then_some(DecisionState::Proposed),
                    protected_policy: false,
                })
                .unwrap()
        }
        fn view(&mut self, time: i64) -> Presentation {
            refresh(
                &self.root,
                &mut self.memory,
                &self.scope,
                ServiceState::Idle,
                time,
            )
            .unwrap()
        }
        fn ack(&mut self, cursor: &BriefCursor) {
            acknowledge(&self.root, &mut self.memory, cursor).unwrap()
        }
    }

    #[test]
    fn cached_read_of_absent_profile_creates_nothing_and_starts_nothing() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("absent");
        let scope = Scope {
            project: Some("alpha".into()),
            ..Scope::default()
        };
        assert!(read_cue(&root, &scope, 100).unwrap().is_none());
        assert!(read_cached(&root, &scope, 100).unwrap().is_none());
        assert!(!root.exists());
    }

    #[test]
    fn contended_refresh_is_bounded_and_retains_last_committed_cache() {
        let mut f = Fixture::new();
        f.add(RecordKind::Finding, "saved answer", 1);
        let mut publisher = Publisher::open(&f.root).unwrap();
        let first = publisher
            .refresh(&mut f.memory, &f.scope, ServiceState::Idle, 100)
            .unwrap();
        for file in ["memory.sqlite", "owner.sqlite"] {
            let blocker = Connection::open(f.root.join(file)).unwrap();
            blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
            let start = std::time::Instant::now();
            assert!(
                publisher
                    .refresh(&mut f.memory, &f.scope, ServiceState::Idle, 101)
                    .is_err()
            );
            assert!(start.elapsed() < std::time::Duration::from_millis(250));
            let cached = read_cached(&f.root, &f.scope, 101).unwrap().unwrap();
            assert_eq!(cached.cursor, first.cursor);
            assert_eq!(cached.cue.asof, 100);
            blocker.execute_batch("ROLLBACK").unwrap();
        }
        let recovered = publisher
            .refresh(&mut f.memory, &f.scope, ServiceState::Idle, 102)
            .unwrap();
        assert_eq!(recovered.cursor, first.cursor);
        assert_eq!(recovered.cue.asof, 102);
    }

    #[test]
    fn cache_reader_rejects_forgotten_generation_before_retention_completes() {
        let mut f = Fixture::new();
        let record = f.add(RecordKind::Finding, "must not reappear after forget", 1);
        f.view(100);
        f.memory.forget(&record.id).unwrap();
        // Simulate a crash or held journal lock before the retention scrub.
        assert!(read_cached(&f.root, &f.scope, 101).unwrap().is_none());
        assert!(read_cue(&f.root, &f.scope, 101).unwrap().is_none());
    }

    #[test]
    fn acknowledgement_is_separate_and_standing_instructions_decisions_survive() {
        let mut f = Fixture::new();
        f.add(RecordKind::UserInstruction, "Use concrete evidence", 1);
        f.add(RecordKind::Decision, "Await my explicit choice", 1);
        f.add(RecordKind::Finding, "First saved answer", 1);
        let first = f.view(100);
        assert_eq!(first.brief["changes"].as_array().unwrap().len(), 1);
        assert_eq!(first.cue.state, "awaiting_decision");
        assert!(first.cue.unread);
        let repaint = f.view(101);
        assert_eq!(repaint.cursor, first.cursor);
        read_cue(&f.root, &f.scope, 101).unwrap();
        assert_eq!(
            f.view(102).cursor,
            first.cursor,
            "viewing must not acknowledge"
        );
        f.ack(&first.cursor);
        let next = f.view(103);
        assert!(!next.cue.unread);
        assert!(next.brief["changes"].as_array().unwrap().is_empty());
        assert_eq!(next.brief["instructions"].as_array().unwrap().len(), 1);
        assert_eq!(next.brief["decisions"].as_array().unwrap().len(), 1);
        assert_eq!(next.cue.state, "awaiting_decision");
        assert!(!f.root.join("provider-home").exists());
        assert!(!f.root.join("view.sock").exists());
    }

    #[test]
    fn every_acknowledged_update_has_a_preview_before_oversized_standing_context() {
        let mut f = Fixture::new();
        for _ in 0..32 {
            f.add(RecordKind::UserInstruction, &"\u{0001}".repeat(1536), 1);
        }
        let standing = f.view(100);
        let previews = standing.brief["updates"].as_array().unwrap();
        assert_eq!(previews.len(), 32);
        assert!(
            previews.iter().all(
                |update| update["text"].as_str().unwrap().chars().count() == 256
                    && update["preview_limited"] == true
            )
        );
        assert!(serde_json::to_vec(previews).unwrap().len() < 64 * 1024);
        f.ack(&standing.cursor);
        let finding = f.add(
            RecordKind::Finding,
            "A new answer that must be visible before acknowledgement",
            2,
        );
        let view = f.view(101);
        let updates = view.brief["updates"].as_array().unwrap();
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0]["id"], finding.id);
        assert_eq!(updates[0]["text"], finding.body);
        assert_eq!(updates[0]["preview_limited"], false);
        assert_eq!(view.brief["presentation_limited"], true);
        assert!(
            view.brief["changes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| entry["id"] == finding.id)
        );
        let cached = read_cached(&f.root, &f.scope, 101).unwrap().unwrap();
        assert_eq!(cached.brief["updates"][0]["text"], finding.body);
        f.ack(&view.cursor);
        assert!(f.view(102).brief["updates"].as_array().unwrap().is_empty());
        assert_eq!(
            f.memory.get(&finding.id).unwrap().unwrap().body,
            finding.body
        );
    }

    #[test]
    fn old_displayed_cursor_cannot_ack_raced_equal_or_older_timestamp_arrivals() {
        let mut f = Fixture::new();
        f.add(RecordKind::Finding, "first", 20);
        let first = f.view(100);
        let second = f.add(RecordKind::Finding, "arrived later, same timestamp", 20);
        let third = f.add(RecordKind::Finding, "arrived later, older timestamp", 1);
        let latest = f.view(101);
        assert!(latest.cursor.revision > first.cursor.revision);
        f.ack(&first.cursor);
        let after = f.view(102);
        let ids: BTreeSet<_> = after.brief["changes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, BTreeSet::from([second.id.as_str(), third.id.as_str()]));
    }

    #[test]
    fn paged_brief_acknowledgement_never_skips_backlog_or_raced_records() {
        let mut f = Fixture::new();
        let mut expected = BTreeSet::new();
        for n in 0..70 {
            expected.insert(f.add(RecordKind::Finding, &format!("answer {n}"), 7).id);
        }
        let first = f.view(100);
        assert!(first.has_more);
        assert_eq!(first.brief["updates"].as_array().unwrap().len(), 32);
        expected.insert(f.add(RecordKind::Finding, "raced", 7).id);
        let mut seen = BTreeSet::new();
        let mut view = first;
        for time in 101..105 {
            for row in view.brief["updates"].as_array().unwrap() {
                assert!(seen.insert(row["id"].as_str().unwrap().to_owned()));
            }
            f.ack(&view.cursor);
            view = f.view(time);
        }
        assert_eq!(seen, expected);
        assert!(!view.cue.unread);
    }

    #[test]
    fn cursors_are_profile_scope_epoch_bound_and_survive_restart() {
        let mut f = Fixture::new();
        f.add(RecordKind::Finding, "answer", 1);
        let view = f.view(100);
        let mut wrong = view.cursor.clone();
        wrong.profile = uuid::Uuid::new_v4().to_string();
        assert!(acknowledge(&f.root, &mut f.memory, &wrong).is_err());
        wrong = view.cursor.clone();
        wrong.scope.project = Some("beta".into());
        assert!(acknowledge(&f.root, &mut f.memory, &wrong).is_err());
        f.ack(&view.cursor);
        f.memory = Store::open(f.root.join("memory.sqlite")).unwrap();
        assert!(!f.view(101).cue.unread);
        let other = Scope {
            project: Some("beta".into()),
            ..Scope::default()
        };
        assert!(read_cue(&f.root, &other, 101).unwrap().is_none());
    }

    #[test]
    fn stale_and_unknown_service_states_never_claim_ready() {
        let mut f = Fixture::new();
        f.add(RecordKind::Finding, "answer", 1);
        let view = f.view(100);
        assert_eq!(view.cue.state, "answer_available");
        let stale = read_cue(&f.root, &f.scope, 106).unwrap().unwrap();
        assert_eq!(stale.state, "unavailable");
        assert!(stale.notice.contains("no host"));
        assert_eq!(
            read_cue(&f.root, &f.scope, 100).unwrap().unwrap().state,
            "answer_available",
            "cache readers never write freshness state"
        );
        let unknown =
            refresh(&f.root, &mut f.memory, &f.scope, ServiceState::Unknown, 107).unwrap();
        assert_eq!(unknown.cue.state, "unavailable");
        let paused = refresh(&f.root, &mut f.memory, &f.scope, ServiceState::Paused, 108).unwrap();
        assert_eq!(paused.cue.state, "paused");
    }

    #[test]
    fn forget_scrubs_brief_and_ack_cursor_and_revision_sequence_never_reuses() {
        let mut f = Fixture::new();
        let old = f.add(RecordKind::Finding, "secret old", 1);
        let view = f.view(100);
        let before = f
            .memory
            .revision_page(&f.scope, 0, None, 64, false)
            .unwrap()
            .through;
        f.memory.forget(&old.id).unwrap();
        crate::assistant_retention::cleanup(&f.root, f.memory.forget_epoch().unwrap()).unwrap();
        assert!(read_cue(&f.root, &f.scope, 101).unwrap().is_none());
        assert!(acknowledge(&f.root, &mut f.memory, &view.cursor).is_err());
        f.add(RecordKind::Finding, "new", 1);
        let page = f
            .memory
            .revision_page(&f.scope, before, None, 64, false)
            .unwrap();
        assert_eq!(page.records.len(), 1);
        assert!(page.records[0].revision > before);
        let fresh = f.view(101);
        assert!(fresh.cue.unread);
        assert!(acknowledge(&f.root, &mut f.memory, &view.cursor).is_err());
    }

    #[test]
    fn scope_revocation_scrubs_presentation_without_removing_other_grants() {
        let mut f = Fixture::new();
        let record = f.add(RecordKind::Finding, "revoked cache marker", 1);
        f.view(100);
        let db = Connection::open(f.root.join("owner.sqlite")).unwrap();
        db.execute_batch("CREATE TABLE assistant_board_shares(scope TEXT PRIMARY KEY,body TEXT); INSERT INTO assistant_board_shares VALUES('beta','preserve');").unwrap();
        f.memory.forget(&record.id).unwrap();
        crate::assistant_retention::cleanup_revoked_context(
            &f.root,
            f.memory.forget_epoch().unwrap(),
        )
        .unwrap();
        assert!(read_cached(&f.root, &f.scope, 101).unwrap().is_none());
        assert_eq!(
            db.query_row("SELECT COUNT(*) FROM assistant_board_shares", [], |r| r
                .get::<_, u64>(0))
                .unwrap(),
            1
        );
    }

    #[test]
    fn archive_pagination_has_frozen_ceiling_exact_scope_and_no_read_ack() {
        let mut f = Fixture::new();
        f.add(RecordKind::Finding, "first", 1);
        f.add(RecordKind::Finding, "second", 1);
        let first = memory_page(&f.memory, &f.scope, None, 1).unwrap();
        let cursor = first.next.clone().unwrap();
        f.add(RecordKind::Finding, "after export started", 1);
        let second = memory_page(&f.memory, &f.scope, Some(&cursor), 1).unwrap();
        assert!(second.next.is_none());
        assert_eq!(second.records[0].record.body, "second");
        let other = Scope {
            project: Some("beta".into()),
            ..Scope::default()
        };
        assert!(memory_page(&f.memory, &other, Some(&cursor), 1).is_err());
        assert!(
            f.view(100).cue.unread,
            "export does not acknowledge a briefing"
        );
    }

    #[test]
    fn large_record_chunk_export_is_lossless_bounded_and_forget_invalidates_cursor() {
        let mut f = Fixture::new();
        let record = f.add(RecordKind::Finding, &"雪\n".repeat(40_000), 1);
        let mut output = String::new();
        let mut cursor = None;
        let mut first_cursor = None;
        loop {
            let part =
                memory_record_chunk(&f.memory, &f.scope, &record.id, cursor.as_ref()).unwrap();
            assert!(part.data.len() <= 64 * 1024);
            output.push_str(&part.data);
            if first_cursor.is_none() {
                first_cursor = part.next.clone();
            }
            cursor = part.next;
            if cursor.is_none() {
                break;
            }
        }
        assert_eq!(serde_json::from_str::<Record>(&output).unwrap(), record);
        f.memory.forget(&record.id).unwrap();
        assert!(
            memory_record_chunk(&f.memory, &f.scope, &record.id, first_cursor.as_ref()).is_err()
        );
    }

    #[test]
    fn stable_revision_backfill_runs_once_and_updates_are_new_revisions() {
        let mut f = Fixture::new();
        let record = f.add(RecordKind::Finding, "old", 1);
        let before = f
            .memory
            .revision_page(&f.scope, 0, None, 64, false)
            .unwrap()
            .through;
        for _ in 0..3 {
            f.memory = Store::open(f.root.join("memory.sqlite")).unwrap();
        }
        assert_eq!(
            f.memory
                .revision_page(&f.scope, 0, None, 64, false)
                .unwrap()
                .through,
            before
        );
        let db = Connection::open(f.memory.path()).unwrap();
        db.execute(
            "UPDATE memory_records SET body='updated' WHERE id=?",
            [record.id],
        )
        .unwrap();
        let page = f
            .memory
            .revision_page(&f.scope, before, None, 64, false)
            .unwrap();
        assert_eq!(page.records[0].record.body, "updated");
        assert!(page.records[0].revision > before);
    }
}
