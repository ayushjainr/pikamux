use pikamux::{
    client_board::{WindowTransport, cached_board, exact_remote, sync_pairings},
    client_bridge::{ClientBridgeError, ClientConfig, ClientNode, WindowLauncher},
    consult::CancellationToken,
    fleet::{
        FleetError, FleetErrorKind, FleetManager, FleetTransport, PROTOCOL_NAME, PROTOCOL_VERSION,
        session_to_wire,
    },
    model::{FleetNode, Provider, Session, Status},
    store::Store,
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

const NODE_A: &str = "10000000-0000-4000-8000-000000000001";
const NODE_B: &str = "20000000-0000-4000-8000-000000000002";
const THREAD: &str = "30000000-0000-4000-8000-000000000003";
fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
}
fn session(provider: Provider, id: &str, name: &str) -> Session {
    Session {
        provider,
        session_id: id.to_owned(),
        name: Some(name.to_owned()),
        cwd: Some("/remote/project".to_owned()),
        branch: Some("main".to_owned()),
        transcript_path: Some("/secret/provider.jsonl".to_owned()),
        tmux_session: Some("pika-c-1".to_owned()),
        tmux_pane: Some("%9".to_owned()),
        root_pid: Some(999),
        status: Status::Working,
        unread: false,
        model: Some("model".to_owned()),
        source: "managed".to_owned(),
        managed: true,
        error: None,
        attention_reason: None,
        created_at: 1.0,
        updated_at: 10.0,
        last_event_at: 9.0,
        last_activity_at: 9.0,
        live: true,
        attached: false,
        // This is the canonical value emitted by native core reconciliation;
        // `session_to_wire` translates it to the protocol's exact_home bit.
        home_state: "exact".to_owned(),
        cpu_percent: Some(0.5),
        rss_kb: Some(1024),
        input_tokens: None,
        output_tokens: None,
        cached_input_tokens: None,
        cache_write_tokens: None,
        total_tokens: None,
        estimated_cost_usd: None,
        active_thread_id: None,
    }
}

fn config() -> ClientConfig {
    let mut config = ClientConfig::empty();
    for (id, alias) in [(NODE_A, "rs6"), (NODE_B, "rs2a")] {
        config.nodes.insert(
            id.into(),
            ClientNode {
                node_id: id.into(),
                alias: alias.into(),
                ssh_target: alias.into(),
                token: "ab".repeat(32),
                remote_port: Some(47654),
                allow_fleet_relay: false,
            },
        );
    }
    config.default_node_id = Some(NODE_A.into());
    config
}
fn snapshot(id: &str) -> Value {
    json!({"type":"snapshot","protocol":PROTOCOL_NAME,"version":PROTOCOL_VERSION,
        "node_id":id, "machine":"remote", "captured_at":now(),
        "sessions":[session_to_wire(&session(Provider::Codex, THREAD, "same-name"), false)],
        "profiles":[], "cards":[]})
}
#[derive(Clone)]
struct FakeTransport {
    wrong: bool,
    offline: bool,
}
impl FleetTransport for FakeTransport {
    fn request(&self, target: &str, payload: &Value, _: bool) -> Result<Value, FleetError> {
        assert_eq!(payload["op"], "snapshot");
        if self.offline {
            return Err(FleetError::new(
                FleetErrorKind::Unreachable,
                "fixture offline",
            ));
        }
        Ok(snapshot(if self.wrong {
            THREAD
        } else if target == "rs6" {
            NODE_A
        } else {
            NODE_B
        }))
    }
    fn run_exact(&self, _: &FleetNode, _: &[String], _: bool) -> Result<i32, FleetError> {
        panic!("must use local window launcher")
    }
}
#[derive(Clone, Default)]
struct FakeLauncher(Arc<Mutex<Vec<Vec<String>>>>);
impl WindowLauncher for FakeLauncher {
    fn launch(&mut self, argv: &[String]) -> Result<(), ClientBridgeError> {
        self.0.lock().unwrap().push(argv.to_vec());
        Ok(())
    }
}
fn fixture() -> (TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::at(dir.path().join("client-fleet.db"));
    sync_pairings(&store, &config()).unwrap();
    (dir, store)
}
fn transport() -> FakeTransport {
    FakeTransport {
        wrong: false,
        offline: false,
    }
}

