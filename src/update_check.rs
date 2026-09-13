//! Advisory release checks only: no installer, provider calls, or agent state writes.
use crate::{consult::CancellationToken, fleet, store::Store, update};
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    cmp::Ordering,
    process::Command,
    sync::mpsc::{self, Receiver},
    thread::{self, JoinHandle},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const SUCCESS_TTL: f64 = 6.0 * 3600.0;
const FAILURE_TTL: f64 = 3600.0;
const LEASE_TTL: f64 = 30.0;

#[derive(Default, Serialize, Deserialize)]
struct Cache {
    checked_at: f64,
    latest: Option<String>,
    failed: bool,
    lease: Option<String>,
}

impl Cache {
    fn read(encoded: Option<String>) -> Self {
        encoded
            .filter(|value| value.len() <= 4096)
            .and_then(|value| serde_json::from_str(&value).ok())
            .unwrap_or_default()
    }

    fn fresh(&self, now: f64) -> bool {
        let ttl = if self.lease.is_some() {
            LEASE_TTL
        } else if self.failed {
            FAILURE_TTL
        } else {
            SUCCESS_TTL
        };
        self.checked_at > 0.0 && (0.0..ttl).contains(&(now - self.checked_at))
    }

    fn notice(&self, current: &str, now: f64) -> Option<String> {
        self.latest
            .as_ref()
            .filter(|latest| {
                self.fresh(now)
                    && !self.failed
                    && self.lease.is_none()
                    && update::compare_versions(latest, current).ok() == Some(Ordering::Greater)
            })
            .cloned()
    }
}

fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

fn enabled(value: Option<&str>) -> bool {
    !value.is_some_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off"
        )
    })
}

/// Own the subprocess through shutdown; never leave a check running after quit.
pub(crate) struct Checker {
    cancel: CancellationToken,
    worker: Option<JoinHandle<()>>,
}

impl Drop for Checker {
    fn drop(&mut self) {
        self.cancel.cancel();
        if let Some(worker) = self.worker.take() {
            worker.thread().unpark();
            let _ = worker.join();
        }
    }
}

pub(crate) fn start(board_store: &Store) -> (Checker, Receiver<Option<String>>) {
    let (sender, receiver) = mpsc::sync_channel(1);
    let cancel = CancellationToken::default();
    let mut checker = Checker {
        cancel: cancel.clone(),
        worker: None,
    };
    if !enabled(std::env::var("PIKA_UPDATE_CHECK").ok().as_deref()) {
        return (checker, receiver);
    }
    // Separate database: advisory metadata cannot invalidate an agent observation.
    let cache = Store::at(board_store.path().with_file_name("update-check.db"));
    checker.worker = Some(thread::spawn(move || {
        let Ok(target) = update::native_target() else {
            return;
        };
        if cache.initialize().is_err() {
            return;
        }
        let key = format!("release:{}:{target}", crate::VERSION);
        while !cancel.is_cancelled() {
            let result = check(&cache, &key, crate::VERSION, target, now(), || {
                fetch(&cancel)
            });
            if let Ok(notice) = result {
                let _ = sender.try_send(notice);
            }
            if cancel.is_cancelled() {
                break;
            }
            // Frequent cache reads are local only. Cross-board leases bound HTTP work.
            thread::park_timeout(Duration::from_secs(60));
        }
    }));
    (checker, receiver)
}

fn check(
    store: &Store,
    key: &str,
    current: &str,
    target: &str,
    timestamp: f64,
    fetch: impl FnOnce() -> Result<Vec<u8>>,
) -> Result<Option<String>> {
    let cached = Cache::read(store.get_meta(key)?);
    if cached.fresh(timestamp) {
        return Ok(cached.notice(current, timestamp));
    }
    let token = uuid::Uuid::new_v4().to_string();
    let claimed = store.reconcile_transaction(|ledger| {
        if Cache::read(ledger.get_meta(key)?).fresh(timestamp) {
            return Ok(false);
        }
        ledger.set_meta(
            key,
            &serde_json::to_string(&Cache {
                checked_at: timestamp,
                lease: Some(token.clone()),
                ..Cache::default()
            })?,
        )?;
        Ok(true)
    })?;
    if !claimed {
        return Ok(None);
    }
    let result = fetch().and_then(|bytes| {
        update::select_latest_release(&bytes, current, target).map_err(Into::into)
    });
    let finished = Cache {
        checked_at: timestamp,
        failed: result.is_err(),
        latest: result.unwrap_or_default(),
        lease: None,
    };
    store.reconcile_transaction(|ledger| {
        // An expired/replaced checker cannot overwrite a newer result.
        if Cache::read(ledger.get_meta(key)?).lease.as_deref() != Some(&token) {
            return Ok(None);
        }
        ledger.set_meta(key, &serde_json::to_string(&finished)?)?;
        Ok(finished.notice(current, timestamp))
    })
}

