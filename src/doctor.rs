//! Read-only recovery diagnostics and narrowly scoped stale-record repair.
//!
//! Doctor never reconciles state, reads message bodies, attaches panes, or
//! signals processes. Runtime facts are captured once so one report cannot mix
//! incompatible process generations.

use crate::{
    VERSION,
    config::Config,
    core::LIVE_OWNER_LEASE_SECONDS,
    model::{Pane, Provider, Session, Status},
    paths::Paths,
    process::{self, ProcessRecord},
    setup,
    store::{PendingLaunch, Store},
    tmux::Tmux,
};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const PROVIDER_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckLevel {
    Ok,
    Warn,
    Error,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DoctorCheck {
    pub code: String,
    pub level: CheckLevel,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct RecoverySummary {
    pub tracked: usize,
    pub recoverable: usize,
    pub exact_homes: usize,
    pub outside: usize,
    pub duplicates: usize,
    pub mismatched_homes: usize,
    pub unbound: usize,
    pub pending: usize,
    pub reservations: usize,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RepairReceipt {
    pub kind: String,
    /// One-way short identifier; raw UUIDs, tokens and paths are excluded.
    pub reference: String,
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DoctorReport {
    pub schema: &'static str,
    pub safe_to_disconnect: bool,
    pub verified_at_unix: f64,
    pub platform: String,
    pub pikamux_version: &'static str,
    pub recovery: RecoverySummary,
    pub repairs: Vec<RepairReceipt>,
    pub checks: Vec<DoctorCheck>,
}

impl DoctorReport {
    pub fn render_human(&self, verbose: bool) -> String {
        let mut output = String::new();
        if verbose || !self.safe_to_disconnect {
            for check in &self.checks {
                let mark = match check.level {
                    CheckLevel::Ok => '✓',
                    CheckLevel::Warn => '!',
                    CheckLevel::Error => '✗',
                };
                output.push_str(&format!("{mark} {}: {}\n", check.code, check.message));
            }
            output.push('\n');
        }
        if self.safe_to_disconnect {
            output.push_str(&format!(
                "Recovery verified · {}/{} exact conversation(s) · unix {:.0}\n",
                self.recovery.recoverable, self.recovery.tracked, self.verified_at_unix
            ));
            output.push_str("Safe to disconnect this terminal. Keep the tmux server running.\n\n");
            output.push_str("Copy-safe recovery passport\n");
            output.push_str(&format!(
                "PIKA VERIFIED · {} recoverable · 0 ambiguous\n{} · pikamux {}\n",
                self.recovery.recoverable, self.platform, self.pikamux_version
            ));
        } else {
            output.push_str(
                "Pika found recovery risks above. Resolve them before relying on terminal disconnects.\n",
            );
        }
        output
    }
}

#[derive(Clone, Debug, Default)]
pub struct ProviderRuntime {
    pub available: bool,
    pub version: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct RuntimeEvidence {
    pub tmux_available: bool,
    pub tmux_snapshot_complete: bool,
    pub tmux_error: Option<String>,
    pub panes: Vec<Pane>,
    pub process_snapshot_complete: bool,
    pub processes: BTreeMap<i64, ProcessRecord>,
    pub providers: BTreeMap<Provider, ProviderRuntime>,
    pub platform: String,
}

/// Capture native read-only evidence with bounded provider version probes.
pub fn collect_runtime_evidence(tmux: &Tmux, config: &Config) -> RuntimeEvidence {
    let tmux_available = tmux.available();
    let (panes, tmux_snapshot_complete, tmux_error) = if tmux_available {
        match tmux.list_panes() {
            Ok(panes) => (panes, true, None),
            Err(error) => (Vec::new(), false, Some(clean(&error.to_string()))),
        }
    } else {
        (Vec::new(), false, None)
    };
    let providers = Provider::ALL
        .into_iter()
        .map(|provider| {
            (
                provider,
                probe_provider(&config.executable(provider), PROVIDER_PROBE_TIMEOUT),
            )
        })
        .collect();
    RuntimeEvidence {
        tmux_available,
        tmux_snapshot_complete,
        tmux_error,
        panes,
        process_snapshot_complete: cfg!(any(target_os = "linux", target_os = "macos")),
        processes: process::snapshot(),
        providers,
        platform: std::env::consts::OS.to_owned(),
    }
}

pub fn inspect_native(
    paths: &Paths,
    store: &Store,
    tmux: &Tmux,
    pika_executable: &Path,
) -> DoctorReport {
    let config = Config::load(paths).unwrap_or_default();
    let evidence = collect_runtime_evidence(tmux, &config);
    inspect_with_evidence(paths, store, pika_executable, &evidence, now())
}

pub fn inspect_native_and_repair_stale(
    paths: &Paths,
    store: &Store,
    tmux: &Tmux,
    pika_executable: &Path,
    older_than_seconds: f64,
) -> DoctorReport {
    let config = Config::load(paths).unwrap_or_default();
    let evidence = collect_runtime_evidence(tmux, &config);
    inspect_and_repair_stale(
        paths,
        store,
        pika_executable,
        &evidence,
        now(),
        older_than_seconds,
    )
}

/// Build a deterministic, read-only report from one captured runtime snapshot.
pub fn inspect_with_evidence(
    paths: &Paths,
    store: &Store,
    pika_executable: &Path,
    evidence: &RuntimeEvidence,
    at: f64,
) -> DoctorReport {
    let mut checks = Vec::new();
    let config = match Config::load(paths) {
        Ok(config) if paths.config.is_file() => {
            check_path_permissions("config.permissions", &paths.config, &mut checks);
            checks.push(ok("config.format", "configuration is valid JSON"));
            config
        }
        Ok(config) => {
            checks.push(warn(
                "config.file",
                "configuration is missing; run `pika setup`",
            ));
            config
        }
        Err(error) => {
            checks.push(error_check("config.format", &clean(&error.to_string())));
            Config::default()
        }
    };

    let sessions = if store.path().is_file() {
        check_path_permissions("database.permissions", store.path(), &mut checks);
        check_database_companions(store.path(), &mut checks);
        match store.list_sessions() {
            Ok(sessions) => {
                checks.push(ok("database.schema", "state database schema is current"));
                Some(sessions)
            }
            Err(error) => {
                checks.push(error_check("database.schema", &clean(&error.to_string())));
                None
            }
        }
    } else {
        checks.push(warn("database.file", "state database is missing"));
        None
    };

    if evidence.tmux_available {
        checks.push(ok("tmux.available", "tmux is available"));
    } else {
        checks.push(error_check(
            "tmux.available",
            "tmux is not installed or cannot execute",
        ));
    }
    if evidence.tmux_available && !evidence.tmux_snapshot_complete {
        checks.push(error_check(
            "tmux.snapshot",
            evidence
                .tmux_error
                .as_deref()
                .unwrap_or("tmux pane inventory could not be verified"),
        ));
    }
    if !evidence.process_snapshot_complete {
        checks.push(error_check(
            "process.snapshot",
            "process identity snapshot is unavailable on this platform",
        ));
    }

    let required: BTreeSet<_> = sessions
        .as_ref()
        .into_iter()
        .flatten()
        .map(|session| session.provider)
        .chain(std::iter::once(config.default_provider))
        .collect();
    for provider in Provider::ALL {
        let runtime = evidence
            .providers
            .get(&provider)
            .cloned()
            .unwrap_or_default();
        if required.contains(&provider) {
            if runtime.available {
                checks.push(ok(
                    &format!("provider.{provider}"),
                    &format!(
                        "{} is available{}",
                        provider,
                        runtime
                            .version
                            .as_deref()
                            .map(|value| format!(" · {}", clean(value)))
                            .unwrap_or_default()
                    ),
                ));
            } else {
                checks.push(error_check(
                    &format!("provider.{provider}"),
                    &format!("{provider} is required but unavailable"),
                ));
            }
        } else {
            checks.push(ok(
                &format!("provider.{provider}"),
                &format!("{provider} is optional and not currently required"),
            ));
        }
        check_hook(
            paths,
            store,
            pika_executable,
            provider,
            required.contains(&provider),
            at,
            &mut checks,
        );
    }

    let mut recovery = RecoverySummary::default();
    if let Some(sessions) = sessions {
        recovery.tracked = sessions.len();
        inspect_identity(
            paths,
            store,
            &sessions,
            evidence,
            at,
            &mut recovery,
            &mut checks,
        );
    }

    let repairs = Vec::new();
    let safe = checks.iter().all(|check| check.level == CheckLevel::Ok);
    DoctorReport {
        schema: "pikamux-doctor/v1",
        safe_to_disconnect: safe,
        verified_at_unix: at,
        platform: clean(&evidence.platform),
        pikamux_version: VERSION,
        recovery,
        repairs,
        checks,
    }
}

pub fn inspect_and_repair_stale(
    paths: &Paths,
    store: &Store,
    pika_executable: &Path,
    evidence: &RuntimeEvidence,
    at: f64,
    older_than_seconds: f64,
) -> DoctorReport {
    let repairs = repair_stale(store, evidence, at, older_than_seconds);
    let mut report = inspect_with_evidence(paths, store, pika_executable, evidence, at);
    report.repairs = repairs;
    report
}

fn inspect_identity(
    paths: &Paths,
    store: &Store,
    sessions: &[Session],
    evidence: &RuntimeEvidence,
    at: f64,
    recovery: &mut RecoverySummary,
    checks: &mut Vec<DoctorCheck>,
) {
    let tracked: BTreeSet<_> = sessions
        .iter()
        .map(|session| (session.provider, session.session_id.clone()))
        .collect();
    let mut invalid = Vec::new();
    let mut non_resumable = Vec::new();
    let mut outside = Vec::new();
    let mut duplicates = Vec::new();
    let mut mismatched = Vec::new();
    let mut missing_cwd = Vec::new();

    for session in sessions {
        let subject = identity_reference(session.provider, &session.session_id);
        if session.status == Status::Unbound || session.session_id.starts_with("unbound:") {
            recovery.unbound += 1;
            continue;
        }
        if !crate::providers::Providers::valid_id(session.provider, &session.session_id) {
            invalid.push(subject);
            continue;
        }
        if session
            .cwd
            .as_deref()
            .is_none_or(|cwd| !Path::new(cwd).is_dir())
        {
            missing_cwd.push(subject.clone());
        }

        let tagged: Vec<_> = evidence
            .panes
            .iter()
            .filter(|pane| {
                pane.pika_provider == Some(session.provider)
                    && pane.pika_session_id.as_deref() == Some(&session.session_id)
            })
            .collect();
        let owned: BTreeSet<_> = tagged
            .iter()
            .flat_map(|pane| process::process_tree(pane.pane_pid, &evidence.processes))
            .collect();
        let direct = direct_identity_pids(session, &evidence.processes);
        let mut identities = direct.clone();
        if let Ok(owners) = store.live_owners(session.provider, &session.session_id) {
            for owner in owners {
                let Some(record) = evidence.processes.get(&owner.pid) else {
                    continue;
                };
                let valid_generation = owner
                    .start_time
                    .is_some_and(|start| u64::try_from(start).ok() == Some(record.start_time));
                let expired_shared = process::shared_provider_process(record, session.provider)
                    && at - owner.last_seen > LIVE_OWNER_LEASE_SECONDS;
                if valid_generation
                    && record.provider() == Some(session.provider)
                    && !expired_shared
                {
                    identities.insert(owner.pid);
                }
            }
        }
        if let Ok(Some(owner)) = store.get_recovery_owner(session.provider, &session.session_id) {
            let valid = evidence.processes.get(&owner.pid).is_some_and(|record| {
                u64::try_from(owner.start_time).ok() == Some(record.start_time)
                    && record.provider() == Some(session.provider)
            }) && store.get_launch_binding(&owner.launch_token).ok().flatten()
                == Some((session.provider, session.session_id.clone()));
            if valid {
                identities.insert(owner.pid);
            }
        }
        let outside_pids: BTreeSet<_> = identities.difference(&owned).copied().collect();
        let candidate_panes: Vec<_> = tagged
            .iter()
            .filter(|pane| {
                process::provider_process(
                    pane.pane_pid,
                    Some(session.provider),
                    &evidence.processes,
                )
                .is_some_and(|pid| identities.contains(&pid))
            })
            .collect();
        for pane in &tagged {
            let expected = process::provider_process(
                pane.pane_pid,
                Some(session.provider),
                &evidence.processes,
            );
            if expected.is_none_or(|pid| !identities.contains(&pid)) {
                mismatched.push(subject.clone());
            }
        }
        let duplicate = identities.len() > 1
            || candidate_panes.len() > 1
            || tagged.len() > 1
            || (!candidate_panes.is_empty() && !outside_pids.is_empty());
        let exact = !duplicate
            && candidate_panes.len() == 1
            && outside_pids.is_empty()
            && !identities.is_empty();
        if duplicate {
            duplicates.push(subject.clone());
        }
        if !outside_pids.is_empty() {
            outside.push(subject.clone());
        }
        if exact {
            recovery.exact_homes += 1;
        }
        let resumable = exact || durable_identity_present(paths, session);
        if !resumable {
            non_resumable.push(subject.clone());
        }
        if resumable && !missing_cwd.contains(&subject) {
            recovery.recoverable += 1;
        }
    }

    let mut hidden_live = Vec::new();
    if let Ok(owners) = store.list_live_owners() {
        for owner in owners {
            if tracked.contains(&(owner.provider, owner.session_id.clone())) {
                continue;
            }
            let valid = evidence.processes.get(&owner.pid).is_some_and(|record| {
                owner
                    .start_time
                    .is_none_or(|start| u64::try_from(start).ok() == Some(record.start_time))
                    && record.provider() == Some(owner.provider)
                    && !(process::shared_provider_process(record, owner.provider)
                        && at - owner.last_seen > LIVE_OWNER_LEASE_SECONDS)
            });
            if valid {
                hidden_live.push(identity_reference(owner.provider, &owner.session_id));
            }
        }
    }
    recovery.outside = outside.len() + hidden_live.len();
    recovery.duplicates = duplicates.len();
    recovery.mismatched_homes = mismatched.len();
    push_subject_check(
        checks,
        "identity.invalid",
        CheckLevel::Error,
        "invalid provider identities",
        &invalid,
    );
    push_subject_check(
        checks,
        "identity.nonresumable",
        CheckLevel::Error,
        "conversations lack durable provider history",
        &non_resumable,
    );
    push_subject_check(
        checks,
        "identity.duplicates",
        CheckLevel::Error,
        "conversations have more than one live owner",
        &duplicates,
    );
    let outside_all = outside
        .iter()
        .chain(&hidden_live)
        .cloned()
        .collect::<Vec<_>>();
    push_subject_check(
        checks,
        "identity.outside",
        CheckLevel::Error,
        "tracked or hidden conversations are live outside exact Pika homes",
        &outside_all,
    );
    push_subject_check(
        checks,
        "identity.mismatch",
        CheckLevel::Error,
        "tagged panes lack independent exact-identity evidence",
        &mismatched,
    );
    push_subject_check(
        checks,
        "working-directories",
        CheckLevel::Warn,
        "saved working directories no longer exist",
        &missing_cwd,
    );

    let pending = store.list_pending().unwrap_or_default();
    let reservations = store.list_resume_reservations().unwrap_or_default();
    recovery.pending = pending.len();
    recovery.reservations = reservations.len();
    if recovery.unbound > 0 || !pending.is_empty() || !reservations.is_empty() {
        checks.push(warn(
            "identity.incomplete",
            &format!(
                "{} unbound, {} pending launch(es), {} active resume reservation(s)",
                recovery.unbound,
                pending.len(),
                reservations.len()
            ),
        ));
    }
    if invalid.is_empty()
        && non_resumable.is_empty()
        && duplicates.is_empty()
        && outside.is_empty()
        && hidden_live.is_empty()
        && mismatched.is_empty()
        && recovery.unbound == 0
        && pending.is_empty()
        && reservations.is_empty()
    {
        checks.push(ok(
            "identity.exact",
            "all tracked conversations have exact provider identities",
        ));
    }

    for session in sessions {
        if store
            .get_meta(&format!(
                "native_name_error:{}:{}",
                session.provider, session.session_id
            ))
            .ok()
            .flatten()
            .is_some()
        {
            checks.push(DoctorCheck {
                code: "native-name.pending".into(),
                level: CheckLevel::Warn,
                message: "provider-native naming is still pending".into(),
                subject: Some(identity_reference(session.provider, &session.session_id)),
            });
        }
    }
}

/// Remove only Pika bookkeeping records disproved by a complete runtime
/// snapshot. No process is signalled and no tmux pane is changed.
pub fn repair_stale(
    store: &Store,
    evidence: &RuntimeEvidence,
    at: f64,
    older_than_seconds: f64,
) -> Vec<RepairReceipt> {
    if !evidence.tmux_snapshot_complete || !evidence.process_snapshot_complete {
        return Vec::new();
    }
    let mut receipts = Vec::new();
    for pending in store.list_pending().unwrap_or_default() {
        if at - pending.created_at < older_than_seconds || pending_is_live(&pending, evidence) {
            continue;
        }
        if store
            .delete_pending_if_created(&pending.launch_token, pending.created_at)
            .unwrap_or(false)
        {
            let _ = store.delete_meta(&format!("attached_launch:{}", pending.launch_token));
            receipts.push(RepairReceipt {
                kind: "pending-launch".into(),
                reference: opaque_reference(&pending.launch_token),
                reason: "expired launch has no matching pane or provider process".into(),
            });
        }
    }
    for reservation in store.list_resume_reservations().unwrap_or_default() {
        if at - reservation.created_at < older_than_seconds {
            continue;
        }
        let stale = match (reservation.owner_pid, reservation.owner_start_time) {
            (Some(pid), Some(start)) => evidence
                .processes
                .get(&pid)
                .is_none_or(|record| u64::try_from(start).ok() != Some(record.start_time)),
            _ => false,
        };
        if stale
            && store
                .release_resume_if_generation(&reservation)
                .unwrap_or(false)
        {
            receipts.push(RepairReceipt {
                kind: "resume-reservation".into(),
                reference: identity_reference(reservation.provider, &reservation.session_id),
                reason: "owner PID generation no longer exists".into(),
            });
        }
    }
    for owner in store.list_live_owners().unwrap_or_default() {
        let stale_generation = match evidence.processes.get(&owner.pid) {
            None => true,
            Some(record) => owner
                .start_time
                .is_some_and(|start| u64::try_from(start).ok() != Some(record.start_time)),
        };
        let expired_shared = evidence.processes.get(&owner.pid).is_some_and(|record| {
            process::shared_provider_process(record, owner.provider)
                && at - owner.last_seen > LIVE_OWNER_LEASE_SECONDS
        });
        if (stale_generation || expired_shared)
            && store
                .delete_live_owner(
                    owner.provider,
                    &owner.session_id,
                    Some(owner.pid),
                    Some(&owner.owner_token),
                )
                .unwrap_or_default()
                == 1
        {
            receipts.push(RepairReceipt {
                kind: "live-owner-lease".into(),
                reference: identity_reference(owner.provider, &owner.session_id),
                reason: if expired_shared {
                    "shared provider ownership lease expired"
                } else {
                    "owner PID generation no longer exists"
                }
                .into(),
            });
        }
    }
    receipts
}

fn pending_is_live(pending: &PendingLaunch, evidence: &RuntimeEvidence) -> bool {
    if let Some(pid) = pending.root_pid
        && let Some(record) = evidence.processes.get(&pid)
    {
        let generation_matches = pending
            .root_pid_start
            .is_none_or(|start| u64::try_from(start).ok() == Some(record.start_time));
        if generation_matches && record.provider() == Some(pending.provider) {
            return true;
        }
    }
    if pending
        .candidate_session_id
        .as_deref()
        .is_some_and(|identity| {
            !process::find_session_processes(identity, pending.provider, &evidence.processes)
                .is_empty()
        })
    {
        return true;
    }
    evidence.panes.iter().any(|pane| {
        let matches = pending.tmux_pane.as_deref() == Some(&pane.pane_id)
            || pending.tmux_session.as_deref() == Some(&pane.session_name)
            || pane.pika_launch_token.as_deref() == Some(&pending.launch_token);
        matches
            && process::provider_process(pane.pane_pid, Some(pending.provider), &evidence.processes)
                .is_some()
    })
}

fn check_hook(
    paths: &Paths,
    store: &Store,
    executable: &Path,
    provider: Provider,
    required: bool,
    at: f64,
    checks: &mut Vec<DoctorCheck>,
) {
    let home = match provider {
        Provider::Codex => &paths.codex_home,
        Provider::Claude => &paths.claude_home,
        Provider::Opencode => &paths.opencode_config_home,
    };
    let installed = setup::hooks_installed(home, provider, executable);
    if !installed {
        checks.push(if required {
            warn(
                &format!("hooks.{provider}"),
                "lifecycle hooks are not installed; run `pika setup`",
            )
        } else {
            ok(
                &format!("hooks.{provider}"),
                "hooks are not installed for this optional provider",
            )
        });
        return;
    }
    let expected = setup::hook_spec_fingerprint(provider, executable).ok();
    let observation = store.get_hook_observation(provider).ok().flatten();
    if required
        && observation
            .as_ref()
            .zip(expected.as_ref())
            .is_none_or(|(observed, expected)| &observed.fingerprint != expected)
    {
        checks.push(warn(
            &format!("hooks.{provider}"),
            "installed hook definition has not produced matching observed evidence",
        ));
        return;
    }
    let age = observation.as_ref().map_or(String::new(), |value| {
        format!(
            " · last event {:.0}s ago",
            (at - value.observed_at).max(0.0)
        )
    });
    checks.push(ok(
        &format!("hooks.{provider}"),
        &format!(
            "lifecycle hooks are installed{}{age}",
            if required { " and verified" } else { "" }
        ),
    ));
}

fn direct_identity_pids(
    session: &Session,
    processes: &BTreeMap<i64, ProcessRecord>,
) -> BTreeSet<i64> {
    let mut result =
        process::find_session_processes(&session.session_id, session.provider, processes)
            .into_iter()
            .collect::<BTreeSet<_>>();
    if let Some(active) = session
        .active_thread_id
        .as_deref()
        .filter(|active| *active != session.session_id)
    {
        result.extend(process::find_session_processes(
            active,
            session.provider,
            processes,
        ));
    }
    result
}

fn durable_identity_present(paths: &Paths, session: &Session) -> bool {
    match session.provider {
        Provider::Codex | Provider::Claude => session
            .transcript_path
            .as_deref()
            .map(Path::new)
            .is_some_and(|path| {
                path.is_file()
                    && !path.components().any(|part| {
                        matches!(
                            part.as_os_str().to_str(),
                            Some("archived_sessions" | "sessions_archived")
                        )
                    })
            }),
        Provider::Opencode => opencode_identity_present(paths, session.provider_thread_id()),
    }
}

fn opencode_identity_present(paths: &Paths, identity: &str) -> bool {
    let path = paths.opencode_data_home.join("opencode.db");
    let uri = format!("file:{}?mode=ro", path.to_string_lossy());
    let Ok(db) = Connection::open_with_flags(
        uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    ) else {
        return false;
    };
    let has_archived = db
        .prepare("PRAGMA table_info(session)")
        .and_then(|mut statement| {
            statement
                .query_map([], |row| row.get::<_, String>(1))?
                .collect::<rusqlite::Result<Vec<_>>>()
        })
        .is_ok_and(|columns| columns.iter().any(|column| column == "time_archived"));
    let sql = if has_archived {
        "SELECT 1 FROM session WHERE id=? AND time_archived IS NULL"
    } else {
        "SELECT 1 FROM session WHERE id=?"
    };
    db.query_row(sql, [identity], |_| Ok(()))
        .optional()
        .ok()
        .flatten()
        .is_some()
}

fn check_path_permissions(code: &str, path: &Path, checks: &mut Vec<DoctorCheck>) {
    let symlink = fs::symlink_metadata(path).is_ok_and(|value| value.file_type().is_symlink());
    if symlink {
        checks.push(error_check(code, "path is a symbolic link"));
        return;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        match fs::metadata(path) {
            Ok(metadata) if metadata.permissions().mode() & 0o077 == 0 => {
                checks.push(ok(code, "owner-only permissions verified"));
            }
            Ok(_) => checks.push(warn(code, "permissions are not owner-only")),
            Err(error) => checks.push(error_check(code, &clean(&error.to_string()))),
        }
    }
    #[cfg(not(unix))]
    checks.push(warn(
        code,
        "owner-only permissions cannot be proven on this platform",
    ));
}

fn check_database_companions(path: &Path, checks: &mut Vec<DoctorCheck>) {
    for suffix in ["-wal", "-shm"] {
        let companion = std::path::PathBuf::from(format!("{}{suffix}", path.to_string_lossy()));
        if companion.exists() {
            check_path_permissions(&format!("database.permissions{suffix}"), &companion, checks);
        }
    }
}

fn push_subject_check(
    checks: &mut Vec<DoctorCheck>,
    code: &str,
    level: CheckLevel,
    message: &str,
    subjects: &[String],
) {
    if subjects.is_empty() {
        return;
    }
    checks.push(DoctorCheck {
        code: code.into(),
        level,
        message: format!("{message}: {}", subjects.join(", ")),
        subject: None,
    });
}

fn probe_provider(executable: &str, timeout: Duration) -> ProviderRuntime {
    probe_provider_result(executable, timeout).unwrap_or_default()
}

fn probe_provider_result(executable: &str, timeout: Duration) -> anyhow::Result<ProviderRuntime> {
    let output =
        crate::consult::run_output_bounded(Path::new(executable), &["--version"], timeout)?;
    let version = clean(&output);
    Ok(ProviderRuntime {
        available: true,
        version: (!version.is_empty()).then_some(version),
    })
}

fn identity_reference(provider: Provider, identity: &str) -> String {
    format!("{}:{}", provider, opaque_reference(identity))
}

fn opaque_reference(value: &str) -> String {
    let digest = format!("{:x}", Sha256::digest(value.as_bytes()));
    digest[..12].to_owned()
}

fn clean(value: &str) -> String {
    let mut result = value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if result.len() > 500 {
        let boundary = (0..=500)
            .rev()
            .find(|index| result.is_char_boundary(*index))
            .unwrap_or(0);
        result.truncate(boundary);
    }
    result
}

fn ok(code: &str, message: &str) -> DoctorCheck {
    DoctorCheck {
        code: code.into(),
        level: CheckLevel::Ok,
        message: clean(message),
        subject: None,
    }
}

fn warn(code: &str, message: &str) -> DoctorCheck {
    DoctorCheck {
        code: code.into(),
        level: CheckLevel::Warn,
        message: clean(message),
        subject: None,
    }
}

fn error_check(code: &str, message: &str) -> DoctorCheck {
    DoctorCheck {
        code: code.into(),
        level: CheckLevel::Error,
        message: clean(message),
        subject: None,
    }
}

fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

#[cfg(all(test, unix))]
mod probe_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Instant;

    #[test]
    fn doctor_probe_deadline_includes_inherited_output_after_root_exit() {
        let root = tempfile::tempdir().unwrap();
        let executable = root.path().join("provider-fixture");
        fs::write(
            &executable,
            "#!/bin/sh\nsleep 30 &\nprintf '2.1.228\\n'\nexec /usr/bin/true\n",
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let started = Instant::now();
        let result = probe_provider(executable.to_str().unwrap(), Duration::from_millis(100));
        assert!(
            !result.available,
            "an incomplete probe must not claim availability"
        );
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn doctor_probe_accepts_complete_success_and_rejects_failure() {
        let root = tempfile::tempdir().unwrap();
        let executable = root.path().join("provider-fixture");
        for (body, available) in [
            ("printf '2.1.228\\n'", true),
            ("printf rejected >&2; exit 7", false),
        ] {
            fs::write(&executable, format!("#!/bin/sh\n{body}\n")).unwrap();
            fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
            // This checks success/failure semantics, not a tighter startup SLA
            // than production. The separate inherited-pipe test keeps its
            // strict 100 ms deadline even under the parallel suite.
            let result =
                probe_provider_result(executable.to_str().unwrap(), PROVIDER_PROBE_TIMEOUT);
            assert_eq!(
                result.is_ok(),
                available,
                "unexpected probe outcome: {result:?}"
            );
            if available {
                let result = result.unwrap();
                assert!(result.available);
                assert_eq!(result.version.as_deref(), Some("2.1.228"));
            } else {
                assert!(result.unwrap_err().to_string().contains("exit status: 7"));
            }
        }
    }
}
