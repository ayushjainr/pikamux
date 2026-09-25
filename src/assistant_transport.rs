//! Isolated Codex app-server stdio transport.
//!
//! This is a process boundary, not a claim that Rust visibility is a sandbox.
//! The caller must provision a separate provider login in `codex_home`; this
//! module never reads, copies, or discovers credentials from the user's home.
//! A thread/model request is refused until the provider's effective disabled
//! capability report has been verified.

use serde_json::{Value, json};
use std::{
    fs,
    io::{BufReader, Read, Write},
    os::unix::{
        fs::{MetadataExt, PermissionsExt},
        io::AsRawFd,
        process::CommandExt,
    },
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc::{self, Receiver},
    thread,
    time::{Duration, Instant},
};

const MAX_FRAME_BYTES: usize = 1024 * 1024;
const MAX_STDERR_BYTES: usize = 64 * 1024;
const MAX_PENDING_FRAMES: usize = 64;
const MAX_NOTIFICATION_BYTES: usize = 4 * 1024 * 1024;
const MAX_DIAGNOSTIC_BYTES: usize = 4096;
const RPC_TIMEOUT: Duration = Duration::from_secs(10);

const DISABLE_FLAGS: &[&str] = &[
    "-c",
    "features.shell_tool=false",
    "-c",
    "features.apps=false",
    "-c",
    "features.hooks=false",
    "-c",
    "features.unified_exec=false",
    "-c",
    "features.multi_agent=false",
    "-c",
    "features.plugins=false",
    "-c",
    "web_search=disabled",
    "-c",
    "agents.enabled=false",
    "-c",
    "default_permissions=pika-assistant",
    "-c",
    "permissions.pika-assistant={filesystem={\":root\"=\"deny\"},network={enabled=false}}",
];

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("assistant provider isolation is not enabled: {0}")]
    Isolation(String),
    #[error("assistant provider transport failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("assistant provider RPC failed: {0}")]
    Rpc(String),
    #[error("assistant provider protocol violation: {0}")]
    Protocol(String),
    #[error("assistant provider transport was cancelled")]
    Cancelled,
}

#[derive(Clone, Debug)]
pub struct TransportConfig {
    pub executable: PathBuf,
    pub codex_home: PathBuf,
    pub scratch: PathBuf,
}

impl TransportConfig {
    pub fn validate(&self) -> Result<(), TransportError> {
        if !self.executable.is_absolute() {
            return Err(TransportError::Isolation(
                "provider executable must be absolute".into(),
            ));
        }
        if !self.codex_home.is_absolute() || !self.scratch.is_absolute() {
            return Err(TransportError::Isolation(
                "provider home and scratch must be absolute".into(),
            ));
        }
        if self.codex_home == self.scratch {
            return Err(TransportError::Isolation(
                "provider home and scratch must differ".into(),
            ));
        }
        private_directory(&self.codex_home)?;
        private_directory(&self.scratch)?;
        let executable = fs::symlink_metadata(&self.executable)?;
        if !executable.is_file() || executable.file_type().is_symlink() {
            return Err(TransportError::Isolation(
                "provider executable must be a regular non-symlink file".into(),
            ));
        }
        #[cfg(unix)]
        if executable.uid() != unsafe { libc::geteuid() }
            || executable.permissions().mode() & 0o022 != 0
        {
            return Err(TransportError::Isolation(
                "provider executable ownership or mode is unsafe".into(),
            ));
        }
        Ok(())
    }
}

fn private_directory(path: &Path) -> Result<(), TransportError> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(TransportError::Isolation(format!(
            "{} is not a private directory",
            path.display()
        )));
    }
    #[cfg(unix)]
    if metadata.uid() != unsafe { libc::geteuid() } || metadata.permissions().mode() & 0o077 != 0 {
        return Err(TransportError::Isolation(format!(
            "{} is not owner-only",
            path.display()
        )));
    }
    Ok(())
}

#[derive(Debug)]
struct Frame {
    id: Option<u64>,
    result: Option<Value>,
    error: Option<Value>,
    event: Option<Value>,
}

