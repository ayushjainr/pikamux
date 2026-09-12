//! Windows-only command surface for Pika's experimental client bridge.
//!
//! This parser intentionally has no agent-hosting commands. Its production
//! runtime reads the client pairing file, invokes explicit SSH pairing calls,
//! and serves exact window-launch requests on loopback. Tests inject the
//! runtime so no SSH process, socket, or Windows Terminal window is created.

use crate::VERSION;
use crate::client_bridge::{
    ClientConfig, ClientLaunchBridge, LoopbackEndpoint, ProcessWindowLauncher,
    TcpClientBridgeTransport, bind_client_bridge, client_bridge_running,
    default_client_config_path, generate_pairing_token, install_client_pairing, load_client_config,
    make_pair_request, serve_client_bridge_once_reloading, validate_client_ssh_target,
    validate_pairing_hello, write_client_config,
};
use crate::consult::{
    CancellablePipe, CancellationToken, OwnedChild, poll_owned_child, terminate_child,
};
use crate::fleet::{PROTOCOL_NAME as FLEET_PROTOCOL, PROTOCOL_VERSION as FLEET_VERSION};
use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use serde_json::{Value, json};
use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

const MAX_PAIR_STDERR_BYTES: usize = 64 * 1024;

#[derive(Parser, Debug)]
#[command(
    name = "pika",
    version = VERSION,
    about = "Pika experimental Windows client and secure local-window bridge.",
    after_help = "This client opens exact conversations hosted by paired macOS/Linux machines. Native Windows agent hosting is unsupported."
)]
struct WindowsClientCli {
    #[command(subcommand)]
    command: Option<WindowsClientCommand>,
}

#[derive(Subcommand, Debug)]
enum WindowsClientCommand {
    /// Show paired machines and local bridge readiness.
    Status,
    /// Pair this client with one trusted Pika machine over SSH.
    Setup(ClientSetupArgs),
    /// Inspect or run the loopback-only window bridge.
    Bridge(ClientBridgeArgs),
}

#[derive(Args, Debug)]
struct ClientSetupArgs {
    ssh_target: String,
    #[arg(long)]
    alias: Option<String>,
    #[arg(long, default_value = "ssh.exe")]
    ssh_executable: String,
    #[arg(long, default_value_t = crate::client_bridge::DEFAULT_REMOTE_PORT)]
    remote_port: u16,
    /// Pair only; do not start the local bridge in the background.
    #[arg(long)]
    no_start: bool,
}

#[derive(Args, Debug)]
struct ClientBridgeArgs {
    #[command(subcommand)]
    command: Option<ClientBridgeCommand>,
}

#[derive(Subcommand, Debug)]
enum ClientBridgeCommand {
    /// Show paired machines and local bridge readiness.
    Status,
    /// Serve paired launch requests on loopback in this process.
    Serve(ClientBridgeOptions),
    /// Start the loopback bridge as a detached client process.
    Start(ClientBridgeOptions),
}

#[derive(Args, Clone, Debug, Eq, PartialEq)]
pub struct ClientBridgeOptions {
    #[arg(long, default_value = "127.0.0.1")]
    pub host: String,
    #[arg(long, default_value_t = crate::client_bridge::DEFAULT_LOCAL_PORT)]
    pub port: u16,
    #[arg(long, default_value = "wt.exe")]
    pub terminal_executable: String,
    #[arg(long, default_value = "ssh.exe")]
    pub ssh_executable: String,
}

impl Default for ClientBridgeOptions {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_owned(),
            port: crate::client_bridge::DEFAULT_LOCAL_PORT,
            terminal_executable: "wt.exe".to_owned(),
            ssh_executable: "ssh.exe".to_owned(),
        }
    }
}

