use anyhow::Result;
use pikamux::client_bridge::{
    BRIDGE_PROTOCOL, BRIDGE_VERSION, ClientConfig, ClientNode, LoopbackEndpoint, PairRequest,
};
use pikamux::client_cli::{ClientBridgeOptions, ClientCliRuntime, run_with as run_client_cli};
use pikamux::fleet::{CAPABILITIES, PROTOCOL_NAME, PROTOCOL_VERSION};
use serde_json::{Value, json};
use std::time::Duration;

#[cfg(unix)]
mod system_ssh_pairing {
    use super::*;
    use pikamux::client_cli::SystemClientRuntime;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::time::Instant;

    fn fixture(body: &str) -> (tempfile::TempDir, PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("fake-ssh");
        fs::write(&executable, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        (directory, executable)
    }

    fn request(executable: &std::path::Path, payload: &Value, timeout: Duration) -> Result<Value> {
        SystemClientRuntime::discover()?.ssh_json(
            "fixture.invalid",
            &["_client-pair".into(), "--stdio".into()],
            payload,
            executable.to_str().unwrap(),
            timeout,
        )
    }

    #[test]
    fn ssh_pairing_deadline_includes_a_peer_that_never_reads_input() {
        let (_directory, executable) = fixture("exec /bin/sleep 5");
        let started = Instant::now();
        let error = request(
            &executable,
            &json!({"padding":"x".repeat(16_000)}),
            Duration::from_millis(50),
        )
        .unwrap_err();
        assert!(error.to_string().contains("timed out"), "{error:#}");
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn ssh_pairing_cleans_exited_launcher_descendants_holding_both_pipes() {
        let (_directory, executable) = fixture(
            "IFS= read -r request\nprintf '{\"paired\":true}\\n'\nprintf diagnostic >&2\n/bin/sleep 10 &\nexec /usr/bin/true",
        );
        let started = Instant::now();
        let output = request(&executable, &json!({}), Duration::from_secs(2)).unwrap();
        assert_eq!(output, json!({"paired":true}));
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn ssh_pairing_preserves_failed_exit_diagnostics() {
        let (_directory, executable) =
            fixture("IFS= read -r request\nprintf 'pairing rejected' >&2\nexit 7");
        let error = request(&executable, &json!({}), Duration::from_secs(2)).unwrap_err();
        assert_eq!(error.to_string(), "pairing rejected");
    }

    #[test]
    fn oversized_pairing_request_is_rejected_before_ssh_starts() {
        let (directory, executable) = fixture("exit 99");
        let marker = directory.path().join("started");
        fs::write(
            &executable,
            format!(
                "#!/bin/sh\nprintf started > '{}'\nexit 99\n",
                marker.display()
            ),
        )
        .unwrap();
        let error = request(
            &executable,
            &json!({"padding":"x".repeat(16 * 1024)}),
            Duration::from_secs(1),
        )
        .unwrap_err();
        assert!(error.to_string().contains("request exceeded"));
        assert!(!marker.exists());
    }
}

const CLIENT_ID: &str = "10000000-0000-4000-8000-000000000001";
const NODE_ID: &str = "20000000-0000-4000-8000-000000000002";
const WRONG_NODE_ID: &str = "30000000-0000-4000-8000-000000000003";
const TOKEN: &str = "abababababababababababababababababababababababababababababababab";

#[derive(Clone, Debug)]
struct SshCall {
    target: String,
    arguments: Vec<String>,
    payload: Value,
    executable: String,
    timeout: Duration,
}

struct FakeRuntime {
    interactive: bool,
    hosts: Vec<String>,
    choices: std::collections::VecDeque<String>,
    opened: Vec<ClientNode>,
    wrong_hello_node: bool,
    old_host: bool,
    config: ClientConfig,
    saved: Vec<ClientConfig>,
    ssh_calls: Vec<SshCall>,
    bridge_ready: bool,
    bridge_checks: Vec<LoopbackEndpoint>,
    starts: Vec<ClientBridgeOptions>,
    serves: Vec<ClientBridgeOptions>,
    wrong_pair_node: bool,
}

impl Default for FakeRuntime {
    fn default() -> Self {
        let mut config = ClientConfig::empty();
        config.client_id = CLIENT_ID.to_owned();
        Self {
            interactive: false,
            hosts: Vec::new(),
            choices: Default::default(),
            opened: Vec::new(),
            wrong_hello_node: false,
            old_host: false,
            config,
            saved: Vec::new(),
            ssh_calls: Vec::new(),
            bridge_ready: false,
            bridge_checks: Vec::new(),
            starts: Vec::new(),
            serves: Vec::new(),
            wrong_pair_node: false,
        }
    }
}

impl ClientCliRuntime for FakeRuntime {
    fn interactive(&self) -> bool {
        self.interactive
    }
    fn discover_hosts(&mut self) -> Result<Vec<String>> {
        Ok(self.hosts.clone())
    }
    fn read_choice(&mut self) -> Result<Option<String>> {
        Ok(self.choices.pop_front())
    }
    fn open_board(&mut self, node: &ClientNode) -> Result<i32> {
        self.opened.push(node.clone());
        Ok(0)
    }
    fn load_config(&mut self) -> Result<ClientConfig> {
        Ok(self.config.clone())
    }

    fn save_config(&mut self, config: &ClientConfig) -> Result<()> {
        self.config = config.clone();
        self.saved.push(config.clone());
        Ok(())
    }

    fn client_label(&mut self) -> Result<String> {
        Ok("test-laptop".to_owned())
    }

    fn pairing_token(&mut self) -> Result<String> {
        Ok(TOKEN.to_owned())
    }

    fn ssh_json(
        &mut self,
        ssh_target: &str,
        remote_arguments: &[String],
        payload: &Value,
        ssh_executable: &str,
        timeout: Duration,
    ) -> Result<Value> {
        self.ssh_calls.push(SshCall {
            target: ssh_target.to_owned(),
            arguments: remote_arguments.to_vec(),
            payload: payload.clone(),
            executable: ssh_executable.to_owned(),
            timeout,
        });
        if remote_arguments.first().map(String::as_str) == Some("_fleet") {
            return Ok(json!({
                "type": "hello",
                "protocol": PROTOCOL_NAME,
                "version": PROTOCOL_VERSION,
                "node_id": if self.wrong_hello_node { WRONG_NODE_ID } else { NODE_ID },
                "machine": "devbox",
                "package_version": "0.6.0-alpha.1",
                "capabilities": CAPABILITIES.iter().filter(|cap| !self.old_host || **cap != "client-board-v1").collect::<Vec<_>>(),
            }));
        }
        let pair: PairRequest = serde_json::from_value(payload.clone())?;
        Ok(json!({
            "type": "paired",
            "protocol": BRIDGE_PROTOCOL,
            "version": BRIDGE_VERSION,
            "node_id": if self.wrong_pair_node { WRONG_NODE_ID } else { NODE_ID },
            "client_id": pair.client_id,
            "port": pair.port,
        }))
    }

    fn bridge_running(&mut self, endpoint: &LoopbackEndpoint) -> bool {
        self.bridge_checks.push(endpoint.clone());
        self.bridge_ready
    }

    fn start_bridge(&mut self, options: &ClientBridgeOptions) -> Result<()> {
        self.starts.push(options.clone());
        self.bridge_ready = true;
        Ok(())
    }

    fn serve_bridge(&mut self, options: &ClientBridgeOptions) -> Result<()> {
        self.serves.push(options.clone());
        Ok(())
    }
}

fn run(arguments: &[&str], runtime: &mut FakeRuntime) -> Result<(i32, String, String)> {
    let mut output = Vec::new();
    let mut errors = Vec::new();
    let code = run_client_cli(arguments, runtime, &mut output, &mut errors)?;
    Ok((
        code,
        String::from_utf8(output).unwrap(),
        String::from_utf8(errors).unwrap(),
    ))
}

#[test]
fn windows_help_exposes_only_client_commands_and_denies_host_commands() {
    let mut runtime = FakeRuntime::default();
    let mut output = Vec::new();
    let mut errors = Vec::new();
    let error =
        run_client_cli(["pika", "--help"], &mut runtime, &mut output, &mut errors).unwrap_err();
    let clap = error.downcast_ref::<clap::Error>().unwrap();
    let help = clap.to_string();
    assert!(help.contains("status"));
    assert!(help.contains("setup"));
    assert!(help.contains("bridge"));
    assert!(help.contains("Native Windows agent hosting is unsupported"));
    assert!(!help.contains("  list"));
    assert!(!help.contains("  new"));
    assert!(!help.contains("  ask"));

    assert!(run_client_cli(["pika", "list"], &mut runtime, &mut output, &mut errors).is_err());
}

#[test]
fn default_and_explicit_status_are_client_only_and_do_not_touch_ssh() {
    let mut runtime = FakeRuntime::default();
    let (code, output, errors) = run(&["pika"], &mut runtime).unwrap();
    assert_eq!(code, 0);
    assert!(errors.is_empty());
    assert!(output.contains("Pika Windows client"));
    assert!(output.contains("STOPPED · pika bridge start"));
    assert!(output.contains("native Windows agent hosting is unsupported"));
    assert!(runtime.ssh_calls.is_empty());
    assert!(runtime.saved.is_empty());

    runtime.bridge_ready = true;
    let (_, output, _) = run(&["pika", "status"], &mut runtime).unwrap();
    assert!(output.contains("bridge READY"));
}

#[test]
fn setup_uses_fleet_v2_two_receipts_and_saves_the_exact_secret_once() {
    let mut runtime = FakeRuntime::default();
    let (code, output, errors) = run(
        &[
            "pika",
            "setup",
            "developer@devbox",
            "--alias",
            "workstation",
            "--remote-port",
            "49000",
            "--no-start",
        ],
        &mut runtime,
    )
    .unwrap();
    assert_eq!(code, 0);
    assert!(errors.is_empty());
    assert_eq!(runtime.ssh_calls.len(), 2);
    let hello = &runtime.ssh_calls[0];
    assert_eq!(hello.target, "developer@devbox");
    assert_eq!(hello.arguments, ["_fleet", "--stdio"]);
    assert_eq!(hello.payload["protocol"], PROTOCOL_NAME);
    assert_eq!(hello.payload["version"], 2);
    assert_eq!(hello.executable, "ssh.exe");
    assert_eq!(hello.timeout, Duration::from_secs(20));
    let pair: PairRequest = serde_json::from_value(runtime.ssh_calls[1].payload.clone()).unwrap();
    assert_eq!(pair.version, 2);
    assert_eq!(pair.expected_node_id, NODE_ID);
    assert_eq!(pair.client_id, CLIENT_ID);
    assert_eq!(pair.client_label, "test-laptop");
    assert_eq!(pair.token, TOKEN);
    assert_eq!(pair.port, 49_000);
    assert_eq!(runtime.saved.len(), 1);
    let saved = &runtime.saved[0].nodes[NODE_ID];
    assert_eq!(saved.alias, "workstation");
    assert_eq!(saved.ssh_target, "developer@devbox");
    assert_eq!(saved.token, TOKEN);
    assert_eq!(saved.remote_port, Some(49_000));
    assert!(runtime.starts.is_empty());
    assert!(output.contains("PAIRED · workstation"));
    assert!(!output.contains("RemoteForward"));
    assert!(output.contains("Run `pika` to open your board"));
    assert_eq!(runtime.config.default_node_id.as_deref(), Some(NODE_ID));
}

#[test]
fn a_wrong_pairing_identity_is_not_saved_or_started() {
    let mut runtime = FakeRuntime {
        wrong_pair_node: true,
        ..FakeRuntime::default()
    };
    let mut output = Vec::new();
    let mut errors = Vec::new();
    let error = run_client_cli(
        ["pika", "setup", "devbox"],
        &mut runtime,
        &mut output,
        &mut errors,
    )
    .unwrap_err();
    assert!(error.to_string().contains("identity validation"));
    assert!(runtime.saved.is_empty());
    assert!(runtime.starts.is_empty());
}

#[test]
fn bridge_start_and_serve_use_validated_loopback_options_only() {
    let mut runtime = FakeRuntime::default();
    let (_, output, _) = run(
        &[
            "pika",
            "bridge",
            "start",
            "--host",
            "::1",
            "--port",
            "49001",
            "--terminal-executable",
            "fake-wt.exe",
            "--ssh-executable",
            "fake-ssh.exe",
        ],
        &mut runtime,
    )
    .unwrap();
    assert!(output.contains("CLIENT BRIDGE STARTED · loopback ::1:49001"));
    assert_eq!(runtime.starts.len(), 1);
    assert_eq!(runtime.starts[0].terminal_executable, "fake-wt.exe");

    let (_, output, _) = run(
        &[
            "pika",
            "bridge",
            "serve",
            "--host",
            "localhost",
            "--port",
            "49002",
        ],
        &mut runtime,
    )
    .unwrap();
    assert!(output.contains("loopback localhost:49002"));
    assert!(output.contains("Native Windows agent hosting is unsupported"));
    assert_eq!(runtime.serves.len(), 1);

    let start_count = runtime.starts.len();
    let mut output = Vec::new();
    let mut errors = Vec::new();
    assert!(
        run_client_cli(
            ["pika", "bridge", "start", "--host", "0.0.0.0"],
            &mut runtime,
            &mut output,
            &mut errors,
        )
        .is_err()
    );
    assert_eq!(runtime.starts.len(), start_count);
}

#[test]
fn setup_starts_only_the_client_bridge_after_pairing_when_requested() {
    let mut runtime = FakeRuntime::default();
    let (_, output, _) = run(&["pika", "setup", "devbox"], &mut runtime).unwrap();
    assert_eq!(runtime.saved.len(), 1);
    assert_eq!(runtime.starts.len(), 1);
    assert!(output.contains("CLIENT BRIDGE STARTED"));
    assert!(output.contains("local window routing are automatic"));
}

#[test]
fn first_bare_run_selects_pairs_remembers_and_opens_without_manual_commands() {
    let mut runtime = FakeRuntime {
        interactive: true,
        hosts: vec!["devbox".into(), "unselected".into()],
        choices: ["1".into()].into(),
        ..FakeRuntime::default()
    };
    let (_, output, _) = run(&["pika"], &mut runtime).unwrap();
    assert_eq!(runtime.opened.len(), 1);
    assert!(runtime.config.nodes[NODE_ID].allow_fleet_relay);
    assert_eq!(runtime.opened[0].node_id, NODE_ID);
    assert_eq!(runtime.config.default_node_id.as_deref(), Some(NODE_ID));
    assert_eq!(runtime.starts.len(), 1);
    assert!(runtime.ssh_calls.iter().all(|call| call.target == "devbox"));
    assert!(!output.contains("RemoteForward"));
    let count = runtime.ssh_calls.len();
    let saved = runtime.saved.len();
    run(&["pika"], &mut runtime).unwrap();
    assert_eq!(
        runtime.ssh_calls.len(),
        count + 1,
        "one identity check, no repeated pairing"
    );
    assert_eq!(runtime.saved.len(), saved);
    assert_eq!(runtime.starts.len(), 1, "reuse ready bridge");
    assert_eq!(runtime.opened.len(), 2);
    let count = runtime.ssh_calls.len();
    run(&["pika", "status"], &mut runtime).unwrap();
    assert_eq!(
        runtime.ssh_calls.len(),
        count,
        "explicit status remains read-only even on a terminal"
    );
}

#[test]
fn older_pairing_without_default_opens_its_only_known_machine() {
    let mut runtime = FakeRuntime::default();
    run(&["pika", "setup", "devbox", "--no-start"], &mut runtime).unwrap();
    runtime.config.default_node_id = None;
    runtime.interactive = true;
    runtime.hosts = vec!["other-machine".into()];
    run(&["pika"], &mut runtime).unwrap();
    assert_eq!(runtime.opened[0].node_id, NODE_ID);
    assert_eq!(runtime.config.default_node_id.as_deref(), Some(NODE_ID));
    assert!(runtime.ssh_calls.iter().all(|call| call.target == "devbox"));
}

#[test]
fn multiple_legacy_pairings_do_not_guess_a_default() {
    let mut runtime = FakeRuntime::default();
    run(&["pika", "setup", "devbox", "--no-start"], &mut runtime).unwrap();
    let mut second = runtime.config.nodes[NODE_ID].clone();
    second.node_id = WRONG_NODE_ID.into();
    second.alias = "another".into();
    second.ssh_target = "another".into();
    runtime.config.nodes.insert(WRONG_NODE_ID.into(), second);
    runtime.config.default_node_id = None;
    runtime.interactive = true;
    runtime.choices.push_back("".into());
    let count = runtime.ssh_calls.len();
    let (_, output, _) = run(&["pika"], &mut runtime).unwrap();
    assert!(output.contains("1. another · paired"));
    assert_eq!(runtime.ssh_calls.len(), count);
    assert!(runtime.opened.is_empty());
    assert!(runtime.config.default_node_id.is_none());
}

#[test]
fn first_run_cancellation_and_invalid_choices_never_contact_candidates() {
    let mut runtime = FakeRuntime {
        interactive: true,
        hosts: vec!["devbox".into()],
        choices: ["0".into(), "999".into(), "-evil".into(), "".into()].into(),
        ..FakeRuntime::default()
    };
    run(&["pika"], &mut runtime).unwrap();
    assert!(runtime.ssh_calls.is_empty());
    assert!(runtime.saved.is_empty());
    assert!(runtime.opened.is_empty());
    assert!(runtime.starts.is_empty());
}

#[test]
fn manual_host_and_setup_chooser_work_without_ssh_config() {
    let mut runtime = FakeRuntime {
        interactive: true,
        choices: ["user@personal-mac".into()].into(),
        ..FakeRuntime::default()
    };
    run(&["pika", "setup"], &mut runtime).unwrap();
    assert_eq!(runtime.opened[0].ssh_target, "user@personal-mac");
    runtime.choices.push_back("".into());
    let count = runtime.ssh_calls.len();
    run(&["pika", "setup"], &mut runtime).unwrap();
    assert_eq!(
        runtime.ssh_calls.len(),
        count,
        "cancel keeps remembered host"
    );
    assert_eq!(runtime.config.default_node_id.as_deref(), Some(NODE_ID));
}

#[test]
fn changed_node_and_older_host_fail_before_bridge_or_board() {
    let mut runtime = FakeRuntime::default();
    run(&["pika", "setup", "devbox", "--no-start"], &mut runtime).unwrap();
    runtime.interactive = true;
    runtime.wrong_hello_node = true;
    let saved = runtime.saved.len();
    assert!(
        run(&["pika"], &mut runtime)
            .unwrap_err()
            .to_string()
            .contains("identity changed")
    );
    runtime.wrong_hello_node = false;
    runtime.old_host = true;
    assert!(
        run(&["pika"], &mut runtime)
            .unwrap_err()
            .to_string()
            .contains("ssh devbox pika update")
    );
    assert_eq!(runtime.saved.len(), saved);
    assert!(runtime.starts.is_empty());
    assert!(runtime.opened.is_empty());
}

#[test]
fn first_run_old_host_is_not_paired_or_remembered() {
    let mut runtime = FakeRuntime {
        interactive: true,
        old_host: true,
        choices: ["devbox".into()].into(),
        ..FakeRuntime::default()
    };
    assert!(
        run(&["pika"], &mut runtime)
            .unwrap_err()
            .to_string()
            .contains("ssh devbox pika update")
    );
    assert_eq!(runtime.ssh_calls.len(), 1);
    assert!(runtime.saved.is_empty());
    assert!(runtime.starts.is_empty());
    assert!(runtime.opened.is_empty());
}

#[test]
fn board_connection_owns_its_forward_and_binds_exact_machine_without_shell_text() {
    let mut runtime = FakeRuntime::default();
    run(
        &["pika", "setup", "user@devbox", "--no-start"],
        &mut runtime,
    )
    .unwrap();
    let node = &runtime.config.nodes[NODE_ID];
    let args = pikamux::client_cli::board_ssh_arguments(node).unwrap();
    assert!(
        args.windows(2)
            .any(|pair| pair == ["-R", "127.0.0.1:47654:127.0.0.1:47653"])
    );
    assert!(args.windows(2).any(|pair| pair == ["-S", "none"]));
    assert!(args.contains(&"ExitOnForwardFailure=yes".into()));
    assert!(args.ends_with(&[
        "user@devbox".into(),
        "pika".into(),
        "_client-board".into(),
        "--expected-node-id".into(),
        NODE_ID.into()
    ]));
    assert!(!args.iter().any(|arg| arg.contains(TOKEN)));
    let mut bad = node.clone();
    bad.node_id = "x; echo unsafe".into();
    assert!(pikamux::client_cli::board_ssh_arguments(&bad).is_err());
}

#[test]
fn remembered_machine_config_roundtrips_and_rejects_unpaired_default() {
    let mut runtime = FakeRuntime::default();
    run(&["pika", "setup", "devbox", "--no-start"], &mut runtime).unwrap();
    let mut value = runtime.config.to_value();
    assert_eq!(ClientConfig::from_value(&value).unwrap(), runtime.config);
    value["default_node_id"] = json!(WRONG_NODE_ID);
    assert!(ClientConfig::from_value(&value).is_err());
    value.as_object_mut().unwrap().remove("default_node_id");
    assert_eq!(
        ClientConfig::from_value(&value).unwrap().default_node_id,
        None
    );
}