fn parse_frame(bytes: &[u8]) -> Result<Frame, TransportError> {
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(TransportError::Protocol("RPC frame exceeds 1 MiB".into()));
    }
    let value: Value = serde_json::from_slice(bytes)
        .map_err(|error| TransportError::Protocol(error.to_string()))?;
    let id = value.get("id").and_then(Value::as_u64);
    if value.get("method").is_some() {
        return Ok(Frame {
            id,
            result: None,
            error: None,
            event: Some(value),
        });
    }
    Ok(Frame {
        id,
        result: value.get("result").cloned(),
        error: value.get("error").cloned(),
        event: None,
    })
}

pub struct CodexTransport {
    child: Child,
    stdin: ChildStdin,
    frames: Receiver<Result<Frame, TransportError>>,
    notifications: Vec<Value>,
    notification_bytes: usize,
    next_id: u64,
    ready: bool,
}

impl CodexTransport {
    /// Start only an explicitly isolated provider process. This does not
    /// provision a login; the operator must separately approve and populate
    /// the isolated Codex home before using a real provider account.
    pub fn spawn(config: TransportConfig) -> Result<Self, TransportError> {
        config.validate()?;
        let mut command = Command::new(&config.executable);
        command
            .env_clear()
            .env("HOME", &config.codex_home)
            .env("CODEX_HOME", &config.codex_home)
            .env("PATH", "/usr/bin:/bin")
            .env("NO_COLOR", "1")
            .current_dir(&config.scratch)
            .arg("app-server")
            .arg("--stdio");
        for flag in DISABLE_FLAGS {
            command.arg(flag);
        }
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        unsafe {
            command.pre_exec(|| {
                if libc::setpgid(0, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| TransportError::Isolation("provider stdin unavailable".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| TransportError::Isolation("provider stdout unavailable".into()))?;
        let mut stderr = child
            .stderr
            .take()
            .ok_or_else(|| TransportError::Isolation("provider stderr unavailable".into()))?;
        let (sender, frames) = mpsc::sync_channel(MAX_PENDING_FRAMES);
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut line = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                match reader.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(count) => {
                        for byte in &chunk[..count] {
                            line.push(*byte);
                            if line.len() > MAX_FRAME_BYTES {
                                let _ = sender.send(Err(TransportError::Protocol(
                                    "RPC frame exceeds 1 MiB".into(),
                                )));
                                return;
                            }
                            if *byte == b'\n' {
                                let frame = parse_frame(line.strip_suffix(b"\n").unwrap_or(&line));
                                if sender.send(frame).is_err() {
                                    return;
                                }
                                line.clear();
                            }
                        }
                    }
                    Err(error) => {
                        let _ = sender.send(Err(TransportError::Io(error)));
                        return;
                    }
                }
            }
        });
        thread::spawn(move || {
            let mut bounded = Vec::new();
            let mut chunk = [0u8; 4096];
            while let Ok(count) = stderr.read(&mut chunk) {
                if count == 0 {
                    break;
                }
                let remaining = MAX_STDERR_BYTES.saturating_sub(bounded.len());
                bounded.extend_from_slice(&chunk[..count.min(remaining)]);
            }
        });
        let transport = Self {
            child,
            stdin,
            frames,
            notifications: Vec::new(),
            notification_bytes: 0,
            next_id: 1,
            ready: false,
        };
        // MainAssistant owns the protocol handshake. The request adapter below
        // adds the mandatory effective-config gate; no guessed provider response
        // is fabricated here.
        Ok(transport)
    }

    pub fn cancel(&mut self) -> Result<(), TransportError> {
        kill_owned(&mut self.child);
        Err(TransportError::Cancelled)
    }

