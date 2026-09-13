//! Read-only, machine-scoped subscription telemetry. Never starts a model turn.
use crate::{
    consult::CancellationToken,
    expert_refresh::{QuotaSnapshot, SystemQuotaSource},
    fleet::{self, FleetTransport, SshTransport},
    model::Provider,
    monitor::{LatestReceiver, LatestSender, latest_channel},
    store::Store,
};
use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub(crate) const PROVIDERS: [Provider; 2] = [Provider::Codex, Provider::Claude];
const REFRESH: Duration = Duration::from_secs(120);

pub(crate) fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

#[derive(Clone, Debug, Default)]
pub(crate) struct View {
    pub node_id: Option<String>,
    pub readings: Vec<QuotaSnapshot>,
    pub delayed: bool,
    pub unavailable: Option<String>,
}

pub(crate) struct Feed {
    focus: Arc<Mutex<Option<String>>>,
    wake: thread::Thread,
    pub updates: LatestReceiver<View>,
}

impl Feed {
    pub fn select(&self, node_id: Option<String>) {
        if let Ok(mut focus) = self.focus.lock()
            && *focus != node_id
        {
            *focus = node_id;
            self.wake.unpark();
        }
    }
}

pub(crate) struct Worker {
    cancel: CancellationToken,
    worker: Option<JoinHandle<()>>,
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.cancel.cancel();
        if let Some(worker) = self.worker.take() {
            worker.thread().unpark();
            let _ = worker.join();
        }
    }
}

pub(crate) fn start(store: Store, local: Option<SystemQuotaSource>) -> (Worker, Feed) {
    let cancel = CancellationToken::default();
    let worker_cancel = cancel.clone();
    let focus = Arc::new(Mutex::new(None));
    let worker_focus = focus.clone();
    let (send, updates) = latest_channel();
    let worker =
        std::thread::spawn(move || observe(store, local, worker_focus, send, worker_cancel));
    let wake = worker.thread().clone();
    (
        Worker {
            cancel,
            worker: Some(worker),
        },
        Feed {
            focus,
            wake,
            updates,
        },
    )
}

fn observe(
    store: Store,
    local: Option<SystemQuotaSource>,
    focus: Arc<Mutex<Option<String>>>,
    send: LatestSender<View>,
    cancel: CancellationToken,
) {
    let transport = SshTransport::new(
        if cfg!(windows) { "ssh.exe" } else { "ssh" },
        Duration::from_secs(3),
        Duration::from_secs(7),
    );
    let mut cache: BTreeMap<Option<String>, (Instant, View)> = BTreeMap::new();
    let mut published: Option<Option<String>> = None;
    while !cancel.is_cancelled() {
        let selected = focus.lock().map(|v| v.clone()).unwrap_or_default();
        if let Some((_, view)) = cache.get(&selected)
            && published.as_ref() != Some(&selected)
        {
            send.publish(view.clone());
        }
        let due = cache
            .get(&selected)
            .is_none_or(|(at, _)| at.elapsed() >= REFRESH);
        if due {
            let result = match &selected {
                None => Ok(local
                    .as_ref()
                    .map(|source| read_local(source, &cancel))
                    .unwrap_or_default()),
                Some(node_id) => read_remote(&store, &transport, node_id, &cancel),
            };
            let mut view = View {
                node_id: selected.clone(),
                ..View::default()
            };
            match result {
                Ok(readings) => view.readings = readings,
                Err(_) => {
                    view.readings = cache
                        .get(&selected)
                        .map(|(_, view)| view.readings.clone())
                        .unwrap_or_default();
                    view.delayed = true;
                    view.unavailable = Some("unavailable on host".into());
                }
            }
            if selected.is_none() && local.is_none() {
                view.unavailable = Some("not available on this device".into());
            }
            if cancel.is_cancelled() {
                break;
            }
            // A lookup finishing after the user moves never labels another host's quota.
            send.publish(view.clone());
            if cache.len() >= 64 && !cache.contains_key(&selected) {
                cache.clear();
            }
            cache.insert(selected.clone(), (Instant::now(), view));
        }
        published = Some(selected.clone());
        if focus.lock().is_ok_and(|current| *current == selected) {
            let wait = cache
                .get(&selected)
                .map(|(at, _)| REFRESH.saturating_sub(at.elapsed()))
                .unwrap_or(REFRESH);
            thread::park_timeout(wait);
        }
    }
}

