//! Binary-host surface contracts: JSON parsing, human request dispatch,
//! persistence and restart. Provider output is never used as a command.
//! Every path and child process is disposable; no provider or fleet is enabled.
#![cfg(unix)]

use pikamux::{
    assistant_evolution::{
        CandidateManifest, EvaluationCase, Expr, Scope as ToolScope, ScopedInput, ToolDefinition,
    },
    assistant_memory::{NewRecord, Origin, RecordKind, Scope, Store},
    assistant_workshop::Workshop,
};
use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};
use tempfile::TempDir;

struct Fixture {
    directory: TempDir,
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        for name in [
            "home", "config", "state", "data", "cache", "tmp", "tmux", "codex", "claude",
            "opencode", "muse",
        ] {
            fs::create_dir_all(directory.path().join(name)).unwrap();
        }
        let root = directory.path().join("state/assistant");
        Self { directory, root }
    }

    fn start(&self) -> Host {
        let base = self.directory.path();
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
            .env("PIKA_TMUX_SOCKET", "assistant-extended-disposable")
            .args(["_assistant-host", "--root"])
            .arg(&self.root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let mut host = Host {
            child: command.spawn().unwrap(),
            stream: None,
            last_reply_bytes: 0,
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(stream) = UnixStream::connect(self.root.join("view.sock")) {
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                host.stream = Some(BufReader::new(stream));
                return host;
            }
            if let Some(status) = host.child.try_wait().unwrap() {
                let mut error = String::new();
                host.child
                    .stderr
                    .take()
                    .unwrap()
                    .read_to_string(&mut error)
                    .unwrap();
                panic!("host exited before connection ({status}): {error}");
            }
            assert!(
                Instant::now() < deadline,
                "host never opened its private endpoint"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn assert_no_provider(&self) {
        for name in ["provider-home", "runtime.sqlite", "author-runtime.sqlite"] {
            assert!(
                !self.root.join(name).exists(),
                "unexpected provider side effect: {name}"
            );
        }
        if self.root.join("policy.sqlite").exists() {
            let policy =
                pikamux::assistant_policy::AssistantPolicy::open(self.root.join("policy.sqlite"))
                    .unwrap();
            assert_eq!(policy.config().unwrap().max_total_calls, 0);
            assert_eq!(policy.config().unwrap().background_calls, 0);
            let db = rusqlite::Connection::open(self.root.join("policy.sqlite")).unwrap();
            let reservations: i64 = db
                .query_row("SELECT COUNT(*) FROM assistant_reservations", [], |r| {
                    r.get(0)
                })
                .unwrap();
            assert_eq!(
                reservations, 0,
                "an idle host must not reserve provider calls"
            );
        }
        assert!(
            !self
                .directory
                .path()
                .join("state/operational.sqlite")
                .exists()
        );
    }
}

struct Host {
    child: Child,
    stream: Option<BufReader<UnixStream>>,
    last_reply_bytes: usize,
}

impl Host {
    fn raw(&mut self, payload: Value) -> Value {
        let stream = self.stream.as_mut().unwrap();
        let mut encoded =
            serde_json::to_vec(&json!({"protocol":1,"generation":null,"payload":payload})).unwrap();
        encoded.push(b'\n');
        stream.get_mut().write_all(&encoded).unwrap();
        let mut reply = String::new();
        self.last_reply_bytes = stream.read_line(&mut reply).unwrap();
        assert!(
            self.last_reply_bytes > 0,
            "host closed the connection without a reply"
        );
        assert!(
            self.last_reply_bytes <= 1024 * 1024 + 1,
            "host exceeded the reply wire bound"
        );
        serde_json::from_str(&reply)
            .unwrap_or_else(|error| panic!("invalid host reply: {error} ({} bytes)", reply.len()))
    }

    fn request(&mut self, payload: Value) -> Value {
        let reply = self.raw(payload);
        assert!(reply["error"].is_null(), "{reply}");
        reply["payload"].clone()
    }

    fn reject(&mut self, payload: Value) {
        let reply = self.raw(payload);
        assert!(
            reply["error"].is_string(),
            "request unexpectedly accepted: {reply}"
        );
    }

    fn stop(mut self) {
        self.stream.take();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success(), "host did not exit normally: {status}");
                return;
            }
            assert!(
                Instant::now() < deadline,
                "foreground host retained no-client lifetime"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        self.stream.take();
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn decision() -> Value {
    json!({"chosen":"Cache immutable snapshots", "rationale":"Avoid duplicate scans", "rejected":["Poll from every view"], "owner":"human", "open_questions":["What happens during an outage?"], "commitments":["Measure stale reads"]})
}

fn saved_id(value: &Value) -> String {
    value["saved"].as_str().unwrap().to_owned()
}

#[test]
fn maintenance_controls_default_denied_due_reconsideration_and_inspection_are_local() {
    let fixture = Fixture::new();
    let mut host = fixture.start();
    let dreams = host.request(json!({"operation":"native_dreams","scope":"alpha"}));
    assert!(dreams["runs"].as_array().unwrap().is_empty());
    assert_eq!(dreams["more_runs"], false);
    host.reject(json!({"operation":"maintenance","scope":"alpha","max_calls":1,"hours":1,"interval_hours":24}));
    let help = host.request(json!({"operation":"help"}));
    assert!(
        help["local_output"]
            .as_str()
            .unwrap()
            .contains("/maintenance")
    );
    let saved=host.request(json!({"operation":"save","scope":"alpha","request_id":"maintenance-evidence","kind":"proposal","body":"Review isolation after the next experiment","timestamp":1}));
    let id = saved_id(&saved);
    host.reject(json!({"operation":"maintenance_revisit","scope":"beta","record_id":id,"hours":1}));
    host.reject(
        json!({"operation":"maintenance_revisit","scope":"alpha","record_id":id,"hours":0}),
    );
    let scheduled = host.request(
        json!({"operation":"maintenance_revisit","scope":"alpha","record_id":id,"hours":1}),
    );
    assert!(
        scheduled["notice"]
            .as_str()
            .unwrap()
            .contains("eligible when due")
    );
    host.request(json!({"operation":"maintenance_off","scope":"alpha"}));
    let status = host.request(json!({"operation":"maintenance_status","scope":"alpha"}));
    assert!(status["maintenance"].is_object());
    host.reject(json!({"operation":"guidance","scope":"alpha","record_id":id,"enabled":true}));
    host.request(
        json!({"operation":"method","scope":"alpha","action":"approve","input":"not-evaluated"}),
    );
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let snapshot = host.request(json!({"operation":"snapshot","scope":"alpha"}));
        if snapshot["method_control"]["state"] == "failed" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "unevaluated approval never failed"
        );
        thread::sleep(Duration::from_millis(10));
    }
    host.stop();
    let mut host = fixture.start();
    host.request(json!({"operation":"maintenance_status","scope":"alpha"}));
    host.request(json!({"operation":"forget","record_id":id}));
    host.stop();
    fixture.assert_no_provider();
}

#[test]
fn native_method_database_contention_does_not_block_another_view_or_forget() {
    let fixture = Fixture::new();
    let mut host = fixture.start();
    let id=saved_id(&host.request(json!({"operation":"save","scope":"alpha","request_id":"method-contended-source","kind":"instruction","body":"private source for cleanup","timestamp":1})));
    let workshop = Workshop::open(&fixture.root.join("workshop.sqlite")).unwrap();
    drop(workshop);
    let lock = rusqlite::Connection::open(fixture.root.join("workshop.sqlite")).unwrap();
    lock.execute_batch("BEGIN IMMEDIATE").unwrap();
    host.request(
        json!({"operation":"method","scope":"alpha","action":"approve","input":"not-evaluated"}),
    );
    thread::sleep(Duration::from_millis(50));
    let second = UnixStream::connect(fixture.root.join("view.sock")).unwrap();
    second
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut second = BufReader::new(second);
    let started = Instant::now();
    writeln!(
        second.get_mut(),
        "{}",
        json!({"protocol":1,"generation":null,"payload":{"operation":"snapshot","scope":"alpha"}})
    )
    .unwrap();
    let mut line = String::new();
    second.read_line(&mut line).unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "native DB work blocked the second view"
    );
    assert!(serde_json::from_str::<Value>(&line).unwrap()["payload"]["profile_id"].is_string());
    let cleanup = host.request(json!({"operation":"forget","record_id":id}));
    assert!(cleanup["notice"].is_string());
    lock.execute_batch("ROLLBACK").unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let snapshot = host.request(json!({"operation":"snapshot","scope":"alpha"}));
        if snapshot["epoch"].as_u64().unwrap_or(0) > 0
            || Store::open(fixture.root.join("memory.sqlite"))
                .unwrap()
                .get(&id)
                .unwrap()
                .is_none()
        {
            break;
        }
        assert!(Instant::now() < deadline, "queued forget did not finish");
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        Store::open(fixture.root.join("memory.sqlite"))
            .unwrap()
            .get(&id)
            .unwrap()
            .is_none()
    );
    drop(second);
    host.stop();
    fixture.assert_no_provider();
}

