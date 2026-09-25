//! Optional admission of personally named conversations reuses provider discovery.
//! Broad provider history is deliberately not the default board picker.
use crate::{
    board_add::{Driver, Source},
    consult::CancellationToken,
    fleet::{FleetError, FleetErrorKind, FleetManager, FleetTransport, SshTransport},
    model::{Candidate, FleetNode},
    store::Store,
};
use anyhow::{Context, Result, bail};
use serde_json::Value;
#[cfg(not(windows))]
use std::collections::BTreeSet;
use std::time::Duration;

fn sources(store: &Store, local: bool) -> Vec<Source> {
    let mut sources = Vec::new();
    if local {
        sources.push(Source {
            id: None,
            label: "This machine".into(),
        });
    }
    // Failure is handled by the normal board health path. No discovery or
    // automatic trust is performed just to build a picker.
    if let Ok(nodes) = store.list_nodes() {
        sources.extend(nodes.into_iter().map(|node| Source {
            id: Some(node.node_id),
            label: node.alias,
        }));
    }
    sources
}

struct Transport {
    ssh: SshTransport,
    cancellation: CancellationToken,
}

impl FleetTransport for Transport {
    fn request(
        &self,
        target: &str,
        payload: &Value,
        mutating: bool,
    ) -> std::result::Result<Value, FleetError> {
        self.ssh
            .request_cancellable(target, payload, mutating, &self.cancellation)
    }
    fn run_exact(
        &self,
        _: &FleetNode,
        _: &[String],
        _: bool,
    ) -> std::result::Result<i32, FleetError> {
        Err(FleetError::new(
            FleetErrorKind::Incompatible,
            "Adding a conversation cannot launch an agent",
        ))
    }
}

fn remote<T: FleetTransport>(
    store: &Store,
    transport: T,
    source: &Source,
    candidate: Option<Candidate>,
) -> Result<Vec<Candidate>> {
    let id = source.id.as_deref().context("Choose a paired machine")?;
    let node = store
        .get_fleet_node(id)?
        .filter(|node| node.node_id == id)
        .context("This machine is no longer paired")?;
    let manager = FleetManager::new(store, transport);
    if let Some(candidate) = candidate {
        manager.adopt_preserving_state(&node, &candidate)?;
        Ok(Vec::new())
    } else {
        // Cached membership may predate an unwatch. Only the owning node
        // decides admission; repeat exact adoption is safe and idempotent.
        Ok(manager
            .remote_candidates(&node, false)?
            .into_iter()
            .filter(|candidate| {
                matches!(
                    candidate.source.as_str(),
                    "claude-live-custom"
                        | "claude-live+explicit-history"
                        | "claude-history"
                        | "codex-name-change"
                )
            })
            .collect())
    }
}

fn transport(windows: bool, cancellation: CancellationToken) -> Transport {
    Transport {
        ssh: SshTransport::new(
            if windows { "ssh.exe" } else { "ssh" },
            Duration::from_secs(5),
            Duration::from_secs(12),
        ),
        cancellation,
    }
}

fn check(cancellation: &CancellationToken) -> Result<()> {
    if cancellation.is_cancelled() {
        bail!("Add cancelled before dispatch");
    }
    Ok(())
}

pub(crate) fn client(store: Store) -> Driver {
    let choices = sources(&store, false);
    let read = store.clone();
    Driver::new(
        choices,
        move |source, cancellation| remote(&read, transport(true, cancellation), &source, None),
        move |source, candidate, cancellation| {
            check(&cancellation)?;
            let name = candidate
                .name
                .clone()
                .unwrap_or_else(|| candidate.session_id.clone());
            remote(
                &store,
                transport(true, cancellation),
                &source,
                Some(candidate),
            )?;
            Ok(format!(
                "Added {name} @{}. No agent was started or stopped.",
                source.label
            ))
        },
    )
}