pub(crate) fn read_local(
    source: &SystemQuotaSource,
    cancel: &CancellationToken,
) -> Vec<QuotaSnapshot> {
    PROVIDERS
        .into_iter()
        .filter_map(|provider| {
            if cancel.is_cancelled() {
                return None;
            }
            source
                .snapshot_cancellable(provider, now(), cancel)
                .ok()
                .flatten()
        })
        .collect()
}

fn read_remote(
    store: &Store,
    transport: &impl FleetTransport,
    node_id: &str,
    cancel: &CancellationToken,
) -> Result<Vec<QuotaSnapshot>> {
    let node = store
        .get_fleet_node(node_id)?
        .ok_or_else(|| anyhow::anyhow!("Machine no longer paired"))?;
    if node.status == "quarantined" || !node.capabilities.iter().any(|v| v == "quota-v1") {
        bail!("Host does not advertise quota support");
    }
    let value = transport.request_cancellable(
        &node.ssh_target,
        &json!({
            "op":"quota", "protocol":fleet::PROTOCOL_NAME, "version":fleet::PROTOCOL_VERSION,
            "expected_node_id":node_id,
        }),
        false,
        cancel,
    )?;
    if store.get_fleet_node(node_id)?.is_none_or(|current| {
        current.ssh_target != node.ssh_target || current.status == "quarantined"
    }) {
        bail!("Machine pairing changed during quota read");
    }
    validate_remote(&value, node_id, now())
}

fn validate_remote(value: &Value, expected: &str, at: f64) -> Result<Vec<QuotaSnapshot>> {
    let object = value
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("Invalid quota reply"))?;
    if object.len() != 3
        || value.get("type").and_then(Value::as_str) != Some("quota")
        || value.get("node_id").and_then(Value::as_str) != Some(expected)
    {
        bail!("Quota identity mismatch");
    }
    let values = value
        .get("readings")
        .and_then(Value::as_array)
        .filter(|v| v.len() <= 2)
        .ok_or_else(|| anyhow::anyhow!("Invalid quota readings"))?;
    let mut readings: Vec<QuotaSnapshot> = Vec::new();
    for value in values {
        let row: QuotaSnapshot = serde_json::from_value(value.clone())?;
        if !valid(&row, at) || readings.iter().any(|v| v.provider == row.provider) {
            bail!("Invalid quota reading");
        }
        readings.push(row);
    }
    Ok(readings)
}

fn valid(row: &QuotaSnapshot, at: f64) -> bool {
    PROVIDERS.contains(&row.provider)
        && row.used_percent.is_finite()
        && (0.0..=100.0).contains(&row.used_percent)
        && row.observed_at.is_finite()
        && row.observed_at > 0.0
        && row.observed_at <= at + 60.0
        && row.reset_at > 0
        && row.reset_at as f64 <= at + 8.0 * 86400.0
        && row.source.len() <= 128
        && !row.source.chars().any(char::is_control)
}

/// Convert at the display host, including DST at the reset instant, not on the remote server.
pub(crate) fn reset_label(timestamp: i64) -> String {
    let stamp = timestamp as libc::time_t;
    let mut tm = std::mem::MaybeUninit::<libc::tm>::uninit();
    // SAFETY: libc receives valid pointers to a timestamp and writable tm; the
    // reentrant variants initialize tm only on success. Neither pointer escapes.
    let success = unsafe {
        #[cfg(unix)]
        {
            !libc::localtime_r(&stamp, tm.as_mut_ptr()).is_null()
        }
        #[cfg(windows)]
        {
            libc::localtime_s(tm.as_mut_ptr(), &stamp) == 0
        }
    };
    if !success {
        return "time unavailable".into();
    }
    // SAFETY: the successful libc call above initialized the complete structure.
    let tm = unsafe { tm.assume_init() };
    let months = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let month = months.get(tm.tm_mon as usize).unwrap_or(&"?");
    format!("{month} {} {:02}:{:02}", tm.tm_mday, tm.tm_hour, tm.tm_min)
}

