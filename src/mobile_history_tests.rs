use super::*;
use std::io::Write;

const CLAUDE: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";

#[test]
fn claude_logical_cursor_refuses_late_projection_change() {
    let temp = tempfile::tempdir().unwrap();
    let mut file = File::create(temp.path().join("source")).unwrap();
    let mut cursor = state(&mut file, Provider::Claude, CLAUDE);
    let records: Vec<_> = (0..85).map(|index|json!({"sessionId":CLAUDE,"type":"user","uuid":format!("record-{index}"),"message":{"role":"user","content":format!("literal-{index}")}})).collect();
    let projection = BTreeMap::new();
    let (_, more) = claude_page(&records, &mut cursor, &projection, true).unwrap();
    assert!(more);
    let mut changed = BTreeMap::new();
    changed.insert(
        "record-1".into(),
        vec![json!({"id":"native-reply","type":"agentMessage","text":"Late attested projection"})],
    );
    assert!(
        claude_page(&records, &mut cursor, &changed, false)
            .unwrap_err()
            .to_string()
            .contains("projection changed")
    );
}

fn state(file: &mut File, provider: Provider, id: &str) -> Cursor {
    let meta = file.metadata().unwrap();
    let prefix_len = meta.len().min(4096) as usize;
    Cursor {
        provider,
        id: id.into(),
        dev: meta.dev(),
        inode: meta.ino(),
        snapshot: meta.len(),
        before: meta.len(),
        prefix_len,
        prefix_hash: prefix_hash(file, prefix_len).unwrap(),
        sql_before: None,
        displayed_runs: BTreeSet::new(),
        claude_before: None,
        claude_items_hash: None,
    }
}

#[test]
fn claude_paging_is_chronological_literal_and_append_stable() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("history.jsonl");
    let mut output = File::create(&path).unwrap();
    for index in 0..85 {
        writeln!(output,"{}",json!({"sessionId":CLAUDE,"isSidechain":false,"uuid":format!("msg-{index}"),"type":"user","message":{"role":"user","content":[{"type":"text","text":format!("  literal {index}\nsecond line  ")},{"type":"tool_result","content":"hidden"}]}})).unwrap();
    }
    let mut file = File::open(&path).unwrap();
    let mut cursor = state(&mut file, Provider::Claude, CLAUDE);
    let (latest, more) = jsonl_page(&mut file, &mut cursor).unwrap();
    assert!(more);
    assert_eq!(latest.first().unwrap()["id"], "msg-45");
    assert_eq!(latest.last().unwrap()["id"], "msg-84");
    assert_eq!(latest[0]["text"], "  literal 45\nsecond line  ");
    writeln!(output,"{}",json!({"sessionId":CLAUDE,"uuid":"new","type":"user","message":{"role":"user","content":"new"}})).unwrap();
    let (older, more) = jsonl_page(&mut file, &mut cursor).unwrap();
    assert!(more);
    assert_eq!(older[0]["id"], "msg-5");
    assert_eq!(older[39]["id"], "msg-44");
    let (oldest, more) = jsonl_page(&mut file, &mut cursor).unwrap();
    assert!(!more);
    assert_eq!(oldest.len(), 5);
}

#[test]
fn excludes_foreign_subagent_tool_and_thinking_content() {
    let fixture = json!({"sessionId":CLAUDE,"isSidechain":false,"uuid":"one","type":"assistant","message":{"role":"assistant","content":[{"type":"thinking","thinking":"secret"},{"type":"text","text":"visible"},{"type":"tool_use","name":"shell"}]}});
    assert_eq!(
        jsonl_item(&fixture, Provider::Claude, CLAUDE)
            .unwrap()
            .unwrap()["text"],
        "visible"
    );
    assert!(
        jsonl_item(
            &fixture,
            Provider::Claude,
            "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb"
        )
        .unwrap()
        .is_none()
    );
    let mut side = fixture;
    side["isSidechain"] = json!(true);
    assert!(
        jsonl_item(&side, Provider::Claude, CLAUDE)
            .unwrap()
            .is_none()
    );
}

