#![cfg(unix)]

use pikamux::consult::{
    CancellationToken, Cleanup, Consultation, ConsultationOptions, ConsultationStage, Delivery,
    FAST_CODEX_MODEL, MAX_QUESTION_BYTES, consultation_policy,
};
use pikamux::model::{Provider, Session, Status};
use rusqlite::{Connection, params};
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;
use std::time::Instant;
use tempfile::TempDir;

fn session(provider: Provider, id: &str, cwd: &Path) -> Session {
    Session {
        provider,
        session_id: id.to_owned(),
        name: Some("expert".to_owned()),
        cwd: Some(cwd.to_string_lossy().into_owned()),
        branch: None,
        transcript_path: None,
        tmux_session: None,
        tmux_pane: None,
        root_pid: None,
        status: Status::Working,
        unread: false,
        model: None,
        source: "fixture".to_owned(),
        managed: true,
        error: None,
        attention_reason: None,
        created_at: 1.0,
        updated_at: 1.0,
        last_event_at: 1.0,
        last_activity_at: 1.0,
        live: true,
        attached: false,
        home_state: String::new(),
        cpu_percent: None,
        rss_kb: None,
        input_tokens: None,
        output_tokens: None,
        cached_input_tokens: None,
        cache_write_tokens: None,
        total_tokens: None,
        estimated_cost_usd: None,
        active_thread_id: Some("active-leaf".to_owned()),
    }
}

fn write_executable(root: &TempDir, name: &str, script: &str) -> PathBuf {
    let path = root.path().join(name);
    fs::write(&path, script).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    path
}

fn shell_quote(value: &Path) -> String {
    format!("'{}'", value.to_string_lossy().replace('\'', "'\\''"))
}

fn codex_fixture(root: &TempDir, mode: &str) -> (PathBuf, PathBuf) {
    let log = root.path().join("codex.log");
    let executable = write_executable(
        root,
        "codex-fixture",
        &format!(
            r#"#!/bin/sh
log={log}
mode={mode}
printf 'ARGV:%s EPHEMERAL:%s\n' "$*" "$PIKA_EPHEMERAL" >> "$log"
turn=0
while IFS= read -r line; do
  printf 'IN:%s\n' "$line" >> "$log"
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*) printf '{{"id":%s,"result":{{}}}}\n' "$id" ;;
    *'"method":"thread/fork"'*)
      if [ "$mode" = mismatch ]; then
        printf '{{"id":%s,"result":{{"thread":{{"id":"side-id","ephemeral":true}},"model":"gpt-5.6-sol","reasoningEffort":"high"}}}}\n' "$id"
      else
        printf '{{"id":%s,"result":{{"thread":{{"id":"side-id","ephemeral":true}},"model":"gpt-5.6-sol","reasoningEffort":"medium"}}}}\n' "$id"
      fi ;;
    *'"method":"turn/start"'*)
      turn=$((turn + 1))
      if [ "$mode" = unknown ]; then exit 9; fi
      if [ "$mode" = notification-flood ]; then
        index=0
        while [ "$index" -lt 129 ]; do
          printf '{{"method":"noise","params":{{"index":%s}}}}\n' "$index"
          index=$((index + 1))
        done
      fi
      printf '{{"id":%s,"result":{{"turn":{{"id":"turn-%s"}}}}}}\n' "$id" "$turn"
      if [ "$mode" != timeout ]; then
        printf '{{"method":"item/completed","params":{{"threadId":"side-id","turnId":"turn-%s","item":{{"type":"agentMessage","phase":"final_answer","text":"answer-%s"}}}}}}\n' "$turn" "$turn"
      fi ;;
  esac
done
"#,
            log = shell_quote(&log),
            mode = mode,
        ),
    );
    (executable, log)
}

fn claude_fixture(root: &TempDir) -> (PathBuf, PathBuf) {
    let log = root.path().join("claude.log");
    let executable = write_executable(
        root,
        "claude-fixture",
        &format!(
            r#"#!/bin/sh
log={log}
printf 'ARGV:%s EPHEMERAL:%s\n' "$*" "$PIKA_EPHEMERAL" >> "$log"
if [ "$1" = "--version" ]; then printf '2.1.228 (Claude Code)\n'; exit 0; fi
turn=0
while IFS= read -r line; do
  turn=$((turn + 1))
  printf 'IN:%s\n' "$line" >> "$log"
  printf '{{"type":"assistant","message":{{"content":[{{"type":"text","text":"claude-%s"}}]}}}}\n' "$turn"
  printf '{{"type":"result","subtype":"success","is_error":false,"result":"claude-%s"}}\n' "$turn"
done
"#,
            log = shell_quote(&log),
        ),
    );
    (executable, log)
}

