//! Shared, bounded cache of provider-verified personally named conversations.
//!
//! This is intentionally only a cache around `Providers::import_candidates`;
//! provider authorship rules live with the provider implementation. Cloned
//! `Pika` handles share one cache so board, feed, and manual reconciliation do
//! not multiply provider scans.

use crate::{
    model::{Candidate, Provider},
    providers::Providers,
};
use std::{
    sync::Mutex,
    time::{Duration, Instant},
};

const REFRESH_INTERVAL: Duration = Duration::from_secs(5);
const MAX_CANDIDATES: usize = 2_000;

#[derive(Default)]
struct Cache {
    scanned_at: Option<Instant>,
    candidates: Vec<Candidate>,
}

#[derive(Default)]
pub(crate) struct NamedDiscovery {
    cache: Mutex<Cache>,
}

impl NamedDiscovery {
    /// Return a bounded snapshot, scanning every provider at most once per
    /// interval for all clones sharing this service.
    pub(crate) fn candidates(&self, providers: &Providers<'_>) -> Vec<Candidate> {
        self.candidates_at(providers, Instant::now())
    }

    fn candidates_at(&self, providers: &Providers<'_>, now: Instant) -> Vec<Candidate> {
        let mut cache = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if cache
            .scanned_at
            .is_none_or(|scanned| now.duration_since(scanned) >= REFRESH_INTERVAL)
        {
            let mut candidates = Vec::new();
            for provider in Provider::ALL {
                candidates.extend(providers.import_candidates(provider));
            }
            candidates.retain(|candidate| {
                candidate
                    .name
                    .as_deref()
                    .is_some_and(|name| !name.trim().is_empty())
                    && Providers::valid_id(candidate.provider, &candidate.session_id)
            });
            // Provider APIs may return duplicate identities. Keep the first
            // authoritative entry and impose a stable cap on retained data.
            candidates.sort_by(|left, right| {
                (left.provider, &left.session_id).cmp(&(right.provider, &right.session_id))
            });
            candidates.dedup_by(|left, right| {
                left.provider == right.provider && left.session_id == right.session_id
            });
            candidates.truncate(MAX_CANDIDATES);
            cache.candidates = candidates;
            cache.scanned_at = Some(now);
        }
        cache.candidates.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::Config, paths::Paths};
    use std::{fs, path::Path, time::Duration};

    fn paths(root: &Path) -> Paths {
        let config_dir = root.join("config");
        let state_dir = root.join("state");
        Paths {
            config: config_dir.join("config.json"),
            config_dir,
            database: state_dir.join("pika.db"),
            state_dir,
            codex_home: root.join("codex"),
            claude_home: root.join("claude"),
            opencode_data_home: root.join("opencode-data"),
            opencode_config_home: root.join("opencode-config"),
            muse_data_home: root.join("muse-data"),
            muse_config_home: root.join("muse-config"),
        }
    }

    #[test]
    fn clones_share_a_five_second_verified_candidate_snapshot() {
        let root = tempfile::tempdir().unwrap();
        let paths = paths(root.path());
        let config = Config::default();
        let providers = Providers::new(&paths, &config);
        let service = NamedDiscovery::default();
        let first = Instant::now();
        assert!(service.candidates_at(&providers, first).is_empty());

        let id = "12345678-1234-4234-8234-123456789abc";
        let sessions = paths.claude_home.join("sessions");
        fs::create_dir_all(&sessions).unwrap();
        fs::write(
            sessions.join(format!("{id}.json")),
            serde_json::to_vec(&serde_json::json!({
                "kind":"interactive", "sessionId":id, "name":"native personal name",
                "nameSource":"custom", "cwd":"/project", "updatedAt":10
            }))
            .unwrap(),
        )
        .unwrap();

        assert!(
            service
                .candidates_at(&providers, first + Duration::from_secs(4))
                .is_empty()
        );
        let refreshed = service.candidates_at(&providers, first + Duration::from_secs(5));
        assert_eq!(refreshed.len(), 1);
        assert_eq!(refreshed[0].session_id, id);
        assert_eq!(
            service.candidates_at(&providers, first + Duration::from_secs(6))[0].session_id,
            id
        );
    }
}