#[test]
fn muse_uses_exact_root_durable_messages_not_background_ingress() {
    let mut fixture = json!({"schema_version":1,"id":"event-one","stream":{"kind":"session","id":CLAUDE},"payload_type":"runtime.session","payload":{"event":{"kind":"started","prompt":"exact\ntext"}}});
    assert_eq!(
        jsonl_item(&fixture, Provider::Muse, CLAUDE)
            .unwrap()
            .unwrap()["text"],
        "exact\ntext"
    );
    fixture["payload"]["event"] =
        json!({"kind":"inbox_item_queued","payload":{"prompt":"background"}});
    assert!(
        jsonl_item(&fixture, Provider::Muse, CLAUDE)
            .unwrap()
            .is_none()
    );
    fixture["payload"]["event"] = json!({"kind":"assistant_message_committed","text":"answer"});
    assert_eq!(
        jsonl_item(&fixture, Provider::Muse, CLAUDE)
            .unwrap()
            .unwrap()["type"],
        "agentMessage"
    );
    fixture["stream"]["kind"] = json!("subagent");
    assert!(
        jsonl_item(&fixture, Provider::Muse, CLAUDE)
            .unwrap()
            .is_none()
    );
}

#[test]
fn symlinked_history_and_unstable_message_identity_fail_closed() {
    let temp = tempfile::tempdir().unwrap();
    let file = temp.path().join("original");
    File::create(&file).unwrap();
    let alias = temp.path().join("alias");
    std::os::unix::fs::symlink(&file, &alias).unwrap();
    assert!(verify_source(temp.path(), &alias).is_err());
    assert!(item("", true, "body".into()).is_err());
    assert!(item("stable", true, "x".repeat(RECORD + 1)).is_err());
}

#[test]
fn opencode_exact_sql_snapshot_pages_exclude_other_sessions_and_tools() {
    let temp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(temp.path()).unwrap();
    let path = root.join("opencode.db");
    let db = Connection::open(&path).unwrap();
    db.execute_batch("CREATE TABLE session(id TEXT PRIMARY KEY,parent_id TEXT); CREATE TABLE message(id TEXT PRIMARY KEY,session_id TEXT,time_created INTEGER,data TEXT); CREATE TABLE part(id TEXT PRIMARY KEY,session_id TEXT,message_id TEXT,time_created INTEGER,data TEXT);").unwrap();
    db.execute("INSERT INTO session VALUES('ses_exact',NULL)", [])
        .unwrap();
    for index in 0..45 {
        let id = format!("msg-{index:03}");
        db.execute(
            "INSERT INTO message VALUES(?,'ses_exact',?,?)",
            params![id, index, json!({"role":"assistant"}).to_string()],
        )
        .unwrap();
        db.execute(
            "INSERT INTO part VALUES(?,'ses_exact',?,?,?)",
            params![
                format!("part-{index}"),
                id,
                index,
                json!({"type":"text","text":format!("answer {index}\n")}).to_string()
            ],
        )
        .unwrap();
    }
    db.execute(
        "INSERT INTO message VALUES('foreign','ses_other',999,'{\"role\":\"user\"}')",
        [],
    )
    .unwrap();
    let mut file = File::open(&path).unwrap();
    let mut cursor = state(&mut file, Provider::Opencode, "ses_exact");
    let (latest, more) = sql_page(&path, "ses_exact", &mut cursor, true).unwrap();
    assert!(more);
    assert_eq!(latest.len(), 40);
    assert_eq!(latest[0]["id"], "msg-005");
    db.execute(
        "INSERT INTO message VALUES('new','ses_exact',999,'{\"role\":\"user\"}')",
        [],
    )
    .unwrap();
    let (older, more) = sql_page(&path, "ses_exact", &mut cursor, false).unwrap();
    assert!(!more);
    assert_eq!(older.len(), 5);
    assert_eq!(older[0]["id"], "msg-000");
    assert_eq!(older[4]["id"], "msg-004");
}