#[test]
fn codex_uses_one_confirmed_ephemeral_child_for_multiple_turns() {
    let root = tempfile::tempdir().unwrap();
    let (executable, log) = codex_fixture(&root, "normal");
    let parent = root.path().join("parent.jsonl");
    fs::write(&parent, b"parent-history\n").unwrap();
    let mut target = session(Provider::Codex, "stable-workstream", root.path());
    target.transcript_path = Some(parent.to_string_lossy().into_owned());
    let mut options = ConsultationOptions::new(executable);
    options.timeout = Duration::from_secs(2);
    let mut side = Consultation::open(&target, options).unwrap();
    assert_eq!(side.parent_id(), "active-leaf");
    assert_eq!(side.child_id(), Some("side-id"));
    assert_eq!(side.ask("one").unwrap(), "answer-1");
    assert_eq!(side.ask("two").unwrap(), "answer-2");
    assert_eq!(side.receipt().answers_received, 2);
    side.close().unwrap();
    assert_eq!(side.receipt().cleanup, Cleanup::Complete);
    assert_eq!(side.receipt().parent_transcript_unchanged, None);
    assert_eq!(
        side.receipt().parent_transcript_verification,
        "not_performed"
    );
    assert_eq!(fs::read(&parent).unwrap(), b"parent-history\n");

    let log = fs::read_to_string(log).unwrap();
    assert_eq!(log.matches("ARGV:app-server --stdio").count(), 1);
    assert!(log.contains("EPHEMERAL:1"));
    assert!(log.contains("\"threadId\":\"active-leaf\""));
    assert!(log.contains("\"ephemeral\":true"));
    assert!(log.contains("\"excludeTurns\":true"));
    assert!(log.contains("\"sandbox\":\"read-only\""));
    assert_eq!(log.matches("\"method\":\"turn/start\"").count(), 2);
    assert_eq!(log.matches("\"threadId\":\"side-id\"").count(), 2);
    assert_eq!(log.matches("\"model\":\"gpt-5.6-sol\"").count(), 3);
}

#[test]
fn codex_reports_unknown_delivery_without_resending() {
    let root = tempfile::tempdir().unwrap();
    let (executable, log) = codex_fixture(&root, "unknown");
    let mut options = ConsultationOptions::new(executable);
    options.timeout = Duration::from_millis(250);
    let mut side = Consultation::open(
        &session(Provider::Codex, "stable-workstream", root.path()),
        options,
    )
    .unwrap();
    let error = side.ask("only once").unwrap_err();
    assert_eq!(error.receipt.stage, ConsultationStage::Turn);
    assert_eq!(error.receipt.delivery, Delivery::Unknown);
    assert!(!error.receipt.retry_safe);
    side.close().unwrap();
    let log = fs::read_to_string(log).unwrap();
    assert_eq!(log.matches("\"method\":\"turn/start\"").count(), 1);
}

#[test]
fn codex_reports_confirmed_delivery_when_response_times_out() {
    let root = tempfile::tempdir().unwrap();
    let (executable, _) = codex_fixture(&root, "timeout");
    let mut options = ConsultationOptions::new(executable);
    options.timeout = Duration::from_millis(100);
    let mut side = Consultation::open(
        &session(Provider::Codex, "stable-workstream", root.path()),
        options,
    )
    .unwrap();
    let error = side.ask("do not resend").unwrap_err();
    assert_eq!(error.receipt.stage, ConsultationStage::Turn);
    assert_eq!(error.receipt.delivery, Delivery::Confirmed);
    assert!(!error.receipt.retry_safe);
    side.close().unwrap();
}

