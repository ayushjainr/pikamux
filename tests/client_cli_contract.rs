use anyhow::Result;
use pikamux::client_bridge::{
    BRIDGE_PROTOCOL, BRIDGE_VERSION, ClientConfig, LoopbackEndpoint, PairRequest,
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
                "node_id": NODE_ID,
                "machine": "devbox",
                "package_version": "0.6.0-alpha.1",
                "capabilities": CAPABILITIES,
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
    assert!(output.contains("RemoteForward 127.0.0.1:49000 127.0.0.1:47653"));
    assert!(output.contains("does not confirm the later attach"));
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
    assert!(output.contains("exact local Windows Terminal window"));
}
