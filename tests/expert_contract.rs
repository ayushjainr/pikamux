use pikamux::experts::{
    CardStatus, PublishInput, PublisherProof, card_state, make_profile, profile_freshness, publish,
    publish_current_work, rank_experts, transcript_fingerprint,
};
use pikamux::model::{Provider, Session, Status};
use pikamux::store::{Store, StoredExpertProfile};
use rusqlite::Connection;
use std::collections::BTreeSet;
use std::fs;
use tempfile::tempdir;

fn session(provider: Provider, id: &str) -> Session {
    Session {
        provider,
        session_id: id.to_owned(),
        name: Some("research".to_owned()),
        cwd: Some("/work/factor_attribution".to_owned()),
        branch: Some("main".to_owned()),
        transcript_path: None,
        tmux_session: None,
        tmux_pane: None,
        root_pid: None,
        status: Status::Parked,
        unread: false,
        model: None,
        source: "test".to_owned(),
        managed: true,
        error: None,
        attention_reason: None,
        created_at: 1.0,
        updated_at: 1.0,
        last_event_at: 1.0,
        last_activity_at: 1.0,
        live: false,
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
        active_thread_id: None,
    }
}

fn input() -> PublishInput {
    PublishInput {
        scope: "  Built   the production factor-attribution pipeline. ".to_owned(),
        current_state: " Validating  a live attribution mismatch. ".to_owned(),
        topics: vec![
            "factor attribution".to_owned(),
            "Factor Attribution".to_owned(),
            "portfolio analytics".to_owned(),
        ],
        artifacts: vec![" reports/attribution.md ".to_owned()],
        source: "self".to_owned(),
    }
}

#[test]
fn card_input_is_cleaned_without_inventing_content() {
    let profile = make_profile(&session(Provider::Codex, "one"), input()).unwrap();
    assert_eq!(
        profile.summary,
        "Built the production factor-attribution pipeline."
    );
    assert_eq!(
        profile.current_state,
        "Validating a live attribution mismatch."
    );
    assert_eq!(
        profile.topics,
        vec!["factor attribution", "portfolio analytics"]
    );
    assert_eq!(profile.artifacts, vec!["reports/attribution.md"]);

    let mut invalid = input();
    invalid.topics.clear();
    assert!(
        make_profile(&session(Provider::Codex, "one"), invalid)
            .unwrap_err()
            .to_string()
            .contains("at least one expert topic")
    );
}

#[test]
fn search_requires_all_lexical_terms_and_explains_ranking() {
    let mut first = session(Provider::Codex, "one");
    first.name = Some("factor_weights".to_owned());
    first.status = Status::Working;
    first.live = true;
    let mut second = session(Provider::Claude, "two");
    second.cwd = Some("/work/research".to_owned());
    let profiles = vec![
        StoredExpertProfile {
            profile: make_profile(&first, input()).unwrap(),
            transcript_mtime_ns: None,
            transcript_size: None,
            current_state_mtime_ns: None,
            current_state_size: None,
        },
        StoredExpertProfile {
            profile: make_profile(
                &second,
                PublishInput {
                    scope: "Studied factor definitions.".to_owned(),
                    current_state: "No active task.".to_owned(),
                    topics: vec!["factor research".to_owned()],
                    ..PublishInput::default()
                },
            )
            .unwrap(),
            transcript_mtime_ns: None,
            transcript_size: None,
            current_state_mtime_ns: None,
            current_state_size: None,
        },
    ];
    let matches = rank_experts(
        &profiles,
        &[first.clone(), second],
        "factor attribution",
        &BTreeSet::new(),
    );
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].session_id, "one");
    assert!(matches[0].matched_on.contains(&"topic".to_owned()));
    assert!(matches[0].matched_on.contains(&"scope".to_owned()));
    assert_eq!(
        rank_experts(&profiles, &[first], "the", &BTreeSet::new()),
        Vec::new()
    );
    assert_eq!(
        rank_experts(
            &profiles,
            &[session(Provider::Codex, "one")],
            "factor unrelated",
            &BTreeSet::new()
        ),
        Vec::new()
    );
}

#[test]
fn exact_publisher_cannot_cross_workstreams_or_active_leaves() {
    let root = tempdir().unwrap();
    let transcript = root.path().join("thread.jsonl");
    fs::write(&transcript, b"history\n").unwrap();
    let mut exact = session(Provider::Codex, "stable-id");
    exact.active_thread_id = Some("active-leaf".to_owned());
    exact.transcript_path = Some(transcript.to_string_lossy().into_owned());
    let mut other = session(Provider::Codex, "other-id");
    other.transcript_path = exact.transcript_path.clone();
    let store = Store::at(root.path().join("pika.db"));
    store.initialize().unwrap();
    store.upsert_session(&exact, true).unwrap();
    store.upsert_session(&other, true).unwrap();

    assert!(
        PublisherProof::from_verified_identity(&exact, Provider::Codex, "stable-id", "wrong-leaf")
            .is_err()
    );
    let proof =
        PublisherProof::from_verified_identity(&exact, Provider::Codex, "stable-id", "active-leaf")
            .unwrap();
    assert!(publish(&store, &other, &proof, input()).is_err());
    let published = publish(&store, &exact, &proof, input()).unwrap();
    assert_eq!(published.profile.session_id, "stable-id");
    assert_eq!(published.profile.source, "self");
    assert!(
        store
            .get_stored_expert_profile(Provider::Codex, "other-id")
            .unwrap()
            .is_none()
    );
}

