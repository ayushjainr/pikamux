use anyhow::{Result, bail};
use pikamux::{
    assistant_client::{self, Args, Binding, State},
    client_bridge::{ClientConfig, ClientNode, LoopbackEndpoint},
    client_cli::{ClientBridgeOptions, ClientCliRuntime},
};
use serde_json::{Value, json};
use std::{collections::VecDeque, time::Duration};

const NODE: &str = "20000000-0000-4000-8000-000000000002";
const PROFILE: &str = "30000000-0000-4000-8000-000000000003";
const OTHER: &str = "40000000-0000-4000-8000-000000000004";

struct Fake {
    config: ClientConfig,
    state: State,
    replies: VecDeque<Value>,
    calls: Vec<Vec<String>>,
    launches: Vec<Vec<String>>,
}
impl Fake {
    fn new() -> Self {
        let mut config = ClientConfig::empty();
        config.nodes.insert(
            NODE.into(),
            ClientNode {
                node_id: NODE.into(),
                alias: "authority".into(),
                ssh_target: "fixture.invalid".into(),
                token: "unused-secret".into(),
                remote_port: None,
                allow_fleet_relay: false,
            },
        );
        config.default_node_id = Some(NODE.into());
        Self {
            config,
            state: State::default(),
            replies: VecDeque::new(),
            calls: vec![],
            launches: vec![],
        }
    }
    fn online(&mut self) {
        self.replies.push_back(json!({"protocol":assistant_client::PROTOCOL,"version":assistant_client::PROTOCOL_VERSION,"capabilities":[assistant_client::CAPABILITY],"node_id":NODE,"profile_id":PROFILE,"snapshot":{"as_of":123,"brief":"cached marker"}}));
    }
    fn bind(&mut self) {
        self.online();
        assistant_client::run(
            self,
            Args {
                bind_node: Some(NODE.into()),
                profile: Some(PROFILE.into()),
                ..Args::default()
            },
            &mut Vec::new(),
        )
        .unwrap();
    }
}
impl ClientCliRuntime for Fake {
    fn load_assistant_state(&mut self) -> Result<State> {
        Ok(self.state.clone())
    }
    fn save_assistant_state(&mut self, state: &mut State) -> Result<()> {
        self.state = state.clone();
        Ok(())
    }
    fn launch_assistant(&mut self, argv: &[String]) -> Result<()> {
        self.launches.push(argv.to_vec());
        Ok(())
    }
    fn load_config(&mut self) -> Result<ClientConfig> {
        Ok(self.config.clone())
    }
    fn save_config(&mut self, _: &ClientConfig) -> Result<()> {
        bail!("unexpected config mutation")
    }
    fn client_label(&mut self) -> Result<String> {
        bail!("unexpected")
    }
    fn pairing_token(&mut self) -> Result<String> {
        bail!("unexpected")
    }
    fn ssh_json(
        &mut self,
        target: &str,
        args: &[String],
        payload: &Value,
        exe: &str,
        _: Duration,
    ) -> Result<Value> {
        assert_eq!(target, "fixture.invalid");
        assert_eq!(exe, "ssh.exe");
        assert_eq!(args.first().map(String::as_str), Some("_assistant-client"));
        assert!(args.iter().any(|arg| arg == "--json"));
        assert!(!payload.to_string().contains("unused-secret"));
        self.calls.push(args.to_vec());
        self.replies
            .pop_front()
            .ok_or_else(|| anyhow::anyhow!("offline fixture"))
    }
    fn bridge_running(&mut self, _: &LoopbackEndpoint) -> bool {
        false
    }
    fn start_bridge(&mut self, _: &ClientBridgeOptions) -> Result<()> {
        bail!("unexpected")
    }
    fn serve_bridge(&mut self, _: &ClientBridgeOptions) -> Result<()> {
        bail!("unexpected")
    }
}

#[test]
fn pairing_default_never_implicitly_selects_an_assistant() {
    let mut fake = Fake::new();
    let error = assistant_client::run(&mut fake, Args::default(), &mut Vec::new()).unwrap_err();
    assert!(error.to_string().contains("explicitly"));
    assert!(fake.calls.is_empty());
}