    fn request_raw(&mut self, method: &str, params: Value) -> Result<Value, TransportError> {
        let id = self.next_id;
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or_else(|| TransportError::Protocol("RPC id exhausted".into()))?;
        let mut encoded =
            serde_json::to_vec(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
                .map_err(|error| TransportError::Protocol(error.to_string()))?;
        encoded.push(b'\n');
        if encoded.len() > MAX_FRAME_BYTES {
            return Err(TransportError::Protocol("RPC request exceeds 1 MiB".into()));
        }
        let deadline = Instant::now() + RPC_TIMEOUT;
        write_bounded(&mut self.stdin, &encoded, deadline)?;
        self.waiting_response(id, deadline)
    }

    fn waiting_response(&mut self, id: u64, deadline: Instant) -> Result<Value, TransportError> {
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(TransportError::Rpc("RPC response timeout".into()));
            }
            match self
                .frames
                .recv_timeout(remaining)
                .map_err(|_| TransportError::Rpc("RPC response timeout".into()))??
            {
                Frame {
                    id: Some(response_id),
                    result,
                    error,
                    ..
                } if response_id == id => {
                    if let Some(error) = error {
                        return Err(TransportError::Rpc(bounded_diagnostic(&error.to_string())));
                    }
                    return result.ok_or_else(|| {
                        TransportError::Protocol("RPC response omitted result".into())
                    });
                }
                Frame {
                    event: Some(event), ..
                } => self.push_notification(event)?,
                Frame { .. } => {
                    return Err(TransportError::Protocol("RPC response id mismatch".into()));
                }
            }
        }
    }

    fn push_notification(&mut self, event: Value) -> Result<(), TransportError> {
        let bytes = serde_json::to_vec(&event)
            .map_err(|error| TransportError::Protocol(error.to_string()))?
            .len();
        if bytes > MAX_FRAME_BYTES
            || self.notification_bytes.saturating_add(bytes) > MAX_NOTIFICATION_BYTES
            || self.notifications.len() >= MAX_PENDING_FRAMES
        {
            return Err(TransportError::Protocol(
                "notification queue exceeds safety bound".into(),
            ));
        }
        self.notification_bytes += bytes;
        self.notifications.push(event);
        Ok(())
    }

    fn drain_notifications(&mut self) -> Result<(), TransportError> {
        loop {
            match self.frames.try_recv() {
                Ok(Ok(Frame {
                    event: Some(event), ..
                })) => self.push_notification(event)?,
                Ok(Ok(Frame { id: Some(_), .. })) => {
                    return Err(TransportError::Protocol(
                        "late RPC response without a request".into(),
                    ));
                }
                Ok(Ok(Frame { .. })) => {}
                Ok(Err(error)) => return Err(error),
                Err(mpsc::TryRecvError::Empty) | Err(mpsc::TryRecvError::Disconnected) => break,
            }
        }
        Ok(())
    }
}

fn write_bounded(
    stdin: &mut ChildStdin,
    bytes: &[u8],
    deadline: Instant,
) -> Result<(), TransportError> {
    #[cfg(unix)]
    {
        let fd = stdin.as_raw_fd();
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(TransportError::Io(std::io::Error::last_os_error()));
        }
        let mut offset = 0;
        while offset < bytes.len() {
            let written =
                unsafe { libc::write(fd, bytes[offset..].as_ptr().cast(), bytes.len() - offset) };
            if written > 0 {
                offset += written as usize;
                continue;
            }
            if written < 0
                && std::io::Error::last_os_error().kind() == std::io::ErrorKind::WouldBlock
            {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Err(TransportError::Rpc("RPC write timeout".into()));
                }
                let mut pollfd = libc::pollfd {
                    fd,
                    events: libc::POLLOUT,
                    revents: 0,
                };
                let millis = left.as_millis().min(i32::MAX as u128) as i32;
                let rc = unsafe { libc::poll(&mut pollfd, 1, millis.max(1)) };
                if rc == 0 {
                    return Err(TransportError::Rpc("RPC write timeout".into()));
                }
                if rc < 0 {
                    return Err(TransportError::Io(std::io::Error::last_os_error()));
                }
                continue;
            }
            return Err(TransportError::Io(std::io::Error::last_os_error()));
        }
        return Ok(());
    }
    #[allow(unreachable_code)]
    {
        stdin.write_all(bytes).map_err(TransportError::Io)
    }
}

fn verify_effective_config(value: &Value) -> Result<(), TransportError> {
    let effective = value
        .get("config")
        .ok_or_else(|| TransportError::Isolation("config/read omitted config".into()))?;
    for name in [
        "features.shell_tool",
        "features.apps",
        "features.hooks",
        "features.unified_exec",
        "features.multi_agent",
        "features.plugins",
    ] {
        let mut current = effective;
        for part in name.split('.') {
            current = current.get(part).ok_or_else(|| {
                TransportError::Isolation(format!("effective config omitted {name}"))
            })?;
        }
        if current != &Value::Bool(false) {
            return Err(TransportError::Isolation(format!(
                "effective config did not disable {name}"
            )));
        }
    }
    if effective.get("web_search") != Some(&Value::String("disabled".into())) {
        return Err(TransportError::Isolation(
            "effective config did not disable web_search".into(),
        ));
    }
    if effective.get("agents").and_then(|v| v.get("enabled")) != Some(&Value::Bool(false)) {
        return Err(TransportError::Isolation(
            "effective config did not disable agents".into(),
        ));
    }
    if effective
        .get("mcp_servers")
        .and_then(Value::as_object)
        .is_none_or(|o| !o.is_empty())
    {
        return Err(TransportError::Isolation(
            "effective config omitted or enables MCP servers".into(),
        ));
    }
    match effective.get("plugins").and_then(Value::as_object) {
        Some(plugins) if plugins.is_empty() => {}
        _ => {
            return Err(TransportError::Isolation(
                "effective config omitted or enables plugins".into(),
            ));
        }
    }
    verify_assistant_permission_profile(effective)?;
    Ok(())
}

