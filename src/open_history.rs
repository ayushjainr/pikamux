//! Durable `pika -` history for every successfully attached open target.

use crate::{
    core::OpenTarget,
    model::Provider,
    store::{PendingLaunch, Store},
};
use anyhow::Result;

pub fn record_session(store: &Store, provider: Provider, session_id: &str) -> Result<()> {
    store.record_attach(provider, session_id)
}

pub fn record_pending(store: &Store, pending: &PendingLaunch) -> Result<()> {
    if let Some(session_id) = pending.expected_session_id.as_deref() {
        record_session(store, pending.provider, session_id)
    } else {
        store.set_meta(&format!("attached_launch:{}", pending.launch_token), "1")
    }
}

pub fn record(store: &Store, target: &OpenTarget) -> Result<()> {
    match target {
        OpenTarget::Session(session) => {
            record_session(store, session.provider, &session.session_id)
        }
        OpenTarget::Pending(pending) => record_pending(store, pending),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Provider, Session, Status};
    use crate::store::PendingLaunch;

    fn session(id: &str) -> Session {
        Session {
            provider: Provider::Codex,
            session_id: id.into(),
            name: Some(id.into()),
            cwd: None,
            branch: None,
            transcript_path: None,
            tmux_session: None,
            tmux_pane: None,
            root_pid: None,
            status: Status::Working,
            unread: false,
            model: None,
            source: "test".into(),
            managed: true,
            error: None,
            attention_reason: None,
            created_at: 0.0,
            updated_at: 0.0,
            last_event_at: 0.0,
            last_activity_at: 0.0,
            live: false,
            attached: false,
            home_state: "missing".into(),
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

    fn pending(expected: Option<&str>) -> PendingLaunch {
        PendingLaunch {
            launch_token: "launch-one".into(),
            provider: Provider::Claude,
            name: "new_thread".into(),
            cwd: "/work".into(),
            tmux_session: Some("pika-c-new".into()),
            tmux_pane: Some("%1".into()),
            expected_session_id: expected.map(str::to_owned),
            root_pid: Some(1),
            root_pid_start: Some(1),
            preexisting_session_ids: Some(Vec::new()),
            candidate_session_id: None,
            candidate_observed_at: None,
            created_at: 0.0,
        }
    }

    #[test]
    fn saved_a_then_b_then_previous_toggles_back_to_b() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::at(temp.path().join("pika.db"));
        record(&store, &OpenTarget::Session(Box::new(session("saved-a")))).unwrap();
        record(&store, &OpenTarget::Session(Box::new(session("saved-b")))).unwrap();
        let previous = store.previous_attached().unwrap().unwrap();
        assert_eq!(previous, (Provider::Codex, "saved-a".into()));
        record_session(&store, previous.0, &previous.1).unwrap();
        assert_eq!(
            store.previous_attached().unwrap(),
            Some((Provider::Codex, "saved-b".into()))
        );
    }

    #[test]
    fn known_new_identity_participates_in_previous_history() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::at(temp.path().join("pika.db"));
        record(
            &store,
            &OpenTarget::Pending(Box::new(pending(Some(
                "cccccccc-cccc-4ccc-8ccc-cccccccccccc",
            )))),
        )
        .unwrap();
        record(&store, &OpenTarget::Session(Box::new(session("saved-a")))).unwrap();
        assert_eq!(
            store.previous_attached().unwrap(),
            Some((
                Provider::Claude,
                "cccccccc-cccc-4ccc-8ccc-cccccccccccc".into()
            ))
        );
    }

    #[test]
    fn unknown_new_identity_sets_only_the_scoped_hook_handoff() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::at(temp.path().join("pika.db"));
        record(&store, &OpenTarget::Pending(Box::new(pending(None)))).unwrap();
        assert_eq!(
            store
                .get_meta("attached_launch:launch-one")
                .unwrap()
                .as_deref(),
            Some("1")
        );
        assert_eq!(store.previous_attached().unwrap(), None);
    }
}