#[test]
fn quote_heavy_memory_page_preserves_records_and_connection() {
    let fixture = Fixture::new();
    let mut host = fixture.start();
    let profile =
        host.request(json!({"operation":"snapshot","scope":"alpha"}))["profile_id"].clone();
    let body = "\"".repeat(16 * 1024);
    let mut ids = std::collections::BTreeSet::new();
    for index in 0..12 {
        ids.insert(saved_id(&host.request(json!({"operation":"save","request_id":format!("quoted-{index}"),"scope":"alpha","kind":"instruction","body":body,"timestamp":index + 1}))));
    }
    // This is the exact request generated by the public /memory command.
    let reply =
        host.request(json!({"operation":"memory_page","scope":"alpha","cursor":null,"limit":32}));
    eprintln!(
        "quote-heavy memory page: {} reply bytes",
        host.last_reply_bytes
    );
    let records = reply["memory_page"]["records"].as_array().unwrap();
    assert_eq!(records.len(), 12);
    let returned = records
        .iter()
        .map(|entry| {
            assert_eq!(entry["record"]["body"].as_str(), Some(body.as_str()));
            entry["record"]["id"].as_str().unwrap().to_owned()
        })
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(returned, ids);
    assert!(reply["memory_page"]["next"].is_null());
    assert!(
        reply["local_output"]
            .as_str()
            .unwrap()
            .contains("/memory-record")
    );
    assert_eq!(
        host.request(json!({"operation":"snapshot","scope":"alpha"}))["profile_id"],
        profile
    );
    host.stop();
    fixture.assert_no_provider();
}