fn bounded_diagnostic(message: &str) -> String {
    let mut bounded = String::new();
    let mut truncated = false;
    for character in message.chars() {
        let character = if character.is_control() {
            ' '
        } else {
            character
        };
        let width = character.len_utf8();
        if bounded.len().saturating_add(width) > MAX_DIAGNOSTIC_BYTES {
            truncated = true;
            break;
        }
        bounded.push(character);
    }
    if truncated {
        bounded.push('…');
    }
    bounded
}

fn verify_assistant_permission_profile(effective: &Value) -> Result<(), TransportError> {
    if effective.get("default_permissions").and_then(Value::as_str)
        != Some(crate::assistant_provider::ASSISTANT_PERMISSION_PROFILE)
    {
        return Err(TransportError::Isolation(
            "effective config selected an unexpected assistant permission profile".into(),
        ));
    }
    let profile = effective
        .get("permissions")
        .and_then(|v| v.get(crate::assistant_provider::ASSISTANT_PERMISSION_PROFILE))
        .ok_or_else(|| {
            TransportError::Isolation(
                "effective config omitted the assistant permission profile".into(),
            )
        })?;
    if profile.get("description") != Some(&Value::Null)
        || profile.get("extends") != Some(&Value::Null)
        || profile.get("workspace_roots") != Some(&Value::Null)
    {
        return Err(TransportError::Isolation(
            "assistant permission profile inherits or grants workspace access".into(),
        ));
    }

    let filesystem = profile
        .get("filesystem")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            TransportError::Isolation(
                "assistant permission profile omitted filesystem restrictions".into(),
            )
        })?;
    if filesystem.len() != 2
        || filesystem.get("glob_scan_max_depth") != Some(&Value::Null)
        || filesystem.get(":root") != Some(&Value::String("deny".into()))
    {
        return Err(TransportError::Isolation(
            "assistant permission profile grants filesystem access".into(),
        ));
    }

    let network = profile
        .get("network")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            TransportError::Isolation(
                "assistant permission profile omitted network restrictions".into(),
            )
        })?;
    if network.len() != 13 || network.get("enabled") != Some(&Value::Bool(false)) {
        return Err(TransportError::Isolation(
            "assistant permission profile enables network access".into(),
        ));
    }
    for key in [
        "proxy_url",
        "enable_socks5",
        "socks_url",
        "enable_socks5_udp",
        "allow_upstream_proxy",
        "dangerously_allow_non_loopback_proxy",
        "dangerously_allow_all_unix_sockets",
        "mode",
        "domains",
        "unix_sockets",
        "allow_local_binding",
        "mitm",
    ] {
        if network.get(key) != Some(&Value::Null) {
            return Err(TransportError::Isolation(format!(
                "assistant permission profile configures network {key}"
            )));
        }
    }
    Ok(())
}

