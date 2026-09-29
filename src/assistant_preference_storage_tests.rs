use super::*;
use crate::assistant_preferences::recognize;

fn human(body: &str, project: &str) -> NewRecord {
    NewRecord {
        kind: RecordKind::Finding,
        origin: Origin::Human,
        scope: Scope {
            project: Some(project.into()),
            ..Default::default()
        },
        body: body.into(),
        provenance:
            serde_json::json!({"type":"submitted_user_message","authority":"conversation_only"})
                .to_string(),
        timestamp: 1,
        supersedes: None,
        dependencies: vec![],
        decision_state: None,
        protected_policy: false,
    }
}

fn save(store: &mut Store, id: &str, body: &str, project: &str) -> Record {
    store
        .append_conversation_input(
            id,
            human(body, project),
            recognize(body),
            store.forget_epoch().unwrap(),
        )
        .unwrap()
}

#[test]
fn revisions_are_exact_scoped_retry_safe_and_forget_does_not_revive_the_old_setting() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("private/memory.sqlite");
    let mut store = Store::open(&path).unwrap();
    let five = "From now on daily briefs should have five bullets.";
    let three = "From now on daily briefs should have three bullets, not five.";
    let old = save(&mut store, "original", five, "alpha");
    let other = save(&mut store, "other", five, "beta");
    let new = save(&mut store, "correction", three, "alpha");
    assert_eq!(new.body, three);
    assert_eq!(new.origin, Origin::Human);
    assert_eq!(new.kind, RecordKind::UserInstruction);
    assert_eq!(new.supersedes.as_ref(), Some(&old.id));
    assert_eq!(new.dependencies, vec![old.id.clone()]);
    assert!(!new.protected_policy);
    assert_eq!(
        store.working_set(&new.scope, 32).unwrap(),
        vec![new.clone()]
    );
    assert_eq!(
        store.working_set(&other.scope, 32).unwrap(),
        vec![other.clone()]
    );
    // A retry after another process/restart observes a later revision must
    // return its old receipt without superseding the new active preference.
    drop(store);
    let mut store = Store::open(&path).unwrap();
    assert_eq!(save(&mut store, "original", five, "alpha"), old);
    assert_eq!(
        store.working_set(&new.scope, 32).unwrap(),
        vec![new.clone()]
    );
    let mut answer = human("Derived answer", "alpha");
    answer.origin = Origin::Worker;
    answer.dependencies = vec![new.id.clone()];
    let derived = store.append(answer).unwrap();
    assert_eq!(store.forget(&new.id).unwrap(), 3);
    for id in [&old.id, &new.id, &derived.id] {
        assert!(store.get(id).unwrap().is_none());
    }
    assert!(
        store
            .search_bm25(&new.scope, "daily briefs", 32)
            .unwrap()
            .is_empty()
    );
    assert!(store.get(&other.id).unwrap().is_some());
    assert!(
        store
            .append_conversation_input(
                "original",
                human(five, "alpha"),
                recognize(five),
                store.forget_epoch().unwrap()
            )
            .is_err()
    );
}

#[test]
fn presentation_revisions_never_cross_any_scope_dimension() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path().join("private/memory.sqlite")).unwrap();
    let body = "From now on daily briefs should have five bullets.";
    let mut base = human(body, "alpha");
    base.scope = Scope {
        project: Some("alpha".into()),
        provider: Some("codex".into()),
        conversation: Some("thread-a".into()),
        node: Some("node-a".into()),
    };
    let original = store
        .append_conversation_input("base", base.clone(), recognize(body), 0)
        .unwrap();
    for dimension in ["project", "provider", "conversation", "node"] {
        let mut other = base.clone();
        let slot = match dimension {
            "project" => &mut other.scope.project,
            "provider" => &mut other.scope.provider,
            "conversation" => &mut other.scope.conversation,
            _ => &mut other.scope.node,
        };
        *slot = Some("different".into());
        let saved = store
            .append_conversation_input(dimension, other, recognize(body), 0)
            .unwrap();
        assert!(saved.supersedes.is_none());
        assert!(saved.dependencies.is_empty());
        assert_eq!(store.working_set(&saved.scope, 32).unwrap(), vec![saved]);
    }
    assert_eq!(
        store.working_set(&original.scope, 32).unwrap(),
        vec![original]
    );
}

#[test]
fn one_offs_and_worker_inventions_cannot_become_human_instructions() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path().join("private/memory.sqlite")).unwrap();
    let body = "From now on daily briefs should have three bullets.";
    let today = save(&mut store, "today", "Three bullets today please", "alpha");
    assert_eq!(today.kind, RecordKind::Finding);
    assert!(today.supersedes.is_none());
    let mut worker = human(body, "alpha");
    worker.origin = Origin::Worker;
    assert!(
        store
            .append_conversation_input("worker", worker, recognize(body), 0)
            .is_err()
    );
    assert!(
        store
            .append_conversation_input("forged", human("unrelated", "alpha"), recognize(body), 0)
            .is_err()
    );
    let mut null_provenance = human(body, "alpha");
    null_provenance.provenance = "null".into();
    assert!(
        store
            .append_conversation_input("null", null_provenance, recognize(body), 0)
            .is_err()
    );
    assert_eq!(store.recent(&today.scope, 32).unwrap(), vec![today]);
}

#[test]
fn storage_failure_rolls_back_supersession_and_receipt_and_forget_fences_new_input() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("private/memory.sqlite");
    let mut store = Store::open(&path).unwrap();
    let old = save(
        &mut store,
        "five",
        "From now on daily briefs should have five bullets.",
        "alpha",
    );
    let body = "From now on daily briefs should have three bullets.";
    let db = Connection::open(&path).unwrap();
    db.execute_batch("CREATE TRIGGER fail_preference_receipt BEFORE INSERT ON memory_receipts WHEN NEW.request_id='three' BEGIN SELECT RAISE(ABORT,'injected receipt failure'); END;").unwrap();
    assert!(
        store
            .append_conversation_input("three", human(body, "alpha"), recognize(body), 0)
            .is_err()
    );
    assert_eq!(store.recent(&old.scope, 32).unwrap(), vec![old.clone()]);
    assert_eq!(
        store.working_set(&old.scope, 32).unwrap(),
        vec![old.clone()]
    );
    let receipts: i64 = db
        .query_row("SELECT count(*) FROM memory_receipts", [], |r| r.get(0))
        .unwrap();
    assert_eq!(receipts, 1);
    db.execute_batch("DROP TRIGGER fail_preference_receipt")
        .unwrap();
    let new = save(&mut store, "three", body, "alpha");
    assert_eq!(new.supersedes, Some(old.id.clone()));
    let stale_epoch = store.forget_epoch().unwrap();
    store.forget(&new.id).unwrap();
    assert!(
        store
            .append_conversation_input("late", human(body, "alpha"), recognize(body), stale_epoch)
            .is_err()
    );
    assert!(store.recent(&new.scope, 32).unwrap().is_empty());
}

#[test]
fn pre_upgrade_input_receipts_are_replayed_without_reclassifying_history() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path().join("private/memory.sqlite")).unwrap();
    let body = "From now on daily briefs should have three bullets.";
    let input = human(body, "alpha");
    let legacy = store.append_idempotent("legacy", input.clone()).unwrap();
    assert_eq!(legacy.kind, RecordKind::Finding);
    assert_eq!(
        store
            .append_conversation_input("legacy", input, recognize(body), 0)
            .unwrap(),
        legacy
    );
}