#[test]
fn codex_notification_flood_is_bounded_before_turn_delivery_is_confirmed() {
    let root = tempfile::tempdir().unwrap();
    let (executable, log) = codex_fixture(&root, "notification-flood");
    let mut options = ConsultationOptions::new(executable);
    options.timeout = Duration::from_secs(2);
    let mut side = Consultation::open(
        &session(Provider::Codex, "stable-workstream", root.path()),
        options,
    )
    .unwrap();
    let error = side.ask("bounded").unwrap_err();
    assert!(error.to_string().contains("notification backlog"));
    assert_eq!(error.receipt.delivery, Delivery::Unknown);
    assert!(!error.receipt.retry_safe);
    side.close().unwrap();
    assert_eq!(
        fs::read_to_string(log)
            .unwrap()
            .matches("\"method\":\"turn/start\"")
            .count(),
        1
    );
}

#[test]
fn cancellation_interrupts_blocked_codex_startup_and_reaps_owned_child() {
    let root = tempfile::tempdir().unwrap();
    let marker = root.path().join("parent-marker");
    let child_pid = root.path().join("owned-child.pid");
    fs::write(&marker, "parent untouched").unwrap();
    let executable = write_executable(
        &root,
        "codex-blocked-startup",
        &format!(
            "#!/bin/sh\nsleep 30 &\nprintf '%s' \"$!\" > {}\nwait\n",
            shell_quote(&child_pid)
        ),
    );
    let cancellation = CancellationToken::default();
    let mut options = ConsultationOptions::new(executable);
    options.cancellation = cancellation.clone();
    let target = session(Provider::Codex, "stable-workstream", root.path());
    let worker = std::thread::spawn(move || Consultation::open(&target, options));
    for _ in 0..2_500 {
        if child_pid.is_file() {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(child_pid.is_file(), "fixture child never started");
    let cancelled_at = Instant::now();
    cancellation.cancel();
    let error = match worker.join().unwrap() {
        Ok(_) => panic!("cancelled startup unexpectedly opened"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("cancelled"));
    assert_eq!(error.receipt.cleanup, Cleanup::Complete);
    assert!(cancelled_at.elapsed() < Duration::from_secs(1));
    assert_eq!(fs::read_to_string(marker).unwrap(), "parent untouched");
    let pid: i32 = fs::read_to_string(child_pid).unwrap().parse().unwrap();
    for _ in 0..50 {
        // SAFETY: signal zero probes the exact fixture PID without changing it.
        if unsafe { libc::kill(pid, 0) } != 0 {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("owned consultation descendant {pid} survived cancellation");
}

#[test]
fn cancellation_interrupts_blocked_codex_turn_without_resending() {
    let root = tempfile::tempdir().unwrap();
    let (executable, log) = codex_fixture(&root, "timeout");
    let cancellation = CancellationToken::default();
    let mut options = ConsultationOptions::new(executable);
    options.timeout = Duration::from_secs(30);
    options.cancellation = cancellation.clone();
    let mut side = Consultation::open(
        &session(Provider::Codex, "stable-workstream", root.path()),
        options,
    )
    .unwrap();
    let started = Instant::now();
    let worker = std::thread::spawn(move || {
        let error = side.ask("only once").unwrap_err();
        let cleanup = side.close();
        (error, cleanup)
    });
    std::thread::sleep(Duration::from_millis(75));
    cancellation.cancel();
    let (error, cleanup) = worker.join().unwrap();
    assert!(error.to_string().contains("cancelled"));
    assert_eq!(error.receipt.delivery, Delivery::Confirmed);
    assert!(cleanup.is_ok());
    assert!(started.elapsed() < Duration::from_secs(1));
    let log = fs::read_to_string(log).unwrap();
    assert_eq!(log.matches("\"method\":\"turn/start\"").count(), 1);
}

#[test]
fn oversized_question_is_rejected_before_provider_delivery() {
    let root = tempfile::tempdir().unwrap();
    let (executable, log) = codex_fixture(&root, "normal");
    let mut side = Consultation::open(
        &session(Provider::Codex, "stable-workstream", root.path()),
        ConsultationOptions::new(executable),
    )
    .unwrap();
    let error = side.ask(&"x".repeat(MAX_QUESTION_BYTES + 1)).unwrap_err();
    assert_eq!(error.receipt.delivery, Delivery::NotSent);
    assert!(error.to_string().contains("64 KiB"));
    side.close().unwrap();
    let log = fs::read_to_string(log).unwrap();
    assert!(!log.contains("\"method\":\"turn/start\""));
}

#[test]
fn codex_rejects_unconfirmed_policy_and_cleans_its_owned_child() {
    let root = tempfile::tempdir().unwrap();
    let (executable, _) = codex_fixture(&root, "mismatch");
    let error = match Consultation::open(
        &session(Provider::Codex, "stable-workstream", root.path()),
        ConsultationOptions::new(executable),
    ) {
        Ok(_) => panic!("mismatched provider policy was accepted"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("did not confirm"));
    assert_eq!(error.receipt.stage, ConsultationStage::Prepare);
    assert_eq!(error.receipt.delivery, Delivery::NotSent);
    assert_eq!(error.receipt.cleanup, Cleanup::Complete);
    assert!(error.receipt.retry_safe);
}

#[test]
fn claude_uses_one_nonpersistent_toolless_process_for_multiple_turns() {
    let root = tempfile::tempdir().unwrap();
    let (executable, log) = claude_fixture(&root);
    let mut options = ConsultationOptions::new(executable);
    options.timeout = Duration::from_secs(2);
    let mut side = Consultation::open(
        &session(Provider::Claude, "stable-workstream", root.path()),
        options,
    )
    .unwrap();
    assert_eq!(side.parent_id(), "active-leaf");
    assert_eq!(side.ask("one").unwrap(), "claude-1");
    assert_eq!(side.ask("two").unwrap(), "claude-2");
    side.close().unwrap();
    let log = fs::read_to_string(log).unwrap();
    assert_eq!(log.matches("ARGV:-p").count(), 1);
    assert!(log.contains("--resume active-leaf"));
    assert!(log.contains("--fork-session"));
    assert!(log.contains("--no-session-persistence"));
    assert!(log.contains("--tools  --input-format"));
    assert_eq!(log.matches("\"type\":\"user\"").count(), 2);
}

#[test]
fn unsupported_fast_profiles_fail_before_starting_a_provider() {
    assert!(consultation_policy(Provider::Claude, true).is_err());
    assert!(consultation_policy(Provider::Opencode, true).is_err());
    let policy = consultation_policy(Provider::Codex, true).unwrap();
    assert_eq!(policy.model.as_deref(), Some(FAST_CODEX_MODEL));
}

fn opencode_fixture(
    root: &TempDir,
    delete_child: bool,
    returned_child: &str,
) -> (PathBuf, PathBuf, PathBuf) {
    let database = root.path().join("opencode.db");
    let db = Connection::open(&database).unwrap();
    db.execute_batch(
        "CREATE TABLE session(id TEXT PRIMARY KEY,parent_id TEXT,time_updated INTEGER,time_archived INTEGER,directory TEXT);\
         CREATE TABLE message(id TEXT PRIMARY KEY,session_id TEXT,time_created INTEGER,data TEXT);\
         CREATE TABLE part(id TEXT PRIMARY KEY,session_id TEXT,message_id TEXT,time_created INTEGER,data TEXT);\
         INSERT INTO session VALUES('ses_parent123',NULL,1,NULL,'fixture');",
    )
    .unwrap();
    drop(db);
    let log = root.path().join("opencode.log");
    let binary = std::env::current_exe().unwrap();
    let executable = write_executable(
        root,
        "opencode-fixture",
        &format!(
            r#"#!/bin/sh
log={log}
db={db}
bin={bin}
printf 'ARGV:%s EPHEMERAL:%s CONFIG:%s\n' "$*" "$PIKA_EPHEMERAL" "$OPENCODE_CONFIG_CONTENT" >> "$log"
if [ "$1" = "--version" ]; then printf '1.18.21\n'; exit 0; fi
mode=''
port=''
sid=''
prev=''
for arg in "$@"; do
  if [ "$prev" = "--port" ]; then port="$arg"; fi
  if [ "$prev" = "--session" ]; then sid="$arg"; fi
  if [ "$prev" = "delete" ]; then sid="$arg"; fi
  prev="$arg"
  if [ "$arg" = "serve" ]; then mode=serve; fi
  if [ "$arg" = "run" ]; then mode=run; fi
  if [ "$arg" = "delete" ]; then mode=delete; fi
done
case "$mode" in
  serve) PIKA_FAKE_DB="$db" PIKA_FAKE_PORT="$port" PIKA_FAKE_CWD={cwd} PIKA_FAKE_CHILD={returned_child} "$bin" --exact fake_opencode_server --nocapture ;;
  run) PIKA_FAKE_DB="$db" PIKA_FAKE_SESSION="$sid" "$bin" --exact fake_opencode_turn --nocapture ;;
  delete)
    if [ {delete_child} = yes ]; then
      PIKA_FAKE_DB="$db" PIKA_FAKE_SESSION="$sid" "$bin" --exact fake_opencode_delete --nocapture
    fi ;;
esac
"#,
            log = shell_quote(&log),
            db = shell_quote(&database),
            bin = shell_quote(&binary),
            cwd = shell_quote(root.path()),
            returned_child = returned_child,
            delete_child = if delete_child { "yes" } else { "no" },
        ),
    );
    (executable, database, log)
}

#[test]
fn opencode_uses_one_exact_readonly_child_then_verifies_deletion() {
    let root = tempfile::tempdir().unwrap();
    let (executable, database, log) = opencode_fixture(&root, true, "ses_child123");
    let mut target = session(Provider::Opencode, "ses_stable123", root.path());
    target.active_thread_id = Some("ses_parent123".to_owned());
    target.model = Some("opencode/x-preview[max]".to_owned());
    let mut options = ConsultationOptions::new(executable);
    options.opencode_database = Some(database.clone());
    options.timeout = Duration::from_secs(5);
    let mut side = Consultation::open(&target, options).unwrap();
    assert_eq!(side.child_id(), None);
    assert_eq!(side.ask("one").unwrap(), "opencode-answer-1");
    assert_eq!(side.child_id(), Some("ses_child123"));
    assert_eq!(side.ask("two").unwrap(), "opencode-answer-2");
    side.close().unwrap();
    assert_eq!(side.receipt().cleanup, Cleanup::Complete);
    let db = Connection::open(database).unwrap();
    assert_eq!(
        db.query_row(
            "SELECT COUNT(*) FROM session WHERE id='ses_parent123'",
            [],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        1
    );
    assert_eq!(
        db.query_row(
            "SELECT COUNT(*) FROM session WHERE id='ses_child123'",
            [],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    assert_eq!(
        db.query_row(
            "SELECT COUNT(*) FROM message WHERE session_id='ses_parent123'",
            [],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    let log = fs::read_to_string(log).unwrap();
    assert_eq!(log.matches(" run ").count(), 2);
    assert!(log.contains("--session ses_child123"));
    assert!(log.contains("--agent pika-readonly"));
    assert!(log.contains("--model opencode/x-preview --variant max"));
    assert!(log.contains("\"*\":\"deny\""));
}

#[test]
fn opencode_answer_survives_failed_exact_child_cleanup() {
    let root = tempfile::tempdir().unwrap();
    let (executable, database, _) = opencode_fixture(&root, false, "ses_child123");
    let mut target = session(Provider::Opencode, "ses_stable123", root.path());
    target.active_thread_id = Some("ses_parent123".to_owned());
    let mut options = ConsultationOptions::new(executable);
    options.opencode_database = Some(database);
    options.timeout = Duration::from_secs(5);
    let mut side = Consultation::open(&target, options).unwrap();
    let answer = side.ask("question").unwrap();
    assert_eq!(answer, "opencode-answer-1");
    let error = side.close().unwrap_err();
    assert_eq!(error.receipt.stage, ConsultationStage::Cleanup);
    assert_eq!(error.receipt.cleanup, Cleanup::Failed);
    assert_eq!(error.receipt.answers_received, 1);
    assert_eq!(side.child_id(), Some("ses_child123"));
    assert!(!error.receipt.retry_safe);
}

#[test]
fn opencode_unknown_fork_identity_is_not_retry_safe_or_guessed_for_deletion() {
    let root = tempfile::tempdir().unwrap();
    let (executable, database, log) = opencode_fixture(&root, true, "invalid");
    let mut target = session(Provider::Opencode, "ses_stable123", root.path());
    target.active_thread_id = Some("ses_parent123".to_owned());
    let mut options = ConsultationOptions::new(executable);
    options.opencode_database = Some(database);
    options.timeout = Duration::from_secs(5);
    let mut side = Consultation::open(&target, options).unwrap();
    let error = side.ask("question").unwrap_err();
    assert_eq!(error.receipt.delivery, Delivery::NotSent);
    assert_eq!(error.receipt.cleanup, Cleanup::Unknown);
    assert!(!error.receipt.retry_safe);
    assert_eq!(side.child_id(), None);
    let cleanup = side.close().unwrap_err();
    assert!(cleanup.to_string().contains("identity is unknown"));
    let log = fs::read_to_string(log).unwrap();
    assert!(!log.contains("session delete"));
}

#[test]
fn fake_opencode_server() {
    let Ok(port) = std::env::var("PIKA_FAKE_PORT") else {
        return;
    };
    let database = PathBuf::from(std::env::var("PIKA_FAKE_DB").unwrap());
    let listener = TcpListener::bind(("127.0.0.1", port.parse::<u16>().unwrap())).unwrap();
    for request_number in 0..2 {
        let (mut stream, _) = listener.accept().unwrap();
        let request = read_http_request(&mut stream);
        assert!(request.contains("Authorization: Basic "));
        let body = if request_number == 0 {
            assert!(request.starts_with("GET /global/health "));
            r#"{"healthy":true}"#.to_owned()
        } else {
            assert!(request.starts_with("POST /session/ses_parent123/fork"));
            let cwd = std::env::var("PIKA_FAKE_CWD").unwrap();
            let child = std::env::var("PIKA_FAKE_CHILD").unwrap();
            if child.starts_with("ses_") && child != "ses_parent123" {
                let db = Connection::open(&database).unwrap();
                db.execute(
                    "INSERT INTO session VALUES(?,'ses_parent123',2,NULL,?)",
                    params![child, cwd],
                )
                .unwrap();
            }
            serde_json::json!({"id":child,"directory":cwd}).to_string()
        };
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
        .unwrap();
        stream.flush().unwrap();
    }
}

#[test]
fn fake_opencode_turn() {
    let Ok(database) = std::env::var("PIKA_FAKE_DB") else {
        return;
    };
    let session_id = std::env::var("PIKA_FAKE_SESSION").unwrap();
    let db = Connection::open(database).unwrap();
    let prior: i64 = db
        .query_row(
            "SELECT COUNT(*) FROM message WHERE session_id=?",
            [&session_id],
            |row| row.get(0),
        )
        .unwrap();
    let turn = prior / 2 + 1;
    let user = format!("user-{turn}");
    let assistant = format!("assistant-{turn}");
    let time = 10 + turn * 10;
    db.execute(
        "INSERT INTO message VALUES(?,?,?,json_object('role','user'))",
        params![user, session_id, time],
    )
    .unwrap();
    db.execute(
        "INSERT INTO message VALUES(?,?,?,json_object('role','assistant','parentID',?,'time',json_object('completed',?),'finish','stop'))",
        params![assistant, session_id, time + 1, user, time + 1],
    )
    .unwrap();
    db.execute(
        "INSERT INTO part VALUES(?,?,?, ?,json_object('type','text','text',?))",
        params![
            format!("part-{turn}"),
            session_id,
            assistant,
            time + 1,
            format!("opencode-answer-{turn}")
        ],
    )
    .unwrap();
}

#[test]
fn fake_opencode_delete() {
    let Ok(database) = std::env::var("PIKA_FAKE_DB") else {
        return;
    };
    let session_id = std::env::var("PIKA_FAKE_SESSION").unwrap();
    let db = Connection::open(database).unwrap();
    db.execute("DELETE FROM part WHERE session_id=?", [&session_id])
        .unwrap();
    db.execute("DELETE FROM message WHERE session_id=?", [&session_id])
        .unwrap();
    db.execute("DELETE FROM session WHERE id=?", [&session_id])
        .unwrap();
}

fn read_http_request(stream: &mut TcpStream) -> String {
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        let size = stream.read(&mut chunk).unwrap();
        if size == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..size]);
        if let Some(split) = bytes.windows(4).position(|value| value == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&bytes[..split]);
            let length = head
                .lines()
                .find_map(|line| {
                    line.strip_prefix("Content-Length: ")
                        .and_then(|value| value.parse::<usize>().ok())
                })
                .unwrap_or(0);
            if bytes.len() >= split + 4 + length {
                break;
            }
        }
    }
    String::from_utf8(bytes).unwrap()
}
