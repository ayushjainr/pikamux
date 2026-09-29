//! Opt-in measurement of the actual private assistant binary/IPC dispatcher.
//! No provider is enabled. Results describe this metadata-only workload, not
//! model throughput, the rendered board, or fifty supported simultaneous views.
#![cfg(unix)]

use pikamux::assistant_policy::AssistantPolicy;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    io::{self, BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{Arc, Barrier},
    thread,
    time::{Duration, Instant},
};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(9);
type Client = BufReader<UnixStream>;

struct Host {
    directory: tempfile::TempDir,
    root: PathBuf,
    child: Child,
}

impl Host {
    fn start() -> (Self, Client) {
        let directory = tempfile::tempdir().unwrap();
        let base = directory.path();
        for name in [
            "home", "config", "state", "data", "cache", "tmp", "tmux", "codex", "claude",
            "opencode", "muse",
        ] {
            fs::create_dir_all(base.join(name)).unwrap();
        }
        let root = base.join("state/assistant");
        // Initialize exactly this disposable policy before startup. No allowance.
        AssistantPolicy::open(root.join("policy.sqlite")).unwrap();
        let stderr = fs::File::create(base.join("host-stderr.txt")).unwrap();
        let mut command = Command::new(assert_cmd::cargo::cargo_bin("pika"));
        command.env_clear().current_dir(base.join("home"));
        for (name, path) in [
            ("HOME", "home"),
            ("XDG_CONFIG_HOME", "config"),
            ("XDG_STATE_HOME", "state"),
            ("XDG_DATA_HOME", "data"),
            ("XDG_CACHE_HOME", "cache"),
            ("TMPDIR", "tmp"),
            ("TMUX_TMPDIR", "tmux"),
            ("PIKA_CONFIG_HOME", "config/pika"),
            ("PIKA_STATE_HOME", "state"),
            ("PIKA_DB_PATH", "state/operational.sqlite"),
            ("CODEX_HOME", "codex"),
            ("CLAUDE_CONFIG_DIR", "claude"),
            ("OPENCODE_DATA_HOME", "opencode"),
            ("OPENCODE_CONFIG_DIR", "config/opencode"),
            ("MUSE_DATA_HOME", "muse"),
            ("MUSE_CONFIG_DIR", "config/muse"),
        ] {
            command.env(name, base.join(path));
        }
        command
            .env("PATH", "/usr/bin:/bin")
            .env("PIKA_UPDATE_CHECK", "0")
            .env("PIKA_TMUX_SOCKET", "assistant-contention-disposable")
            .args(["_assistant-host", "--root"])
            .arg(&root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(stderr);
        let child = command.spawn().unwrap();
        let mut host = Self {
            directory,
            root,
            child,
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(client) = host.connect() {
                return (host, client);
            }
            if let Some(status) = host.child.try_wait().unwrap() {
                panic!("private host startup failed ({status}): {}", host.stderr());
            }
            assert!(
                Instant::now() < deadline,
                "private host endpoint did not open"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn connect(&self) -> io::Result<Client> {
        let stream = UnixStream::connect(self.root.join("view.sock"))?;
        stream.set_read_timeout(Some(REQUEST_TIMEOUT))?;
        stream.set_write_timeout(Some(REQUEST_TIMEOUT))?;
        Ok(BufReader::new(stream))
    }

    fn stderr(&self) -> String {
        fs::read_to_string(self.directory.path().join("host-stderr.txt")).unwrap()
    }

    fn accepted_views(&self, first: Client, count: usize) -> Vec<Client> {
        let mut clients = vec![first];
        for _ in 1..count {
            clients.push(self.connect().unwrap());
        }
        // Warm each connection: all 32 are actually admitted before timing.
        for client in &mut clients {
            let reply = exchange(client, snapshot()).unwrap();
            assert!(reply["error"].is_null(), "warm-up failed: {reply}");
        }
        clients
    }

    fn assert_no_provider(&self) {
        for name in ["provider-home", "runtime.sqlite", "author-runtime.sqlite"] {
            assert!(
                !self.root.join(name).exists(),
                "unexpected provider artifact {name}"
            );
        }
        assert!(
            !self
                .directory
                .path()
                .join("state/operational.sqlite")
                .exists()
        );
        let db = rusqlite::Connection::open(self.root.join("policy.sqlite")).unwrap();
        let reserved: i64 = db
            .query_row("SELECT COUNT(*) FROM assistant_reservations", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            reserved, 0,
            "measurement must not reserve or spend model calls"
        );
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

fn snapshot() -> Value {
    json!({"operation":"snapshot","scope":"contention-fixture"})
}

fn exchange(client: &mut Client, payload: Value) -> io::Result<Value> {
    let mut bytes = serde_json::to_vec(&json!({"protocol":1,"generation":null,"payload":payload}))?;
    bytes.push(b'\n');
    client.get_mut().write_all(&bytes)?;
    let mut reply = String::new();
    if client.read_line(&mut reply)? == 0 {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "host disconnected",
        ));
    }
    serde_json::from_str(&reply).map_err(io::Error::other)
}

struct Sample {
    operation: &'static str,
    elapsed_ms: f64,
    outcome: &'static str,
    detail: Option<String>,
}

fn burst(clients: Vec<Client>, include_save: bool) -> (Vec<Client>, Vec<Sample>) {
    let barrier = Arc::new(Barrier::new(clients.len() + 1));
    let workers = clients.into_iter().enumerate().map(|(index, mut client)| {
        let barrier = barrier.clone();
        thread::spawn(move || {
            let (operation, payload) = if include_save && index == 0 {
                ("save", json!({"operation":"save","scope":"contention-fixture","request_id":"contended-save","kind":"draft","body":"synthetic unsent fixture","timestamp":1}))
            } else if index == usize::from(include_save) {
                ("cancel", json!({"operation":"cancel"}))
            } else { ("snapshot", snapshot()) };
            barrier.wait();
            let start = Instant::now();
            let reply = exchange(&mut client, payload);
            let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
            let (outcome, detail) = match reply {
                Ok(value) if value["error"].is_null() => ("ok", None),
                Ok(value) => ("operation_error", Some(value["error"].to_string())),
                Err(error) if matches!(error.kind(), io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock) => ("timeout", Some(error.to_string())),
                Err(error) => ("io_error", Some(error.to_string())),
            };
            (client, Sample { operation, elapsed_ms, outcome, detail })
        })
    }).collect::<Vec<_>>();
    barrier.wait();
    workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .unzip()
}

fn distribution(samples: &[&Sample]) -> Value {
    let mut times = samples.iter().map(|s| s.elapsed_ms).collect::<Vec<_>>();
    times.sort_by(f64::total_cmp);
    let percentile = |p: f64| -> Option<f64> {
        if times.is_empty() {
            return None;
        }
        Some(times[((times.len() as f64 * p).ceil() as usize).saturating_sub(1)])
    };
    let mut outcomes = BTreeMap::new();
    for sample in samples {
        *outcomes.entry(sample.outcome).or_insert(0) += 1;
    }
    json!({"count":samples.len(),"p50_ms":percentile(0.50),"p95_ms":percentile(0.95),"max_ms":times.last(),"outcomes":outcomes})
}

fn report(name: &str, samples: &[Sample], held_ms: Option<f64>, exited: bool, stderr: String) {
    let all = samples.iter().collect::<Vec<_>>();
    let successful = samples
        .iter()
        .filter(|s| s.outcome == "ok")
        .collect::<Vec<_>>();
    let by_operation = ["snapshot", "cancel", "save"]
        .into_iter()
        .map(|operation| {
            (
                operation,
                distribution(
                    &samples
                        .iter()
                        .filter(|s| s.operation == operation)
                        .collect::<Vec<_>>(),
                ),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let errors = samples
        .iter()
        .filter_map(|s| s.detail.as_deref())
        .collect::<std::collections::BTreeSet<_>>();
    println!(
        "ASSISTANT_CONTENTION {}",
        json!({
        "scenario":name,"workload":"real binary IPC, provider disabled, synthetic metadata",
        "cancel_metric":"IPC acceptance latency, not cancellation of a running provider",
        "sampling":"one synchronized burst; not a steady-state throughput benchmark",
            "max_supported_views":32,"request_timeout_ms":REQUEST_TIMEOUT.as_millis(),
            "all_outcomes":distribution(&all),"successful_only":distribution(&successful),
            "by_operation":by_operation,"held_writer_ms":held_ms,
            "host_exited_while_views_open":exited,"errors":errors,"host_stderr":stderr,
        })
    );
}

fn scenario(name: &str, lock: Option<(&str, Duration, Duration)>, include_save: bool) {
    let (mut host, first) = Host::start();
    let clients = host.accepted_views(first, 32);
    let holder = lock.map(|(filename, hold, lead)| {
        let db = rusqlite::Connection::open(host.root.join(filename)).unwrap();
        db.busy_timeout(Duration::from_secs(2)).unwrap();
        db.execute_batch("BEGIN IMMEDIATE").unwrap();
        let holder = thread::spawn(move || {
            let start = Instant::now();
            thread::sleep(hold);
            db.execute_batch("ROLLBACK").unwrap();
            start.elapsed().as_secs_f64() * 1000.0
        });
        thread::sleep(lead);
        holder
    });
    let (clients, samples) = burst(clients, include_save);
    let held_ms = holder.map(|h| h.join().unwrap());
    let exited = host.child.try_wait().unwrap().is_some();
    report(name, &samples, held_ms, exited, host.stderr());
    if lock.is_none() {
        assert!(
            samples.iter().all(|s| s.outcome == "ok"),
            "uncontended baseline failed"
        );
        assert!(!exited);
    }
    if name.starts_with("policy_writer_") {
        assert!(!exited, "a held policy writer must not kill the host");
        assert!(
            samples.iter().all(|s| s.outcome == "ok"),
            "idle policy contention must not fail view/control requests"
        );
        let values = samples.iter().collect::<Vec<_>>();
        assert!(
            distribution(&values)["p95_ms"].as_f64().unwrap() < 500.0,
            "policy writer stalled IPC beyond the bounded responsiveness target"
        );
    }
    host.assert_no_provider();
    drop(clients);
}

/// The test prints measurements instead of asserting an aspirational latency
/// threshold. A passing measurement is not a claim that the host is responsive.
#[test]
#[ignore = "opt-in real Unix IPC contention measurement; requires socket-capable test sandbox"]
fn measure_real_assistant_host_contention_and_admission() {
    scenario("baseline_32_admitted", None, false);
    scenario(
        "policy_writer_2200ms_32_admitted",
        Some((
            "policy.sqlite",
            Duration::from_millis(2200),
            Duration::from_millis(1100),
        )),
        false,
    );
    scenario(
        "memory_writer_2200ms_save_plus_31_views",
        Some(("memory.sqlite", Duration::from_millis(2200), Duration::ZERO)),
        true,
    );
    scenario(
        "policy_writer_6200ms_over_busy_timeout",
        Some((
            "policy.sqlite",
            Duration::from_millis(6200),
            Duration::from_millis(1100),
        )),
        false,
    );

    let (mut host, first) = Host::start();
    let mut clients = host.accepted_views(first, 1);
    for _ in 1..50 {
        clients.push(host.connect().unwrap());
    }
    let (clients, samples) = burst(clients, false);
    let exited = host.child.try_wait().unwrap().is_some();
    report(
        "50_connections_against_32_view_admission_limit",
        &samples,
        None,
        exited,
        host.stderr(),
    );
    assert!(samples.iter().any(|s| s.outcome == "ok"));
    host.assert_no_provider();
    drop(clients);
}