#[test]
fn bind_and_attach_use_exact_profile_without_token_or_forwarding() {
    let mut fake = Fake::new();
    fake.bind();
    assert_eq!(fake.calls.len(), 1);
    fake.online();
    assistant_client::run(&mut fake, Args::default(), &mut Vec::new()).unwrap();
    let argv = &fake.launches[0];
    let remote = argv.last().unwrap();
    assert!(remote.contains("_assistant-client"));
    assert!(remote.contains(PROFILE));
    assert!(remote.contains(NODE));
    assert!(argv.contains(&"ClearAllForwardings=yes".into()));
    assert!(!argv.contains(&"-R".into()));
    assert!(!argv.join(" ").contains("unused-secret"));
    assert!(
        !serde_json::to_string(&fake.state)
            .unwrap()
            .contains("unused-secret")
    );
}

#[test]
fn offline_cache_and_draft_never_queue_or_launch_work() {
    let mut fake = Fake::new();
    fake.bind();
    assistant_client::run(
        &mut fake,
        Args {
            draft: Some("unsent human draft".into()),
            ..Args::default()
        },
        &mut Vec::new(),
    )
    .unwrap();
    assert_eq!(fake.calls.len(), 1);
    let mut output = Vec::new();
    assert_eq!(
        assistant_client::run(&mut fake, Args::default(), &mut output).unwrap(),
        1
    );
    let text = String::from_utf8(output).unwrap();
    assert!(text.contains("OFFLINE"));
    assert!(text.contains("cached marker"));
    assert!(text.contains("unsent human draft"));
    fake.online();
    assistant_client::run(&mut fake, Args::default(), &mut Vec::new()).unwrap();
    assert_eq!(fake.state.draft.as_deref(), Some("unsent human draft"));
    assert!(!fake.launches[0].join(" ").contains("unsent human draft"));
}

#[test]
fn changed_route_and_unpaired_authority_fail_without_network_fallback() {
    let mut fake = Fake::new();
    fake.bind();
    fake.config.nodes.get_mut(NODE).unwrap().ssh_target = "changed.invalid".into();
    assert_eq!(
        assistant_client::run(&mut fake, Args::default(), &mut Vec::new()).unwrap(),
        1
    );
    fake.config.nodes.clear();
    assert_eq!(
        assistant_client::run(&mut fake, Args::default(), &mut Vec::new()).unwrap(),
        1
    );
    assert_eq!(fake.calls.len(), 1);
    assert!(fake.launches.is_empty());
}

#[test]
fn profile_mismatch_preserves_cache_and_draft() {
    let mut fake = Fake::new();
    fake.bind();
    let previous = fake.state.snapshot.clone();
    fake.online();
    fake.replies[0]["profile_id"] = json!(OTHER);
    assert_eq!(
        assistant_client::run(&mut fake, Args::default(), &mut Vec::new()).unwrap(),
        1
    );
    assert_eq!(fake.state.snapshot, previous);
    assert!(fake.launches.is_empty());
}

#[test]
fn missing_capability_cannot_replace_snapshot_or_launch() {
    let mut fake = Fake::new();
    fake.bind();
    let cached = fake.state.snapshot.clone();
    fake.online();
    fake.replies[0]["capabilities"] = json!([]);
    assert_eq!(
        assistant_client::run(&mut fake, Args::default(), &mut Vec::new()).unwrap(),
        1
    );
    assert_eq!(fake.calls.len(), 2);
    assert_eq!(fake.state.snapshot, cached);
    assert!(fake.launches.is_empty());
}

#[test]
fn incompatible_or_missing_probe_protocol_preserves_binding_and_cache() {
    for (key, value) in [
        ("protocol", json!("pikamux-fleet")),
        ("version", json!(assistant_client::PROTOCOL_VERSION + 1)),
        ("protocol", Value::Null),
        ("capabilities", Value::Null),
    ] {
        let mut fake = Fake::new();
        fake.bind();
        let before = serde_json::to_value(&fake.state).unwrap();
        fake.online();
        fake.replies[0][key] = value;
        assert_eq!(
            assistant_client::run(&mut fake, Args::default(), &mut Vec::new()).unwrap(),
            1
        );
        assert_eq!(serde_json::to_value(&fake.state).unwrap(), before);
        assert!(fake.launches.is_empty());
    }
}