#[test]
fn control_heavy_memory_search_preserves_hits_and_connection() {
    let fixture = Fixture::new();
    let mut host = fixture.start();
    let profile =
        host.request(json!({"operation":"snapshot","scope":"alpha"}))["profile_id"].clone();
    // Six-byte JSON escapes greatly exceed the raw UTF-8 storage size. Each
    // save still fits the request frame; five hits fit the encoded 256 KiB
    // search budget, but a sixth does not.
    let body = format!("searchneedle {}", "\u{1}".repeat(8_000));
    let mut ids = Vec::new();
    for index in 0..5 {
        ids.push(saved_id(&host.request(json!({"operation":"save","request_id":format!("control-{index}"),"scope":"alpha","kind":"instruction","body":body,"timestamp":index + 1}))));
    }
    let reply = host.request(
        json!({"operation":"memory_search","scope":"alpha","query":"searchneedle\u{1}\u{2}"}),
    );
    eprintln!(
        "control-heavy memory search: {} reply bytes",
        host.last_reply_bytes
    );
    let hits = reply["records"].as_array().unwrap();
    assert_eq!(hits.len(), 5);
    let returned = hits
        .iter()
        .map(|record| {
            assert_eq!(record["body"].as_str(), Some(body.as_str()));
            record["id"].as_str().unwrap().to_owned()
        })
        .collect::<Vec<_>>();
    assert_eq!(returned, ids.iter().rev().cloned().collect::<Vec<_>>());
    assert!(!reply["local_output"].as_str().unwrap().contains('\u{1}'));
    for index in 5..11 {
        ids.push(saved_id(&host.request(json!({"operation":"save","request_id":format!("control-{index}"),"scope":"alpha","kind":"instruction","body":body,"timestamp":index + 1}))));
    }
    let reply =
        host.request(json!({"operation":"memory_search","scope":"alpha","query":"searchneedle"}));
    let hits = reply["records"].as_array().unwrap();
    assert_eq!(hits.len(), 5);
    let returned = hits
        .iter()
        .map(|record| {
            assert_eq!(record["body"].as_str(), Some(body.as_str()));
            record["id"].as_str().unwrap().to_owned()
        })
        .collect::<Vec<_>>();
    // All bodies have identical BM25 relevance; the documented timestamp
    // tie-break must return exactly the five newest, not an arbitrary subset.
    assert_eq!(
        returned,
        ids[6..11].iter().rev().cloned().collect::<Vec<_>>()
    );
    let coverage = reply["coverage"].as_str().unwrap();
    assert!(
        coverage.contains("20") && coverage.contains("256"),
        "{coverage}"
    );
    assert!(coverage.contains("not a complete archive"), "{coverage}");
    assert!(reply["local_output"].as_str().unwrap().contains(coverage));
    assert_eq!(
        host.request(json!({"operation":"snapshot","scope":"alpha"}))["profile_id"],
        profile
    );
    host.stop();
    fixture.assert_no_provider();
}