fn kill_owned(child: &mut Child) {
    #[cfg(unix)]
    if let Ok(pid) = i32::try_from(child.id()) {
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

impl Drop for CodexTransport {
    fn drop(&mut self) {
        kill_owned(&mut self.child);
    }
}

impl crate::assistant_provider::RpcTransport for CodexTransport {
    fn request(
        &mut self,
        method: &str,
        params: Value,
    ) -> std::result::Result<Value, crate::assistant_provider::ProviderError> {
        if method != "initialize" && method != "config/read" && !self.ready {
            return Err(crate::assistant_provider::ProviderError::Transport(
                "provider effective configuration was not verified".into(),
            ));
        }
        let result = self.request_raw(method, params).map_err(|error| {
            crate::assistant_provider::ProviderError::Transport(error.to_string())
        })?;
        Ok(result)
    }

    fn notify(
        &mut self,
        method: &str,
        params: Value,
    ) -> std::result::Result<(), crate::assistant_provider::ProviderError> {
        let mut encoded = serde_json::to_vec(
            &json!({"jsonrpc":"2.0","method":method,"params":params}),
        )
        .map_err(|error| crate::assistant_provider::ProviderError::Transport(error.to_string()))?;
        encoded.push(b'\n');
        if encoded.len() > MAX_FRAME_BYTES {
            return Err(crate::assistant_provider::ProviderError::Transport(
                "RPC notification exceeds 1 MiB".into(),
            ));
        }
        let deadline = Instant::now() + RPC_TIMEOUT;
        write_bounded(&mut self.stdin, &encoded, deadline).map_err(|error| {
            crate::assistant_provider::ProviderError::Transport(error.to_string())
        })?;
        if method == "initialized" {
            let effective = self
                .request_raw("config/read", json!({"includeLayers":false}))
                .map_err(|error| {
                    crate::assistant_provider::ProviderError::Transport(error.to_string())
                })?;
            verify_effective_config(&effective).map_err(|error| {
                crate::assistant_provider::ProviderError::Transport(error.to_string())
            })?;
            self.ready = true;
        }
        Ok(())
    }

    fn notifications(
        &mut self,
    ) -> std::result::Result<
        Vec<crate::assistant_provider::ServerEvent>,
        crate::assistant_provider::ProviderError,
    > {
        if let Ok(frame) = self.frames.recv_timeout(Duration::from_millis(25)) {
            match frame {
                Ok(Frame {
                    event: Some(event), ..
                }) => self.push_notification(event),
                Ok(Frame { id: Some(_), .. }) => Err(TransportError::Protocol(
                    "late RPC response without a request".into(),
                )),
                Ok(Frame { .. }) => Ok(()),
                Err(error) => Err(error),
            }
            .map_err(|error| {
                crate::assistant_provider::ProviderError::Transport(error.to_string())
            })?;
        }
        self.drain_notifications().map_err(|error| {
            crate::assistant_provider::ProviderError::Transport(error.to_string())
        })?;
        let pending = std::mem::take(&mut self.notifications);
        self.notification_bytes = 0;
        let mut events = Vec::new();
        for value in pending {
            events.push(
                crate::assistant_provider::ServerEvent::from_json(&value).map_err(|error| {
                    crate::assistant_provider::ProviderError::Transport(error.to_string())
                })?,
            );
        }
        Ok(events)
    }

    fn interrupt(
        &mut self,
        thread_id: &str,
        turn_id: &str,
    ) -> std::result::Result<(), crate::assistant_provider::ProviderError> {
        self.request(
            "turn/interrupt",
            json!({"threadId":thread_id,"turnId":turn_id}),
        )
        .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn fake(root: &Path, capabilities: &str) -> PathBuf {
        let executable = root.join("fake-codex-correct");
        let script = "#!/bin/sh\nwhile IFS= read -r line; do\ncase \"$line\" in\n*initialized*) : ;;\n*initialize*) printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}' ;;\n*config/read*) printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"config\":CAPS}}' ;;\n*thread/start*) printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{\"thread\":{\"id\":\"fake-thread\"}}}' ; printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"method\":\"item/agentMessage/delta\",\"params\":{\"threadId\":\"fake-thread\",\"turnId\":\"turn-1\",\"delta\":\"late\"}}' ;;\nesac\ndone\n";
        let profile = r#","plugins":{},"default_permissions":"pika-assistant","permissions":{"pika-assistant":{"description":null,"extends":null,"workspace_roots":null,"filesystem":{"glob_scan_max_depth":null,":root":"deny"},"network":{"enabled":false,"proxy_url":null,"enable_socks5":null,"socks_url":null,"enable_socks5_udp":null,"allow_upstream_proxy":null,"dangerously_allow_non_loopback_proxy":null,"dangerously_allow_all_unix_sockets":null,"mode":null,"domains":null,"unix_sockets":null,"allow_local_binding":null,"mitm":null}}}"#;
        let capabilities = format!(
            "{}{}{}",
            capabilities.strip_suffix('}').unwrap_or(capabilities),
            profile,
            "}"
        );
        fs::write(&executable, script.replace("CAPS", &capabilities)).unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        executable
    }

    fn config(root: &Path, executable: PathBuf) -> TransportConfig {
        let home = root.join("codex-home");
        let scratch = root.join("scratch");
        fs::create_dir(&home).unwrap();
        fs::create_dir(&scratch).unwrap();
        fs::set_permissions(&home, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(&scratch, fs::Permissions::from_mode(0o700)).unwrap();
        TransportConfig {
            executable,
            codex_home: home,
            scratch,
        }
    }

    #[test]
    fn fake_stdio_requires_effective_tools_disabled_and_starts_thread() {
        let root = tempfile::tempdir().unwrap();
        let executable = fake(
            root.path(),
            r#"{"features":{"shell_tool":false,"apps":false,"hooks":false,"unified_exec":false,"multi_agent":false,"plugins":false},"web_search":"disabled","agents":{"enabled":false},"mcp_servers":{}}"#,
        );
        let mut transport = CodexTransport::spawn(config(root.path(), executable)).unwrap();
        <CodexTransport as crate::assistant_provider::RpcTransport>::request(
            &mut transport,
            "initialize",
            json!({"clientInfo": {"name":"test"}}),
        )
        .unwrap();
        <CodexTransport as crate::assistant_provider::RpcTransport>::notify(
            &mut transport,
            "initialized",
            json!({}),
        )
        .unwrap();
        let started = <CodexTransport as crate::assistant_provider::RpcTransport>::request(
            &mut transport,
            "thread/start",
            json!({"model":"gpt-5.6-luna"}),
        )
        .unwrap();
        assert_eq!(started["thread"]["id"], "fake-thread");
        let events = <CodexTransport as crate::assistant_provider::RpcTransport>::notifications(
            &mut transport,
        )
        .unwrap();
        assert_eq!(events.len(), 1, "post-response event must remain drainable");
    }

    #[test]
    fn incompatible_effective_capability_fails_before_thread_start() {
        let root = tempfile::tempdir().unwrap();
        let executable = fake(
            root.path(),
            r#"{"features":{"shell_tool":true,"apps":false,"hooks":false,"unified_exec":false,"multi_agent":false,"plugins":false},"web_search":"disabled","agents":{"enabled":false},"mcp_servers":{}}"#,
        );
        let mut transport = CodexTransport::spawn(config(root.path(), executable)).unwrap();
        <CodexTransport as crate::assistant_provider::RpcTransport>::request(
            &mut transport,
            "initialize",
            json!({"clientInfo": {"name":"test"}}),
        )
        .unwrap();
        let error = <CodexTransport as crate::assistant_provider::RpcTransport>::notify(
            &mut transport,
            "initialized",
            json!({}),
        )
        .unwrap_err();
        assert!(error.to_string().contains("shell"));
    }

    #[test]
    fn missing_plugin_inventory_blocks_thread_start() {
        use crate::assistant_provider::RpcTransport;

        let root = tempfile::tempdir().unwrap();
        let executable = fake(
            root.path(),
            r#"{"features":{"shell_tool":false,"apps":false,"hooks":false,"unified_exec":false,"multi_agent":false,"plugins":false},"web_search":"disabled","agents":{"enabled":false},"mcp_servers":{}}"#,
        );
        let script = fs::read_to_string(&executable).unwrap();
        assert!(script.contains("\"plugins\":{},"));
        fs::write(&executable, script.replace("\"plugins\":{},", "")).unwrap();
        let mut transport = CodexTransport::spawn(config(root.path(), executable)).unwrap();
        transport.request("initialize", json!({})).unwrap();
        let error = transport.notify("initialized", json!({})).unwrap_err();
        assert!(error.to_string().contains("plugins"));
        let error = transport.request("thread/start", json!({})).unwrap_err();
        assert!(error.to_string().contains("configuration was not verified"));
    }

    #[test]
    fn relative_or_shared_paths_are_rejected_before_spawn() {
        let root = tempfile::tempdir().unwrap();
        let executable = fake(
            root.path(),
            r#"{"features":{"shell_tool":false,"apps":false,"hooks":false,"unified_exec":false,"multi_agent":false,"plugins":false},"web_search":"disabled","agents":{"enabled":false},"mcp_servers":{}}"#,
        );
        let mut invalid = config(root.path(), executable);
        invalid.executable = PathBuf::from("codex");
        assert!(matches!(
            invalid.validate(),
            Err(TransportError::Isolation(_))
        ));
        let same = root.path().join("same");
        fs::create_dir(&same).unwrap();
        fs::set_permissions(&same, fs::Permissions::from_mode(0o700)).unwrap();
        let executable = fake(
            root.path(),
            r#"{"features":{"shell_tool":false,"apps":false,"hooks":false,"unified_exec":false,"multi_agent":false,"plugins":false},"web_search":"disabled","agents":{"enabled":false},"mcp_servers":{}}"#,
        );
        let invalid = TransportConfig {
            executable,
            codex_home: same.clone(),
            scratch: same,
        };
        assert!(matches!(
            invalid.validate(),
            Err(TransportError::Isolation(_))
        ));
    }

    #[test]
    fn overlong_frame_is_rejected_incrementally() {
        let root = tempfile::tempdir().unwrap();
        let executable = root.path().join("overlong");
        fs::write(
            &executable,
            "#!/bin/sh\nIFS= read -r line\nhead -c 1048577 /dev/zero | tr '\\0' x\nprintf '\\n'\n",
        )
        .unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        let mut transport = CodexTransport::spawn(config(root.path(), executable)).unwrap();
        let error = <CodexTransport as crate::assistant_provider::RpcTransport>::request(
            &mut transport,
            "initialize",
            json!({"clientInfo": {"name":"test"}}),
        )
        .unwrap_err();
        assert!(error.to_string().contains("1 MiB"));
    }

    #[test]
    fn stderr_is_drained_after_capture_cap() {
        let root = tempfile::tempdir().unwrap();
        let executable = root.path().join("stderr-flood");
        fs::write(&executable, "#!/bin/sh\nhead -c 131072 /dev/zero >&2\nwhile IFS= read -r line; do\nprintf '%s\\n' '{\"id\":1,\"result\":{}}'\ndone\n").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        // This must fail closed on the missing effective-config response, not hang
        // behind a full stderr pipe.
        let mut transport = CodexTransport::spawn(config(root.path(), executable)).unwrap();
        <CodexTransport as crate::assistant_provider::RpcTransport>::request(
            &mut transport,
            "initialize",
            json!({"clientInfo": {"name":"test"}}),
        )
        .unwrap();
        let error = <CodexTransport as crate::assistant_provider::RpcTransport>::notify(
            &mut transport,
            "initialized",
            json!({}),
        )
        .unwrap_err();
        assert!(error.to_string().contains("config") || error.to_string().contains("RPC"));
    }

    #[test]
    fn permission_profile_rejects_any_hidden_grant_and_diagnostics_are_bounded() {
        let effective = json!({
            "features": {
                "shell_tool": false,
                "apps": false,
                "hooks": false,
                "unified_exec": false,
                "multi_agent": false,
                "plugins": false
            },
            "web_search": "disabled",
            "agents": { "enabled": false },
            "mcp_servers": {},
            "plugins": {},
            "default_permissions": "pika-assistant",
            "permissions": {
                "pika-assistant": {
                    "description": null,
                    "extends": null,
                    "workspace_roots": null,
                    "filesystem": {
                        "glob_scan_max_depth": null,
                        ":root": "deny"
                    },
                    "network": {
                        "enabled": false,
                        "proxy_url": null,
                        "enable_socks5": null,
                        "socks_url": null,
                        "enable_socks5_udp": null,
                        "allow_upstream_proxy": null,
                        "dangerously_allow_non_loopback_proxy": null,
                        "dangerously_allow_all_unix_sockets": null,
                        "mode": null,
                        "domains": null,
                        "unix_sockets": null,
                        "allow_local_binding": null,
                        "mitm": null
                    }
                }
            }
        });
        let effective = json!({ "config": effective });
        assert!(verify_effective_config(&effective).is_ok());

        let mut broadened = effective.clone();
        broadened["config"]["permissions"]["pika-assistant"]["filesystem"]["/tmp"] = json!("read");
        assert!(matches!(
            verify_effective_config(&broadened),
            Err(TransportError::Isolation(_))
        ));
        let mut network = effective;
        network["config"]["permissions"]["pika-assistant"]["network"]["domains"] =
            json!(["example.com"]);
        assert!(matches!(
            verify_effective_config(&network),
            Err(TransportError::Isolation(_))
        ));

        let diagnostic = bounded_diagnostic(&format!("prefix\u{1b}[31m{}", "x".repeat(16 * 1024)));
        assert!(diagnostic.len() <= MAX_DIAGNOSTIC_BYTES + "…".len());
        assert!(!diagnostic.contains('\u{1b}'));
    }
}