#[test]
fn null_cache_is_valid_and_local_storage_roundtrips_without_authority_state() {
    let mut fake = Fake::new();
    fake.online();
    fake.replies[0]["snapshot"] = Value::Null;
    assistant_client::run(
        &mut fake,
        Args {
            bind_node: Some(NODE.into()),
            profile: Some(PROFILE.into()),
            ..Args::default()
        },
        &mut Vec::new(),
    )
    .unwrap();
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("client/state.sqlite");
    assistant_client::save(&path, &mut fake.state).unwrap();
    let loaded = assistant_client::load(&path).unwrap();
    assert_eq!(loaded.binding, fake.state.binding);
    assert!(!path.with_file_name("policy.sqlite").exists());
    assert!(!path.with_file_name("memory.sqlite").exists());
}

#[test]
fn argument_injection_and_malformed_id_are_rejected_before_launch() {
    let fake = Fake::new();
    let binding = Binding {
        node_id: NODE.into(),
        profile_id: "; arbitrary command".into(),
        ssh_target: "fixture.invalid".into(),
    };
    assert!(assistant_client::terminal_command(&binding, &fake.config).is_err());
}

#[test]
fn windows_parser_exposes_pika_and_rejects_queued_approval_flags() {
    let mut fake = Fake::new();
    fake.bind();
    fake.online();
    pikamux::client_cli::run_with(
        ["pika", "pika", "--json"],
        &mut fake,
        &mut Vec::new(),
        &mut Vec::new(),
    )
    .unwrap();
    assert!(
        pikamux::client_cli::run_with(
            ["pika", "pika", "--approve", "x"],
            &mut fake,
            &mut Vec::new(),
            &mut Vec::new()
        )
        .is_err()
    );
}

#[test]
fn stale_window_cannot_overwrite_rebinding_or_new_draft() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("client/state.sqlite");
    let mut state = assistant_client::load(&path).unwrap();
    let mut stale = state.clone();
    state.draft = Some("newer local draft".into());
    assistant_client::save(&path, &mut state).unwrap();
    stale.draft = Some("stale response".into());
    assert!(assistant_client::save(&path, &mut stale).is_err());
    assert_eq!(
        assistant_client::load(&path).unwrap().draft.as_deref(),
        Some("newer local draft")
    );
}

#[test]
fn rebind_with_unsent_draft_is_rejected_before_contacting_another_authority() {
    let mut fake = Fake::new();
    fake.bind();
    fake.state.draft = Some("private unsent draft".into());
    assert!(
        assistant_client::run(
            &mut fake,
            Args {
                bind_node: Some(NODE.into()),
                profile: Some(OTHER.into()),
                ..Args::default()
            },
            &mut Vec::new()
        )
        .is_err()
    );
    assert_eq!(fake.calls.len(), 1);
    assert_eq!(fake.state.binding.as_ref().unwrap().profile_id, PROFILE);
}

#[test]
fn wrong_probe_identity_cannot_create_a_binding() {
    let mut fake = Fake::new();
    fake.online();
    fake.replies[0]["node_id"] = json!(OTHER);
    assert!(
        assistant_client::run(
            &mut fake,
            Args {
                bind_node: Some(NODE.into()),
                profile: Some(PROFILE.into()),
                ..Args::default()
            },
            &mut Vec::new()
        )
        .is_err()
    );
    assert_eq!(fake.calls.len(), 1);
    assert!(fake.state.binding.is_none());
}

#[test]
fn oversized_snapshot_or_draft_cannot_replace_existing_state() {
    let mut fake = Fake::new();
    fake.bind();
    let original = fake.state.snapshot.clone();
    fake.online();
    fake.replies[0]["snapshot"] = json!({"huge":"x".repeat(16*1024)});
    assert_eq!(
        assistant_client::run(&mut fake, Args::default(), &mut Vec::new()).unwrap(),
        1
    );
    assert_eq!(fake.state.snapshot, original);
    assert!(
        assistant_client::run(
            &mut fake,
            Args {
                draft: Some("x".repeat(16 * 1024 + 1)),
                ..Args::default()
            },
            &mut Vec::new()
        )
        .is_err()
    );
    assert!(fake.state.draft.is_none());
}

#[test]
fn offline_json_remains_machine_readable_and_explicitly_not_fresh() {
    let mut fake = Fake::new();
    fake.bind();
    let mut output = Vec::new();
    assert_eq!(
        assistant_client::run(
            &mut fake,
            Args {
                json: true,
                ..Args::default()
            },
            &mut output
        )
        .unwrap(),
        1
    );
    let value: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(value["availability"], "offline");
    assert_eq!(value["actions_queued"], false);
    assert_eq!(value["state"]["snapshot"]["as_of"], 123);
}