/// OS/process boundary for deterministic Windows-client workflow tests.
pub trait ClientCliRuntime {
    fn load_config(&mut self) -> Result<ClientConfig>;
    fn save_config(&mut self, config: &ClientConfig) -> Result<()>;
    fn client_label(&mut self) -> Result<String>;
    fn pairing_token(&mut self) -> Result<String>;
    fn ssh_json(
        &mut self,
        ssh_target: &str,
        remote_arguments: &[String],
        payload: &Value,
        ssh_executable: &str,
        timeout: Duration,
    ) -> Result<Value>;
    fn bridge_running(&mut self, endpoint: &LoopbackEndpoint) -> bool;
    fn start_bridge(&mut self, options: &ClientBridgeOptions) -> Result<()>;
    fn serve_bridge(&mut self, options: &ClientBridgeOptions) -> Result<()>;
}

pub struct SystemClientRuntime {
    config_path: PathBuf,
}

impl SystemClientRuntime {
    pub fn discover() -> Result<Self> {
        Ok(Self {
            config_path: default_client_config_path()?,
        })
    }
}

impl ClientCliRuntime for SystemClientRuntime {
    fn load_config(&mut self) -> Result<ClientConfig> {
        load_client_config(&self.config_path).map_err(Into::into)
    }

    fn save_config(&mut self, config: &ClientConfig) -> Result<()> {
        write_client_config(config, &self.config_path).map_err(Into::into)
    }

    fn client_label(&mut self) -> Result<String> {
        let label = std::env::var("COMPUTERNAME")
            .or_else(|_| std::env::var("HOSTNAME"))
            .unwrap_or_else(|_| "pika-client".to_owned());
        Ok(label
            .split('.')
            .next()
            .filter(|value| !value.is_empty())
            .unwrap_or("pika-client")
            .to_owned())
    }

    fn pairing_token(&mut self) -> Result<String> {
        Ok(generate_pairing_token())
    }