#[cfg(not(windows))]
pub(crate) fn host(pika: crate::core::Pika) -> Driver {
    use crate::{model::Provider, providers::Providers};
    let choices = sources(&pika.store, true);
    let read = pika.clone();
    Driver::new(
        choices,
        move |source, cancellation| {
            check(&cancellation)?;
            if source.id.is_some() {
                return remote(&read.store, transport(false, cancellation), &source, None);
            }
            let watched: BTreeSet<_> = read
                .store
                .list_sessions()?
                .into_iter()
                .flat_map(|row| {
                    let mut ids = vec![(row.provider, row.session_id)];
                    if let Some(active) = row.active_thread_id {
                        ids.push((row.provider, active));
                    }
                    ids
                })
                .collect();
            let providers = Providers::new(&read.paths, &read.config);
            let mut choices = Vec::new();
            for provider in Provider::ALL {
                check(&cancellation)?;
                choices.extend(
                    providers
                        .import_candidates(provider)
                        .into_iter()
                        .filter(|row| !watched.contains(&(row.provider, row.session_id.clone()))),
                );
            }
            Ok(choices)
        },
        move |source, selected, cancellation| {
            check(&cancellation)?;
            let name = selected
                .name
                .clone()
                .unwrap_or_else(|| selected.session_id.clone());
            if source.id.is_some() {
                remote(
                    &pika.store,
                    transport(false, cancellation),
                    &source,
                    Some(selected),
                )?;
            } else {
                // Revalidate the frozen identity from current provider metadata;
                // an old picker row cannot restore an archived/deleted thread.
                let candidate = Providers::new(&pika.paths, &pika.config)
                    .find(selected.provider, &selected.session_id)
                    .into_iter()
                    .find(|row| row.session_id == selected.session_id)
                    .context("This conversation is no longer available; reopen Add")?;
                check(&cancellation)?;
                pika.adopt_candidate(&candidate)?;
            }
            Ok(format!(
                "Added {name} · {}. No agent was started or stopped.",
                source.label
            ))
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        fleet::{candidate_to_wire, session_to_wire},
        model::{Provider, Status},
    };
    use std::sync::Mutex;

    struct Fake {
        candidate: Candidate,
        calls: Mutex<Vec<(Value, bool)>>,
        modern: bool,
    }

    impl FleetTransport for &Fake {
        fn request(
            &self,
            target: &str,
            payload: &Value,
            mutating: bool,
        ) -> std::result::Result<Value, FleetError> {
            assert_eq!(target, "fixture.invalid");
            self.calls.lock().unwrap().push((payload.clone(), mutating));
            match payload["op"].as_str() {
                Some("hello") => Ok(
                    serde_json::json!({"type":"hello","protocol":crate::fleet::PROTOCOL_NAME,"version":crate::fleet::PROTOCOL_VERSION,"node_id":payload["expected_node_id"],"machine":"fixture","package_version":"0.6.28","capabilities":crate::fleet::CAPABILITIES.iter().filter(|capability| self.modern || **capability != "adopt-preserves-state-v1").collect::<Vec<_>>()}),
                ),
                Some("candidates") => Ok(
                    serde_json::json!({"type":"candidates","node_id":payload["expected_node_id"],"candidates":[candidate_to_wire(&self.candidate)]}),
                ),
                Some("adopt") => Ok(
                    serde_json::json!({"type":"adopted","node_id":payload["expected_node_id"],"request_id":payload["request_id"],"session":session_to_wire(&crate::core::session_from_candidate(&self.candidate), false)}),
                ),
                _ => Err(FleetError::new(
                    FleetErrorKind::Unreachable,
                    "Fixture refresh unavailable",
                )),
            }
        }
        fn run_exact(
            &self,
            _: &FleetNode,
            _: &[String],
            _: bool,
        ) -> std::result::Result<i32, FleetError> {
            panic!("Adding may not launch or attach");
        }
    }

    fn fixture() -> (tempfile::TempDir, Store, Source, Fake) {
        let root = tempfile::tempdir().unwrap();
        let store = Store::at(root.path().join("pika.db"));
        store.initialize().unwrap();
        let id = "11111111-1111-4111-8111-111111111111";
        store
            .upsert_fleet_node(&FleetNode {
                node_id: id.into(),
                alias: "Server".into(),
                ssh_target: "fixture.invalid".into(),
                sources: vec!["test".into()],
                status: "unreachable".into(),
                protocol_version: Some(crate::fleet::PROTOCOL_VERSION),
                package_version: None,
                capabilities: vec!["setup-explicit-names-v1".into()],
                last_seen: 1.0,
                last_attempt_at: 2.0,
                last_error: Some("cache is stale".into()),
                created_at: 1.0,
                updated_at: 2.0,
            })
            .unwrap();
        let fake = Fake {
            candidate: Candidate {
                provider: Provider::Codex,
                session_id: "22222222-2222-4222-8222-222222222222".into(),
                name: Some("Same title".into()),
                cwd: Some("/project".into()),
                branch: None,
                transcript_path: None,
                model: None,
                created_at: 1.0,
                updated_at: 2.0,
                live: false,
                pid: None,
                source: "codex-name-change".into(),
                parent_session_id: None,
                lifecycle_status: Some(Status::Ready),
            },
            calls: Mutex::new(Vec::new()),
            modern: true,
        };
        (
            root,
            store,
            Source {
                id: Some(id.into()),
                label: "Server".into(),
            },
            fake,
        )
    }

    #[test]
    fn remote_discovery_uses_owner_inventory_and_never_mutates_or_attaches() {
        let (_root, store, source, fake) = fixture();
        let choices = remote(&store, &fake, &source, None).unwrap();
        assert_eq!(choices.len(), 1);
        assert_eq!(choices[0].session_id, fake.candidate.session_id);
        let calls = fake.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert!(!calls[0].1);
        assert_eq!(calls[0].0["include_unconfirmed"], false);
        assert_eq!(
            calls[0].0["expected_node_id"],
            source.id.as_deref().unwrap()
        );
        assert!(store.list_sessions().unwrap().is_empty());
    }

    #[test]
    fn remote_picker_excludes_legacy_codex_title_without_rename_evidence() {
        let (_root, store, source, mut fake) = fixture();
        fake.candidate.source = "codex-state".into();
        let choices = remote(&store, &fake, &source, None).unwrap();
        assert!(choices.is_empty());
        assert!(fake.calls.lock().unwrap().iter().all(|(_, write)| !write));
    }

    #[test]
    fn removed_source_is_rejected_before_dispatch_and_add_uses_exact_uuid() {
        let (_root, store, mut source, fake) = fixture();
        source.id = Some("33333333-3333-4333-8333-333333333333".into());
        assert!(remote(&store, &fake, &source, Some(fake.candidate.clone())).is_err());
        assert!(fake.calls.lock().unwrap().is_empty());
        source.id = Some("11111111-1111-4111-8111-111111111111".into());
        remote(&store, &fake, &source, Some(fake.candidate.clone())).unwrap();
        let calls = fake.calls.lock().unwrap();
        let writes = calls
            .iter()
            .filter(|(_, mutating)| *mutating)
            .collect::<Vec<_>>();
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0].0["op"], "adopt");
        assert_eq!(writes[0].0["session_id"], fake.candidate.session_id);
        assert_eq!(writes[0].0["provider"], "codex");
        assert!(
            store.list_sessions().unwrap().is_empty(),
            "remote history must not become local state"
        );
    }

    #[test]
    fn old_host_fails_before_adoption_without_touching_unread() {
        let (_root, store, source, mut fake) = fixture();
        fake.modern = false;
        let error = remote(&store, &fake, &source, Some(fake.candidate.clone())).unwrap_err();
        assert!(error.to_string().contains("Update Pika"));
        assert!(
            fake.calls
                .lock()
                .unwrap()
                .iter()
                .all(|(_, mutating)| !mutating)
        );
    }
}