#[test]
fn combines_every_paired_machine_without_name_or_uuid_deduplication() {
    let (_dir, store) = fixture();
    let manager = FleetManager::new(&store, transport());
    for id in [NODE_A, NODE_B] {
        manager.refresh_node(id).unwrap();
    }
    let (items, health) = cached_board(&store).unwrap();
    assert_eq!(items.len(), 2);
    assert!(health.is_empty());
    assert_eq!(items[0].session.session_id, items[1].session.session_id);
    assert_ne!(items[0].node_id, items[1].node_id);
    assert!(items.iter().all(|item| !item.stale));
    assert_eq!(
        store.list_sessions().unwrap().len(),
        0,
        "no remote rows in local ledger"
    );
    sync_pairings(&store, &config()).unwrap();
    assert_eq!(
        cached_board(&store).unwrap().0,
        items,
        "relaunch preserves valid cache"
    );
}

#[test]
fn one_offline_machine_retains_its_cache_without_hiding_the_other() {
    let (_dir, store) = fixture();
    for id in [NODE_A, NODE_B] {
        FleetManager::new(&store, transport())
            .refresh_node(id)
            .unwrap();
    }
    assert!(
        FleetManager::new(
            &store,
            FakeTransport {
                wrong: false,
                offline: true
            }
        )
        .refresh_node(NODE_A)
        .is_err()
    );
    let (items, health) = cached_board(&store).unwrap();
    assert_eq!(items.len(), 2);
    assert!(
        items
            .iter()
            .find(|i| i.node_id.as_deref() == Some(NODE_A))
            .unwrap()
            .stale
    );
    assert!(
        !items
            .iter()
            .find(|i| i.node_id.as_deref() == Some(NODE_B))
            .unwrap()
            .stale
    );
    assert!(health.iter().any(|h| h.contains("offline")));
}

#[test]
fn exact_open_uses_the_selected_server_directly_after_fresh_validation() {
    let (_dir, store) = fixture();
    FleetManager::new(&store, transport())
        .refresh_node(NODE_B)
        .unwrap();
    let item = cached_board(&store).unwrap().0.remove(0);
    let remote = exact_remote(&store, &item).unwrap();
    let launcher = FakeLauncher::default();
    let calls = launcher.0.clone();
    let t = WindowTransport {
        transport: transport(),
        launcher: Mutex::new(launcher),
        cancellation: CancellationToken::default(),
    };
    FleetManager::new(&store, t).attach(&remote).unwrap();
    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    let argv = &calls[0];
    assert!(argv.contains(&"rs2a".into()));
    assert!(!argv.contains(&"rs6".into()));
    assert!(!argv.contains(&"-R".into()));
    assert!(argv.contains(&"ClearAllForwardings=yes".into()));
    assert!(argv.ends_with(&[
        "--expected-node-id".into(),
        NODE_B.into(),
        "--provider".into(),
        "codex".into(),
        "--session-id".into(),
        THREAD.into()
    ]));
}

#[test]
fn changed_identity_and_cancelled_board_never_launch_a_window() {
    let (_dir, store) = fixture();
    FleetManager::new(&store, transport())
        .refresh_node(NODE_A)
        .unwrap();
    let item = cached_board(&store).unwrap().0.remove(0);
    let remote = exact_remote(&store, &item).unwrap();
    let launcher = FakeLauncher::default();
    let calls = launcher.0.clone();
    let t = WindowTransport {
        transport: FakeTransport {
            wrong: true,
            offline: false,
        },
        launcher: Mutex::new(launcher.clone()),
        cancellation: CancellationToken::default(),
    };
    assert!(FleetManager::new(&store, t).attach(&remote).is_err());
    assert!(calls.lock().unwrap().is_empty());
    let stop = CancellationToken::default();
    stop.cancel();
    let t = WindowTransport {
        transport: transport(),
        launcher: Mutex::new(launcher),
        cancellation: stop,
    };
    assert!(FleetManager::new(&store, t).attach(&remote).is_err());
    assert!(calls.lock().unwrap().is_empty());
    assert_eq!(
        store.get_fleet_node(NODE_A).unwrap().unwrap().node_id,
        NODE_A
    );
}

#[test]
fn changing_or_removing_a_pairing_invalidates_its_cache_only() {
    let (_dir, store) = fixture();
    for id in [NODE_A, NODE_B] {
        FleetManager::new(&store, transport())
            .refresh_node(id)
            .unwrap();
    }
    let mut config = config();
    config.nodes.get_mut(NODE_A).unwrap().ssh_target = "changed".into();
    sync_pairings(&store, &config).unwrap();
    assert!(!store.has_remote_snapshot(NODE_A).unwrap());
    assert!(store.has_remote_snapshot(NODE_B).unwrap());
    config.nodes.remove(NODE_B);
    sync_pairings(&store, &config).unwrap();
    assert!(store.get_fleet_node(NODE_B).unwrap().is_none());
}
