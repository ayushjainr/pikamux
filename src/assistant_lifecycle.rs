//! Opt-in host lifetime and a conservative background-call sub-allowance.
//! No observer, provider call, startup registration, or notification transport.
//! The single owner opens this store; views use its existing authenticated IPC.
use crate::assistant_memory::{Origin, Scope};
use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicI64, AtomicUsize, Ordering},
    },
};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BackgroundConfig {
    pub scope: Scope,
    pub executable: PathBuf,
    /// Subset of the already approved foreground lifetime call allowance.
    /// All calls still require the main policy ledger's independent reservation.
    pub max_calls: u64,
    pub expires_at: i64,
    pub job_timeout_secs: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Saved {
    config: Option<BackgroundConfig>,
    enabled: bool,
    paused: bool,
    revision: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct Status {
    pub config: Option<BackgroundConfig>,
    pub enabled: bool,
    pub paused: bool,
    pub state: &'static str,
    pub reserved_calls: u64,
    pub remaining_calls: u64,
    pub notice: &'static str,
}

/// A cache populated only by the host's lifecycle controller, not by request
/// text or model output. Reading it performs no I/O and never calls a model.
#[derive(Clone, Default)]
pub struct LifetimeGate(Arc<AtomicI64>, Arc<AtomicUsize>);
impl LifetimeGate {
    pub fn keeps_alive(&self, now: i64) -> bool {
        now >= 0 && (now < self.0.load(Ordering::Acquire) || self.1.load(Ordering::Acquire) > 0)
    }
    /// Finish an already accepted local cleanup after the last view detaches.
    /// This is not a reasoning grant; it never changes policy or call allowance.
    pub(crate) fn retain_cleanup(&self) -> CleanupHold {
        self.1.fetch_add(1, Ordering::AcqRel);
        CleanupHold(self.1.clone())
    }
}

pub(crate) struct CleanupHold(Arc<AtomicUsize>);
impl Drop for CleanupHold {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum Outcome {
    Completed,
    Failed,
    Unknown,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Reservation {
    pub id: String,
    pub calls: u64,
    pub deadline: i64,
    pub state: String,
    pub config_revision: u64,
    pub confirmed_calls: Option<u64>,
}

pub struct Lifecycle {
    db: Connection,
    gate: LifetimeGate,
}

impl Lifecycle {
    /// Call only after acquiring the authority host lock. Interrupted dispatch
    /// becomes unknown; queued work is abandoned, charged, and never replayed.
    pub fn open(root: &Path) -> Result<Self> {
        let path = root.join("owner.sqlite");
        crate::assistant_storage::database(&path)?;
        let db = Connection::open(path)?;
        db.busy_timeout(std::time::Duration::from_millis(50))?;
        db.execute_batch("CREATE TABLE IF NOT EXISTS assistant_lifetime(id INTEGER PRIMARY KEY CHECK(id=1), body TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS assistant_background_roots(id TEXT PRIMARY KEY, calls INTEGER NOT NULL CHECK(calls>0), deadline INTEGER NOT NULL, state TEXT NOT NULL, config_revision INTEGER NOT NULL);
            UPDATE assistant_background_roots SET state='abandoned' WHERE state='reserved';
            UPDATE assistant_background_roots SET state='unknown' WHERE state='dispatched';")?;
        let has_measurement: bool = db.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('assistant_background_roots') WHERE name='confirmed_calls')", [], |row| row.get(0))?;
        if !has_measurement {
            db.execute_batch("ALTER TABLE assistant_background_roots ADD COLUMN confirmed_calls INTEGER CHECK(confirmed_calls>=0 AND confirmed_calls<=calls)")?;
        }
        let lifecycle = Self {
            db,
            gate: LifetimeGate::default(),
        };
        let saved = lifecycle.saved()?;
        lifecycle.refresh_gate(&saved);
        Ok(lifecycle)
    }

    pub fn gate(&self) -> LifetimeGate {
        self.gate.clone()
    }

    /// Secondary component of the already locked authority; never replay the
    /// startup recovery mutations while another component has active work.
    pub(crate) fn attach(root: &Path) -> Result<Self> {
        let path = root.join("owner.sqlite");
        crate::assistant_storage::database(&path)?;
        let db = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        db.busy_timeout(std::time::Duration::from_millis(50))?;
        let lifecycle = Self {
            db,
            gate: LifetimeGate::default(),
        };
        lifecycle.refresh_gate(&lifecycle.saved()?);
        Ok(lifecycle)
    }

    fn saved(&self) -> Result<Saved> {
        load_saved(&self.db)
    }

    fn refresh_gate(&self, saved: &Saved) {
        // Pausing reasoning does not revoke already approved host lifetime.
        let expires = saved
            .config
            .as_ref()
            .filter(|_| saved.enabled)
            .map_or(0, |c| c.expires_at);
        self.gate.0.store(expires, Ordering::Release);
    }

    fn save(&mut self, saved: Saved, cancel_pending: bool) -> Result<()> {
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        tx.execute("INSERT INTO assistant_lifetime(id,body) VALUES(1,?) ON CONFLICT(id) DO UPDATE SET body=excluded.body", [serde_json::to_string(&saved)?])?;
        if cancel_pending {
            tx.execute(
                "UPDATE assistant_background_roots SET state='cancelled' WHERE state='reserved'",
                [],
            )?;
        }
        tx.commit()?;
        self.refresh_gate(&saved);
        Ok(())
    }

    /// Scope, binary, and ceiling come from the already-approved foreground
    /// configuration, not from the proposal being approved. This separate
    /// human approval permits background lifetime; it grants no extra content.
    pub fn approve(
        &mut self,
        origin: Origin,
        config: BackgroundConfig,
        foreground_scope: &Scope,
        foreground_executable: &Path,
        foreground_lifetime_calls: u64,
        now: i64,
    ) -> Result<()> {
        human(origin)?;
        if config.scope != *foreground_scope
            || config.executable != foreground_executable
            || config.max_calls == 0
            || config.max_calls > foreground_lifetime_calls
            || config.max_calls > i64::MAX as u64
            || config.expires_at <= now
            || now < 0
            || !(1..=3600).contains(&config.job_timeout_secs)
            || !config.executable.is_absolute()
            || config
                .scope
                .project
                .as_ref()
                .is_none_or(|s| s.trim().is_empty())
        {
            bail!(
                "Background approval must match the exact foreground scope/binary, fit its lifetime allowance, and have a future expiry and 1–3600 second job deadline"
            );
        }
        validate_scope(&config.scope)?;
        let revision = self
            .saved()?
            .revision
            .checked_add(1)
            .context("Background approval revision exhausted")?;
        self.save(
            Saved {
                config: Some(config),
                enabled: true,
                paused: false,
                revision,
            },
            true,
        )
    }

    pub fn pause(&mut self, origin: Origin, _now: i64) -> Result<()> {
        human(origin)?;
        let mut saved = self.saved()?;
        saved.paused = true;
        self.save(saved, true)
    }

    pub fn resume(&mut self, origin: Origin, now: i64) -> Result<()> {
        human(origin)?;
        let mut saved = self.saved()?;
        if !saved.enabled
            || saved
                .config
                .as_ref()
                .is_none_or(|c| now < 0 || c.expires_at <= now)
        {
            bail!(
                "Background approval is absent, disabled, or expired; explicit approval is required"
            );
        }
        saved.paused = false;
        self.save(saved, false)
    }

    pub fn disable(&mut self, origin: Origin, _now: i64) -> Result<()> {
        human(origin)?;
        let mut saved = self.saved()?;
        saved.enabled = false;
        saved.paused = true;
        self.save(saved, true)
    }

    pub fn snapshot(&self, now: i64) -> Result<Status> {
        let saved = self.saved()?;
        let reserved_calls = spent(&self.db)?;
        let remaining_calls = saved
            .config
            .as_ref()
            .map_or(0, |c| c.max_calls.saturating_sub(reserved_calls));
        let state = if !saved.enabled {
            "off"
        } else if saved
            .config
            .as_ref()
            .is_none_or(|c| now < 0 || c.expires_at <= now)
        {
            "expired"
        } else if saved.paused {
            "paused"
        } else if remaining_calls == 0 {
            "exhausted"
        } else {
            "enabled"
        };
        Ok(Status {
            config: saved.config,
            enabled: saved.enabled,
            paused: saved.paused,
            state,
            reserved_calls,
            remaining_calls,
            notice: "Background call counts are a subset of the foreground allowance, not a dollar ceiling. Only known completion with policy-confirmed calls settles a reservation. Failed, interrupted, unknown, or unverified work stays fully charged and is never automatically replayed. Notices stay in the board; no startup service or push notifications are installed.",
        })
    }

    /// The caller supplies one deterministic material-revision root ID, never
    /// a new random ID per repaint. A duplicate is refused even after restart,
    /// pause/resume, reapproval, or an uncertain delivery.
    pub fn reserve(&mut self, material_root: &str, calls: u64, now: i64) -> Result<Reservation> {
        validate_reservation(material_root, calls)?;
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let saved = load_saved(&tx)?;
        let config = dispatch_config(&saved, now)?;
        let deadline = now
            .checked_add(config.job_timeout_secs as i64)
            .context("Background deadline overflow")?
            .min(config.expires_at);
        if calls > config.max_calls.saturating_sub(spent(&tx)?) {
            bail!("Background call allowance exhausted");
        }
        let existing: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM assistant_background_roots WHERE id=?)",
            [material_root],
            |row| row.get(0),
        )?;
        if existing {
            bail!("Material revision was already reserved; no automatic replay");
        }
        tx.execute("INSERT INTO assistant_background_roots(id,calls,deadline,state,config_revision) VALUES(?,?,?,'reserved',?)",
            params![material_root, calls as i64, deadline, saved.revision])?;
        tx.commit()?;
        Ok(Reservation {
            id: material_root.into(),
            calls,
            deadline,
            state: "reserved".into(),
            config_revision: saved.revision,
            confirmed_calls: None,
        })
    }

    /// Recheck pause/revocation/expiry immediately before the main policy
    /// ledger dispatch. The caller must also obtain that ledger's allowance;
    /// this receipt alone never authorizes a provider call.
    pub fn begin_dispatch(&mut self, id: &str, now: i64) -> Result<Reservation> {
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let saved = load_saved(&tx)?;
        dispatch_config(&saved, now)?;
        let mut reservation =
            get_reservation(&tx, id)?.context("Background reservation not found")?;
        if reservation.state != "reserved"
            || reservation.config_revision != saved.revision
            || now >= reservation.deadline
        {
            bail!("Background reservation is no longer dispatchable; no automatic retry");
        }
        tx.execute(
            "UPDATE assistant_background_roots SET state='dispatched' WHERE id=?",
            [id],
        )?;
        tx.commit()?;
        reservation.state = "dispatched".into();
        Ok(reservation)
    }

    pub fn finish(&mut self, id: &str, outcome: Outcome) -> Result<()> {
        self.finish_measured(id, outcome, None)
    }

    /// Only pass a measurement from the authoritative main-policy receipt for
    /// this exact root, after known completion. A model-reported estimate is not
    /// a receipt. Unknown/failed/unverified outcomes retain the original ceiling.
    /// Original reservations stay immutable; confirmed calls are separate.
    pub fn finish_measured(
        &mut self,
        id: &str,
        outcome: Outcome,
        confirmed_calls: Option<u64>,
    ) -> Result<()> {
        let state = match outcome {
            Outcome::Completed => "completed",
            Outcome::Failed => "failed",
            Outcome::Unknown => "unknown",
        };
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let current = get_reservation(&tx, id)?.context("Background reservation not found")?;
        let measured = if outcome == Outcome::Completed {
            confirmed_calls
        } else {
            None
        };
        if measured.is_some_and(|calls| calls > current.calls) {
            bail!("Confirmed background calls exceed the original reservation");
        }
        if current.confirmed_calls.is_some()
            && measured.is_some()
            && current.confirmed_calls != measured
        {
            bail!("A completed background measurement cannot be rewritten");
        }
        if current.state != "dispatched" && current.state != state {
            bail!("Only the exact dispatched background root can receive an outcome");
        }
        tx.execute(
            "UPDATE assistant_background_roots SET state=?,confirmed_calls=COALESCE(confirmed_calls,?) WHERE id=?",
            params![state, measured, id],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn reservation(&self, id: &str) -> Result<Option<Reservation>> {
        get_reservation(&self.db, id)
    }
}

fn human(origin: Origin) -> Result<()> {
    if origin != Origin::Human {
        bail!("Only explicit human input may change background approval or lifecycle");
    }
    Ok(())
}
fn validate_scope(scope: &Scope) -> Result<()> {
    for value in [
        &scope.node,
        &scope.project,
        &scope.provider,
        &scope.conversation,
    ]
    .into_iter()
    .flatten()
    {
        if value.is_empty() || value.len() > 512 || value.chars().any(char::is_control) {
            bail!("Background scope is invalid");
        }
    }
    Ok(())
}
fn validate_reservation(material_root: &str, calls: u64) -> Result<()> {
    if material_root.is_empty()
        || material_root.len() > 256
        || material_root.chars().any(char::is_control)
        || calls == 0
        || calls > i64::MAX as u64
    {
        bail!(
            "Background reservation requires a bounded exact material root and positive call count"
        );
    }
    Ok(())
}
fn load_saved(db: &Connection) -> Result<Saved> {
    let json: Option<String> = db
        .query_row(
            "SELECT body FROM assistant_lifetime WHERE id=1",
            [],
            |row| row.get(0),
        )
        .optional()?;
    Ok(match json {
        Some(json) => serde_json::from_str(&json)?,
        None => Saved {
            paused: true,
            ..Saved::default()
        },
    })
}
fn dispatch_config(saved: &Saved, now: i64) -> Result<&BackgroundConfig> {
    let config = saved
        .config
        .as_ref()
        .context("Background is not approved")?;
    if !saved.enabled || saved.paused || now < 0 || now >= config.expires_at {
        bail!("Background is disabled, paused, or expired");
    }
    Ok(config)
}
fn spent(db: &Connection) -> Result<u64> {
    let total: i64 = db.query_row(
        "SELECT COALESCE(SUM(COALESCE(confirmed_calls,calls)),0) FROM assistant_background_roots",
        [],
        |row| row.get(0),
    )?;
    u64::try_from(total).context("Invalid background accounting")
}
fn get_reservation(db: &Connection, id: &str) -> Result<Option<Reservation>> {
    Ok(db.query_row("SELECT id,calls,deadline,state,config_revision,confirmed_calls FROM assistant_background_roots WHERE id=?", [id], |row| {
        Ok(Reservation { id: row.get(0)?, calls: row.get(1)?, deadline: row.get(2)?, state: row.get(3)?, config_revision: row.get(4)?, confirmed_calls: row.get(5)? })
    }).optional()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn accepted_cleanup_keeps_owner_alive_without_enabling_reasoning() {
        let temp = tempfile::tempdir().unwrap();
        let mut lifecycle = Lifecycle::open(&temp.path().join("private")).unwrap();
        let gate = lifecycle.gate();
        assert!(!gate.keeps_alive(1));
        let first = gate.retain_cleanup();
        let second = gate.retain_cleanup();
        assert!(gate.keeps_alive(i64::MAX));
        assert_eq!(lifecycle.snapshot(1).unwrap().state, "off");
        assert!(
            lifecycle
                .reserve("cleanup-does-not-grant-calls", 1, 1)
                .is_err()
        );
        drop(first);
        assert!(gate.keeps_alive(1));
        drop(second);
        assert!(!gate.keeps_alive(1));
    }
    fn config() -> BackgroundConfig {
        BackgroundConfig {
            scope: Scope {
                project: Some("alpha".into()),
                ..Scope::default()
            },
            executable: PathBuf::from("/fake/codex"),
            max_calls: 3,
            expires_at: 1000,
            job_timeout_secs: 60,
        }
    }
    fn approve(lifecycle: &mut Lifecycle) {
        let config = config();
        lifecycle
            .approve(
                Origin::Human,
                config.clone(),
                &config.scope,
                &config.executable,
                3,
                1,
            )
            .unwrap();
    }

    #[test]
    fn defaults_off_human_approval_exact_scope_and_expiry() {
        let temp = tempfile::tempdir().unwrap();
        let mut lifecycle = Lifecycle::open(&temp.path().join("private")).unwrap();
        assert!(!lifecycle.gate().keeps_alive(1));
        assert_eq!(lifecycle.snapshot(1).unwrap().state, "off");
        assert!(lifecycle.snapshot(1).unwrap().paused);
        assert!(lifecycle.reserve("material-1", 1, 1).is_err());
        let config = config();
        for origin in [Origin::Worker, Origin::System] {
            assert!(
                lifecycle
                    .approve(
                        origin,
                        config.clone(),
                        &config.scope,
                        &config.executable,
                        3,
                        1
                    )
                    .is_err()
            );
            assert!(lifecycle.resume(origin, 1).is_err());
        }
        assert!(
            lifecycle
                .approve(
                    Origin::Human,
                    config.clone(),
                    &Scope::default(),
                    &config.executable,
                    3,
                    1
                )
                .is_err()
        );
        assert!(
            lifecycle
                .approve(
                    Origin::Human,
                    config.clone(),
                    &config.scope,
                    Path::new("/fake/other"),
                    3,
                    1
                )
                .is_err()
        );
        assert!(
            lifecycle
                .approve(
                    Origin::Human,
                    config.clone(),
                    &config.scope,
                    &config.executable,
                    2,
                    1
                )
                .is_err()
        );
        approve(&mut lifecycle);
        let gate = lifecycle.gate();
        assert!(gate.keeps_alive(999));
        assert!(!gate.keeps_alive(1000));
        assert_eq!(lifecycle.snapshot(1000).unwrap().state, "expired");
        assert!(lifecycle.reserve("material-1", 1, 1000).is_err());
        assert!(lifecycle.resume(Origin::Human, 1000).is_err());
    }

    #[test]
    fn pause_revokes_queued_dispatch_without_stopping_approved_lifetime() {
        let temp = tempfile::tempdir().unwrap();
        let mut lifecycle = Lifecycle::open(&temp.path().join("private")).unwrap();
        approve(&mut lifecycle);
        lifecycle.reserve("revision-1", 1, 2).unwrap();
        lifecycle.pause(Origin::Human, 3).unwrap();
        assert!(lifecycle.gate().keeps_alive(3));
        assert_eq!(lifecycle.snapshot(3).unwrap().state, "paused");
        assert!(lifecycle.begin_dispatch("revision-1", 3).is_err());
        assert!(lifecycle.reserve("revision-2", 1, 3).is_err());
        lifecycle.resume(Origin::Human, 4).unwrap();
        assert!(lifecycle.reserve("revision-1", 1, 4).is_err());
        lifecycle.reserve("revision-2", 1, 4).unwrap();
        lifecycle.begin_dispatch("revision-2", 4).unwrap();
        lifecycle.finish("revision-2", Outcome::Completed).unwrap();
        lifecycle.disable(Origin::Human, 5).unwrap();
        assert!(!lifecycle.gate().keeps_alive(5));
        assert!(lifecycle.resume(Origin::Human, 5).is_err());
    }

    #[test]
    fn restart_never_replays_unknown_or_reserved_work_and_reapproval_cannot_refill() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("private");
        let mut lifecycle = Lifecycle::open(&root).unwrap();
        approve(&mut lifecycle);
        lifecycle.reserve("revision-1", 1, 2).unwrap();
        lifecycle.begin_dispatch("revision-1", 3).unwrap();
        lifecycle.reserve("revision-2", 1, 2).unwrap();
        drop(lifecycle);
        let mut reopened = Lifecycle::open(&root).unwrap();
        assert!(reopened.gate().keeps_alive(4));
        assert_eq!(
            reopened.reservation("revision-1").unwrap().unwrap().state,
            "unknown"
        );
        assert_eq!(
            reopened.reservation("revision-2").unwrap().unwrap().state,
            "abandoned"
        );
        for id in ["revision-1", "revision-2"] {
            assert!(reopened.reserve(id, 1, 4).is_err());
            assert!(reopened.begin_dispatch(id, 4).is_err());
        }
        approve(&mut reopened);
        assert_eq!(reopened.snapshot(4).unwrap().remaining_calls, 1);
        reopened.reserve("revision-3", 1, 4).unwrap();
        assert!(reopened.reserve("revision-4", 1, 4).is_err());
        assert!(reopened.begin_dispatch("revision-3", 64).is_err());
        assert_eq!(reopened.snapshot(64).unwrap().state, "exhausted");
    }

    #[test]
    fn only_verified_completed_roots_settle_reserved_calls() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("private");
        let mut lifecycle = Lifecycle::open(&root).unwrap();
        let config = BackgroundConfig {
            max_calls: 10,
            ..config()
        };
        lifecycle
            .approve(
                Origin::Human,
                config.clone(),
                &config.scope,
                &config.executable,
                10,
                1,
            )
            .unwrap();
        lifecycle.reserve("complete", 4, 2).unwrap();
        lifecycle.begin_dispatch("complete", 2).unwrap();
        lifecycle
            .finish_measured("complete", Outcome::Completed, Some(1))
            .unwrap();
        lifecycle
            .finish_measured("complete", Outcome::Completed, Some(1))
            .unwrap();
        assert!(
            lifecycle
                .finish_measured("complete", Outcome::Completed, Some(0))
                .is_err()
        );
        assert_eq!(lifecycle.snapshot(2).unwrap().reserved_calls, 1);
        let completed = lifecycle.reservation("complete").unwrap().unwrap();
        assert_eq!(completed.calls, 4);
        assert_eq!(completed.confirmed_calls, Some(1));
        lifecycle.reserve("unknown", 4, 2).unwrap();
        lifecycle.begin_dispatch("unknown", 2).unwrap();
        lifecycle
            .finish_measured("unknown", Outcome::Unknown, Some(0))
            .unwrap();
        assert!(
            lifecycle
                .finish_measured("unknown", Outcome::Completed, Some(0))
                .is_err()
        );
        lifecycle.reserve("failed", 4, 2).unwrap();
        lifecycle.begin_dispatch("failed", 2).unwrap();
        lifecycle
            .finish_measured("failed", Outcome::Failed, Some(1))
            .unwrap();
        assert_eq!(lifecycle.snapshot(2).unwrap().reserved_calls, 9);
        lifecycle.reserve("unverified", 1, 2).unwrap();
        lifecycle.begin_dispatch("unverified", 2).unwrap();
        lifecycle
            .finish_measured("unverified", Outcome::Completed, None)
            .unwrap();
        drop(lifecycle);
        let reopened = Lifecycle::open(&root).unwrap();
        assert_eq!(reopened.snapshot(3).unwrap().reserved_calls, 10);
        assert_eq!(reopened.snapshot(3).unwrap().remaining_calls, 0);
    }
}
