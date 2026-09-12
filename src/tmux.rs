use crate::{
    consult::{CancellablePipe, CancellationToken, OwnedChild, poll_owned_child, terminate_child},
    model::{Pane, Provider},
    terminal::{self, BACKGROUND_ENV, FOREGROUND_ENV, Palette},
};
use anyhow::{Context, Result, bail};
use std::{
    collections::BTreeMap,
    ffi::OsStr,
    io::Read,
    process::{Command, Output, Stdio},
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};

const SEPARATOR: &str = "\u{1f}";
pub const HISTORY_LIMIT: usize = 100_000;
pub const WINDOWS_TERMINAL_DA2_RESPONSE: &str = "\u{1b}[>0;10;1c";
const TERMINAL_REPLY_KEY_OPTION: &str = "@pika_terminal_reply_key";
const COMMAND_TIMEOUT: Duration = Duration::from_secs(2);
const STDOUT_LIMIT: usize = 16 * 1024 * 1024;
const STDERR_LIMIT: usize = 64 * 1024;

#[derive(Clone, Debug)]
pub struct Tmux {
    executable: String,
    socket_name: Option<String>,
}

impl Default for Tmux {
    fn default() -> Self {
        Self {
            executable: "tmux".into(),
            socket_name: std::env::var("PIKA_TMUX_SOCKET").ok(),
        }
    }
}

impl Tmux {
    pub fn with_executable(executable: impl Into<String>, socket_name: Option<String>) -> Self {
        Self {
            executable: executable.into(),
            socket_name,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(&self.executable);
        if let Some(socket) = &self.socket_name {
            command.args(["-L", socket]);
        }
        command
    }

    fn output<I, S>(&self, args: I, check: bool) -> Result<std::process::Output>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.output_with_timeout(args, check, COMMAND_TIMEOUT)
    }

