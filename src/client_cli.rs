//! Windows fleet client with an optional loopback window bridge.
//!
//! This parser intentionally has no agent-hosting commands. Its production
//! runtime renders the combined fleet locally, invokes selected SSH pairing calls,
//! and can serve optional exact window-launch requests on loopback. Tests inject the
//! runtime so no SSH process, socket, or Windows Terminal window is created.

use crate::VERSION;
use crate::client_bridge::{
    ClientConfig, ClientLaunchBridge, ClientNode, LoopbackEndpoint, ProcessWindowLauncher,
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
use std::io::{BufRead, IsTerminal, Read, Write};
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
    about = "Open your Pika board from Windows.",
    after_help = "This client opens exact conversations hosted by paired macOS/Linux machines. Native Windows agent hosting is unsupported."
)]
struct WindowsClientCli {
    #[command(subcommand)]
    command: Option<WindowsClientCommand>,
}

#[derive(Subcommand, Debug)]
enum WindowsClientCommand {
    /// Install the latest verified Pika client on this machine.
    Update,
    /// Show selected machines and optional bridge readiness.
    Status,
    /// Add machines to your combined board.
    Setup(ClientSetupArgs),
    /// Inspect or run the loopback-only window bridge.
    Bridge(ClientBridgeArgs),
}

#[derive(Args, Debug)]
struct ClientSetupArgs {
    ssh_target: Option<String>,
    #[arg(long)]
    alias: Option<String>,
    #[arg(long, default_value = "ssh.exe")]
    ssh_executable: String,
    #[arg(long, default_value_t = crate::client_bridge::DEFAULT_REMOTE_PORT)]
    remote_port: u16,
    /// Compatibility flag; normal boards no longer need a bridge.
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
    /// Real terminal presentation is opt-in; injected runtimes stay finite.
    fn styled_setup(&self) -> bool {
        false
    }
    fn update(&mut self) -> Result<i32> {
        bail!("Client updater is unavailable")
    }
    fn interactive(&self) -> bool {
        false
    }
    fn discover_hosts(&mut self) -> Result<Vec<String>> {
        Ok(Vec::new())
    }
    fn read_choice(&mut self) -> Result<Option<String>> {
        Ok(None)
    }
    fn open_fleet_board(&mut self, _config: &ClientConfig) -> Result<i32> {
        bail!("Interactive fleet board is unavailable")
    }
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
    fn styled_setup(&self) -> bool {
        crate::onboarding::supported()
    }
    fn update(&mut self) -> Result<i32> {
        crate::windows_update::install(None, false)
    }
    fn interactive(&self) -> bool {
        std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
    }

    fn discover_hosts(&mut self) -> Result<Vec<String>> {
        let home = directories::BaseDirs::new().context("Cannot find your SSH configuration")?;
        let mut candidates = crate::fleet::discover_ssh_candidates(&home.home_dir().join(".ssh"));
        let (tailscale, _) = crate::fleet::discover_tailscale_candidates(
            std::path::Path::new("tailscale.exe"),
            Duration::from_secs(2),
        );
        candidates.extend(tailscale);
        let mut targets = Vec::new();
        for candidate in candidates {
            if !targets.contains(&candidate.ssh_target) {
                targets.push(candidate.ssh_target);
            }
        }
        Ok(targets)
    }

    fn read_choice(&mut self) -> Result<Option<String>> {
        let mut line = Vec::new();
        std::io::stdin()
            .lock()
            .take(1025)
            .read_until(b'\n', &mut line)?;
        if line.len() > 1024 {
            bail!("Machine selection is too long");
        }
        if line.is_empty() {
            return Ok(None);
        }
        Ok(Some(String::from_utf8(line)?.trim().to_owned()))
    }