pub(crate) fn row(provider: Provider, view: &View, at: f64, bars: bool) -> String {
    let name = if provider == Provider::Codex {
        "Codex "
    } else {
        "Claude"
    };
    let Some(reading) = view
        .readings
        .iter()
        .find(|v| v.provider == provider)
        .filter(|v| valid(v, at))
    else {
        return format!(
            "{name} — {}",
            view.unavailable.as_deref().unwrap_or("awaiting usage")
        );
    };
    let age = (at - reading.observed_at).max(0.0);
    let stale = view.delayed || age > 300.0;
    let freshness = if stale {
        format!(" · stale {}m", (age / 60.0).floor() as u64)
    } else {
        format!(" · seen {}m", (age / 60.0).floor() as u64)
    };
    if reading.reset_at as f64 <= at {
        return format!("{name} — awaiting reset reading{freshness}");
    }
    let remaining = reading.remaining_percent();
    let bar = if bars {
        let filled = (remaining / 10.0).round().clamp(0.0, 10.0) as usize;
        format!("{}{} ", "█".repeat(filled), "░".repeat(10 - filled))
    } else {
        String::new()
    };
    let percent = if remaining > 0.0 && remaining < 1.0 {
        "<1".to_owned()
    } else {
        format!("{:.0}", remaining.floor())
    };
    format!(
        "{name} {bar}{percent}% · ↻ {}{freshness}",
        reset_label(reading.reset_at)
    )
}