fn fetch(cancel: &CancellationToken) -> Result<Vec<u8>> {
    let mut command = Command::new(if cfg!(windows) { "curl.exe" } else { "curl" });
    command.args([
        "--disable",
        "--fail",
        "--silent",
        "--show-error",
        "--location",
        "--proto",
        "=https",
        "--proto-redir",
        "=https",
        "--connect-timeout",
        "2",
        "--max-time",
        "5",
        "--user-agent",
        concat!("pika/", env!("CARGO_PKG_VERSION")),
        "--header",
        "Accept: application/vnd.github+json",
        update::RELEASE_API,
    ]);
    let output = fleet::run_bounded_command_cancellable(
        &mut command,
        None,
        Duration::from_secs(6),
        update::MAX_RELEASE_LIST_BYTES as usize,
        4096,
        cancel,
    )?;
    if !output.status.success() {
        bail!("Release check unavailable");
    }
    Ok(output.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::{
        Arc, Barrier,
        atomic::{AtomicUsize, Ordering as AtomicOrdering},
    };

    const TARGET: &str = "x86_64-pc-windows-msvc";
    const TIME: f64 = 100_000.0;

    fn fixture() -> (tempfile::TempDir, Store) {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::at(directory.path().join("update-check.db"));
        store.initialize().unwrap();
        (directory, store)
    }

    fn releases(version: &str) -> Vec<u8> {
        let archive = update::artifact_name(version, TARGET).unwrap();
        serde_json::to_vec(&json!([{
            "tag_name": format!("v{version}"), "draft": false, "prerelease": version.contains('-'),
            "assets": [
                {"name": update::NATIVE_MANIFEST_FILE, "state": "uploaded"},
                {"name": archive, "state": "uploaded"},
                {"name": format!("{archive}.sha256"), "state": "uploaded"}
            ]
        }]))
        .unwrap()
    }

    #[test]
    fn newer_release_cached_across_board_restarts_and_rechecked_after_six_hours() {
        let (_directory, store) = fixture();
        let result = check(&store, "key", "0.6.7", TARGET, TIME, || {
            Ok(releases("0.6.8"))
        })
        .unwrap();
        assert_eq!(result.as_deref(), Some("0.6.8"));
        let reopened = Store::at(store.path());
        assert_eq!(
            check(&reopened, "key", "0.6.7", TARGET, TIME + 30.0, || panic!(
                "cached"
            ))
            .unwrap(),
            result
        );
        assert_eq!(
            check(&store, "key", "0.6.7", TARGET, TIME + SUCCESS_TTL, || Ok(
                releases("0.6.9")
            ))
            .unwrap()
            .as_deref(),
            Some("0.6.9")
        );
    }

    #[test]
    fn failures_are_quiet_and_back_off_for_an_hour() {
        let (_directory, store) = fixture();
        assert_eq!(
            check(&store, "key", "0.6.7", TARGET, TIME, || bail!("offline")).unwrap(),
            None
        );
        assert_eq!(
            check(&store, "key", "0.6.7", TARGET, TIME + 30.0, || panic!(
                "failure cached"
            ))
            .unwrap(),
            None
        );
        assert_eq!(
            check(&store, "key", "0.6.7", TARGET, TIME + FAILURE_TTL, || Ok(
                releases("0.6.8")
            ))
            .unwrap()
            .as_deref(),
            Some("0.6.8")
        );
    }

    #[test]
    fn no_notice_for_current_older_preview_or_incomplete_release() {
        for version in ["0.6.7", "0.6.6", "0.6.8-rc.1"] {
            let (_directory, store) = fixture();
            assert_eq!(
                check(&store, "key", "0.6.7", TARGET, TIME, || Ok(releases(
                    version
                )))
                .unwrap(),
                None
            );
        }
        let (_directory, store) = fixture();
        assert_eq!(
            check(&store, "key", "0.6.7", TARGET, TIME, || Ok(
                br#"[{"tag_name":"v9.0.0","assets":[]}]"#.to_vec()
            ))
            .unwrap(),
            None
        );
        assert_eq!(
            check(&store, "key", "0.6.7", TARGET, TIME + 1.0, || panic!(
                "up-to-date cached"
            ))
            .unwrap(),
            None
        );
    }

    #[test]
    fn corrupt_cache_and_clock_rollback_recover() {
        let (_directory, store) = fixture();
        for bad in [
            "not json".to_owned(),
            "x".repeat(4097),
            serde_json::to_string(&Cache {
                checked_at: TIME + 50.0,
                ..Cache::default()
            })
            .unwrap(),
        ] {
            store.set_meta("key", &bad).unwrap();
            assert_eq!(
                check(&store, "key", "0.6.7", TARGET, TIME, || Ok(releases(
                    "0.6.8"
                )))
                .unwrap()
                .as_deref(),
                Some("0.6.8")
            );
        }
    }

    #[test]
    fn concurrent_boards_make_one_request() {
        let (_directory, store) = fixture();
        let gate = Arc::new(Barrier::new(5));
        let requests = Arc::new(AtomicUsize::new(0));
        let workers = (0..4)
            .map(|_| {
                let (store, gate, requests) = (store.clone(), gate.clone(), requests.clone());
                thread::spawn(move || {
                    gate.wait();
                    check(&store, "key", "0.6.7", TARGET, TIME, || {
                        requests.fetch_add(1, AtomicOrdering::SeqCst);
                        Ok(releases("0.6.8"))
                    })
                    .unwrap();
                })
            })
            .collect::<Vec<_>>();
        gate.wait();
        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(requests.load(AtomicOrdering::SeqCst), 1);
    }

    #[test]
    fn abandoned_lease_expires_and_late_response_cannot_replace_newer_result() {
        let (_directory, store) = fixture();
        let result = check(&store, "key", "0.6.7", TARGET, TIME, || {
            assert_eq!(
                check(&store, "key", "0.6.7", TARGET, TIME + 1.0, || panic!(
                    "leased"
                ))
                .unwrap(),
                None
            );
            assert_eq!(
                check(&store, "key", "0.6.7", TARGET, TIME + LEASE_TTL, || Ok(
                    releases("0.6.9")
                ))
                .unwrap()
                .as_deref(),
                Some("0.6.9")
            );
            Ok(releases("0.6.8"))
        })
        .unwrap();
        assert_eq!(result, None);
        assert_eq!(
            check(
                &store,
                "key",
                "0.6.7",
                TARGET,
                TIME + LEASE_TTL + 1.0,
                || panic!("cached")
            )
            .unwrap()
            .as_deref(),
            Some("0.6.9")
        );
    }

    #[test]
    fn opt_out_and_version_scoped_cache() {
        for value in ["0", "false", "OFF", " False "] {
            assert!(!enabled(Some(value)));
        }
        assert!(enabled(None));
        assert!(enabled(Some("1")));
        let (_directory, store) = fixture();
        check(
            &store,
            "release:0.6.7:windows",
            "0.6.7",
            TARGET,
            TIME,
            || Ok(releases("0.6.8")),
        )
        .unwrap();
        assert_eq!(
            check(
                &store,
                "release:0.6.8:windows",
                "0.6.8",
                TARGET,
                TIME,
                || Ok(releases("0.6.8"))
            )
            .unwrap(),
            None
        );
        assert!(store.list_sessions().unwrap().is_empty());
    }

    #[test]
    fn quitting_cancels_and_joins_the_parked_worker() {
        let cancel = CancellationToken::default();
        let worker_cancel = cancel.clone();
        let (started, ready) = mpsc::sync_channel(1);
        let worker = thread::spawn(move || {
            started.send(()).unwrap();
            while !worker_cancel.is_cancelled() {
                thread::park_timeout(Duration::from_secs(60));
            }
        });
        ready.recv_timeout(Duration::from_secs(1)).unwrap();
        let checker = Checker {
            cancel: cancel.clone(),
            worker: Some(worker),
        };
        let begin = std::time::Instant::now();
        drop(checker);
        assert!(cancel.is_cancelled());
        assert!(begin.elapsed() < Duration::from_secs(1));
    }
}