#[test]
fn human_pause_persists_and_invalid_scope_cannot_grant_background_authority() {
    let fixture = Fixture::new();
    let mut host = fixture.start();
    host.request(json!({"operation":"pause"}));
    assert_eq!(
        host.request(json!({"operation":"snapshot","scope":"alpha"}))["control"]["paused"],
        true
    );
    let rejected = host.raw(json!({"operation":"send","request_id":"paused-send","scope":"alpha","body":"a user question"}));
    assert!(rejected["error"].as_str().unwrap().contains("paused"));
    host.reject(json!({"operation":"background","scope":"alpha","max_calls":4,"hours":1}));
    host.stop();

    let mut host = fixture.start();
    assert_eq!(
        host.request(json!({"operation":"snapshot","scope":"alpha"}))["control"]["paused"],
        true
    );
    host.request(json!({"operation":"resume"}));
    assert_eq!(
        host.request(json!({"operation":"snapshot","scope":"alpha"}))["control"]["paused"],
        false
    );
    for invalid in ["", "alpha\nbeta"] {
        host.reject(json!({"operation":"snapshot","scope":invalid}));
        host.reject(json!({"operation":"share_board","scope":invalid}));
        host.reject(json!({"operation":"background","scope":invalid,"max_calls":4,"hours":1}));
    }
    host.reject(
        json!({"operation":"background","scope":"alpha","max_calls":4,"hours":1,"approved":true}),
    );
    host.reject(json!({"operation":"send","request_id":"still-unapproved","scope":"alpha","body":"resume must not create a model grant"}));
    host.request(json!({"operation":"stop_background"}));
    host.stop();
    fixture.assert_no_provider();
}