    fn output_with_timeout<I, S>(&self, args: I, check: bool, timeout: Duration) -> Result<Output>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let output = bounded_output(self.command().args(args), timeout)?;
        if check && !output.status.success() {
            let message = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            bail!(
                "{}",
                if message.is_empty() {
                    "tmux command failed"
                } else {
                    &message
                }
            );
        }
        Ok(output)
    }

    pub fn available(&self) -> bool {
        self.output(["-V"], false)
            .is_ok_and(|output| output.status.success())
    }

    pub fn list_panes(&self) -> Result<Vec<Pane>> {
        let format = [
            "#{session_name}",
            "#{pane_id}",
            "#{pane_pid}",
            "#{pane_current_path}",
            "#{pane_current_command}",
            "#{session_attached}",
            "#{window_active}",
            "#{pane_active}",
            "#{pane_dead}",
            "#{pane_dead_status}",
            "#{session_activity}",
            "#{session_created}",
            "#{@pika_provider}",
            "#{@pika_session_id}",
            "#{@pika_name}",
            "#{@pika_launch_token}",
        ]
        .join(SEPARATOR);
        let output = self.output(["list-panes", "-a", "-F", &format], false)?;
        if !output.status.success() {
            let message = String::from_utf8_lossy(&output.stderr);
            if message.contains("no server running")
                || message.contains("failed to connect to server")
                || message.contains("No such file or directory")
            {
                return Ok(Vec::new());
            }
            bail!("{}", message.trim());
        }
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(parse_pane)
            .collect())
    }

    pub fn get_pane(&self, target: &str) -> Result<Option<Pane>> {
        Ok(self.list_panes()?.into_iter().find(|pane| {
            target == pane.pane_id
                || target == pane.session_name
                || target == format!("{}:{}", pane.session_name, pane.pane_id)
        }))
    }

    pub fn tag_pane(
        &self,
        target: &str,
        provider: Option<Provider>,
        session_id: Option<&str>,
        name: Option<&str>,
        launch_token: Option<&str>,
    ) -> Result<()> {
        let pane = self
            .get_pane(target)?
            .context("tmux pane disappeared before tag mutation")?;
        let transition_is_bound = match launch_token {
            Some(token) => pane.pika_launch_token.as_deref() == Some(token),
            None => pane.pika_provider == provider && pane.pika_session_id.as_deref() == session_id,
        };
        if !transition_is_bound {
            bail!("tmux tag mutation lacks an exact launch or conversation binding")
        }
        self.tag_pane_if_unchanged(&pane, provider, session_id, name, launch_token)
    }

    pub fn tag_pane_if_unchanged(
        &self,
        pane: &Pane,
        provider: Option<Provider>,
        session_id: Option<&str>,
        name: Option<&str>,
        launch_token: Option<&str>,
    ) -> Result<()> {
        // The launch token is written first. If a later option write fails,
        // the pane remains attributable to the recoverable launch record; the
        // provider is never started until a full readback succeeds.
        let values = [
            ("@pika_launch_token", launch_token),
            ("@pika_provider", provider.map(|item| item.as_str())),
            ("@pika_session_id", session_id),
            ("@pika_name", name),
        ];
        self.mutate_pane_options_if_unchanged(pane, &values)
    }

    pub fn clear_tags(&self, target: &str) -> Result<()> {
        let _ = target;
        bail!("tmux tag removal requires a fresh exact pane binding")
    }

    pub fn clear_tags_if_unchanged(&self, pane: &Pane) -> Result<()> {
        let values = [
            ("@pika_launch_token", None),
            ("@pika_provider", None),
            ("@pika_session_id", None),
            ("@pika_name", None),
        ];
        self.mutate_pane_options_if_unchanged(pane, &values)
    }

    fn mutate_pane_options_if_unchanged(
        &self,
        pane: &Pane,
        values: &[(&str, Option<&str>)],
    ) -> Result<()> {
        let condition = pane_generation_condition(pane);
        let mut commands = Vec::new();
        for (option, value) in values {
            if let Some(value) = value {
                commands.push(format!(
                    "set-option -p -t {} {} {}",
                    shell_words::quote(&pane.pane_id),
                    option,
                    shell_words::quote(value)
                ));
            } else {
                commands.push(format!(
                    "set-option -p -u -t {} {}",
                    shell_words::quote(&pane.pane_id),
                    option
                ));
            }
        }
        let mutation = commands.join(" ; ");
        self.output(
            [
                "if-shell",
                "-F",
                "-t",
                &pane.pane_id,
                &condition,
                &mutation,
                "run-shell 'exit 75'",
            ],
            true,
        )?;
        Ok(())
    }

    pub fn display_alert(&self, message: &str) -> Result<()> {
        let clients = self.output(["list-clients", "-F", "#{client_name}"], false)?;
        if !clients.status.success() {
            return Ok(());
        }
        for client in String::from_utf8_lossy(&clients.stdout)
            .lines()
            .map(str::trim)
            .filter(|client| !client.is_empty())
        {
            let _ = self.output(["display-message", "-c", client, message], false);
        }
        Ok(())
    }

    fn client_is_attached_to_pane(&self, client_pid: i32, pane_id: &str) -> bool {
        let Ok(output) = self.output(["list-clients", "-F", "#{client_pid}\t#{pane_id}"], false)
        else {
            return false;
        };
        output.status.success()
            && String::from_utf8_lossy(&output.stdout).lines().any(|line| {
                line.split_once('\t').is_some_and(|(pid, pane)| {
                    pid.parse::<i32>() == Ok(client_pid) && pane == pane_id
                })
            })
    }

    fn attached_client_name(&self, client_pid: i32, pane_id: &str) -> Option<String> {
        let format = format!("#{{client_name}}{SEPARATOR}#{{client_pid}}{SEPARATOR}#{{pane_id}}");
        let output = self.output(["list-clients", "-F", &format], false).ok()?;
        output.status.success().then_some(()).and_then(|_| {
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .find_map(|line| {
                    let mut fields = line.split(SEPARATOR);
                    let client = fields.next()?;
                    let pid = fields.next()?;
                    let pane = fields.next()?;
                    (fields.next().is_none()
                        && !client.is_empty()
                        && pid.parse::<i32>() == Ok(client_pid)
                        && pane == pane_id)
                        .then(|| client.to_owned())
                })
        })
    }

    pub fn configure_home(&self, session: &str, pane: Option<&str>) -> Result<()> {
        if !is_pika_session(session) {
            return Ok(());
        }
        self.output(["set-option", "-t", session, "status", "off"], true)?;
        self.output(["set-option", "-t", session, "mouse", "on"], true)?;
        let limit = HISTORY_LIMIT.to_string();
        self.output(
            [
                "set-option",
                "-w",
                "-t",
                pane.unwrap_or(session),
                "history-limit",
                &limit,
            ],
            true,
        )?;
        Ok(())
    }

    pub fn configure_exact_home(&self, pane: &Pane) -> Result<()> {
        if !is_pika_session(&pane.session_name) {
            bail!("Pika will not configure a user-owned tmux pane");
        }
        let condition = pane_generation_condition(pane);
        let mutation = [
            format!(
                "set-option -t {} status off",
                shell_words::quote(&pane.pane_id)
            ),
            format!(
                "set-option -t {} mouse on",
                shell_words::quote(&pane.pane_id)
            ),
            format!(
                "set-option -w -t {} history-limit {}",
                shell_words::quote(&pane.pane_id),
                HISTORY_LIMIT
            ),
        ]
        .join(" ; ");
        self.output(
            [
                "if-shell",
                "-F",
                "-t",
                &pane.pane_id,
                &condition,
                &mutation,
                "run-shell 'exit 75'",
            ],
            true,
        )?;
        Ok(())
    }

    pub fn ensure_rgb(&self) {
        let Ok(current) = self.output(["show-options", "-s", "-v", "terminal-features"], false)
        else {
            return;
        };
        if current.status.success()
            && String::from_utf8_lossy(&current.stdout)
                .lines()
                .any(|line| line.starts_with("xterm*") && line.split(':').any(|part| part == "RGB"))
        {
            return;
        }
        let _ = self.output(
            ["set-option", "-as", "terminal-features", "xterm*:RGB"],
            false,
        );
    }

    /// Consume Windows Terminal's exact DA2 report only in Pika-tagged panes.
    /// User panes receive the same bytes back unchanged.
    pub fn ensure_terminal_reply_guard(&self) -> bool {
        let Ok(marker) = self.output(
            ["show-options", "-s", "-v", TERMINAL_REPLY_KEY_OPTION],
            false,
        ) else {
            return false;
        };
        let marker = Some(marker)
            .filter(|output| output.status.success())
            .and_then(|output| String::from_utf8(output.stdout).ok())
            .and_then(|value| value.trim().parse::<u16>().ok());
        let mut candidates = Vec::new();
        if let Some(value) = marker.filter(|value| (500..=509).contains(value)) {
            candidates.push(value);
        }
        candidates.extend((500..=509).filter(|value| Some(*value) != marker));
        let Ok(bindings) = self.output(["list-keys", "-T", "root"], false) else {
            return false;
        };
        let bindings = Some(bindings)
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
            .unwrap_or_default();
        for value in candidates {
            let option = format!("user-keys[{value}]");
            let Ok(current) = self.output(["show-options", "-s", "-v", &option], false) else {
                return false;
            };
            let existing = Some(&current)
                .filter(|output| output.status.success())
                .and_then(|output| {
                    let value = String::from_utf8_lossy(&output.stdout).trim().to_owned();
                    (!value.is_empty()).then_some(value)
                });
            let key = format!("User{value}");
            let binding = bindings
                .lines()
                .find(|line| line.split_whitespace().any(|field| field == key));
            if existing.is_some() || binding.is_some() {
                // Never change either half of an allocated user key. Our own
                // existing option+binding pair is already effective and needs
                // no rewrite; any other allocation belongs to the user.
                if marker == Some(value)
                    && existing.as_deref() == Some(WINDOWS_TERMINAL_DA2_RESPONSE)
                    && binding.is_some_and(|line| line.contains("@pika_provider"))
                {
                    return true;
                }
                continue;
            }
            let Ok(option_set) = self.output(
                ["set-option", "-s", &option, WINDOWS_TERMINAL_DA2_RESPONSE],
                false,
            ) else {
                return false;
            };
            if !option_set.status.success() {
                continue;
            }
            let replay = format!(
                "send-keys -l {}",
                shell_words::quote(WINDOWS_TERMINAL_DA2_RESPONSE)
            );
            let Ok(bound) = self.output(
                [
                    "bind-key",
                    "-T",
                    "root",
                    &key,
                    "if-shell",
                    "-F",
                    "#{@pika_provider}",
                    "",
                    &replay,
                ],
                false,
            ) else {
                return false;
            };
            if bound.status.success() {
                let _ = self.output(
                    [
                        "set-option",
                        "-s",
                        TERMINAL_REPLY_KEY_OPTION,
                        &value.to_string(),
                    ],
                    false,
                );
                return true;
            }
        }
        false
    }

    pub fn capture(&self, target: &str, lines: usize) -> Result<String> {
        let start = format!("-{}", lines.max(1));
        let output = self.output(
            ["capture-pane", "-p", "-e", "-J", "-S", &start, "-t", target],
            true,
        )?;
        Ok(String::from_utf8_lossy(&output.stdout)
            .trim_end()
            .to_owned())
    }

    pub fn attach(&self, session: &str, pane: Option<&str>) -> Result<i32> {
        self.attach_with_started(session, pane, || Ok(()))
    }

    /// Attach to an already verified home and report the successful handoff
    /// before waiting for the interactive client to exit.
    pub fn attach_with_started<F>(
        &self,
        session: &str,
        pane: Option<&str>,
        on_started: F,
    ) -> Result<i32>
    where
        F: FnOnce() -> Result<()>,
    {
        self.attach_with_started_mode(
            session,
            pane,
            std::env::var_os("TMUX").is_some(),
            on_started,
        )
    }

    /// Attach only if the pane still has the exact tmux generation and Pika
    /// identity that the caller proved. The condition and attach execute in
    /// one tmux server command queue, closing the check/use gap between a
    /// client-side read and terminal handoff.
    pub fn attach_exact_with_started<F>(&self, pane: &Pane, on_started: F) -> Result<i32>
    where
        F: FnOnce() -> Result<()>,
    {
        self.attach_exact_with_started_mode(pane, std::env::var_os("TMUX").is_some(), on_started)
    }

    /// Attach to an exact pane, commit the caller's proof, then show the
    /// resulting receipt to only the client that invoked this handoff.
    pub fn attach_exact_with_receipt<F>(&self, pane: &Pane, on_started: F) -> Result<i32>
    where
        F: FnOnce() -> Result<String>,
    {
        self.attach_exact_with_started_mode_inner(
            pane,
            std::env::var_os("TMUX").is_some(),
            true,
            move || on_started().map(Some),
        )
    }

    fn attach_exact_with_started_mode<F>(
        &self,
        pane: &Pane,
        inside_tmux: bool,
        on_started: F,
    ) -> Result<i32>
    where
        F: FnOnce() -> Result<()>,
    {
        self.attach_exact_with_started_mode_inner(pane, inside_tmux, false, move || {
            on_started()?;
            Ok(None)
        })
    }

    fn attach_exact_with_started_mode_inner<F>(
        &self,
        pane: &Pane,
        inside_tmux: bool,
        wants_receipt: bool,
        on_started: F,
    ) -> Result<i32>
    where
        F: FnOnce() -> Result<Option<String>>,
    {
        self.ensure_terminal_reply_guard();
        self.ensure_rgb();
        self.configure_exact_home(pane)?;
        let condition = pane_generation_condition(pane);
        let attach = if inside_tmux {
            format!("switch-client -t {}", shell_words::quote(&pane.pane_id))
        } else {
            format!("attach-session -t {}", shell_words::quote(&pane.pane_id))
        };
        let mut argv = vec![self.executable.clone()];
        if let Some(socket) = &self.socket_name {
            argv.extend(["-L".into(), socket.clone()]);
        }
        argv.extend([
            "if-shell".into(),
            "-F".into(),
            "-t".into(),
            pane.pane_id.clone(),
            condition,
            attach,
            "run-shell 'exit 75'".into(),
        ]);
        if inside_tmux {
            // Capture the invoking client before switch-client changes its
            // selected pane. A receipt must never leak to every tmux client.
            let client_name = wants_receipt
                .then(|| {
                    self.output(["display-message", "-p", "#{client_name}"], false)
                        .ok()
                        .filter(|output| output.status.success())
                        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
                        .filter(|value| !value.is_empty())
                })
                .flatten();
            let status =
                bounded_output(Command::new(&argv[0]).args(&argv[1..]), COMMAND_TIMEOUT)?.status;
            if status.success() {
                if let Some(receipt) = on_started()? {
                    let mut args = vec!["display-message"];
                    if let Some(client) = client_name.as_deref() {
                        args.extend(["-c", client]);
                    }
                    args.extend(["-d", "3000", "-l", &receipt]);
                    let _ = self.output(args, false);
                }
            }
            Ok(status.code().unwrap_or(1))
        } else {
            let attached_client = Arc::new(Mutex::new(None::<String>));
            let proof_client = Arc::clone(&attached_client);
            let receipt_client = Arc::clone(&attached_client);
            terminal::run_pty_bridge_with_handoff(
                &argv,
                None,
                true,
                |client_pid| {
                    let client = self.attached_client_name(client_pid, &pane.pane_id);
                    if let Some(client) = client {
                        *proof_client.lock().expect("tmux client proof poisoned") = Some(client);
                        true
                    } else if wants_receipt {
                        // PID + pane proves the handoff for legacy adapters, but
                        // it cannot identify the one tmux client allowed to see
                        // a receipt. Wait for the stronger proof instead of
                        // consuming the callback in the startup race between
                        // those two observations.
                        false
                    } else {
                        // Compatibility for older/fake tmux adapters that
                        // prove only PID + pane. Real tmux supplies the client
                        // name above.
                        self.client_is_attached_to_pane(client_pid, &pane.pane_id)
                    }
                },
                || {
                    if let Some(receipt) = on_started()? {
                        if let Some(client) = receipt_client
                            .lock()
                            .expect("tmux receipt target poisoned")
                            .as_deref()
                        {
                            let _ = self.output(
                                [
                                    "display-message",
                                    "-c",
                                    client,
                                    "-d",
                                    "3000",
                                    "-l",
                                    &receipt,
                                ],
                                false,
                            );
                        }
                    }
                    Ok(())
                },
            )
        }
    }

    fn attach_with_started_mode<F>(
        &self,
        session: &str,
        pane: Option<&str>,
        inside_tmux: bool,
        on_started: F,
    ) -> Result<i32>
    where
        F: FnOnce() -> Result<()>,
    {
        self.ensure_terminal_reply_guard();
        self.ensure_rgb();
        self.configure_home(session, pane)?;
        if inside_tmux {
            let status = self.output(["switch-client", "-t", session], false)?.status;
            if status.success() {
                if let Some(pane) = pane {
                    let window = self.output(["select-window", "-t", pane], false)?.status;
                    if window.success() {
                        let selected = self.output(["select-pane", "-t", pane], false)?.status;
                        if selected.success() {
                            on_started()?;
                        }
                        return Ok(selected.code().unwrap_or(1));
                    }
                    return Ok(window.code().unwrap_or(1));
                } else {
                    on_started()?;
                    return Ok(0);
                }
            }
            Ok(status.code().unwrap_or(1))
        } else {
            let mut argv = Vec::new();
            argv.push(self.executable.clone());
            if let Some(socket) = &self.socket_name {
                argv.extend(["-L".into(), socket.clone()]);
            }
            argv.extend([
                "attach-session".into(),
                "-t".into(),
                pane.unwrap_or(session).into(),
            ]);
            let target_pane = pane.map(str::to_owned);
            terminal::run_pty_bridge_with_handoff(
                &argv,
                None,
                true,
                |client_pid| {
                    target_pane
                        .as_deref()
                        .is_some_and(|pane_id| self.client_is_attached_to_pane(client_pid, pane_id))
                },
                on_started,
            )
        }
    }

    pub fn create_holding_session(&self, tmux_name: &str, cwd: &str) -> Result<Pane> {
        self.output(
            ["new-session", "-d", "-s", tmux_name, "-c", cwd, "sleep 30"],
            true,
        )?;
        self.get_pane(tmux_name)?
            .context("tmux holding pane disappeared")
    }

    #[allow(clippy::too_many_arguments)]
    pub fn prepare_agent_session(
        &self,
        tmux_name: &str,
        cwd: &str,
        provider: Provider,
        session_id: Option<&str>,
        display_name: &str,
        launch_token: &str,
    ) -> Result<Pane> {
        let holding = self.create_holding_session(tmux_name, cwd)?;
        self.configure_exact_home(&holding)?;
        self.prepare_agent_pane(&holding, provider, session_id, display_name, launch_token)
    }

    pub fn prepare_agent_pane(
        &self,
        pane: &Pane,
        provider: Provider,
        session_id: Option<&str>,
        display_name: &str,
        launch_token: &str,
    ) -> Result<Pane> {
        let fresh = self
            .get_pane(&pane.pane_id)?
            .context("tmux pane disappeared before launch reservation")?;
        if fresh.session_name != pane.session_name
            || fresh.pane_pid != pane.pane_pid
            || fresh.created != pane.created
        {
            bail!("tmux pane generation changed before launch reservation");
        }
        self.tag_pane_if_unchanged(
            &fresh,
            Some(provider),
            session_id,
            Some(display_name),
            Some(launch_token),
        )?;
        self.require_prepared_pane(&fresh, provider, session_id, display_name, launch_token)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn start_prepared_agent(
        &self,
        pane: &Pane,
        cwd: &str,
        provider: Provider,
        agent_argv: &[String],
        environment: &BTreeMap<String, String>,
        session_id: Option<&str>,
        display_name: &str,
        launch_token: &str,
    ) -> Result<Pane> {
        let reserved =
            self.require_prepared_pane(pane, provider, session_id, display_name, launch_token)?;
        let wrapper = agent_wrapper(
            provider,
            agent_argv,
            environment,
            session_id,
            Some(launch_token),
        )?;
        let condition = pane_generation_condition(&reserved);
        let mutation = format!(
            "respawn-pane -k -t {} -c {} {}",
            shell_words::quote(&reserved.pane_id),
            shell_words::quote(cwd),
            shell_words::quote(&wrapper),
        );
        self.output(
            [
                "if-shell",
                "-F",
                "-t",
                &reserved.pane_id,
                &condition,
                &mutation,
                "run-shell 'exit 75'",
            ],
            true,
        )?;
        self.ensure_rgb();
        let launched = self
            .get_pane(&reserved.pane_id)?
            .context("tmux agent pane disappeared after respawn")?;
        if launched.pika_provider != Some(provider)
            || launched.pika_session_id.as_deref() != session_id
            || launched.pika_launch_token.as_deref() != Some(launch_token)
        {
            bail!(
                "provider may have started, but the tmux launch reservation changed; the recoverable launch record was retained"
            );
        }
        Ok(launched)
    }

    fn require_prepared_pane(
        &self,
        expected: &Pane,
        provider: Provider,
        session_id: Option<&str>,
        display_name: &str,
        launch_token: &str,
    ) -> Result<Pane> {
        let current = self
            .get_pane(&expected.pane_id)?
            .context("tmux pane disappeared before provider execution")?;
        if current.session_name != expected.session_name
            || current.pane_pid != expected.pane_pid
            || current.created != expected.created
            || current.current_command != expected.current_command
            || current.pika_provider != Some(provider)
            || current.pika_session_id.as_deref() != session_id
            || current.pika_name.as_deref() != Some(display_name)
            || current.pika_launch_token.as_deref() != Some(launch_token)
        {
            bail!(
                "tmux pane generation or exact launch reservation changed; provider was not started"
            )
        }
        Ok(current)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_agent_session(
        &self,
        tmux_name: &str,
        cwd: &str,
        provider: Provider,
        agent_argv: &[String],
        environment: &BTreeMap<String, String>,
        session_id: Option<&str>,
        display_name: &str,
        launch_token: Option<&str>,
    ) -> Result<Pane> {
        let launch_token = launch_token.context("agent launches require a recovery token")?;
        let holding = self.prepare_agent_session(
            tmux_name,
            cwd,
            provider,
            session_id,
            display_name,
            launch_token,
        )?;
        self.start_prepared_agent(
            &holding,
            cwd,
            provider,
            agent_argv,
            environment,
            session_id,
            display_name,
            launch_token,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn respawn_agent_pane(
        &self,
        pane: &Pane,
        cwd: &str,
        provider: Provider,
        agent_argv: &[String],
        environment: &BTreeMap<String, String>,
        session_id: &str,
        display_name: &str,
        launch_token: &str,
    ) -> Result<Pane> {
        if !is_pika_session(&pane.session_name) {
            bail!("Pika will not respawn a user-owned tmux pane");
        }
        self.configure_exact_home(pane)?;
        let prepared =
            self.prepare_agent_pane(pane, provider, Some(session_id), display_name, launch_token)?;
        self.start_prepared_agent(
            &prepared,
            cwd,
            provider,
            agent_argv,
            environment,
            Some(session_id),
            display_name,
            launch_token,
        )
    }

    pub fn internal_name(provider: Provider, identity: &str) -> String {
        let suffix: String = identity
            .chars()
            .filter(|value| *value != '-')
            .take(10)
            .collect();
        let prefix = match provider {
            Provider::Codex => 'c',
            Provider::Claude => 'a',
            Provider::Opencode => 'o',
        };
        format!("pika-{prefix}-{suffix}")
    }
}

pub fn is_pika_session(name: &str) -> bool {
    name.starts_with("pika-c-") || name.starts_with("pika-a-") || name.starts_with("pika-o-")
}

/// Noninteractive tmux clients are disposable, but their server and the panes
/// it owns are not. Only this fresh client process group may be cleaned up.
/// Pipe pumps are cancellable even when a descendant escapes that group.
fn bounded_output(command: &mut Command, timeout: Duration) -> Result<Output> {
    let deadline = Instant::now() + timeout;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = OwnedChild::spawn(command).context("cannot run tmux")?;
    let stop = CancellationToken::default();
    let result = (|| {
        let stdout = CancellablePipe::new(
            child.stdout.take().expect("stdout configured"),
            stop.clone(),
        )?;
        let stderr = CancellablePipe::new(
            child.stderr.take().expect("stderr configured"),
            stop.clone(),
        )?;
        let (sender, receiver) = mpsc::sync_channel(2);
        let stdout_sender = sender.clone();
        thread::spawn(move || {
            let mut bytes = Vec::new();
            let result = stdout
                .take((STDOUT_LIMIT + 1) as u64)
                .read_to_end(&mut bytes);
            let _ = stdout_sender.send((true, result.map(|_| bytes)));
        });
        thread::spawn(move || {
            let mut bytes = Vec::new();
            let result = stderr
                .take((STDERR_LIMIT + 1) as u64)
                .read_to_end(&mut bytes);
            let _ = sender.send((false, result.map(|_| bytes)));
        });
        let mut stdout = None;
        let mut stderr = None;
        let mut status = None;
        loop {
            while let Ok((is_stdout, result)) = receiver.try_recv() {
                let bytes = result.context("cannot read tmux output")?;
                let limit = if is_stdout {
                    STDOUT_LIMIT
                } else {
                    STDERR_LIMIT
                };
                if bytes.len() > limit {
                    bail!(
                        "tmux {} exceeded the {limit}-byte safety limit",
                        if is_stdout { "stdout" } else { "stderr" }
                    );
                }
                if is_stdout {
                    stdout = Some(bytes);
                } else {
                    stderr = Some(bytes);
                }
            }
            if status.is_none() {
                // This kills only owned descendants before reaping the group
                // leader, so its numeric PGID cannot race with PID reuse.
                status = poll_owned_child(&mut child)?;
            }
            if let (Some(status), Some(stdout), Some(stderr)) =
                (status, stdout.as_mut(), stderr.as_mut())
            {
                return Ok(Output {
                    status,
                    stdout: std::mem::take(stdout),
                    stderr: std::mem::take(stderr),
                });
            }
            if Instant::now() >= deadline {
                bail!("tmux timed out after {}s", timeout.as_secs_f64());
            }
            thread::sleep(Duration::from_millis(2));
        }
    })();
    stop.cancel();
    // Do not join pipe readers: cancelled nonblocking reads exit promptly and
    // no foreign process holding a descriptor can own this caller's lifetime.
    let cleanup = terminate_child(&mut child);
    match result {
        Ok(output) => {
            cleanup?;
            Ok(output)
        }
        Err(error) => Err(error),
    }
}

fn parse_pane(line: &str) -> Option<Pane> {
    let parts: Vec<_> = if line.contains(SEPARATOR) {
        line.split(SEPARATOR).collect()
    } else {
        line.split(r"\037").collect()
    };
    if parts.len() != 16 {
        return None;
    }
    Some(Pane {
        session_name: parts[0].into(),
        pane_id: parts[1].into(),
        pane_pid: parts[2].parse().ok()?,
        cwd: parts[3].into(),
        current_command: parts[4].into(),
        attached: parts[5] != "0" && parts[6] != "0" && parts[7] != "0",
        dead: parts[8] != "0",
        dead_status: if parts[9].is_empty() {
            None
        } else {
            parts[9].parse().ok()
        },
        activity: parts[10].parse().unwrap_or(0.0),
        created: parts[11].parse().unwrap_or(0.0),
        pika_provider: if parts[12].is_empty() {
            None
        } else {
            parts[12].parse().ok()
        },
        pika_session_id: nonempty(parts[13]),
        pika_name: nonempty(parts[14]),
        pika_launch_token: nonempty(parts[15]),
    })
}

fn nonempty(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_owned())
}

fn pane_generation_condition(pane: &Pane) -> String {
    let pane_pid = pane.pane_pid.to_string();
    let created = pane.created.to_string();
    let fields = [
        ("#{pane_id}", pane.pane_id.as_str()),
        ("#{session_name}", pane.session_name.as_str()),
        ("#{pane_pid}", pane_pid.as_str()),
        ("#{session_created}", created.as_str()),
        (
            "#{@pika_provider}",
            pane.pika_provider.map(Provider::as_str).unwrap_or(""),
        ),
        (
            "#{@pika_session_id}",
            pane.pika_session_id.as_deref().unwrap_or(""),
        ),
        ("#{@pika_name}", pane.pika_name.as_deref().unwrap_or("")),
        (
            "#{@pika_launch_token}",
            pane.pika_launch_token.as_deref().unwrap_or(""),
        ),
    ];
    fields
        .into_iter()
        .map(|(field, value)| format!("#{{==:{field},{}}}", format_literal(value)))
        .reduce(|left, right| format!("#{{&&:{left},{right}}}"))
        .expect("pane generation has fields")
}

fn format_literal(value: &str) -> String {
    value
        .replace('#', "##")
        .replace(',', "#,")
        .replace('}', "#}")
}

fn agent_wrapper(
    provider: Provider,
    agent_argv: &[String],
    environment: &BTreeMap<String, String>,
    session_id: Option<&str>,
    launch_token: Option<&str>,
) -> Result<String> {
    let pika = std::env::current_exe()?.to_string_lossy().into_owned();
    let mut launch = vec!["env".to_owned(), "-u".into(), "NO_COLOR".into()];
    for key in [
        "CODEX_THREAD_ID",
        "CODEX_COMPANION_SESSION_ID",
        "CLAUDE_CODE_SESSION_ID",
        "CLAUDE_CODE_BRIDGE_SESSION_ID",
        "CLAUDE_CODE_CHILD_SESSION",
        "CLAUDE_CODE_MESSAGING_SOCKET",
        "CLAUDE_CODE_MESSAGING_TOKEN",
        "CLAUDE_PID",
        "PIKA_ACTIVE_THREAD_ID",
        "PIKA_SESSION_ID",
        "PIKA_LAUNCH_TOKEN",
        "PIKA_OWNER_TOKEN",
        "PIKA_NAME",
        "PIKA_PROVIDER",
    ] {
        launch.extend(["-u".into(), key.into()]);
    }
    for (key, value) in environment {
        launch.push(format!("{key}={value}"));
    }
    launch.push(format!(
        "PATH={}",
        std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into())
    ));
    launch.push("TERM=tmux-direct".into());
    if let Ok(value) = std::env::var("COLORTERM") {
        launch.push(format!("COLORTERM={value}"));
    }
    let palette = environment
        .get(FOREGROUND_ENV)
        .and_then(|foreground| terminal::decode_color(foreground))
        .zip(
            environment
                .get(BACKGROUND_ENV)
                .and_then(|background| terminal::decode_color(background)),
        )
        .map(|(foreground, background)| Palette {
            foreground,
            background,
        });
    if provider == Provider::Codex
        && let Some(palette) = palette
    {
        launch.extend([
            pika.clone(),
            "_terminal-bridge".into(),
            "--foreground".into(),
            terminal::encode_color(palette.foreground),
            "--background".into(),
            terminal::encode_color(palette.background),
            "--".into(),
        ]);
    }
    launch.extend(agent_argv.iter().cloned());

    let mut exit = vec![
        pika,
        "_process-exit".into(),
        "--provider".into(),
        provider.to_string(),
    ];
    if let Some(identity) = session_id {
        exit.extend(["--session-id".into(), identity.into()]);
    }
    if let Some(token) = launch_token {
        exit.extend(["--launch-token".into(), token.into()]);
    }
    if let Some(token) = environment.get("PIKA_OWNER_TOKEN") {
        exit.extend(["--owner-token".into(), token.clone()]);
    }
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    Ok(format!(
        "{}; pika_rc=$?; {} --code \"$pika_rc\"; exec {} -l",
        shell_words::join(launch),
        shell_words::join(exit),
        shell_words::quote(&shell)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    use std::{fs, os::unix::fs::PermissionsExt};

    #[test]
    fn inventory_parser_keeps_raw_names() {
        let line = [
            "pika-c-abc",
            "%1",
            "42",
            "/tmp/a b",
            "codex",
            "1",
            "1",
            "1",
            "0",
            "",
            "10",
            "9",
            "codex",
            "abc",
            "my agent",
            "token",
        ]
        .join(SEPARATOR);
        let pane = parse_pane(&line).unwrap();
        assert_eq!(pane.cwd, "/tmp/a b");
        assert_eq!(pane.pika_name.as_deref(), Some("my agent"));
        assert!(pane.attached);
    }

    #[test]
    fn internal_names_are_short_and_provider_specific() {
        assert_eq!(
            Tmux::internal_name(Provider::Codex, "abcd-efgh-ijkl"),
            "pika-c-abcdefghij"
        );
    }

    fn exact_test_pane() -> Pane {
        parse_pane(
            &[
                "pika-c-workstream",
                "%1",
                "42",
                "/tmp",
                "codex",
                "0",
                "1",
                "1",
                "0",
                "",
                "10",
                "9",
                "codex",
                "11111111-1111-4111-8111-111111111111",
                "workstream",
                "launch-token",
            ]
            .join(SEPARATOR),
        )
        .unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn exact_home_configuration_is_one_generation_guarded_mutation() {
        let temp = tempfile::tempdir().unwrap();
        let executable = temp.path().join("tmux-fixture");
        let trace = temp.path().join("trace");
        fs::write(
            &executable,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {}\nexit 0\n",
                shell_words::quote(&trace.to_string_lossy())
            ),
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let tmux = Tmux::with_executable(executable.to_string_lossy(), None);
        tmux.configure_exact_home(&exact_test_pane()).unwrap();
        let calls = fs::read_to_string(trace).unwrap();
        assert_eq!(calls.lines().count(), 1);
        assert!(calls.contains("if-shell -F -t %1"));
        assert!(calls.contains("#{@pika_session_id}"));
        assert!(calls.contains("#{==:#{session_name},pika-c-workstream}"));
        assert!(calls.contains(&format!(
            "set-option -t {} status off",
            shell_words::quote("%1")
        )));
        assert!(calls.contains(&format!(
            "set-option -t {} mouse on",
            shell_words::quote("%1")
        )));
        assert!(!calls.contains("set-option -t pika-c-workstream"));
        assert!(calls.contains("set-option -w -t"));
        assert!(calls.contains("history-limit 100000"));
    }

    #[cfg(unix)]
    fn tmux_fixture(temp: &tempfile::TempDir, body: &str) -> Tmux {
        let executable = temp.path().join("bounded-tmux-fixture");
        fs::write(&executable, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        Tmux::with_executable(executable.to_string_lossy(), None)
    }

    #[cfg(unix)]
    #[test]
    fn exact_home_rejects_rename_even_when_old_name_now_belongs_to_a_replacement() {
        let temp = tempfile::tempdir().unwrap();
        let mutation = temp.path().join("replacement-was-mutated");
        // Model a server whose %1 was renamed to user-work and whose old name
        // has been reused by a new session. All other cached fields still match.
        // The old condition accepted this; the session-name term must reject it.
        let tmux = tmux_fixture(
            &temp,
            &format!(
                "case \"$5\" in *'#{{==:#{{session_name}},pika-c-workstream}}'*) exit 75;; esac\nprintf changed > {}",
                shell_words::quote(&mutation.to_string_lossy())
            ),
        );
        assert!(tmux.configure_exact_home(&exact_test_pane()).is_err());
        assert!(!mutation.exists());
    }

    #[cfg(unix)]
    #[test]
    fn noninteractive_tmux_deadline_kills_owned_descendants() {
        let temp = tempfile::tempdir().unwrap();
        let pid_file = temp.path().join("descendant.pid");
        let tmux = tmux_fixture(
            &temp,
            &format!(
                "sleep 30 &\nprintf '%s' \"$!\" > {}\nwait",
                shell_words::quote(&pid_file.to_string_lossy())
            ),
        );
        let started = Instant::now();
        let error = tmux
            .output_with_timeout(["list-panes"], false, Duration::from_secs(3))
            .unwrap_err();
        assert!(error.to_string().contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(4));
        assert_fixture_child_gone(&pid_file);
    }

    #[cfg(unix)]
    #[test]
    fn exited_tmux_adapter_does_not_leave_a_descendant_holding_output_open() {
        let temp = tempfile::tempdir().unwrap();
        let pid_file = temp.path().join("descendant.pid");
        let tmux = tmux_fixture(
            &temp,
            &format!(
                "sleep 30 &\nprintf '%s' \"$!\" > {}\nprintf complete\nexit 0",
                shell_words::quote(&pid_file.to_string_lossy())
            ),
        );
        let started = Instant::now();
        let output = tmux
            .output_with_timeout(["list-panes"], true, Duration::from_secs(2))
            .unwrap();
        assert_eq!(output.stdout, b"complete");
        assert!(started.elapsed() < Duration::from_secs(3));
        assert_fixture_child_gone(&pid_file);
    }

    #[cfg(unix)]
    fn assert_fixture_child_gone(pid_file: &std::path::Path) {
        let pid: i32 = fs::read_to_string(pid_file).unwrap().parse().unwrap();
        for _ in 0..100 {
            // Signal zero is read-only and this PID came from our own fixture.
            if unsafe { libc::kill(pid, 0) } == -1 {
                return;
            }
            thread::sleep(Duration::from_millis(5));
        }
        panic!("owned fixture descendant survived cleanup");
    }

    #[cfg(unix)]
    #[test]
    fn tmux_stdout_and_stderr_floods_fail_closed() {
        for (redirect, channel) in [("", "stdout"), (">&2", "stderr")] {
            let temp = tempfile::tempdir().unwrap();
            let tmux = tmux_fixture(&temp, &format!("exec /usr/bin/yes flood {redirect}"));
            let started = Instant::now();
            let error = tmux.output(["list-panes"], false).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains(&format!("tmux {channel} exceeded")),
                "{error:#}"
            );
            assert!(started.elapsed() < Duration::from_secs(3));
        }
    }

    #[cfg(unix)]
    #[test]
    fn best_effort_terminal_setup_stops_after_adapter_io_failure() {
        let temp = tempfile::tempdir().unwrap();
        let trace = temp.path().join("calls");
        let tmux = tmux_fixture(
            &temp,
            &format!(
                "printf '%s\\n' \"$*\" >> {}\nexec /usr/bin/yes flood >&2",
                shell_words::quote(&trace.to_string_lossy())
            ),
        );
        assert!(!tmux.ensure_terminal_reply_guard());
        tmux.ensure_rgb();
        assert_eq!(fs::read_to_string(trace).unwrap().lines().count(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn inside_tmux_handoff_is_bounded_and_never_acknowledges_a_stalled_switch() {
        let temp = tempfile::tempdir().unwrap();
        let tmux = tmux_fixture(
            &temp,
            "case \"$*\" in *switch-client*) sleep 30;; *) exit 0;; esac",
        );
        let mut started = false;
        let before = Instant::now();
        let error = tmux
            .attach_exact_with_started_mode(&exact_test_pane(), true, || {
                started = true;
                Ok(())
            })
            .unwrap_err();
        assert!(error.to_string().contains("timed out"));
        assert!(!started);
        assert!(before.elapsed() < Duration::from_secs(5));
    }

    #[cfg(unix)]
    #[test]
    fn guarded_attach_rejects_a_generation_change_before_handoff() {
        let temp = tempfile::tempdir().unwrap();
        let executable = temp.path().join("tmux-fixture");
        fs::write(
            &executable,
            "#!/bin/sh\ncase \"$*\" in *if-shell*attach-session*) exit 75;; *) exit 0;; esac\n",
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let tmux = Tmux::with_executable(executable.to_string_lossy(), None);
        let mut started = false;
        let code = tmux
            .attach_exact_with_started_mode(&exact_test_pane(), false, || {
                started = true;
                Ok(())
            })
            .unwrap();
        assert_eq!(code, 75);
        assert!(!started);
    }

    #[cfg(unix)]
    #[test]
    fn guarded_attach_rejects_a_delayed_nonzero_exit() {
        let temp = tempfile::tempdir().unwrap();
        let executable = temp.path().join("tmux-fixture");
        fs::write(
            &executable,
            "#!/bin/sh\ncase \"$*\" in *if-shell*attach-session*) sleep 0.25; exit 130;; *) exit 0;; esac\n",
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let tmux = Tmux::with_executable(executable.to_string_lossy(), None);
        let mut started = false;
        let code = tmux
            .attach_exact_with_started_mode(&exact_test_pane(), false, || {
                started = true;
                Ok(())
            })
            .unwrap();
        assert_eq!(code, 130);
        assert!(!started);
    }

    #[cfg(unix)]
    #[test]
    fn attach_handoff_rejects_a_delayed_interrupted_client() {
        let temp = tempfile::tempdir().unwrap();
        let executable = temp.path().join("tmux-fixture");
        fs::write(
            &executable,
            "#!/bin/sh\ncase \"$*\" in *attach-session*) sleep 0.25; exit 130;; *) exit 0;; esac\n",
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let tmux = Tmux::with_executable(executable.to_string_lossy(), None);
        let mut started = false;
        let code = tmux
            .attach_with_started_mode("pika-c-workstream", Some("%1"), false, || {
                started = true;
                Ok(())
            })
            .unwrap();
        assert_eq!(code, 130);
        assert!(!started, "a non-zero terminal exit must remain fail-closed");
    }

    #[cfg(unix)]
    #[test]
    fn successful_exit_without_client_proof_does_not_record_handoff() {
        let temp = tempfile::tempdir().unwrap();
        let executable = temp.path().join("tmux-fixture");
        fs::write(
            &executable,
            "#!/bin/sh\ncase \"$*\" in *attach-session*) sleep 0.25; exit 0;; *) exit 0;; esac\n",
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let tmux = Tmux::with_executable(executable.to_string_lossy(), None);
        let mut started = false;
        let code = tmux
            .attach_with_started_mode("pika-c-workstream", Some("%1"), false, || {
                started = true;
                Ok(())
            })
            .unwrap();
        assert_eq!(code, 0);
        assert!(!started, "exit status alone is not terminal handoff proof");
    }

    #[cfg(unix)]
    #[test]
    fn positive_client_proof_records_handoff_before_detach() {
        let temp = tempfile::tempdir().unwrap();
        let pid_file = temp.path().join("client.pid");
        let release = temp.path().join("release");
        let executable = temp.path().join("tmux-fixture");
        fs::write(
            &executable,
            format!(
                "#!/bin/sh\ncase \"$1\" in\n  list-clients) if test -f {pid}; then printf '%s\\t%%1\\n' \"$(cat {pid})\"; fi; exit 0;;\nesac\ncase \"$*\" in\n  *attach-session*) printf '%s' \"$$\" > {pid}; n=0; while test ! -f {release} && test \"$n\" -lt 1000; do n=$((n + 1)); sleep 0.01; done; test -f {release};;\n  *) exit 0;;\nesac\n",
                pid = shell_words::quote(&pid_file.to_string_lossy()),
                release = shell_words::quote(&release.to_string_lossy()),
            ),
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let tmux = Tmux::with_executable(executable.to_string_lossy(), None);
        let mut started = false;
        let code = tmux
            .attach_with_started_mode("pika-c-workstream", Some("%1"), false, || {
                started = true;
                fs::write(&release, "attached").map_err(Into::into)
            })
            .unwrap();
        assert_eq!(code, 0);
        assert!(started);
        assert_eq!(fs::read_to_string(release).unwrap(), "attached");
    }

    #[cfg(unix)]
    #[test]
    fn inside_handoff_targets_receipt_after_proof_callback() {
        let temp = tempfile::tempdir().unwrap();
        let trace = temp.path().join("trace");
        let proof = temp.path().join("proof");
        let tmux = tmux_fixture(
            &temp,
            &format!(
                "printf '%s\\n' \"$*\" >> {trace}\ncase \"$*\" in\n  'display-message -p #{{client_name}}') printf '%s\\n' invoking-client;;\n  *'display-message -c invoking-client -d 3000 -l exact receipt'*) test -f {proof} || printf '%s\\n' BEFORE_PROOF >> {trace};;\nesac\nexit 0",
                trace = shell_words::quote(&trace.to_string_lossy()),
                proof = shell_words::quote(&proof.to_string_lossy()),
            ),
        );
        let code = tmux
            .attach_exact_with_started_mode_inner(&exact_test_pane(), true, true, || {
                fs::write(&proof, "proved")?;
                Ok(Some("exact receipt".into()))
            })
            .unwrap();
        assert_eq!(code, 0);
        let trace = fs::read_to_string(trace).unwrap();
        assert!(!trace.contains("BEFORE_PROOF"));
        assert!(trace.contains("display-message -c invoking-client -d 3000 -l exact receipt"));
    }

    #[cfg(unix)]
    #[test]
    fn failed_proof_callback_never_displays_a_receipt() {
        let temp = tempfile::tempdir().unwrap();
        let trace = temp.path().join("trace");
        let tmux = tmux_fixture(
            &temp,
            &format!(
                "printf '%s\\n' \"$*\" >> {}\ncase \"$*\" in 'display-message -p #{{client_name}}') printf '%s\\n' invoking-client;; esac\nexit 0",
                shell_words::quote(&trace.to_string_lossy()),
            ),
        );
        let error = tmux
            .attach_exact_with_started_mode_inner(&exact_test_pane(), true, true, || {
                anyhow::bail!("identity changed during callback")
            })
            .unwrap_err();
        assert!(error.to_string().contains("identity changed"));
        assert!(!fs::read_to_string(trace).unwrap().contains("-d 3000 -l"));
    }

    #[cfg(unix)]
    #[test]
    fn outside_handoff_sends_receipt_only_to_proven_client_before_return() {
        let temp = tempfile::tempdir().unwrap();
        let trace = temp.path().join("trace");
        let pid_file = temp.path().join("client.pid");
        let release = temp.path().join("release");
        let proof = temp.path().join("proof");
        let executable = temp.path().join("tmux-fixture");
        fs::write(
            &executable,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {trace}\ncase \"$*\" in\n  *'list-clients -F #{{client_name}}'*) test -f {pid} && printf 'other\\037999\\037%%2\\ninvoking-client\\037%s\\037%%1\\n' \"$(cat {pid})\"; exit 0;;\n  *'list-clients -F #{{client_pid}}'*) test -f {pid} && printf '%s\\t%%1\\n' \"$(cat {pid})\"; exit 0;;\n  *'display-message -c invoking-client -d 3000 -l exact receipt'*) test -f {proof} || printf '%s\\n' BEFORE_PROOF >> {trace}; touch {release}; exit 0;;\n  *attach-session*) printf '%s' \"$$\" > {pid}; n=0; while test ! -f {release} && test \"$n\" -lt 1000; do n=$((n + 1)); sleep 0.01; done; test -f {release}; exit;;\nesac\nexit 0\n",
                trace = shell_words::quote(&trace.to_string_lossy()),
                pid = shell_words::quote(&pid_file.to_string_lossy()),
                release = shell_words::quote(&release.to_string_lossy()),
                proof = shell_words::quote(&proof.to_string_lossy()),
            ),
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let tmux = Tmux::with_executable(executable.to_string_lossy(), None);
        let code = tmux
            .attach_exact_with_started_mode_inner(&exact_test_pane(), false, true, || {
                fs::write(&proof, "proved")?;
                Ok(Some("exact receipt".into()))
            })
            .unwrap();
        assert_eq!(
            code,
            0,
            "tmux fixture trace:\n{}",
            fs::read_to_string(&trace).unwrap_or_default()
        );
        let trace = fs::read_to_string(trace).unwrap();
        assert!(!trace.contains("BEFORE_PROOF"));
        assert!(trace.contains("display-message -c invoking-client -d 3000 -l exact receipt"));
        assert!(!trace.contains("display-message -c other"));
    }

    #[cfg(unix)]
    #[test]
    fn failed_attach_before_handoff_does_not_run_callback() {
        let temp = tempfile::tempdir().unwrap();
        let executable = temp.path().join("tmux-fixture");
        fs::write(
            &executable,
            "#!/bin/sh\ncase \"$*\" in *attach-session*) exit 127;; *) exit 0;; esac\n",
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let tmux = Tmux::with_executable(executable.to_string_lossy(), None);
        let mut started = false;
        let code = tmux
            .attach_with_started_mode("pika-c-workstream", Some("%1"), false, || {
                started = true;
                Ok(())
            })
            .unwrap();
        assert_eq!(code, 127);
        assert!(!started, "failed pre-attach must not change open history");
    }
}