    fn ssh_json(
        &mut self,
        ssh_target: &str,
        remote_arguments: &[String],
        payload: &Value,
        ssh_executable: &str,
        timeout: Duration,
    ) -> Result<Value> {
        let deadline = Instant::now() + timeout;
        let target = validate_client_ssh_target(ssh_target)?;
        if ssh_executable.is_empty() {
            bail!("SSH executable is missing");
        }
        // Reject oversized requests before starting SSH or exposing a token.
        let mut encoded = serde_json::to_vec(payload)?;
        encoded.push(b'\n');
        if encoded.len() > crate::client_bridge::MAX_BRIDGE_MESSAGE_BYTES {
            bail!("SSH pairing request exceeded the safety limit");
        }
        let mut command = Command::new(ssh_executable);
        command
            .args(["-T", "-o", "BatchMode=yes", "-o", "ConnectTimeout=8"])
            .arg(target)
            .arg("pika")
            .args(remote_arguments)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = OwnedChild::spawn(&mut command)
            .with_context(|| format!("SSH pairing could not start {ssh_executable}"))?;
        let stop = CancellationToken::default();
        let stdin = CancellablePipe::new(
            child
                .stdin
                .take()
                .context("SSH pairing has no input pipe")?,
            stop.clone(),
        )?;
        let stdout = CancellablePipe::new(
            child
                .stdout
                .take()
                .context("SSH pairing has no output pipe")?,
            stop.clone(),
        )?;
        let stderr = CancellablePipe::new(
            child
                .stderr
                .take()
                .context("SSH pairing has no error pipe")?,
            stop.clone(),
        )?;
        enum Stream {
            Input,
            Output,
            Error,
        }
        let (sender, receiver) = mpsc::sync_channel(3);
        let input_sender = sender.clone();
        let input_writer = thread::spawn(move || {
            let result = write_pairing_request(stdin, &encoded).map(|()| Vec::new());
            let _ = input_sender.send((Stream::Input, result));
        });
        let output_sender = sender.clone();
        let output_reader = thread::spawn(move || {
            let result = read_limited(stdout, crate::client_bridge::MAX_BRIDGE_MESSAGE_BYTES);
            let _ = output_sender.send((Stream::Output, result));
        });
        let error_reader = thread::spawn(move || {
            let result = read_limited(stderr, MAX_PAIR_STDERR_BYTES);
            let _ = sender.send((Stream::Error, result));
        });
        let outcome = (|| {
            let mut input_written = false;
            let mut stdout = None;
            let mut stderr = None;
            loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    bail!("SSH pairing timed out");
                }
                // Observing exit must not release the leader's PID before
                // inherited-pipe descendants in its group have been stopped.
                let status = poll_owned_child(&mut child).context("SSH pairing wait failed")?;
                let io_complete = input_written && stdout.is_some() && stderr.is_some();
                if io_complete {
                    if let Some(status) = status {
                        return Ok((status, stdout.unwrap(), stderr.unwrap()));
                    }
                    thread::sleep(remaining.min(Duration::from_millis(10)));
                    continue;
                }
                match receiver.recv_timeout(remaining.min(Duration::from_millis(10))) {
                    Ok((stream, result)) => match stream {
                        Stream::Input => {
                            result?;
                            input_written = true;
                        }
                        Stream::Output => stdout = Some(result?),
                        Stream::Error => stderr = Some(result?),
                    },
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        bail!("SSH pairing I/O worker failed");
                    }
                }
            }
        })();
        let cleanup = terminate_child(&mut child);
        stop.cancel();
        for worker in [input_writer, output_reader, error_reader] {
            // Unix pipes are nonblocking and cancellation bounds these joins.
            // The experimental Windows client lacks cancellable OS pipe I/O:
            // detach unfinished workers there instead of hanging the caller.
            #[cfg(unix)]
            let _ = worker.join();
            #[cfg(not(unix))]
            if worker.is_finished() {
                let _ = worker.join();
            }
        }
        cleanup.context("SSH pairing cleanup failed")?;
        let (status, stdout, stderr) = outcome?;
        if !status.success() {
            let detail = String::from_utf8_lossy(if stderr.is_empty() { &stdout } else { &stderr });
            let detail = detail.trim();
            bail!(
                "{}",
                if detail.is_empty() {
                    format!("SSH pairing exited {status}")
                } else {
                    detail.chars().take(500).collect()
                }
            );
        }
        crate::client_bridge::read_bridge_message(&mut std::io::Cursor::new(stdout))
            .map_err(Into::into)
    }

    fn bridge_running(&mut self, endpoint: &LoopbackEndpoint) -> bool {
        client_bridge_running(&mut TcpClientBridgeTransport, endpoint)
    }

    fn start_bridge(&mut self, options: &ClientBridgeOptions) -> Result<()> {
        #[cfg(not(windows))]
        {
            let _ = options;
            bail!("The detached client bridge is available only on Windows");
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
            const DETACHED_PROCESS: u32 = 0x0000_0008;
            let log_path = self.config_path.with_file_name("client-bridge.log");
            if let Some(parent) = log_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let log = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log_path)?;
            let errors = log.try_clone()?;
            Command::new(std::env::current_exe()?)
                .args([
                    "bridge",
                    "serve",
                    "--host",
                    &options.host,
                    "--port",
                    &options.port.to_string(),
                    "--terminal-executable",
                    &options.terminal_executable,
                    "--ssh-executable",
                    &options.ssh_executable,
                ])
                .stdin(Stdio::null())
                .stdout(log)
                .stderr(errors)
                .creation_flags(CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS)
                .spawn()
                .with_context(|| {
                    format!(
                        "Client bridge did not start; inspect {}",
                        log_path.display()
                    )
                })?;
            let endpoint = client_endpoint(options)?;
            for _ in 0..20 {
                if self.bridge_running(&endpoint) {
                    return Ok(());
                }
                thread::sleep(Duration::from_millis(50));
            }
            bail!(
                "Client bridge did not start; inspect {}",
                log_path.display()
            );
        }
    }

    fn serve_bridge(&mut self, options: &ClientBridgeOptions) -> Result<()> {
        let endpoint = client_endpoint(options)?;
        let listener = bind_client_bridge(&endpoint)?;
        let config = self.load_config()?;
        let mut bridge = ClientLaunchBridge::new(
            config,
            ProcessWindowLauncher,
            &options.terminal_executable,
            &options.ssh_executable,
        )?;
        loop {
            serve_client_bridge_once_reloading(&listener, &mut bridge, || {
                load_client_config(&self.config_path)
            })?;
        }
    }
}