#[test]
fn current_work_is_separate_deduplicated_and_requires_a_card() {
    let root = tempdir().unwrap();
    let transcript = root.path().join("thread.jsonl");
    fs::write(&transcript, b"history\n").unwrap();
    let mut current = session(Provider::Codex, "one");
    current.transcript_path = Some(transcript.to_string_lossy().into_owned());
    let store = Store::at(root.path().join("pika.db"));
    store.initialize().unwrap();
    store.upsert_session(&current, true).unwrap();
    let proof =
        PublisherProof::from_verified_identity(&current, Provider::Codex, "one", "one").unwrap();
    assert!(publish_current_work(&store, &current, &proof, "checkpoint").is_err());
    let original = publish(&store, &current, &proof, input()).unwrap();
    let changed = publish_current_work(&store, &current, &proof, " Awaiting   review. ").unwrap();
    assert_eq!(changed.profile.summary, original.profile.summary);
    assert_eq!(changed.profile.current_state, "Awaiting review.");
    assert_eq!(
        changed.profile.scope_updated_at,
        original.profile.scope_updated_at
    );
    let duplicate = publish_current_work(&store, &current, &proof, "Awaiting review.").unwrap();
    assert_eq!(duplicate, changed);
}

#[test]
fn transcript_growth_stales_work_but_not_durable_publication_age() {
    let root = tempdir().unwrap();
    let transcript = root.path().join("thread.jsonl");
    fs::write(&transcript, b"history\n").unwrap();
    let mut current = session(Provider::Codex, "one");
    current.transcript_path = Some(transcript.to_string_lossy().into_owned());
    let store = Store::at(root.path().join("pika.db"));
    store.initialize().unwrap();
    store.upsert_session(&current, true).unwrap();
    let proof =
        PublisherProof::from_verified_identity(&current, Provider::Codex, "one", "one").unwrap();
    let profile = publish(&store, &current, &proof, input()).unwrap();
    assert_eq!(
        card_state(&current, Some(&profile)).status,
        CardStatus::Current
    );
    let scope_time = profile.profile.scope_updated_at;
    fs::write(&transcript, b"history\nmore work\n").unwrap();
    assert_eq!(
        card_state(&current, Some(&profile)).status,
        CardStatus::Stale
    );
    let freshness = profile_freshness(&current, Some(&profile), scope_time + 50.0);
    assert_eq!(freshness.scope_updated_at, Some(scope_time));
    assert_eq!(freshness.scope_age_seconds, Some(50.0));
    assert_eq!(freshness.current_state_status, CardStatus::Stale);
}

#[test]
fn opencode_fingerprint_is_scoped_to_parent_tree() {
    let root = tempdir().unwrap();
    let database = root.path().join("opencode.db");
    let db = Connection::open(&database).unwrap();
    db.execute_batch(
        "CREATE TABLE session(id TEXT PRIMARY KEY,parent_id TEXT,time_updated INTEGER,time_archived INTEGER);\
         CREATE TABLE message(id TEXT PRIMARY KEY,session_id TEXT,time_created INTEGER,data TEXT);\
         CREATE TABLE part(id TEXT PRIMARY KEY,session_id TEXT,message_id TEXT,time_created INTEGER,data TEXT);\
         INSERT INTO session VALUES('ses_one',NULL,100,NULL);\
         INSERT INTO session VALUES('ses_other',NULL,100,NULL);",
    )
    .unwrap();
    drop(db);
    let mut current = session(Provider::Opencode, "ses_one");
    current.transcript_path = Some(database.to_string_lossy().into_owned());
    let before = transcript_fingerprint(&current).unwrap();
    let db = Connection::open(&database).unwrap();
    db.execute(
        "UPDATE session SET time_updated=200 WHERE id='ses_other'",
        [],
    )
    .unwrap();
    db.execute(
        "INSERT INTO message VALUES('m_other','ses_other',200,'{}')",
        [],
    )
    .unwrap();
    drop(db);
    assert_eq!(transcript_fingerprint(&current).unwrap(), before);
    let db = Connection::open(&database).unwrap();
    db.execute(
        "INSERT INTO session VALUES('ses_child','ses_one',300,NULL)",
        [],
    )
    .unwrap();
    db.execute(
        "INSERT INTO message VALUES('m_child','ses_child',300,'{}')",
        [],
    )
    .unwrap();
    db.execute(
        "INSERT INTO part VALUES('p_child','ses_child','m_child',300,'{}')",
        [],
    )
    .unwrap();
    drop(db);
    assert_ne!(transcript_fingerprint(&current).unwrap(), before);
}

#[test]
fn unwatched_card_remains_discoverable_without_retracking() {
    let root = tempdir().unwrap();
    let transcript = root.path().join("thread.jsonl");
    fs::write(&transcript, b"history\n").unwrap();
    let mut current = session(Provider::Codex, "one");
    current.transcript_path = Some(transcript.to_string_lossy().into_owned());
    let store = Store::at(root.path().join("pika.db"));
    store.initialize().unwrap();
    store.upsert_session(&current, true).unwrap();
    let proof =
        PublisherProof::from_verified_identity(&current, Provider::Codex, "one", "one").unwrap();
    publish(&store, &current, &proof, input()).unwrap();
    store.untrack_session(Provider::Codex, "one").unwrap();
    let profiles = store.list_stored_expert_profiles().unwrap();
    let sessions = store.list_untracked_sessions().unwrap();
    let untracked = BTreeSet::from([(Provider::Codex, "one".to_owned())]);
    let matches = rank_experts(&profiles, &sessions, "factor attribution", &untracked);
    assert_eq!(matches.len(), 1);
    assert!(!matches[0].watched);
    assert!(matches[0].discoverable);
    assert!(store.list_sessions().unwrap().is_empty());
}