#[test]
fn decisions_keep_history_state_and_commitments_through_binary_host_restart() {
    let fixture = Fixture::new();
    let mut host = fixture.start();
    let profile =
        host.request(json!({"operation":"snapshot","scope":"alpha"}))["profile_id"].clone();
    let original = saved_id(&host.request(json!({"operation":"save","request_id":"proposal","scope":"alpha","kind":"proposal","body":decision().to_string(),"timestamp":1})));
    let brief = host.request(json!({"operation":"brief","scope":"alpha"}));
    assert_eq!(
        brief["presentation"]["brief"]["decisions"][0]["decision_state"],
        "Proposed"
    );
    assert_eq!(brief["presentation"]["brief"]["commitments"], json!([]));
    let deferred = saved_id(&host.request(json!({"operation":"decision_state","request_id":"defer","scope":"alpha","record_id":original,"state":"Deferred","timestamp":2})));
    let acceptance = json!({"operation":"decision_state","request_id":"accept","scope":"alpha","record_id":deferred,"state":"Accepted","timestamp":3});
    let accepted = saved_id(&host.request(acceptance.clone()));
    assert_eq!(saved_id(&host.request(acceptance)), accepted);
    host.reject(json!({"operation":"decision_state","request_id":"stale-predecessor","scope":"alpha","record_id":original,"state":"Rejected","timestamp":4}));
    host.reject(json!({"operation":"decision_state","request_id":"cross-scope","scope":"beta","record_id":accepted,"state":"Rejected","timestamp":4}));
    host.reject(json!({"operation":"correct","request_id":"erase-decision","scope":"alpha","record_id":accepted,"body":"Just rewrite all meaning"}));
    let mut revised = decision();
    revised["rationale"] = json!("Preserve exact historical evidence while reducing repeated work");
    let current = saved_id(&host.request(json!({"operation":"decision_revision","request_id":"revise","scope":"alpha","record_id":accepted,"decision":revised,"timestamp":5})));
    host.stop();
    let mut host = fixture.start();
    assert_eq!(
        host.request(json!({"operation":"snapshot","scope":"alpha"}))["profile_id"],
        profile
    );
    let brief = host.request(json!({"operation":"brief","scope":"alpha"}));
    assert_eq!(
        brief["presentation"]["brief"]["decisions"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        brief["presentation"]["brief"]["decisions"][0]["id"],
        current
    );
    assert_eq!(
        brief["presentation"]["brief"]["decisions"][0]["decision_state"],
        "Accepted"
    );
    assert_eq!(
        brief["presentation"]["brief"]["commitments"][0]["text"],
        "Measure stale reads"
    );
    let historical =
        host.request(json!({"operation":"recall","scope":"alpha","record_id":original}));
    assert_eq!(
        historical["recall"]["structured"]["rationale"],
        "Avoid duplicate scans"
    );
    assert_eq!(historical["recall"]["structured"]["owner"], "human");
    let latest = host.request(json!({"operation":"recall","scope":"alpha","record_id":current}));
    assert_eq!(
        latest["recall"]["structured"]["rationale"],
        revised["rationale"]
    );
    assert!(
        latest["local_output"]
            .as_str()
            .unwrap()
            .contains("Historical rationale")
    );
    host.reject(json!({"operation":"decision_state","request_id":"unknown-fields","scope":"alpha","record_id":current,"state":"Accepted","timestamp":6,"origin":"Worker"}));
    host.stop();
    fixture.assert_no_provider();
}

#[test]
fn typed_corrections_and_nonexecuting_improvement_preserve_scope_after_restart() {
    let fixture = Fixture::new();
    let mut memory = Store::open(fixture.root.join("memory.sqlite")).unwrap();
    let finding = memory
        .append(NewRecord {
            kind: RecordKind::Finding,
            origin: Origin::Worker,
            scope: Scope {
                project: Some("alpha".into()),
                ..Scope::default()
            },
            body: "All work is finished".into(),
            provenance: "synthetic finding; not user instruction".into(),
            timestamp: 1,
            supersedes: None,
            dependencies: vec![],
            decision_state: None,
            protected_policy: false,
        })
        .unwrap();
    drop(memory);
    let mut host = fixture.start();
    let instruction = saved_id(&host.request(json!({"operation":"save","request_id":"instruction","scope":"alpha","kind":"instruction","body":"Keep briefings terse","timestamp":2})));
    let instruction_correction = saved_id(&host.request(json!({"operation":"correct","request_id":"instruction-correction","scope":"alpha","record_id":instruction,"body":"Explain consequential trade-offs"})));
    let fact_correction = saved_id(&host.request(json!({"operation":"correct","request_id":"fact-correction","scope":"alpha","record_id":finding.id,"body":"The release still needs approval"})));
    let proposal = host.request(
        json!({"operation":"improvement","scope":"alpha","correction_id":instruction_correction}),
    );
    assert_eq!(proposal["details"]["sent_to_provider"], false);
    assert_eq!(proposal["details"]["proposal"]["kind"], "Proposal");
    let proposal_id = proposal["details"]["proposal"]["id"].clone();
    assert_eq!(host.request(json!({"operation":"improvement","scope":"alpha","correction_id":instruction_correction}))["details"]["proposal"]["id"],proposal_id);
    host.reject(
        json!({"operation":"improvement","scope":"beta","correction_id":instruction_correction}),
    );
    host.stop();
    let mut host = fixture.start();
    let brief = host.request(json!({"operation":"brief","scope":"alpha"}));
    assert_eq!(
        brief["presentation"]["brief"]["instructions"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        brief["presentation"]["brief"]["instructions"][0]["text"],
        "Explain consequential trade-offs"
    );
    assert!(
        brief["presentation"]["brief"]["changes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|record| record["id"] == fact_correction)
    );
    assert_eq!(
        host.request(json!({"operation":"brief","scope":"beta"}))["presentation"]["brief"]["instructions"],
        json!([])
    );
    host.request(json!({"operation":"forget","record_id":instruction_correction}));
    host.stop();
    let mut host = fixture.start();
    host.reject(
        json!({"operation":"improvement","scope":"alpha","correction_id":instruction_correction}),
    );
    let snapshot = host.request(json!({"operation":"snapshot","scope":"alpha"}));
    assert!(
        !snapshot["records"]
            .as_array()
            .unwrap()
            .iter()
            .any(|record| record["id"] == proposal_id)
    );
    host.stop();
    fixture.assert_no_provider();
}

fn seed_candidate(fixture: &Fixture, version: u64) -> String {
    let workshop = Workshop::open(&fixture.root.join("workshop.sqlite")).unwrap();
    let scope = ToolScope::new(["alpha"]);
    workshop
        .protect_cases(
            "protected-identity",
            &[
                EvaluationCase {
                    inputs: vec![ScopedInput {
                        value: json!("reproduction"),
                        scope: scope.clone(),
                    }],
                    expected: json!(["reproduction"]),
                },
                EvaluationCase {
                    inputs: vec![],
                    expected: json!([]),
                },
            ],
        )
        .unwrap();
    let candidate = CandidateManifest {
        definition: ToolDefinition {
            name: "alpha-identity".into(),
            version,
            input_scope: scope,
            expression: Expr::Input,
        },
        authoring_evidence: "synthetic isolated author; no live provider".into(),
    };
    let report = workshop
        .submit_candidate("protected-identity", &candidate, None)
        .unwrap();
    assert!(report.passed);
    report.tool_hash
}

#[test]
fn tools_are_discoverable_approvable_and_retirable_through_binary_host_restart() {
    let fixture = Fixture::new();
    let first = seed_candidate(&fixture, 1);
    let second = seed_candidate(&fixture, 2);
    let mut host = fixture.start();
    host.reject(
        json!({"operation":"invoke_tool","scope":"alpha","name":"alpha-identity","inputs":"[]"}),
    );
    let catalog = host.request(json!({"operation":"tool_catalog","scope":"alpha","hash":null}));
    assert_eq!(catalog["details"]["tools"].as_array().unwrap().len(), 2);
    assert_eq!(
        host.request(json!({"operation":"tool_catalog","scope":"beta","hash":second}))["details"]["tools"],
        json!([])
    );
    host.request(json!({"operation":"approve_tool","scope":"alpha","hash":first}));
    assert_eq!(host.request(json!({"operation":"invoke_tool","scope":"alpha","name":"alpha-identity","inputs":"[\"withheld\"]"}))["tool_result"],json!(["withheld"]));
    host.request(json!({"operation":"approve_tool","scope":"alpha","hash":second}));
    host.stop();
    let mut host = fixture.start();
    let detail = host.request(json!({"operation":"tool_catalog","scope":"alpha","hash":second}));
    let grants = detail["details"]["tools"][0]["grants"].as_array().unwrap();
    assert_eq!(grants.len(), 1);
    assert_eq!(grants[0].get("expires_at"), Some(&Value::Null));
    assert_eq!(host.request(json!({"operation":"invoke_tool","scope":"alpha","name":"alpha-identity","inputs":"[\"after restart\"]"}))["tool_result"],json!(["after restart"]));
    assert_eq!(
        detail["details"]["tools"][0]["definition"]["name"],
        "alpha-identity"
    );
    assert_eq!(
        detail["details"]["tools"][0]["tests"][0]["evaluation"]["passed"],
        true
    );
    assert!(detail["details"]["tools"][0]["tests"][0]["comparison"]["candidate_fuel"].is_number());
    let assessed=host.request(json!({"operation":"assess_tool","scope":"alpha","hash":second,"outcome":"regression","evidence":"Fresh inputs need the earlier behavior","rollback":first}));
    assert_eq!(assessed["details"]["rollback"]["completed"], true);
    host.reject(json!({"operation":"approve_tool","scope":"alpha","hash":second}));
    host.stop();
    let mut host = fixture.start();
    let detail = host.request(json!({"operation":"tool_catalog","scope":"alpha","hash":second}));
    assert_eq!(detail["details"]["tools"][0]["retired"], true);
    let grants = detail["details"]["tools"][0]["grants"].as_array().unwrap();
    assert_eq!(
        grants.len(),
        1,
        "retired approval must not create another grant"
    );
    assert!(grants.iter().all(|grant| grant["revoked"] == true));
    host.request(json!({"operation":"assess_tool","scope":"alpha","hash":first,"outcome":"retire","evidence":"This capability is obsolete","rollback":null}));
    host.reject(
        json!({"operation":"invoke_tool","scope":"alpha","name":"alpha-identity","inputs":"[]"}),
    );
    host.stop();
    fixture.assert_no_provider();
}
