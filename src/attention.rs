//! One oldest-first attention queue across local and cached remote sessions.

use crate::{fleet::FleetSession, model::Session};

#[derive(Clone, Debug)]
pub enum AttentionTarget {
    Local(Box<Session>),
    Remote(Box<FleetSession>),
}

pub fn choose(local: Vec<Session>, remote: Vec<FleetSession>) -> Option<AttentionTarget> {
    let mut candidates = local
        .into_iter()
        .filter(Session::needs_attention)
        .map(|session| AttentionTarget::Local(Box::new(session)))
        .chain(
            remote
                .into_iter()
                .filter(FleetSession::needs_attention)
                .map(|session| AttentionTarget::Remote(Box::new(session))),
        )
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| {
        key(left)
            .0
            .cmp(&key(right).0)
            .then_with(|| key(left).1.total_cmp(&key(right).1))
            .then_with(|| key(left).2.cmp(&key(right).2))
    });
    candidates.into_iter().next()
}

fn key(item: &AttentionTarget) -> (u8, f64, String) {
    match item {
        AttentionTarget::Local(session) => (
            session.status.attention_order(),
            session.last_activity_at,
            session.display_name().to_lowercase(),
        ),
        AttentionTarget::Remote(remote) => (
            remote.session.status.attention_order(),
            remote.session.last_activity_at,
            remote.qualified_name().to_lowercase(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Provider, Status};

    fn session(name: &str, status: Status, activity: f64) -> Session {
        Session {
            provider: Provider::Codex,
            session_id: format!("id-{name}"),
            name: Some(name.into()),
            cwd: None,
            branch: None,
            transcript_path: None,
            tmux_session: None,
            tmux_pane: None,
            root_pid: None,
            status,
            unread: true,
            model: None,
            source: "test".into(),
            managed: true,
            error: None,
            attention_reason: None,
            created_at: 0.0,
            updated_at: activity,
            last_event_at: activity,
            last_activity_at: activity,
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

    fn remote(name: &str, activity: f64, stale: bool) -> FleetSession {
        FleetSession {
            node_id: "node-id".into(),
            node_name: "research-node".into(),
            session: session(name, Status::NeedsYou, activity),
            stale,
            remote_error: None,
            seen_at: activity,
            card_status: None,
            card_detail: None,
            watched: true,
            availability: Some("available".into()),
            scope_updated_at: None,
            current_state_updated_at: None,
            current_state_status: None,
        }
    }

    #[test]
    fn fresh_cached_remote_participates_in_the_same_oldest_first_queue() {
        let chosen = choose(
            vec![session("local", Status::NeedsYou, 20.0)],
            vec![remote("remote", 10.0, false)],
        )
        .unwrap();
        assert!(matches!(chosen, AttentionTarget::Remote(_)));
    }

    #[test]
    fn stale_remote_is_never_actionable_and_priority_precedes_age() {
        let chosen = choose(
            vec![session("local", Status::Error, 20.0)],
            vec![
                remote("stale", 1.0, true),
                remote("needs-input", 30.0, false),
            ],
        )
        .unwrap();
        assert!(matches!(chosen, AttentionTarget::Remote(_)));
    }
}
