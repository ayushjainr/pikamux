//! Scriptable attention-wait semantics shared with the frozen Python release.

use crate::model::{Session, Status};

pub const TIMEOUT_EXIT_CODE: i32 = 124;

pub fn matches(session: &Session, condition: &str) -> bool {
    match condition {
        "needs-you" => session.status == Status::NeedsYou,
        "ready" => session.status == Status::Ready && session.unread,
        "error" => session.unread && matches!(session.status, Status::Error | Status::OpenTwice),
        "any" => session.needs_attention() || (session.status == Status::Ready && session.unread),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Provider;

    fn session(status: Status, unread: bool) -> Session {
        Session {
            provider: Provider::Codex,
            session_id: "00000000-0000-4000-8000-000000000001".into(),
            name: Some("work".into()),
            cwd: None,
            branch: None,
            transcript_path: None,
            tmux_session: None,
            tmux_pane: None,
            root_pid: None,
            status,
            unread,
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

    #[test]
    fn ready_and_error_require_unread_but_needs_you_does_not() {
        assert!(!matches(&session(Status::Ready, false), "ready"));
        assert!(matches(&session(Status::Ready, true), "ready"));
        assert!(!matches(&session(Status::Error, false), "error"));
        assert!(matches(&session(Status::OpenTwice, true), "error"));
        assert!(matches(&session(Status::NeedsYou, false), "needs-you"));
    }

    #[test]
    fn any_is_only_actionable_or_unread_ready() {
        assert!(!matches(&session(Status::Working, true), "any"));
        assert!(!matches(&session(Status::Ready, false), "any"));
        assert!(matches(&session(Status::Ready, true), "any"));
        assert!(matches(&session(Status::Error, true), "any"));
        assert_eq!(TIMEOUT_EXIT_CODE, 124);
    }
}