    fn open_fleet_board(&mut self, config: &ClientConfig) -> Result<i32> {
        crate::client_board::run(
            config.clone(),
            &self.config_path.with_file_name("client-fleet.db"),
        )
    }

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
            .args([
                "-T",
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=8",
                "-o",
                "ClearAllForwardings=yes",
                "-o",
                "RemoteCommand=none",
            ])
            .arg(target)
            .arg(crate::fleet::remote_pika_command(remote_arguments)?)
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
            if let Err(error) = serve_client_bridge_once_reloading(&listener, &mut bridge, || {
                load_client_config(&self.config_path)
            }) {
                // A caller that times out or disconnects must not kill the
                // listener (nor erase its completed-request deduplication).
                eprintln!("Client bridge request ended: {error}");
                thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

pub fn run<I, T>(args: I) -> Result<i32>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let args = args.into_iter().map(Into::into).collect::<Vec<OsString>>();
    #[cfg(windows)]
    if let Some(code) = crate::windows_update::forward(&args)? {
        return Ok(code);
    }
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
        Some(WindowsClientCommand::Update) => {
            if !runtime.interactive() {
                bail!("Run `pika update` in an interactive terminal to confirm the update");
            }
            write!(output, "Update Pika on this machine? [y/N] ")?;
            output.flush()?;
            if runtime
                .read_choice()?
                .is_some_and(|choice| matches!(choice.trim(), "y" | "Y"))
            {
                runtime.update()
            } else {
                writeln!(output, "Update skipped.")?;
                Ok(0)
            }
        }
        None if runtime.interactive() => home(runtime, output, false),
        None | Some(WindowsClientCommand::Status) => status(runtime, output),
        Some(WindowsClientCommand::Setup(arguments)) if arguments.ssh_target.is_none() => {
            home(runtime, output, true)
        }
        Some(WindowsClientCommand::Setup(arguments)) => {
            let ui = crate::onboarding::Screen::new(runtime.styled_setup())?;
            if !ui.active() {
                return setup(runtime, output, arguments, false);
            }
            ui.progress(
                "Connect your machine",
                &format!(
                    "Connecting to {}…",
                    arguments.ssh_target.as_deref().unwrap_or("your host")
                ),
            )?;
            let mut receipt = Vec::new();
            if let Err(error) = setup(runtime, &mut receipt, arguments, false) {
                ui.details(
                    "Connection needs attention",
                    &format!("{error:#}\n\nExisting connections were kept."),
                )?;
                return Ok(1);
            }
            let open = ui.choice(
                "Connected",
                "This machine's conversations can now appear on your board.",
                &["Open board", "Done"],
            )? == Some(0);
            drop(ui);
            if open {
                let config = runtime.load_config()?;
                runtime.open_fleet_board(&config)
            } else {
                Ok(0)
            }
        }
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

fn home<R: ClientCliRuntime, W: Write>(
    runtime: &mut R,
    output: &mut W,
    choose: bool,
) -> Result<i32> {
    if !runtime.interactive() {
        bail!(
            "Run `pika setup` in an interactive PowerShell window, or use `pika setup SSH_HOST`."
        );
    }
    let config = runtime.load_config()?;
    if !choose && !config.nodes.is_empty() {
        // Existing pairings are already user-selected. No single default host,
        // blocking handshake, bridge startup, or topology import before paint.
        return runtime.open_fleet_board(&config);
    }
    let ui = crate::onboarding::Screen::new(runtime.styled_setup())?;
    ui.progress(
        "Find your machines",
        "Reading saved SSH connections. No machines are being contacted.",
    )?;
    let mut targets = runtime.discover_hosts()?;
    targets.retain(|target| validate_client_ssh_target(target).is_ok());
    for node in config.nodes.values() {
        if !targets.contains(&node.ssh_target) {
            targets.push(node.ssh_target.clone());
        }
    }
    if !ui.active() {
        writeln!(
            output,
            "Which machines should appear together in Pika?\nDiscovery is passive. Only selected machines are contacted; existing pairings stay selected."
        )?;
        for (index, target) in targets.iter().enumerate() {
            let paired = if config.nodes.values().any(|node| node.ssh_target == *target) {
                " · paired"
            } else {
                ""
            };
            writeln!(output, "  {}. {target}{paired}", index + 1)?;
        }
    }
    let selected = if ui.active() {
        let labels = targets
            .iter()
            .map(|target| {
                format!(
                    "{target}{}",
                    if config.nodes.values().any(|n| n.ssh_target == *target) {
                        " · connected"
                    } else {
                        ""
                    }
                )
            })
            .collect::<Vec<_>>();
        match ui.choice("One home for your coding agents", "Connect a Mac or Linux machine to see its conversations here.\nYour selected machines will appear together on one board.", &["Choose saved connections", "Enter an SSH host", "Not now"])? {
            Some(0) => ui.select("Choose your machines", "Only selected machines will be contacted.\nExisting connections stay on your board.", &labels, true)?.unwrap_or_default().into_iter().map(|i| targets[i].clone()).collect(),
            Some(1) => {
                let mut selected = Vec::new();
                while let Some(value) = ui.input("Connect an SSH host", "Enter one SSH alias or user@host.")? {
                    match validate_client_ssh_target(&value) {
                        Ok(_) => { selected.push(value); break; }
                        Err(error) => ui.details("Check the SSH host", &error.to_string())?,
                    }
                }
                selected
            }
            _ => Vec::new(),
        }
    } else {
        loop {
            write!(
                output,
                "Machine numbers, all, or SSH hosts (Enter to keep current): "
            )?;
            output.flush()?;
            let Some(choice) = runtime.read_choice()?.filter(|value| !value.is_empty()) else {
                return if config.nodes.is_empty() {
                    Ok(0)
                } else {
                    runtime.open_fleet_board(&config)
                };
            };
            match select_targets(&choice, &targets) {
                Ok(selected) => break selected,
                Err(error) => writeln!(output, "{error}")?,
            }
        }
    };
    let mut notices = Vec::new();
    for target in selected {
        if config.nodes.values().any(|node| node.ssh_target == target) {
            continue;
        }
        ui.progress("Connect your machines", &format!("Connecting to {target}…"))?;
        let mut receipt = Vec::new();
        let pairing = setup(
            runtime,
            &mut receipt,
            ClientSetupArgs {
                ssh_target: Some(target.clone()),
                alias: None,
                ssh_executable: "ssh.exe".into(),
                remote_port: crate::client_bridge::DEFAULT_REMOTE_PORT,
                no_start: true,
            },
            true,
        );
        if !ui.active() {
            output.write_all(&receipt)?;
        }
        if let Err(error) = pairing {
            // One unreachable candidate must not hide successfully paired nodes.
            if ui.active() {
                notices.push(format!("{target} was not connected.\n{error:#}"));
            } else {
                writeln!(output, "Not added · {target} · {error:#}")?;
            }
        }
    }
    let config = runtime.load_config()?;
    if ui.active() && !notices.is_empty() {
        ui.details("Some connections need attention", &format!("{} machine(s) connected. Successful connections were kept.\n\n{}\n\nCheck SSH access and that Pika is installed on these hosts, then run pika setup to try again.", config.nodes.len(), notices.join("\n\n")))?;
    }
    drop(ui);
    if config.nodes.is_empty() {
        return Ok(0);
    }
    output.flush()?;
    runtime.open_fleet_board(&config)
}

pub fn select_targets(choice: &str, targets: &[String]) -> Result<Vec<String>> {
    if choice.trim().eq_ignore_ascii_case("all") {
        return Ok(targets.to_vec());
    }
    let mut selected = Vec::new();
    for part in choice
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter(|p| !p.is_empty())
    {
        let target = if let Ok(number) = part.parse::<usize>() {
            targets
                .get(
                    number
                        .checked_sub(1)
                        .context("Choose a listed number or SSH host.")?,
                )
                .context("Choose a listed number or SSH host.")?
                .clone()
        } else {
            validate_client_ssh_target(part)?.to_owned()
        };
        if !selected.contains(&target) {
            selected.push(target);
        }
    }
    if selected.is_empty() {
        bail!("Choose a listed number or SSH host.");
    }
    Ok(selected)
}

/// No caller-supplied shell text; the host checks its UUID on this connection.
pub fn board_ssh_arguments(node: &ClientNode) -> Result<Vec<String>> {
    validate_client_ssh_target(&node.ssh_target)?;
    if uuid::Uuid::parse_str(&node.node_id).is_err() {
        bail!("Invalid board machine identity");
    }
    let port = node
        .remote_port
        .unwrap_or(crate::client_bridge::DEFAULT_REMOTE_PORT);
    if port < 1024 {
        bail!("Invalid reverse bridge port");
    }
    Ok(vec![
        "-tt".into(),
        "-S".into(),
        "none".into(),
        "-o".into(),
        "ControlMaster=no".into(),
        "-o".into(),
        "RemoteCommand=none".into(),
        "-o".into(),
        "ExitOnForwardFailure=yes".into(),
        "-o".into(),
        "ConnectTimeout=8".into(),
        "-o".into(),
        "ServerAliveInterval=15".into(),
        "-o".into(),
        "ServerAliveCountMax=3".into(),
        "-R".into(),
        format!(
            "127.0.0.1:{port}:127.0.0.1:{}",
            crate::client_bridge::DEFAULT_LOCAL_PORT
        ),
        node.ssh_target.clone(),
        crate::fleet::remote_pika_command(&[
            "_client-board".into(),
            "--expected-node-id".into(),
            node.node_id.clone(),
        ])?,
    ])
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
        "Pika Windows client · {} paired machine(s) · optional bridge {bridge}",
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
        writeln!(
            output,
            "Run `pika` in an interactive terminal to choose your machine."
        )?;
    } else {
        writeln!(
            output,
            "Run `pika` for all paired machines together. Use `pika setup` to add machines."
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
    opening: bool,
) -> Result<i32> {
    let ssh_target = arguments
        .ssh_target
        .as_deref()
        .context("Choose a machine with `pika`")?;
    validate_client_ssh_target(ssh_target)?;
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
        ssh_target,
        &["_fleet".to_owned(), "--stdio".to_owned()],
        &hello_request,
        &arguments.ssh_executable,
        Duration::from_secs(20),
    )?;
    let validated = validate_pairing_hello(&hello)?;
    if let Some(existing) = config
        .nodes
        .values()
        .find(|node| node.ssh_target == ssh_target)
        && existing.node_id != validated.node_id
    {
        bail!("Machine identity changed for {ssh_target}. Your pairing was not replaced.");
    }
    let hello = validated;
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
        ssh_target,
        &["_client-pair".to_owned(), "--stdio".to_owned()],
        &pair_value,
        &arguments.ssh_executable,
        Duration::from_secs(20),
    )?;
    install_client_pairing(
        &mut config,
        &hello.node_id,
        arguments.alias.as_deref().unwrap_or(&hello.machine),
        ssh_target,
        &token,
        arguments.remote_port,
        &receipt,
    )?;
    config.default_node_id = Some(hello.node_id.clone());
    runtime.save_config(&config)?;
    let node = &config.nodes[&hello.node_id];
    writeln!(
        output,
        "PAIRED · {} · node {} · exact launches only",
        node.alias,
        &node.node_id[..8]
    )?;
    if !opening {
        writeln!(
            output,
            "Run `pika` to open your board. The connection and local window routing are automatic."
        )?;
    }
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