pub fn run<I, T>(args: I) -> Result<i32>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let mut runtime = SystemClientRuntime::discover()?;
    let stdout = std::io::stdout();
    let stderr = std::io::stderr();
    run_with(args, &mut runtime, &mut stdout.lock(), &mut stderr.lock())
}

pub fn run_with<I, T, R, W, E>(
    args: I,
    runtime: &mut R,
    output: &mut W,
    _errors: &mut E,
) -> Result<i32>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
    R: ClientCliRuntime,
    W: Write,
    E: Write,
{
    let cli = WindowsClientCli::try_parse_from(args)?;
    match cli.command {
        None | Some(WindowsClientCommand::Status) => status(runtime, output),
        Some(WindowsClientCommand::Setup(arguments)) => setup(runtime, output, arguments),
        Some(WindowsClientCommand::Bridge(arguments)) => match arguments.command {
            None | Some(ClientBridgeCommand::Status) => status(runtime, output),
            Some(ClientBridgeCommand::Serve(options)) => {
                let endpoint = client_endpoint(&options)?;
                let count = runtime.load_config()?.nodes.len();
                writeln!(
                    output,
                    "Pika Windows client bridge · loopback {}:{} · {} paired machine(s)\nNative Windows agent hosting is unsupported; leave this client window router running.",
                    endpoint.host, endpoint.port, count
                )?;
                runtime.serve_bridge(&options)?;
                Ok(0)
            }
            Some(ClientBridgeCommand::Start(options)) => {
                start_bridge(runtime, output, &options)?;
                Ok(0)
            }
        },
    }
}

fn status<R: ClientCliRuntime, W: Write>(runtime: &mut R, output: &mut W) -> Result<i32> {
    let config = runtime.load_config()?;
    let endpoint = client_endpoint(&ClientBridgeOptions::default())?;
    let bridge = if runtime.bridge_running(&endpoint) {
        "READY"
    } else {
        "STOPPED · pika bridge start"
    };
    writeln!(
        output,
        "Pika Windows client · bridge {bridge} · {} paired machine(s)",
        config.nodes.len()
    )?;
    let mut nodes = config.nodes.values().collect::<Vec<_>>();
    nodes.sort_by_key(|node| node.alias.to_ascii_lowercase());
    for node in nodes {
        writeln!(
            output,
            "  {:<20} node {} · {}",
            node.alias,
            &node.node_id[..8],
            node.ssh_target
        )?;
    }
    if config.nodes.is_empty() {
        writeln!(output, "Pair one with `pika setup SSH_HOST`.")?;
    } else {
        writeln!(
            output,
            "Start local window routing with `pika bridge start`."
        )?;
    }
    writeln!(
        output,
        "Client/bridge only · native Windows agent hosting is unsupported."
    )?;
    Ok(0)
}

fn setup<R: ClientCliRuntime, W: Write>(
    runtime: &mut R,
    output: &mut W,
    arguments: ClientSetupArgs,
) -> Result<i32> {
    validate_client_ssh_target(&arguments.ssh_target)?;
    if arguments.remote_port < 1024 {
        bail!("Reverse bridge port must be between 1024 and 65535");
    }
    let mut config = runtime.load_config()?;
    let hello_request = json!({
        "op": "hello",
        "protocol": FLEET_PROTOCOL,
        "version": FLEET_VERSION,
    });
    let hello = runtime.ssh_json(
        &arguments.ssh_target,
        &["_fleet".to_owned(), "--stdio".to_owned()],
        &hello_request,
        &arguments.ssh_executable,
        Duration::from_secs(20),
    )?;
    let hello = validate_pairing_hello(&hello)?;
    let token = runtime.pairing_token()?;
    let pair = make_pair_request(
        &hello.node_id,
        &config.client_id,
        &runtime.client_label()?,
        &token,
        arguments.remote_port,
    )?;
    let pair_value = serde_json::to_value(&pair)?;
    let receipt = runtime.ssh_json(
        &arguments.ssh_target,
        &["_client-pair".to_owned(), "--stdio".to_owned()],
        &pair_value,
        &arguments.ssh_executable,
        Duration::from_secs(20),
    )?;
    install_client_pairing(
        &mut config,
        &hello.node_id,
        arguments.alias.as_deref().unwrap_or(&hello.machine),
        &arguments.ssh_target,
        &token,
        arguments.remote_port,
        &receipt,
    )?;
    runtime.save_config(&config)?;
    let node = &config.nodes[&hello.node_id];
    writeln!(
        output,
        "PAIRED · {} · node {} · exact launches only",
        node.alias,
        &node.node_id[..8]
    )?;
    writeln!(
        output,
        "\nAdd this to the matching Host block in your local SSH config:\n  RemoteForward 127.0.0.1:{} 127.0.0.1:{}",
        arguments.remote_port,
        crate::client_bridge::DEFAULT_LOCAL_PORT
    )?;
    if !arguments.no_start {
        let options = ClientBridgeOptions {
            ssh_executable: arguments.ssh_executable,
            ..ClientBridgeOptions::default()
        };
        start_bridge(runtime, output, &options)?;
    }
    writeln!(
        output,
        "Reconnect SSH after adding the forward. Enter on the remote board will launch an exact local Windows Terminal window; the bridge does not confirm the later attach."
    )?;
    Ok(0)
}