pub(crate) fn compact_row(provider: Provider, view: &View, at: f64) -> String {
    let full = row(provider, view, at, false);
    let Some((head, _)) = full.split_once(" · ↻") else {
        return format!(
            "{} —",
            if provider == Provider::Codex {
                "Codex"
            } else {
                "Claude"
            }
        );
    };
    format!(
        "{head}{}",
        if full.contains(" · stale ") {
            " stale"
        } else {
            ""
        }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    const AT: f64 = 1_800_000_000.0;

    fn reading(used: f64) -> QuotaSnapshot {
        QuotaSnapshot {
            provider: Provider::Codex,
            used_percent: used,
            reset_at: AT as i64 + 86400,
            observed_at: AT,
            source: "account RPC".into(),
        }
    }

    #[test]
    fn quota_bar_represents_remaining_not_used_and_distinguishes_unknown_zero_and_tiny() {
        let mut view = View {
            readings: vec![reading(37.0)],
            ..View::default()
        };
        let rendered = row(Provider::Codex, &view, AT, true);
        assert!(rendered.contains("██████░░░░ 63%"));
        assert!(rendered.contains("seen 0m"));
        assert!(row(Provider::Claude, &view, AT, true).contains("— awaiting usage"));
        view.readings[0].used_percent = 100.0;
        assert!(row(Provider::Codex, &view, AT, true).contains("░░░░░░░░░░ 0%"));
        view.readings[0].used_percent = 99.9;
        assert!(row(Provider::Codex, &view, AT, true).contains("<1%"));
    }

    #[test]
    fn stale_and_reset_crossing_never_imply_new_charge() {
        let view = View {
            readings: vec![reading(37.0)],
            ..View::default()
        };
        assert!(row(Provider::Codex, &view, AT + 600.0, true).contains("stale 10m"));
        assert!(compact_row(Provider::Codex, &view, AT + 600.0).contains("63% stale"));
        let reset = row(Provider::Codex, &view, AT + 86400.0, true);
        assert!(reset.contains("awaiting reset reading"));
        assert!(!reset.contains("63%") && !reset.contains("100%"));
    }

    #[test]
    fn quota_reply_requires_exact_node_and_bounded_valid_unique_readings() {
        let valid_reply = json!({"type":"quota", "node_id":"node-a", "readings":[reading(37.0)]});
        assert_eq!(
            validate_remote(&valid_reply, "node-a", AT).unwrap().len(),
            1
        );
        assert!(validate_remote(&valid_reply, "node-b", AT).is_err());
        for used in [-1.0, 101.0] {
            let bad = json!({"type":"quota", "node_id":"node-a", "readings":[reading(used)]});
            assert!(validate_remote(&bad, "node-a", AT).is_err());
        }
        let mut bad = valid_reply.clone();
        bad["readings"][0]["source"] = json!("\u{1b}[31m");
        assert!(validate_remote(&bad, "node-a", AT).is_err());
        bad = valid_reply.clone();
        bad["readings"][0]["observed_at"] = json!(AT + 61.0);
        assert!(validate_remote(&bad, "node-a", AT).is_err());
        bad = valid_reply;
        bad["readings"]
            .as_array_mut()
            .unwrap()
            .push(json!(reading(20.0)));
        assert!(validate_remote(&bad, "node-a", AT).is_err());
    }

    #[test]
    fn missing_provider_is_unavailable_and_failed_fetch_marks_old_reading_stale() {
        let view = View {
            readings: vec![reading(37.0)],
            delayed: true,
            ..View::default()
        };
        assert!(row(Provider::Codex, &view, AT, true).contains("stale 0m"));
        assert!(!row(Provider::Claude, &view, AT, true).contains('%'));
    }

    #[test]
    fn observer_without_local_providers_starts_and_stops_without_network_or_agent_state() {
        let root = tempfile::tempdir().unwrap();
        let store = Store::at(root.path().join("test.db"));
        store.initialize().unwrap();
        let (worker, feed) = start(store.clone(), None);
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            if let Some(view) = feed.updates.take() {
                assert!(view.readings.is_empty());
                break;
            }
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        }
        let at = Instant::now();
        drop(worker);
        assert!(at.elapsed() < Duration::from_secs(1));
        assert!(store.list_sessions().unwrap().is_empty());
    }

    #[test]
    fn local_reset_label_has_month_day_and_time() {
        let label = reset_label(AT as i64);
        assert!(label.contains(':'));
        assert_eq!(label.split_whitespace().count(), 3);
    }

    #[test]
    fn remote_quota_uses_only_paired_route_and_rejects_changed_identity() {
        use crate::{
            fleet::{CAPABILITIES, FleetError},
            model::FleetNode,
        };
        struct Fake {
            expected: String,
            response: Value,
            calls: Mutex<usize>,
        }
        impl FleetTransport for Fake {
            fn request(
                &self,
                target: &str,
                payload: &Value,
                mutating: bool,
            ) -> std::result::Result<Value, FleetError> {
                assert_eq!(target, "fixture-ssh");
                assert_eq!(payload["op"], "quota");
                assert_eq!(payload["expected_node_id"], self.expected);
                assert!(!mutating);
                *self.calls.lock().unwrap() += 1;
                Ok(self.response.clone())
            }
            fn run_exact(
                &self,
                _: &FleetNode,
                _: &[String],
                _: bool,
            ) -> std::result::Result<i32, FleetError> {
                panic!("Quota must not open an agent")
            }
        }
        let root = tempfile::tempdir().unwrap();
        let store = Store::at(root.path().join("state.db"));
        store.initialize().unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let mut node = FleetNode {
            node_id: id.clone(),
            alias: "fixture".into(),
            ssh_target: "fixture-ssh".into(),
            sources: vec!["explicit".into()],
            status: "ready".into(),
            protocol_version: Some(fleet::PROTOCOL_VERSION),
            package_version: Some(crate::VERSION.into()),
            capabilities: CAPABILITIES.iter().map(|v| (*v).into()).collect(),
            last_seen: 0.0,
            last_attempt_at: 0.0,
            last_error: None,
            created_at: 1.0,
            updated_at: 1.0,
        };
        store.upsert_fleet_node(&node).unwrap();
        let mut sample = reading(37.0);
        sample.observed_at = now();
        sample.reset_at = now() as i64 + 86400;
        let mut transport = Fake {
            expected: id.clone(),
            response: json!({"type":"quota", "node_id":id, "readings":[sample]}),
            calls: Mutex::new(0),
        };
        let cancel = CancellationToken::default();
        assert_eq!(
            read_remote(&store, &transport, &id, &cancel).unwrap()[0].remaining_percent(),
            63.0
        );
        transport.response["node_id"] = json!(uuid::Uuid::new_v4().to_string());
        assert!(read_remote(&store, &transport, &id, &cancel).is_err());
        node.capabilities.retain(|v| v != "quota-v1");
        store.upsert_fleet_node(&node).unwrap();
        assert!(read_remote(&store, &transport, &id, &cancel).is_err());
        assert_eq!(
            *transport.calls.lock().unwrap(),
            2,
            "Old hosts must not receive unsupported requests"
        );
        assert!(store.list_sessions().unwrap().is_empty());
    }
}
