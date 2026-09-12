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
        let values = [
            ("@pika_provider", provider.map(|item| item.as_str())),
            ("@pika_session_id", session_id),
            ("@pika_name", name),
            ("@pika_launch_token", launch_token),
        ];
        for (option, value) in values {
            if let Some(value) = value {
                self.output(["set-option", "-p", "-t", target, option, value], true)?;
            }
        }
        Ok(())
    }

    pub fn clear_tags(&self, target: &str) -> Result<()> {
        for option in [
            "@pika_provider",
            "@pika_session_id",
            "@pika_name",
            "@pika_launch_token",
        ] {
            self.output(["set-option", "-p", "-u", "-t", target, option], false)?;
        }
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
        if let Some(value) = marker.filter(|value| (9876..=9885).contains(value)) {
            candidates.push(value);
        }
        candidates.extend((9876..=9885).filter(|value| Some(*value) != marker));
        for value in candidates {
            let option = format!("user-keys[{value}]");
            let current = self.output(["show-options", "-s", "-v", &option], false);
            let existing = current
                .as_ref()
                .ok()
                .filter(|output| output.status.success())
                .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned());
            if existing.is_some()
                && !(marker == Some(value)
                    && existing.as_deref() == Some(WINDOWS_TERMINAL_DA2_RESPONSE))
            {
                continue;
            }
            if existing.is_none()
                && self
                    .output(
                        ["set-option", "-s", &option, WINDOWS_TERMINAL_DA2_RESPONSE],
                        false,
                    )
                    .is_err()
            {
                continue;
            }
            let key = format!("User{value}");
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
        self.ensure_terminal_reply_guard();
        self.ensure_rgb();
        self.configure_home(session, pane)?;
        if std::env::var_os("TMUX").is_some() {
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
                        return Ok(self
                            .command()
                            .args(["select-pane", "-t", pane])
                            .status()?
                            .code()
                            .unwrap_or(1));
                    }
                    return Ok(window.code().unwrap_or(1));
                } else {
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
            terminal::run_pty_bridge(&argv, None, true)
        }
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
        self.output(
            ["new-session", "-d", "-s", tmux_name, "-c", cwd, "sleep 30"],
            true,
        )?;
        self.configure_home(tmux_name, None)?;
        let holding = self
            .get_pane(tmux_name)?
            .context("tmux holding pane disappeared")?;
        let wrapper = agent_wrapper(provider, agent_argv, environment, session_id, launch_token)?;
        self.output(
            [
                "respawn-pane",
                "-k",
                "-t",
                &holding.pane_id,
                "-c",
                cwd,
                &wrapper,
            ],
            true,
        )?;
        self.tag_pane(
            &holding.pane_id,
            Some(provider),
            session_id,
            Some(display_name),
            launch_token,
        )?;
        self.ensure_rgb();
        self.get_pane(&holding.pane_id)?
            .context("tmux agent pane disappeared")
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
        let wrapper = agent_wrapper(
            provider,
            agent_argv,
            environment,
            Some(session_id),
            Some(launch_token),
        )?;
        self.configure_home(&pane.session_name, Some(&pane.pane_id))?;
        self.output(
            [
                "respawn-pane",
                "-k",
                "-t",
                &pane.pane_id,
                "-c",
                cwd,
                &wrapper,
            ],
            true,
        )?;
        self.tag_pane(
            &pane.pane_id,
            Some(provider),
            Some(session_id),
            Some(display_name),
            Some(launch_token),
        )?;
        self.ensure_rgb();
        self.get_pane(&pane.pane_id)?
            .context("tmux agent pane disappeared after respawn")
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
    if provider == Provider::Codex {
        if let Some(palette) = palette {
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
}