#[test]
fn muse_display_wins_across_page_boundary_and_only_user_steers_render() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("session.jsonl");
    let mut output = File::create(&path).unwrap();
    let record = |id: &str, event: Value| json!({"schema_version":1,"id":id,"stream":{"kind":"session","id":CLAUDE},"payload_type":"runtime.session","payload":{"run_id":"run-one","event":event}});
    writeln!(
        output,
        "{}",
        record(
            "start",
            json!({"kind":"started","prompt":"model form with hidden expansion"})
        )
    )
    .unwrap();
    writeln!(
        output,
        "{}",
        record(
            "display",
            json!({"kind":"user_prompt_display","text":"visible original [Image 1]"})
        )
    )
    .unwrap();
    for index in 0..39 {
        writeln!(
            output,
            "{}",
            record(
                &format!("reply-{index}"),
                json!({"kind":"assistant_message_committed","text":format!("reply {index}")})
            )
        )
        .unwrap();
    }
    let mut file = File::open(&path).unwrap();
    let mut cursor = state(&mut file, Provider::Muse, CLAUDE);
    let (latest, more) = jsonl_page(&mut file, &mut cursor).unwrap();
    assert!(more);
    assert_eq!(latest[0]["text"], "visible original [Image 1]");
    assert_eq!(latest[0]["id"], "muse-user-run-one");
    let (older, more) = jsonl_page(&mut file, &mut cursor).unwrap();
    assert!(!more);
    assert!(older.is_empty());
    for (source, expected) in [
        ("user_steer", true),
        ("background", false),
        ("external_agent", false),
    ] {
        let queued = record(
            "queued",
            json!({"kind":"inbox_item_queued","source":{"source":source},"payload":{"prompt":"exact queued\nreply"}}),
        );
        let item = jsonl_item(&queued, Provider::Muse, CLAUDE).unwrap();
        assert_eq!(item.is_some(), expected);
        if let Some(item) = item {
            assert_eq!(item["text"], "exact queued\nreply");
        }
    }
}

#[test]
fn sqlite_readonly_wal_reader_coordination_is_not_a_database_write() {
    let temp = tempfile::tempdir().unwrap();
    let root = fs::canonicalize(temp.path()).unwrap();
    let live = root.join("live.db");
    let writer = Connection::open(&live).unwrap();
    writer.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; CREATE TABLE session(id TEXT,parent_id TEXT); CREATE TABLE message(id TEXT,session_id TEXT,time_created INTEGER,data TEXT); CREATE TABLE part(id TEXT,session_id TEXT,message_id TEXT,time_created INTEGER,data TEXT); INSERT INTO session VALUES('ses_fixture',NULL);").unwrap();
    let clone = root.join("snapshot.db");
    fs::copy(&live, &clone).unwrap();
    fs::copy(root.join("live.db-wal"), root.join("snapshot.db-wal")).unwrap();
    let before = fs::read(&clone).unwrap();
    let wal_before = fs::read(root.join("snapshot.db-wal")).unwrap();
    let db = sql_connection(&clone, "ses_fixture").unwrap();
    assert_eq!(fs::read(&clone).unwrap(), before);
    assert_eq!(fs::read(root.join("snapshot.db-wal")).unwrap(), wal_before);
    // SQLite's READ_ONLY connection can create shared-memory coordination.
    assert!(root.join("snapshot.db-shm").exists());
    assert!(db.execute("DELETE FROM session", []).is_err());
}

#[test]
fn claude_rewind_and_compaction_never_return_append_order_as_native_history() {
    for special in [
        json!({"uuid":"C","parentUuid":"A","sessionId":CLAUDE}),
        json!({"uuid":"compact","parentUuid":"B","sessionId":CLAUDE,"subtype":"compact_boundary","compactMetadata":{"preservedMessages":["A"],"preservedSegment":{"headUuid":"A","tailUuid":"B"}}}),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("history.jsonl");
        let mut output = File::create(&path).unwrap();
        for record in [
            json!({"uuid":"A","parentUuid":null,"sessionId":CLAUDE}),
            json!({"uuid":"B","parentUuid":"A","sessionId":CLAUDE}),
            special,
        ] {
            writeln!(output, "{record}").unwrap();
        }
        let mut file = File::open(path).unwrap();
        let cursor = state(&mut file, Provider::Claude, CLAUDE);
        assert!(verify_claude_chain(&mut file, &cursor).is_err());
    }
}
