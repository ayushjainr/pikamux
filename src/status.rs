use crate::model::{ObservationKind, Status, StatusObservation};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StatusProjection {
    pub status: Status,
    pub unread: bool,
    pub attention_reason: Option<String>,
    pub error: Option<String>,
    pub observed_at: f64,
    pub source: String,
    pub kind: String,
    pub rule: String,
}

#[derive(Clone, Debug)]
pub struct ProjectionFallback<'a> {
    pub status: Status,
    pub unread: bool,
    pub attention_reason: Option<&'a str>,
    pub error: Option<&'a str>,
    pub observed_at: f64,
}

impl Default for ProjectionFallback<'_> {
    fn default() -> Self {
        Self {
            status: Status::Parked,
            unread: false,
            attention_reason: None,
            error: None,
            observed_at: 0.0,
        }
    }
}

pub fn project_status(
    observations: &[StatusObservation],
    live: bool,
    home_state: &str,
    fallback: ProjectionFallback<'_>,
) -> StatusProjection {
    let newest = |kind| {
        observations
            .iter()
            .filter(|item| item.kind == kind)
            .max_by(|a, b| a.observed_at.total_cmp(&b.observed_at))
    };
    let safety = newest(ObservationKind::Safety);
    if let Some(item) =
        safety.filter(|item| matches!(item.status, Status::OpenTwice | Status::Error))
    {
        return from_observation(item, item.status, item.unread, "safety_precedence");
    }

    let runtime = newest(ObservationKind::Runtime);
    if let Some(item) =
        runtime.filter(|item| !live && matches!(item.status, Status::Error | Status::OpenTwice))
    {
        return from_observation(
            item,
            item.status,
            item.unread,
            "runtime_failure_without_live_process",
        );
    }

    if let Some(item) = newest(ObservationKind::Lifecycle) {
        match item.status {
            Status::NeedsYou | Status::Error | Status::OpenTwice => {
                return from_observation(item, item.status, item.unread, "lifecycle");
            }
            Status::Working if live => {
                return from_observation(item, item.status, item.unread, "lifecycle");
            }
            Status::Working => {
                return normalized(item, Status::Parked, "working_process_gone");
            }
            Status::Ready if live || item.unread => {
                return from_observation(item, item.status, item.unread, "lifecycle");
            }
            Status::Ready => {
                return normalized(item, Status::Parked, "completed_and_collected_process_gone");
            }
            Status::Parked => {
                return from_observation(item, Status::Parked, false, "lifecycle");
            }
            Status::Starting | Status::Unbound => {}
        }
    }

    if home_state == "unbound" {
        return synthetic(
            Status::Unbound,
            false,
            fallback.observed_at,
            "ownership",
            "ownership",
            "unbound_without_lifecycle",
        );
    }
    if live {
        return synthetic(
            Status::Ready,
            false,
            fallback.observed_at,
            "process",
            "runtime",
            "live_without_turn_evidence",
        );
    }
    if matches!(
        fallback.status,
        Status::NeedsYou | Status::Ready | Status::Error | Status::OpenTwice
    ) {
        return StatusProjection {
            status: fallback.status,
            unread: fallback.unread,
            attention_reason: fallback.attention_reason.map(str::to_owned),
            error: fallback.error.map(str::to_owned),
            observed_at: fallback.observed_at,
            source: "legacy".into(),
            kind: "legacy".into(),
            rule: "legacy_observation".into(),
        };
    }
    synthetic(
        Status::Parked,
        false,
        fallback.observed_at,
        "process",
        "runtime",
        "no_live_process",
    )
}

fn from_observation(
    item: &StatusObservation,
    status: Status,
    unread: bool,
    rule: &str,
) -> StatusProjection {
    StatusProjection {
        status,
        unread,
        attention_reason: item.attention_reason.clone(),
        error: item.error.clone(),
        observed_at: item.observed_at,
        source: item.source.clone(),
        kind: match item.kind {
            ObservationKind::Lifecycle => "lifecycle",
            ObservationKind::Runtime => "runtime",
            ObservationKind::Safety => "safety",
        }
        .into(),
        rule: rule.into(),
    }
}

fn normalized(item: &StatusObservation, status: Status, rule: &str) -> StatusProjection {
    let mut projected = from_observation(item, status, false, rule);
    projected.attention_reason = None;
    projected.error = None;
    projected
}

fn synthetic(
    status: Status,
    unread: bool,
    observed_at: f64,
    source: &str,
    kind: &str,
    rule: &str,
) -> StatusProjection {
    StatusProjection {
        status,
        unread,
        attention_reason: None,
        error: None,
        observed_at,
        source: source.into(),
        kind: kind.into(),
        rule: rule.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obs(kind: ObservationKind, status: Status, unread: bool, time: f64) -> StatusObservation {
        StatusObservation {
            kind,
            status,
            unread,
            attention_reason: None,
            error: None,
            observed_at: time,
            source: "test".into(),
        }
    }

    fn project(values: &[StatusObservation], live: bool, home: &str) -> StatusProjection {
        project_status(values, live, home, ProjectionFallback::default())
    }

    #[test]
    fn safety_wins_over_newer_lifecycle() {
        let values = [
            obs(ObservationKind::Safety, Status::OpenTwice, true, 1.0),
            obs(ObservationKind::Lifecycle, Status::Working, false, 2.0),
        ];
        assert_eq!(
            project(&values, true, "exact-live").status,
            Status::OpenTwice
        );
    }

    #[test]
    fn runtime_failure_is_hidden_while_live() {
        let values = [
            obs(ObservationKind::Runtime, Status::Error, true, 2.0),
            obs(ObservationKind::Lifecycle, Status::Working, false, 1.0),
        ];
        assert_eq!(project(&values, true, "exact-live").status, Status::Working);
        assert_eq!(project(&values, false, "saved-idle").status, Status::Error);
    }

    #[test]
    fn finished_seen_work_parks_when_not_live() {
        let values = [obs(ObservationKind::Lifecycle, Status::Ready, false, 1.0)];
        let projected = project(&values, false, "saved-idle");
        assert_eq!(projected.status, Status::Parked);
        assert_eq!(projected.rule, "completed_and_collected_process_gone");
    }

    #[test]
    fn needs_you_beats_unbound_ownership() {
        let values = [obs(ObservationKind::Lifecycle, Status::NeedsYou, true, 1.0)];
        assert_eq!(project(&values, true, "unbound").status, Status::NeedsYou);
    }

    #[test]
    fn unbound_requires_absent_lifecycle() {
        assert_eq!(project(&[], true, "unbound").status, Status::Unbound);
    }

    #[test]
    fn a_live_process_without_turn_evidence_is_ready_but_read() {
        let projected = project(&[], true, "exact-live");
        assert_eq!(projected.status, Status::Ready);
        assert!(!projected.unread);
    }
}
