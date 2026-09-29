//! Bounded, transcript-free projection of the existing observation producer.
//! Consumers read a latest snapshot; they never scan providers or acknowledge work.
use anyhow::{Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, path::Path};

const MAX_ROWS: usize = 256;
const MAX_BYTES: usize = 128 * 1024;
const MAX_AGE: i64 = 60;

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Identity {
    pub node: String,
    pub provider: String,
    pub conversation: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Row {
    pub identity: Identity,
    pub name: String,
    pub status: String,
    pub stale: bool,
    /// Opaque committed lifecycle identity, never a polling timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_id: Option<String>,
    /// Persisted material episode; unchanged across names/freshness refreshes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub occurrence: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Snapshot {
    pub revision: u64,
    pub sampled_at: i64,
    pub rows: Vec<Row>,
    pub partial: bool,
}

fn open(root: &Path) -> Result<Connection> {
    let path = root.join("latest.sqlite");
    crate::assistant_storage::database(&path)?;
    let db = Connection::open(path)?;
    db.busy_timeout(std::time::Duration::from_millis(100))?;
    db.execute_batch("CREATE TABLE IF NOT EXISTS activity_projection(id INTEGER PRIMARY KEY CHECK(id=1), revision INTEGER NOT NULL, sampled INTEGER NOT NULL, body TEXT NOT NULL);")?;
    db.execute_batch("CREATE TABLE IF NOT EXISTS activity_material(identity TEXT PRIMARY KEY,status TEXT NOT NULL,event_id TEXT,occurrence TEXT NOT NULL);")?;
    Ok(db)
}

/// Only committed board metadata is projected. No expert cards, paths, outputs,
/// usage credentials, pending pane identities, or transcript fields are copied.
pub(crate) fn publish(
    root: &Path,
    local_node: &str,
    items: &[crate::monitor::BoardItem],
    partial: bool,
    now: i64,
    store_path: Option<&Path>,
) -> Result<()> {
    let mut rows = BTreeMap::new();
    for item in items.iter().filter(|item| item.pending_token.is_none()) {
        let identity = Identity {
            node: item.node_id.as_deref().unwrap_or(local_node).to_owned(),
            provider: item.session.provider.as_str().into(),
            conversation: item.session.session_id.clone(),
        };
        if !valid_identity(&identity) {
            continue;
        }
        let row = Row {
            identity: identity.clone(),
            name: crate::fleet::sanitize_terminal_text(&item.session.display_name())
                .chars()
                .take(120)
                .collect(),
            status: format!("{:?}", item.session.status).to_lowercase(),
            stale: item.stale,
            event_id: None,
            occurrence: None,
        };
        rows.insert(identity, row);
    }
    let partial = partial || rows.len() > MAX_ROWS;
    let mut rows: Vec<_> = rows.into_values().take(MAX_ROWS).collect();
    if let Some(path) = store_path {
        // A busy/unavailable event journal must not fabricate a new episode.
        // Status-only observation remains usable, without claiming unseen events.
        let events = lifecycle_events(path, local_node, &rows).unwrap_or_default();
        for row in &mut rows {
            row.event_id = events.get(&row.identity).cloned();
        }
    }
    publish_rows(root, rows, partial, now)
}

/// One bounded indexed metadata query per producer publication, never per view.
/// Only canonical hook lifecycle observations are event identities: safety and
/// reconcile timestamps can advance on every scan without a new user question.
fn lifecycle_events(path: &Path, node: &str, rows: &[Row]) -> Result<BTreeMap<Identity, String>> {
    let local: Vec<_> = rows
        .iter()
        .filter(|row| row.identity.node == node && !row.stale)
        .take(MAX_ROWS)
        .collect();
    if local.is_empty() {
        return Ok(BTreeMap::new());
    }
    let db = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    db.busy_timeout(std::time::Duration::ZERO)?;
    let values = vec!["(?,?,?)"; local.len()].join(",");
    let parameters: Vec<_> = local
        .iter()
        .flat_map(|row| {
            [
                &row.identity.provider,
                &row.identity.conversation,
                &row.status,
            ]
        })
        .collect();
    let mut query = db.prepare(&format!("WITH requested(provider,session_id,status) AS (VALUES {values}) SELECT o.provider,o.session_id,o.source,o.observed_at FROM requested q JOIN session_status_observations o ON o.provider=q.provider AND o.session_id=q.session_id AND o.kind='lifecycle' WHERE o.source LIKE 'hook:%' AND replace(lower(o.status),' ','')=q.status"))?;
    let values = query
        .query_map(rusqlite::params_from_iter(parameters), |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, f64>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut result = BTreeMap::new();
    for (provider, conversation, source, at) in values {
        if let Some(event) = canonical_event_id(&source, at) {
            let identity = Identity {
                node: node.into(),
                provider,
                conversation,
            };
            result.insert(identity, event);
        }
    }
    Ok(result)
}

fn canonical_event_id(source: &str, at: f64) -> Option<String> {
    if !at.is_finite() || at <= 0.0 || source.len() > 128 {
        return None;
    }
    let mut hash = Sha256::new();
    hash.update(source.as_bytes());
    hash.update([0]);
    hash.update(at.to_bits().to_be_bytes());
    Some(format!("{:x}", hash.finalize()))
}

fn valid_identity(identity: &Identity) -> bool {
    [&identity.node, &identity.conversation]
        .into_iter()
        .all(|v| {
            !v.is_empty()
                && v.len() <= 128
                && v.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_.:".contains(&b))
        })
        && matches!(
            identity.provider.as_str(),
            "codex" | "claude" | "opencode" | "muse"
        )
}

pub(crate) fn publish_rows(root: &Path, mut rows: Vec<Row>, partial: bool, now: i64) -> Result<()> {
    if rows.len() > MAX_ROWS {
        bail!("Activity rows exceed bound");
    }
    let mut db = open(root)?;
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    for row in &mut rows {
        assign_occurrence(&tx, row)?;
    }
    let body = serde_json::to_string(&(rows, partial))?;
    if body.len() > MAX_BYTES {
        bail!("Activity projection exceeds bound");
    }
    let revision = projection_revision(&tx, &body)?;
    tx.execute("INSERT INTO activity_projection VALUES(1,?,?,?) ON CONFLICT(id) DO UPDATE SET revision=excluded.revision,sampled=excluded.sampled,body=excluded.body", params![revision, now, body])?;
    tx.commit()?;
    Ok(())
}

fn projection_revision(db: &Connection, body: &str) -> Result<u64> {
    let previous: Option<(u64, String)> = db
        .query_row(
            "SELECT revision,body FROM activity_projection WHERE id=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let revision = match previous {
        Some((revision, old)) if old == body => revision,
        Some((revision, _)) => revision
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("Activity revision exhausted"))?,
        None => 1,
    };
    Ok(revision)
}

fn assign_occurrence(db: &Connection, row: &mut Row) -> Result<()> {
    let key = serde_json::to_string(&row.identity)?;
    let previous: Option<(String, Option<String>, String)> = db
        .query_row(
            "SELECT status,event_id,occurrence FROM activity_material WHERE identity=?",
            [&key],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    if row.stale {
        row.occurrence = previous.map(|(_, _, occurrence)| occurrence);
        return Ok(());
    }
    if let Some((status, event, occurrence)) = previous {
        if status == row.status && (row.event_id.is_none() || row.event_id == event) {
            row.occurrence = Some(occurrence);
            return Ok(());
        }
        if status == row.status && event.is_none() {
            // Learning the identity of an already seen question is not proof
            // that another question occurred while the event journal was absent.
            db.execute(
                "UPDATE activity_material SET event_id=? WHERE identity=?",
                params![row.event_id, key],
            )?;
            row.occurrence = Some(occurrence);
            return Ok(());
        }
    }
    let occurrence = uuid::Uuid::new_v4().to_string();
    db.execute("INSERT INTO activity_material VALUES(?,?,?,?) ON CONFLICT(identity) DO UPDATE SET status=excluded.status,event_id=excluded.event_id,occurrence=excluded.occurrence",params![key,row.status,row.event_id,occurrence])?;
    row.occurrence = Some(occurrence);
    Ok(())
}

pub(crate) fn load(root: &Path, now: i64) -> Result<Option<Snapshot>> {
    let path = root.join("latest.sqlite");
    if !path.exists() {
        return Ok(None);
    }
    crate::assistant_storage::existing_database(&path)?;
    let db = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    db.busy_timeout(std::time::Duration::from_millis(15))?;
    let value: Option<(u64, i64, String)> = db
        .query_row(
            "SELECT revision,sampled,body FROM activity_projection WHERE id=1 AND length(body)<=?",
            [MAX_BYTES],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let Some((revision, sampled_at, body)) = value else {
        return Ok(None);
    };
    Ok(Some(decode_snapshot(revision, sampled_at, &body, now)?))
}

fn decode_snapshot(revision: u64, sampled_at: i64, body: &str, now: i64) -> Result<Snapshot> {
    let (mut rows, mut partial): (Vec<Row>, bool) = serde_json::from_str(body)?;
    if rows.len() > MAX_ROWS
        || rows
            .iter()
            .any(|row| !valid_identity(&row.identity) || row.name.len() > 480)
    {
        bail!("Invalid activity projection");
    }
    if sampled_at > now || now.saturating_sub(sampled_at) > MAX_AGE {
        partial = true;
        for row in &mut rows {
            row.stale = true;
        }
    }
    Ok(Snapshot {
        revision,
        sampled_at,
        rows,
        partial,
    })
}

impl Snapshot {
    pub(crate) fn allowed(&self, identities: &[Identity]) -> Self {
        let mut value = self.clone();
        value.rows.retain(|row| identities.contains(&row.identity));
        value
    }

    pub(crate) fn attention(&self) -> Vec<&Row> {
        self.rows
            .iter()
            .filter(|row| {
                !row.stale && matches!(row.status.as_str(), "needsyou" | "error" | "opentwice")
            })
            .take(8)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn row(id: &str) -> Row {
        Row {
            identity: Identity {
                node: "node".into(),
                provider: "codex".into(),
                conversation: id.into(),
            },
            name: "job".into(),
            status: "needsyou".into(),
            stale: false,
            event_id: None,
            occurrence: None,
        }
    }
    #[test]
    fn clocks_do_not_trigger_work_and_partial_or_old_never_claims_live() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("feed");
        publish_rows(&root, vec![row("a")], false, 10).unwrap();
        publish_rows(&root, vec![row("a")], false, 20).unwrap();
        let current = load(&root, 21).unwrap().unwrap();
        assert_eq!(current.revision, 1);
        assert_eq!(current.attention().len(), 1);
        let old = load(&root, 81).unwrap().unwrap();
        assert!(old.partial);
        assert!(old.attention().is_empty());
        publish_rows(&root, vec![row("b")], true, 90).unwrap();
        let changed = load(&root, 90).unwrap().unwrap();
        assert_eq!(changed.revision, 2);
        assert!(changed.partial);
        assert!(changed.allowed(&[row("a").identity]).rows.is_empty());
    }
    #[test]
    fn missing_feed_does_not_create_a_scanner_or_state() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("missing");
        assert!(load(&root, 1).unwrap().is_none());
        assert!(!root.exists());
    }

    #[test]
    fn material_episodes_survive_reopen_and_ignore_stale_missing_and_name_noise() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("feed");
        let mut item = row("a");
        publish_rows(&root, vec![item.clone()], false, 10).unwrap();
        let original = load(&root, 10).unwrap().unwrap().rows[0].occurrence.clone();
        item.name = "Renamed".into();
        publish_rows(&root, vec![item.clone()], true, 20).unwrap();
        assert_eq!(
            load(&root, 20).unwrap().unwrap().rows[0].occurrence,
            original
        );
        item.status = "working".into();
        item.stale = true;
        publish_rows(&root, vec![item.clone()], true, 21).unwrap();
        publish_rows(&root, vec![], true, 22).unwrap();
        item.status = "needsyou".into();
        item.stale = false;
        publish_rows(&root, vec![item.clone()], false, 23).unwrap();
        assert_eq!(
            load(&root, 23).unwrap().unwrap().rows[0].occurrence,
            original
        );
        item.status = "working".into();
        publish_rows(&root, vec![item.clone()], false, 24).unwrap();
        item.status = "needsyou".into();
        publish_rows(&root, vec![item], false, 25).unwrap();
        assert_ne!(
            load(&root, 25).unwrap().unwrap().rows[0].occurrence,
            original
        );
    }

    #[test]
    fn only_distinct_committed_hook_events_change_same_status_episode() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("feed");
        let store = crate::store::Store::at(tmp.path().join("pika.sqlite"));
        store.initialize().unwrap();
        let mut observed = crate::model::StatusObservation {
            kind: crate::model::ObservationKind::Lifecycle,
            status: crate::model::Status::NeedsYou,
            unread: true,
            attention_reason: Some("question".into()),
            error: None,
            observed_at: 10.0,
            source: "hook:QuestionRequest".into(),
        };
        let mut item = row("a");
        store
            .record_status_observation(crate::model::Provider::Codex, "a", &observed)
            .unwrap();
        let first = lifecycle_events(store.path(), "node", &[item.clone()]).unwrap();
        item.event_id = first.get(&item.identity).cloned();
        assert!(item.event_id.is_some());
        publish_rows(&root, vec![item.clone()], false, 10).unwrap();
        let original = load(&root, 10).unwrap().unwrap().rows[0].occurrence.clone();
        publish_rows(&root, vec![item.clone()], false, 20).unwrap();
        assert_eq!(
            load(&root, 20).unwrap().unwrap().rows[0].occurrence,
            original
        );
        observed.observed_at = 21.0;
        store
            .record_status_observation(crate::model::Provider::Codex, "a", &observed)
            .unwrap();
        item.event_id = lifecycle_events(store.path(), "node", &[item.clone()])
            .unwrap()
            .get(&item.identity)
            .cloned();
        publish_rows(&root, vec![item.clone()], false, 21).unwrap();
        let second = load(&root, 21).unwrap().unwrap().rows[0].occurrence.clone();
        assert_ne!(second, original);
        observed.observed_at = 22.0;
        observed.source = "reconcile".into();
        store
            .record_status_observation(crate::model::Provider::Codex, "a", &observed)
            .unwrap();
        item.event_id = lifecycle_events(store.path(), "node", &[item.clone()])
            .unwrap()
            .get(&item.identity)
            .cloned();
        assert!(item.event_id.is_none());
        publish_rows(&root, vec![item.clone()], false, 22).unwrap();
        assert_eq!(load(&root, 22).unwrap().unwrap().rows[0].occurrence, second);
        assert!(
            lifecycle_events(store.path(), "other-node", &[item])
                .unwrap()
                .is_empty()
        );
    }
}
