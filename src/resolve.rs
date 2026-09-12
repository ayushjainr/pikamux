use crate::model::{Provider, Status};
use std::collections::HashMap;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EvidenceState {
    Available,
    Archived,
    Missing,
    Unknown,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SelectionEvidence {
    pub state: EvidenceState,
    /// A canonical, existing directory established by the provider adapter.
    /// `None` means that equivalence has not been proven.
    pub canonical_cwd: Option<String>,
    /// Provider activity time. Pika reconciliation time must never be used here.
    pub provider_updated_at: Option<f64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct NameCandidate {
    pub provider: Provider,
    pub session_id: String,
    pub active_thread_id: Option<String>,
    pub name: Option<String>,
    pub live: bool,
    pub exact_home: bool,
    pub status: Status,
    /// Remote and pending candidates are deliberately not collapsed with local
    /// sessions. Their authoritative identity includes more than this record.
    pub local: bool,
    pub evidence: SelectionEvidence,
}

impl NameCandidate {
    fn provider_thread_id(&self) -> &str {
        self.active_thread_id.as_deref().unwrap_or(&self.session_id)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum NameResolutionError {
    #[error("no exact saved conversation matches")]
    NotFound,
    #[error("matching saved conversations are archived or confirmed missing")]
    SavedUnavailable,
}

/// Resolve a daily name or exact provider identity without guessing that two
/// distinct conversations are equivalent. Returned indexes refer to `choices`.
pub fn resolve_name(
    query: &str,
    choices: &[NameCandidate],
) -> Result<Vec<usize>, NameResolutionError> {
    let exact: Vec<usize> = choices
        .iter()
        .enumerate()
        .filter_map(|(index, item)| {
            (item.session_id == query || item.provider_thread_id() == query).then_some(index)
        })
        .collect();
    if !exact.is_empty() {
        return reduce_name_choices(choices, exact, true);
    }

    let folded = query.to_lowercase();
    let named: Vec<usize> = choices
        .iter()
        .enumerate()
        .filter_map(|(index, item)| {
            item.name
                .as_ref()
                .is_some_and(|name| name.to_lowercase() == folded)
                .then_some(index)
        })
        .collect();
    if !named.is_empty() {
        return reduce_name_choices(choices, named, false);
    }

    let prefixes: Vec<usize> = choices
        .iter()
        .enumerate()
        .filter_map(|(index, item)| {
            (item.session_id.starts_with(query) || item.provider_thread_id().starts_with(query))
                .then_some(index)
        })
        .collect();
    if prefixes.len() == 1 {
        return Ok(prefixes);
    }
    Err(NameResolutionError::NotFound)
}

fn reduce_name_choices(
    choices: &[NameCandidate],
    matched: Vec<usize>,
    exact_query: bool,
) -> Result<Vec<usize>, NameResolutionError> {
    let mut deduped = Vec::new();
    let mut native: HashMap<(Provider, String), usize> = HashMap::new();
    for index in matched {
        let item = &choices[index];
        if !item.local {
            deduped.push(index);
            continue;
        }
        let key = (item.provider, item.provider_thread_id().to_owned());
        if let Some(&position) = native.get(&key) {
            let previous = &choices[deduped[position]];
            let replace = (item.exact_home && !previous.exact_home)
                || (item.exact_home == previous.exact_home
                    && item.active_thread_id.is_some()
                    && previous.active_thread_id.is_none());
            if replace {
                deduped[position] = index;
            }
        } else {
            native.insert(key, deduped.len());
            deduped.push(index);
        }
    }

    // Exact provider identities remain inspectable even when archived. The
    // archive filter applies only to ordinary daily-name selection.
    if exact_query {
        return Ok(deduped);
    }

    let kept: Vec<usize> = deduped
        .iter()
        .copied()
        .filter(|&index| {
            let item = &choices[index];
            item.evidence.state != EvidenceState::Archived
                && (item.evidence.state != EvidenceState::Missing || item.live)
        })
        .collect();
    if kept.is_empty() && !deduped.is_empty() {
        return Err(NameResolutionError::SavedUnavailable);
    }
    if kept.len() < 2 || kept.iter().any(|&index| !choices[index].local) {
        return Ok(kept);
    }

    let first = &choices[kept[0]];
    let first_name = first.name.as_deref().unwrap_or_default().to_lowercase();
    if kept.iter().any(|&index| {
        let item = &choices[index];
        item.provider != first.provider
            || item.name.as_deref().unwrap_or_default().to_lowercase() != first_name
    }) {
        return Ok(kept);
    }

    let Some(first_cwd) = first.evidence.canonical_cwd.as_deref() else {
        return Ok(kept);
    };
    if kept
        .iter()
        .any(|&index| choices[index].evidence.canonical_cwd.as_deref() != Some(first_cwd))
    {
        return Ok(kept);
    }

    let live: Vec<usize> = kept
        .iter()
        .copied()
        .filter(|&index| {
            let item = &choices[index];
            item.live || item.exact_home || item.status == Status::OpenTwice
        })
        .collect();
    if live.len() > 1
        || live
            .iter()
            .any(|&index| choices[index].status == Status::OpenTwice)
    {
        return Ok(kept);
    }
    if live.len() == 1 {
        return if choices[live[0]].exact_home {
            Ok(live)
        } else {
            Ok(kept)
        };
    }

    if kept.iter().any(|&index| {
        let evidence = &choices[index].evidence;
        evidence.state != EvidenceState::Available
            || !evidence
                .provider_updated_at
                .is_some_and(|value| value > 0.0)
    }) {
        return Ok(kept);
    }
    let newest = kept
        .iter()
        .filter_map(|&index| choices[index].evidence.provider_updated_at)
        .max_by(f64::total_cmp)
        .expect("positive provider timestamps were checked above");
    let winners: Vec<usize> = kept
        .iter()
        .copied()
        .filter(|&index| choices[index].evidence.provider_updated_at == Some(newest))
        .collect();
    if winners.len() == 1 {
        Ok(winners)
    } else {
        Ok(kept)
    }
}