fn start_bridge<R: ClientCliRuntime, W: Write>(
    runtime: &mut R,
    output: &mut W,
    options: &ClientBridgeOptions,
) -> Result<()> {
    let endpoint = client_endpoint(options)?;
    if runtime.bridge_running(&endpoint) {
        writeln!(
            output,
            "CLIENT BRIDGE READY · loopback {}:{}",
            endpoint.host, endpoint.port
        )?;
        return Ok(());
    }
    runtime.start_bridge(options)?;
    writeln!(
        output,
        "CLIENT BRIDGE STARTED · loopback {}:{}",
        endpoint.host, endpoint.port
    )?;
    Ok(())
}

fn client_endpoint(options: &ClientBridgeOptions) -> Result<LoopbackEndpoint> {
    LoopbackEndpoint::new(&options.host, options.port, Duration::from_millis(350))
        .map_err(Into::into)
}

fn write_pairing_request(mut stdin: impl Write, encoded: &[u8]) -> Result<()> {
    stdin.write_all(encoded).context("SSH pairing write failed")
}

fn read_limited<R: Read>(input: R, limit: usize) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    input.take((limit + 1) as u64).read_to_end(&mut output)?;
    if output.len() > limit {
        bail!("SSH pairing output exceeded the safety limit");
    }
    Ok(output)
}

#[cfg(all(test, unix))]
mod pairing_pipe_tests {
    use super::*;
    use std::os::unix::net::UnixStream;

    #[test]
    fn cancelled_pairing_writer_does_not_wait_for_input_capacity() {
        let (writer, _held_reader) = UnixStream::pair().unwrap();
        let stop = CancellationToken::default();
        let writer = CancellablePipe::new(writer, stop.clone()).unwrap();
        // Exercise the worker below the request-size gate with more bytes
        // than the socket can buffer. The peer deliberately never reads.
        let worker =
            thread::spawn(move || write_pairing_request(writer, &vec![b'x'; 4 * 1024 * 1024]));
        thread::sleep(Duration::from_millis(30));
        assert!(!worker.is_finished());
        let started = Instant::now();
        stop.cancel();
        let error = worker.join().unwrap().unwrap_err();
        assert!(error.to_string().contains("write failed"));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn cancelled_pairing_reader_does_not_wait_for_inherited_pipe_eof() {
        let (reader, mut held_writer) = UnixStream::pair().unwrap();
        held_writer.write_all(b"partial").unwrap();
        let stop = CancellationToken::default();
        let reader = CancellablePipe::new(reader, stop.clone()).unwrap();
        let worker = thread::spawn(move || read_limited(reader, MAX_PAIR_STDERR_BYTES));
        thread::sleep(Duration::from_millis(30));
        assert!(!worker.is_finished());
        let started = Instant::now();
        stop.cancel();
        assert_eq!(worker.join().unwrap().unwrap(), b"partial");
        assert!(started.elapsed() < Duration::from_secs(1));
        drop(held_writer);
    }
}