#[cfg(unix)]
mod binary_probe {
    use super::*;
    use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf};

    struct Host {
        temp: tempfile::TempDir,
        profile: String,
    }

    impl Host {
        fn empty() -> Self {
            let temp = tempfile::tempdir().unwrap();
            for name in ["home", "tmp", "empty-bin"] {
                fs::create_dir(temp.path().join(name)).unwrap();
            }
            Self {
                temp,
                profile: PROFILE.into(),
            }
        }

        fn existing() -> Self {
            let mut host = Self::empty();
            let memory =
                pikamux::assistant_memory::Store::open(host.root().join("memory.sqlite")).unwrap();
            host.profile = memory.profile_id().into();
            drop(memory);
            let operational =
                rusqlite::Connection::open(host.temp.path().join("operational.sqlite")).unwrap();
            operational
                .execute_batch("CREATE TABLE meta(key TEXT PRIMARY KEY,value TEXT)")
                .unwrap();
            operational
                .execute("INSERT INTO meta VALUES('fleet:node_id',?)", [NODE])
                .unwrap();
            host
        }

        fn root(&self) -> PathBuf {
            self.temp.path().join("state/assistant")
        }

        fn command(&self, node: &str, profile: &str) -> assert_cmd::Command {
            let base = self.temp.path();
            let mut command = assert_cmd::Command::cargo_bin("pika").unwrap();
            command.env_clear().current_dir(base.join("home"));
            for (key, path) in [
                ("HOME", "home"),
                ("XDG_CONFIG_HOME", "config"),
                ("XDG_STATE_HOME", "state"),
                ("XDG_DATA_HOME", "data"),
                ("XDG_CACHE_HOME", "cache"),
                ("TMPDIR", "tmp"),
                ("TMUX_TMPDIR", "tmux"),
                ("PIKA_CONFIG_HOME", "config/pika"),
                ("PIKA_STATE_HOME", "state"),
                ("PIKA_DB_PATH", "operational.sqlite"),
                ("CODEX_HOME", "codex"),
                ("CLAUDE_CONFIG_DIR", "claude"),
                ("OPENCODE_DATA_HOME", "opencode"),
                ("OPENCODE_CONFIG_DIR", "config/opencode"),
                ("MUSE_DATA_HOME", "muse"),
                ("MUSE_CONFIG_DIR", "config/muse"),
            ] {
                command.env(key, base.join(path));
            }
            command
                .env("PATH", base.join("empty-bin"))
                .env("PIKA_UPDATE_CHECK", "0")
                .env("PIKA_TMUX_SOCKET", "assistant-probe-fixture")
                .args([
                    "_assistant-client",
                    "--expected-node-id",
                    node,
                    "--expected-profile-id",
                    profile,
                    "--scope",
                    "personal",
                    "--json",
                ])
                .write_stdin("{}\n")
                .timeout(Duration::from_secs(3));
            command
        }

        fn assert_no_host_or_provider(&self) {
            for name in [
                "view.sock",
                "owner.lock",
                "policy.sqlite",
                "control.sqlite",
                "provider-home",
            ] {
                assert!(!self.root().join(name).exists(), "Probe created {name}");
            }
            for name in ["codex", "claude", "opencode", "muse", "tmux"] {
                assert!(
                    !self.temp.path().join(name).exists(),
                    "Probe touched {name}"
                );
            }
        }

        fn cache(&self) -> PathBuf {
            let path = self.root().join("owner.sqlite");
            let db = rusqlite::Connection::open(&path).unwrap();
            db.execute_batch(
                "CREATE TABLE assistant_briefing_cache(profile TEXT,epoch INTEGER,scope TEXT,asof INTEGER,body TEXT)",
            )
            .unwrap();
            let scope = pikamux::assistant_memory::Scope {
                project: Some("personal".into()),
                ..Default::default()
            };
            let view = json!({"cue":{"profile":self.profile,"scope":scope,"revision":7,"asof":123,"state":"idle","unread":true,"unread_summary":"fixture change","notice":"fixture cache"},"cursor":{"profile":self.profile,"scope":scope,"revision":7,"epoch":0},"brief":{"marker":"approved cache only"},"has_more":false});
            db.execute(
                "INSERT INTO assistant_briefing_cache VALUES(?,0,?,123,?)",
                rusqlite::params![
                    self.profile,
                    serde_json::to_string(&scope).unwrap(),
                    view.to_string()
                ],
            )
            .unwrap();
            drop(db);
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
            path
        }
    }

    #[test]
    fn actual_probe_matches_existing_identities_without_starting_host() {
        let host = Host::existing();
        let before = fs::read(host.root().join("memory.sqlite")).unwrap();
        let receipt = host.command(NODE, &host.profile).assert().success();
        let value: Value = serde_json::from_slice(&receipt.get_output().stdout).unwrap();
        assert_eq!(value["protocol"], assistant_client::PROTOCOL);
        assert_eq!(value["version"], assistant_client::PROTOCOL_VERSION);
        assert_eq!(value["capabilities"], json!([assistant_client::CAPABILITY]));
        assert_eq!(value["node_id"], NODE);
        assert_eq!(value["profile_id"], host.profile);
        assert!(value["snapshot"].is_null());
        assert_eq!(fs::read(host.root().join("memory.sqlite")).unwrap(), before);
        assert!(!host.root().join("owner.sqlite").exists());
        host.assert_no_host_or_provider();
    }

    #[test]
    fn actual_single_probe_does_not_require_operational_writer() {
        let host = Host::existing();
        let path = host.temp.path().join("operational.sqlite");
        let before = fs::read(&path).unwrap();
        let db = rusqlite::Connection::open(&path).unwrap();
        db.execute_batch("BEGIN IMMEDIATE").unwrap();
        // A read-only probe can finish while this writer remains held. The old
        // preliminary fleet hello required a second writer and could not.
        host.command(NODE, &host.profile).assert().success();
        db.execute_batch("ROLLBACK").unwrap();
        assert_eq!(fs::read(&path).unwrap(), before);
        host.assert_no_host_or_provider();
    }

    #[test]
    fn actual_missing_identity_probe_creates_no_state() {
        let host = Host::empty();
        host.command(NODE, PROFILE).assert().failure();
        assert!(!host.temp.path().join("operational.sqlite").exists());
        assert!(!host.temp.path().join("state").exists());
        host.assert_no_host_or_provider();
    }

    #[test]
    fn actual_existing_node_without_profile_does_not_create_replacement() {
        let host = Host::empty();
        let db = rusqlite::Connection::open(host.temp.path().join("operational.sqlite")).unwrap();
        db.execute_batch("CREATE TABLE meta(key TEXT PRIMARY KEY,value TEXT)")
            .unwrap();
        db.execute("INSERT INTO meta VALUES('fleet:node_id',?)", [NODE])
            .unwrap();
        drop(db);
        host.command(NODE, PROFILE).assert().failure();
        assert!(!host.temp.path().join("state").exists());
        host.assert_no_host_or_provider();
    }

    #[test]
    fn actual_wrong_profile_or_node_cannot_return_cache() {
        let host = Host::existing();
        host.cache();
        for (node, profile) in [(NODE, OTHER), (OTHER, host.profile.as_str())] {
            let receipt = host.command(node, profile).assert().failure();
            assert!(receipt.get_output().stdout.is_empty());
        }
        host.assert_no_host_or_provider();
    }

    #[test]
    fn actual_probe_reads_only_private_dated_cache() {
        let host = Host::existing();
        let path = host.cache();
        let before = fs::read(&path).unwrap();
        let receipt = host.command(NODE, &host.profile).assert().success();
        let value: Value = serde_json::from_slice(&receipt.get_output().stdout).unwrap();
        assert_eq!(value["snapshot"]["brief"]["marker"], "approved cache only");
        assert_eq!(value["snapshot"]["cue"]["asof"], 123);
        assert_eq!(fs::read(&path).unwrap(), before);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        host.command(NODE, &host.profile).assert().failure();
        host.assert_no_host_or_provider();
    }

    #[test]
    fn actual_probe_refuses_symlinked_cache_without_reading_target() {
        let host = Host::existing();
        let target = host.temp.path().join("private-target");
        fs::write(&target, "private sentinel, not a database").unwrap();
        std::os::unix::fs::symlink(&target, host.root().join("owner.sqlite")).unwrap();
        let receipt = host.command(NODE, &host.profile).assert().failure();
        assert!(receipt.get_output().stdout.is_empty());
        assert_eq!(
            fs::read_to_string(target).unwrap(),
            "private sentinel, not a database"
        );
        host.assert_no_host_or_provider();
    }
}
