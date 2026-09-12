use crate::{
    model::{Pane, Provider},
    terminal::{self, BACKGROUND_ENV, FOREGROUND_ENV, Palette},
};
use anyhow::{Context, Result, bail};
use std::{collections::BTreeMap, ffi::OsStr, process::Command};

const SEPARATOR: &str = "\u{1f}";
pub const HISTORY_LIMIT: usize = 100_000;
pub const WINDOWS_TERMINAL_DA2_RESPONSE: &str = "\u{1b}[>0;10;1c";
const TERMINAL_REPLY_KEY_OPTION: &str = "@pika_terminal_reply_key";

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
        let output = self
            .command()
            .args(args)
            .output()
            .context("cannot run tmux")?;
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
                shell_words::quote(&pane.session_name)
            ),
            format!(
                "set-option -t {} mouse on",
                shell_words::quote(&pane.session_name)
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
        let current = self.output(["show-options", "-s", "-v", "terminal-features"], false);
        if current.is_ok_and(|output| {
            output.status.success()
                && String::from_utf8_lossy(&output.stdout).lines().any(|line| {
                    line.starts_with("xterm*") && line.split(':').any(|part| part == "RGB")
                })
        }) {
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
        let marker = self
            .output(
                ["show-options", "-s", "-v", TERMINAL_REPLY_KEY_OPTION],
                false,
            )
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| String::from_utf8(output.stdout).ok())
            .and_then(|value| value.trim().parse::<u16>().ok());
        let mut candidates = Vec::new();
        if let Some(value) = marker.filter(|value| (500..=509).contains(value)) {
            candidates.push(value);
        }
        candidates.extend((500..=509).filter(|value| Some(*value) != marker));
        let bindings = self
            .output(["list-keys", "-T", "root"], false)
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
            .unwrap_or_default();
        for value in candidates {
            let option = format!("user-keys[{value}]");
            let current = self.output(["show-options", "-s", "-v", &option], false);
            let existing = current
                .as_ref()
                .ok()
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
            let option_set = self.output(
                ["set-option", "-s", &option, WINDOWS_TERMINAL_DA2_RESPONSE],
                false,
            );
            if !option_set.is_ok_and(|output| output.status.success()) {
                continue;
            }
            let replay = format!(
                "send-keys -l {}",
                shell_words::quote(WINDOWS_TERMINAL_DA2_RESPONSE)
            );
            let bound = self.output(
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
            );
            if bound.is_ok_and(|output| output.status.success()) {
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

    fn attach_exact_with_started_mode<F>(
        &self,
        pane: &Pane,
        inside_tmux: bool,
        on_started: F,
    ) -> Result<i32>
    where
        F: FnOnce() -> Result<()>,
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
            let status = Command::new(&argv[0]).args(&argv[1..]).status()?;
            if status.success() {
                on_started()?;
            }
            Ok(status.code().unwrap_or(1))
        } else {
            terminal::run_pty_bridge_with_started(&argv, None, true, on_started)
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
            let status = self
                .command()
                .args(["switch-client", "-t", session])
                .status()?;
            if status.success() {
                if let Some(pane) = pane {
                    let window = self
                        .command()
                        .args(["select-window", "-t", pane])
                        .status()?;
                    if window.success() {
                        let selected = self.command().args(["select-pane", "-t", pane]).status()?;
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
            terminal::run_pty_bridge_with_started(&argv, None, true, on_started)
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
        assert!(calls.contains("set-option -t pika-c-workstream status off"));
        assert!(calls.contains("set-option -w -t"));
        assert!(calls.contains("history-limit 100000"));
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
    fn guarded_attach_records_only_after_the_server_accepts_the_exact_pane() {
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
        assert!(started);
    }

    #[cfg(unix)]
    #[test]
    fn attach_handoff_runs_callback_before_interrupted_client_exits() {
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
        assert!(
            started,
            "attach history must survive Ctrl-C/detach exit codes"
        );
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
