use crate::{
    config::Config,
    model::Provider,
    paths::Paths,
    store::{HookObservation, Store},
};
use anyhow::{Context, Result, bail};
use regex::Regex;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

pub const CODEX_EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PermissionRequest",
    "PreToolUse",
    "PostToolUse",
    "Stop",
    "SessionEnd",
];

pub const CLAUDE_EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PermissionRequest",
    "PreToolUse",
    "PostToolUse",
    "Notification",
    "Stop",
    "StopFailure",
    "SessionEnd",
];

const CLAUDE_NOTIFICATION_MATCHER: &str =
    "permission_prompt|idle_prompt|elicitation_dialog|agent_needs_input|agent_completed";
const OPENCODE_TEMPLATE: &str = include_str!("../assets/pika-opencode.js.in");
pub const PENDING_LAUNCH_GRACE_SECONDS: f64 = 90.0;
const MAX_VERSION_OUTPUT_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderExecutableEvidence {
    pub available: bool,
    pub version: Option<String>,
    pub compatibility_error: Option<String>,
}

impl ProviderExecutableEvidence {
    pub fn compatible(&self) -> bool {
        self.available && self.version.is_some() && self.compatibility_error.is_none()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProviderCommissioning {
    pub provider: Provider,
    pub required: bool,
    pub executable: String,
    pub runtime: ProviderExecutableEvidence,
    pub hooks_active: bool,
    pub expected_fingerprint: String,
    pub observation: Option<HookObservation>,
    pub overdue_launches: Vec<(String, f64)>,
}

impl ProviderCommissioning {
    pub fn observed(&self) -> bool {
        self.observation
            .as_ref()
            .is_some_and(|value| value.fingerprint == self.expected_fingerprint)
    }

    pub fn healthy(&self) -> bool {
        self.runtime.compatible()
            && self.hooks_active
            && self.observed()
            && self.overdue_launches.is_empty()
    }

    pub fn missing_proofs(&self) -> Vec<String> {
        let label = provider_label(self.provider);
        let mut missing = Vec::new();
        if !self.runtime.available {
            missing.push(format!(
                "{label} executable unavailable ({})",
                sanitize_version(&self.executable)
            ));
        } else if self.runtime.version.is_none() {
            missing.push(format!("{label} version could not be verified"));
        } else if let Some(error) = &self.runtime.compatibility_error {
            missing.push(error.clone());
        }
        if !self.hooks_active {
            missing.push(format!("{label} hooks are not active"));
        }
        if !self.observed() {
            missing.push(format!("{label} matching hook event has not been observed"));
        }
        for (name, age) in &self.overdue_launches {
            missing.push(format!(
                "{label} launch {} has awaited exact identity for {:.0}s",
                sanitize_version(name),
                age.max(0.0),
            ));
        }
        missing
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct CommissioningReport {
    pub providers: Vec<ProviderCommissioning>,
}

impl CommissioningReport {
    pub fn commissioned(&self) -> bool {
        self.providers
            .iter()
            .filter(|value| value.required)
            .all(ProviderCommissioning::healthy)
    }

    pub fn missing_proofs(&self) -> Vec<String> {
        self.providers
            .iter()
            .filter(|value| value.required)
            .flat_map(ProviderCommissioning::missing_proofs)
            .collect()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileChange {
    pub path: PathBuf,
    pub before: String,
    pub after: String,
    pub notice: Option<String>,
}

impl FileChange {
    pub fn changed(&self) -> bool {
        self.before != self.after
    }
}

#[derive(Clone, Debug)]
pub struct SetupPaths {
    pub pika_config: PathBuf,
    pub codex_home: PathBuf,
    pub claude_home: PathBuf,
    pub opencode_config_home: PathBuf,
}

impl From<&Paths> for SetupPaths {
    fn from(paths: &Paths) -> Self {
        Self {
            pika_config: paths.config.clone(),
            codex_home: paths.codex_home.clone(),
            claude_home: paths.claude_home.clone(),
            opencode_config_home: paths.opencode_config_home.clone(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct SetupOptions {
    pub default_provider: Provider,
    pub machine_alias: Option<String>,
    pub provider_executables: BTreeMap<String, String>,
    pub provider_runtime_path: Option<String>,
    /// Absolute path to the installed, immutable Pika executable.
    pub pika_executable: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApplyReceipt {
    pub written: Vec<PathBuf>,
    pub backups: Vec<PathBuf>,
}

/// Revalidate the complete commissioning claim from current evidence. File
/// writes alone never prove that a provider can run or that its hook has fired.
pub fn commissioning_report(
    paths: &SetupPaths,
    store: &Store,
    options: &SetupOptions,
    required: &BTreeSet<Provider>,
    at: f64,
) -> CommissioningReport {
    let executables = Provider::ALL.map(|provider| {
        (
            provider,
            options
                .provider_executables
                .get(provider.as_str())
                .cloned()
                .unwrap_or_else(|| provider.as_str().to_owned()),
        )
    });
    let runtimes = thread::scope(|scope| {
        let handles = executables.each_ref().map(|(provider, executable)| {
            scope.spawn(move || {
                probe_provider_executable(*provider, executable, Duration::from_secs(3))
            })
        });
        handles.map(|handle| {
            handle
                .join()
                .unwrap_or_else(|_| ProviderExecutableEvidence {
                    available: false,
                    version: None,
                    compatibility_error: Some("provider version probe failed".into()),
                })
        })
    });
    let pending = store.list_pending().unwrap_or_default();
    let providers = Provider::ALL
        .into_iter()
        .enumerate()
        .map(|(index, provider)| {
            let home = match provider {
                Provider::Codex => &paths.codex_home,
                Provider::Claude => &paths.claude_home,
                Provider::Opencode => &paths.opencode_config_home,
            };
            let expected_fingerprint =
                hook_spec_fingerprint(provider, &options.pika_executable).unwrap_or_default();
            ProviderCommissioning {
                provider,
                required: required.contains(&provider),
                executable: executables[index].1.clone(),
                runtime: runtimes[index].clone(),
                hooks_active: hooks_installed(home, provider, &options.pika_executable),
                expected_fingerprint,
                observation: store.get_hook_observation(provider).ok().flatten(),
                overdue_launches: pending
                    .iter()
                    .filter(|launch| {
                        launch.provider == provider
                            && at - launch.created_at > PENDING_LAUNCH_GRACE_SECONDS
                    })
                    .map(|launch| (launch.name.clone(), at - launch.created_at))
                    .collect(),
            }
        })
        .collect();
    CommissioningReport { providers }
}

pub fn probe_provider_executable(
    provider: Provider,
    executable: &str,
    timeout: Duration,
) -> ProviderExecutableEvidence {
    let mut command = Command::new(executable);
    command
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) == 0 {
                Ok(())
            } else {
                Err(std::io::Error::last_os_error())
            }
        });
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(_) => {
            return ProviderExecutableEvidence {
                available: false,
                version: None,
                compatibility_error: None,
            };
        }
    };
    let pid = child.id();
    let output = child.stdout.take().map(read_probe_stream);
    let errors = child.stderr.take().map(read_probe_stream);
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            _ => {
                terminate_probe_group(pid);
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let stdout = receive_probe_stream(output, pid);
    let stderr = receive_probe_stream(errors, pid);
    let version = [stdout, stderr]
        .into_iter()
        .find(|value| !value.trim().is_empty())
        .map(|value| sanitize_version(&value));
    let available = status.is_some_and(|value| value.success());
    let compatibility_error = if available {
        provider_compatibility_error(provider, version.as_deref())
    } else {
        None
    };
    ProviderExecutableEvidence {
        available,
        version,
        compatibility_error,
    }
}

fn provider_compatibility_error(provider: Provider, version: Option<&str>) -> Option<String> {
    if provider != Provider::Opencode {
        return version
            .is_none()
            .then(|| format!("{} version could not be verified", provider_label(provider)));
    }
    let required = (1_u64, 18_u64, 21_u64);
    let found = Regex::new(r"(?x)(?:^|[^0-9])(\d+)\.(\d+)\.(\d+)(?:[^0-9]|$)")
        .expect("static semantic version expression")
        .captures(version.unwrap_or_default())
        .and_then(|captures| {
            Some((
                captures.get(1)?.as_str().parse().ok()?,
                captures.get(2)?.as_str().parse().ok()?,
                captures.get(3)?.as_str().parse().ok()?,
            ))
        });
    match found {
        Some(found) if found >= required => None,
        Some(_) => Some(format!(
            "OpenCode requires version 1.18.21 or newer; found {}",
            version.unwrap_or("unknown")
        )),
        None => Some(format!(
            "OpenCode requires version 1.18.21 or newer; could not verify {}",
            version.unwrap_or("unknown")
        )),
    }
}

fn read_probe_stream(mut stream: impl Read + Send + 'static) -> mpsc::Receiver<Vec<u8>> {
    let (sender, receiver) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let mut retained = Vec::new();
        let mut buffer = [0_u8; 8192];
        loop {
            match stream.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(count) => {
                    let remaining = MAX_VERSION_OUTPUT_BYTES.saturating_sub(retained.len());
                    retained.extend_from_slice(&buffer[..count.min(remaining)]);
                }
            }
        }
        let _ = sender.send(retained);
    });
    receiver
}

fn receive_probe_stream(receiver: Option<mpsc::Receiver<Vec<u8>>>, pid: u32) -> String {
    let Some(receiver) = receiver else {
        return String::new();
    };
    match receiver.recv_timeout(Duration::from_millis(250)) {
        Ok(value) => String::from_utf8_lossy(&value).into_owned(),
        Err(_) => {
            terminate_probe_group(pid);
            receiver
                .recv_timeout(Duration::from_millis(250))
                .map(|value| String::from_utf8_lossy(&value).into_owned())
                .unwrap_or_default()
        }
    }
}

#[cfg(unix)]
fn terminate_probe_group(pid: u32) {
    if let Ok(pid) = i32::try_from(pid) {
        // SAFETY: the provider probe was placed in its own process group in
        // `pre_exec`; the negative PID can target only that owned group.
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
}

#[cfg(not(unix))]
fn terminate_probe_group(_pid: u32) {}

fn sanitize_version(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(240)
        .collect()
}

fn provider_label(provider: Provider) -> &'static str {
    match provider {
        Provider::Codex => "Codex",
        Provider::Claude => "Claude",
        Provider::Opencode => "OpenCode",
    }
}

pub fn proposed_hook_changes(
    paths: &SetupPaths,
    options: &SetupOptions,
) -> Result<Vec<FileChange>> {
    if !options.pika_executable.is_absolute() {
        bail!("Pika hook executable must be an absolute path");
    }
    Ok(vec![
        pika_config_change(&paths.pika_config, options)?,
        codex_hooks_change(&paths.codex_home, &options.pika_executable)?,
        codex_config_change(&paths.codex_home)?,
        claude_settings_change(&paths.claude_home, &options.pika_executable)?,
        opencode_plugin_change(&paths.opencode_config_home, &options.pika_executable)?,
    ])
}

pub fn pika_config_change(path: &Path, options: &SetupOptions) -> Result<FileChange> {
    let before = read_optional_text(path)?;
    let mut value = if before.is_empty() {
        serde_json::to_value(Config::default())?
    } else {
        parse_object(path, &before)?
    };
    let object = value
        .as_object_mut()
        .expect("parse_object always returns an object");
    object.insert(
        "default_provider".into(),
        Value::String(options.default_provider.as_str().into()),
    );
    if let Some(alias) = &options.machine_alias {
        object.insert("machine_alias".into(), Value::String(alias.clone()));
    }
    object.insert(
        "provider_executables".into(),
        serde_json::to_value(&options.provider_executables)?,
    );
    if let Some(path) = &options.provider_runtime_path {
        object.insert("provider_runtime_path".into(), Value::String(path.clone()));
    }
    let after = pretty_object_preserving_top_order(&before, object)?;
    Ok(FileChange {
        path: path.to_owned(),
        before,
        after,
        notice: None,
    })
}

pub fn codex_hooks_change(home: &Path, executable: &Path) -> Result<FileChange> {
    json_hooks_change(
        home.join("hooks.json"),
        Provider::Codex,
        CODEX_EVENTS,
        executable,
    )
}

pub fn claude_settings_change(home: &Path, executable: &Path) -> Result<FileChange> {
    validate_executable(executable)?;
    let target = home.join("settings.json");
    let before = read_optional_text(&target)?;
    let mut value = if before.is_empty() {
        Value::Object(Map::new())
    } else {
        parse_object(&target, &before)?
    };
    value
        .as_object_mut()
        .expect("parse_object always returns an object")
        .insert("disableAllHooks".into(), Value::Bool(false));
    merge_hooks(
        &target,
        &mut value,
        Provider::Claude,
        CLAUDE_EVENTS,
        executable,
    )?;
    let after = pretty_object_preserving_top_order(
        &before,
        value.as_object().expect("value is an object"),
    )?;
    Ok(FileChange {
        path: target,
        before,
        after,
        notice: None,
    })
}

fn json_hooks_change(
    target: PathBuf,
    provider: Provider,
    events: &[&str],
    executable: &Path,
) -> Result<FileChange> {
    validate_executable(executable)?;
    let before = read_optional_text(&target)?;
    let mut value = if before.is_empty() {
        Value::Object(Map::new())
    } else {
        parse_object(&target, &before)?
    };
    if provider == Provider::Codex {
        value
            .as_object_mut()
            .expect("parse_object always returns an object")
            .entry("description")
            .or_insert_with(|| {
                Value::String("User lifecycle hooks, including Pika session tracking.".into())
            });
    }
    merge_hooks(&target, &mut value, provider, events, executable)?;
    let after = pretty_object_preserving_top_order(
        &before,
        value.as_object().expect("value is an object"),
    )?;
    Ok(FileChange {
        path: target,
        before,
        after,
        notice: None,
    })
}

fn merge_hooks(
    target: &Path,
    value: &mut Value,
    provider: Provider,
    events: &[&str],
    executable: &Path,
) -> Result<()> {
    let root = value
        .as_object_mut()
        .context("provider settings must contain a JSON object")?;
    let hooks = root
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .with_context(|| {
            format!(
                "cannot safely merge non-object hooks at {}",
                target.display()
            )
        })?;
    for event in events {
        let groups = hooks
            .entry((*event).to_owned())
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
            .with_context(|| {
                format!(
                    "cannot safely merge non-list {} hook event {event}",
                    provider.as_str()
                )
            })?;
        if groups
            .iter()
            .any(|group| contains_handler(group, provider, event, executable))
        {
            continue;
        }
        groups.retain(|group| !is_pika_group(group, provider));
        let mut group = Map::new();
        if let Some(matcher) = event_matcher(provider, event) {
            group.insert("matcher".into(), Value::String(matcher.into()));
        }
        group.insert(
            "hooks".into(),
            json!([{
                "type": "command",
                "command": handler_command(executable, provider),
                "timeout": handler_timeout(event),
            }]),
        );
        groups.push(Value::Object(group));
    }
    Ok(())
}

pub fn codex_config_change(home: &Path) -> Result<FileChange> {
    let target = home.join("config.toml");
    let before = read_optional_text(&target)?;
    before
        .parse::<toml_edit::DocumentMut>()
        .with_context(|| format!("cannot safely merge invalid TOML at {}", target.display()))?;
    let mut lines: Vec<String> = before.split_inclusive('\n').map(str::to_owned).collect();
    if !before.is_empty() && !before.ends_with('\n') {
        // `split_inclusive` already retained the final unterminated line.
    }
    let mut section_start = None;
    let mut section_end = lines.len();
    for (index, line) in lines.iter().enumerate() {
        let logical = line.trim_end_matches(['\r', '\n']);
        let header = logical.split('#').next().unwrap_or_default().trim();
        if header == "[features]" {
            if section_start.is_some() {
                bail!(
                    "cannot safely merge duplicate [features] tables at {}",
                    target.display()
                );
            }
            section_start = Some(index);
        } else if section_start.is_some() && header.starts_with('[') && header.ends_with(']') {
            section_end = index;
            break;
        }
    }
    let after = if let Some(start) = section_start {
        let mut found = false;
        for line in lines.iter_mut().take(section_end).skip(start + 1) {
            let logical = line.trim_end_matches(['\r', '\n']);
            let content = logical.split('#').next().unwrap_or_default();
            let Some((key, _)) = content.split_once('=') else {
                continue;
            };
            if key.trim() != "hooks" {
                continue;
            }
            let indent_len = logical.len() - logical.trim_start().len();
            let indent = &logical[..indent_len];
            let comment = logical
                .find('#')
                .map(|index| &logical[index..])
                .unwrap_or_default();
            let spacer = if comment.is_empty() { "" } else { " " };
            let newline = if line.ends_with("\r\n") {
                "\r\n"
            } else if line.ends_with('\n') {
                "\n"
            } else {
                ""
            };
            *line = format!("{indent}hooks = true{spacer}{comment}{newline}");
            found = true;
            break;
        }
        if !found {
            lines.insert(section_end, "hooks = true\n".into());
        }
        lines.concat()
    } else {
        let mut prefix = before.clone();
        if !prefix.is_empty() && !prefix.ends_with('\n') {
            prefix.push('\n');
        }
        if !prefix.is_empty() && !prefix.ends_with("\n\n") {
            prefix.push('\n');
        }
        prefix.push_str("[features]\nhooks = true\n");
        prefix
    };
    Ok(FileChange {
        path: target,
        before,
        after,
        notice: None,
    })
}

pub fn opencode_plugin_source(executable: &Path) -> Result<String> {
    validate_executable(executable)?;
    let command = serde_json::to_string(&vec![
        executable.to_string_lossy().into_owned(),
        "hook".into(),
        "--provider".into(),
        "opencode".into(),
    ])?;
    Ok(OPENCODE_TEMPLATE.replace("__PIKA_COMMAND_JSON__", &command))
}

pub fn opencode_plugin_change(home: &Path, executable: &Path) -> Result<FileChange> {
    let target = home.join("plugins/pika.js");
    let before = read_optional_text(&target)?;
    Ok(FileChange {
        path: target,
        before,
        after: opencode_plugin_source(executable)?,
        notice: None,
    })
}

pub fn hook_spec_fingerprint(provider: Provider, executable: &Path) -> Result<String> {
    validate_executable(executable)?;
    let content = if provider == Provider::Opencode {
        opencode_plugin_source(executable)?
    } else {
        let events = if provider == Provider::Codex {
            CODEX_EVENTS
        } else {
            CLAUDE_EVENTS
        };
        serde_json::to_string(
            &events
                .iter()
                .map(|event| {
                    json!({
                        "event": event,
                        "command": handler_command(executable, provider),
                        "timeout": handler_timeout(event),
                        "matcher": event_matcher(provider, event),
                    })
                })
                .collect::<Vec<_>>(),
        )?
    };
    Ok(format!("{:x}", Sha256::digest(content.as_bytes())))
}

pub fn hooks_installed(home: &Path, provider: Provider, executable: &Path) -> bool {
    if !executable.is_file() {
        return false;
    }
    if provider == Provider::Opencode {
        return fs::read_to_string(home.join("plugins/pika.js"))
            .ok()
            .zip(opencode_plugin_source(executable).ok())
            .is_some_and(|(actual, expected)| actual == expected);
    }
    let path = if provider == Provider::Codex {
        home.join("hooks.json")
    } else {
        home.join("settings.json")
    };
    let Ok(content) = fs::read_to_string(&path) else {
        return false;
    };
    let Ok(value) = parse_object(&path, &content) else {
        return false;
    };
    if provider == Provider::Claude && value.get("disableAllHooks") == Some(&Value::Bool(true)) {
        return false;
    }
    if provider == Provider::Codex && !codex_hooks_enabled(home) {
        return false;
    }
    let Some(hooks) = value.get("hooks").and_then(Value::as_object) else {
        return false;
    };
    let events = if provider == Provider::Codex {
        CODEX_EVENTS
    } else {
        CLAUDE_EVENTS
    };
    events.iter().all(|event| {
        hooks
            .get(*event)
            .and_then(Value::as_array)
            .is_some_and(|groups| {
                groups
                    .iter()
                    .any(|group| contains_handler(group, provider, event, executable))
            })
    })
}

pub fn codex_hooks_enabled(home: &Path) -> bool {
    let path = home.join("config.toml");
    let Ok(content) = fs::read_to_string(path) else {
        return true;
    };
    let Ok(document) = content.parse::<toml_edit::DocumentMut>() else {
        return false;
    };
    document
        .get("features")
        .and_then(|item| item.get("hooks"))
        .and_then(toml_edit::Item::as_value)
        .and_then(toml_edit::Value::as_bool)
        .unwrap_or(true)
}

/// Apply an already reviewed batch. Every target is revalidated before any write.
pub fn apply_changes(changes: &[FileChange], stamp: &str) -> Result<ApplyReceipt> {
    validate_stamp(stamp)?;
    let mut targets = BTreeSet::new();
    for change in changes.iter().filter(|change| change.changed()) {
        if !targets.insert(change.path.clone()) {
            bail!(
                "setup batch contains duplicate target {}",
                change.path.display()
            );
        }
        reject_symlink_target(&change.path)?;
        let current = read_optional_text(&change.path)?;
        if current != change.before {
            bail!(
                "{} changed since preview; run Pika setup again",
                change.path.display()
            );
        }
    }

    let mut staged = Vec::new();
    for (index, change) in changes.iter().filter(|change| change.changed()).enumerate() {
        let parent = change
            .path
            .parent()
            .context("setup target has no parent directory")?;
        fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
        reject_symlink_target(&change.path)?;
        let temporary = change.path.with_file_name(format!(
            ".{}.{}.{}.tmp",
            change
                .path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("pika"),
            std::process::id(),
            index
        ));
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        options.mode(0o600);
        let stage_result = (|| -> Result<()> {
            let mut file = options
                .open(&temporary)
                .with_context(|| format!("cannot stage {}", change.path.display()))?;
            file.write_all(change.after.as_bytes())?;
            file.sync_all()?;
            Ok(())
        })();
        if let Err(error) = stage_result {
            remove_staged(&staged);
            let _ = fs::remove_file(&temporary);
            return Err(error);
        }
        staged.push((change, temporary));
    }

    // Directory creation and staging widen the preview-to-write window. Close it
    // again before making backups or replacing any target.
    for (change, _) in &staged {
        if let Err(error) = reject_symlink_target(&change.path) {
            remove_staged(&staged);
            return Err(error);
        }
        match read_optional_text(&change.path) {
            Ok(current) if current == change.before => {}
            Ok(_) => {
                remove_staged(&staged);
                bail!(
                    "{} changed since preview; run Pika setup again",
                    change.path.display()
                );
            }
            Err(error) => {
                remove_staged(&staged);
                return Err(error);
            }
        }
    }

    let mut backups = Vec::new();
    for (change, _) in &staged {
        if !change.path.exists() {
            continue;
        }
        let mut backup = change.path.with_file_name(format!(
            "{}.pika-backup-{stamp}",
            change
                .path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("config")
        ));
        let mut suffix = 2;
        while backup.exists() {
            backup = change.path.with_file_name(format!(
                "{}.pika-backup-{stamp}-{suffix}",
                change
                    .path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("config")
            ));
            suffix += 1;
        }
        if let Err(error) = fs::copy(&change.path, &backup)
            .with_context(|| format!("cannot back up {}", change.path.display()))
        {
            remove_staged(&staged);
            for (_, path) in &backups {
                let _ = fs::remove_file(path);
            }
            return Err(error);
        }
        backups.push((change.path.clone(), backup));
    }

    let mut written = Vec::new();
    for (change, temporary) in &staged {
        let preflight_result = (|| -> Result<()> {
            reject_symlink_target(&change.path)?;
            if read_optional_text(&change.path)? != change.before {
                bail!(
                    "{} changed since preview; run Pika setup again",
                    change.path.display()
                );
            }
            Ok(())
        })();
        if let Err(error) = preflight_result {
            restore_written(&written, &backups);
            remove_staged(&staged);
            return Err(error);
        }
        if let Err(error) = fs::rename(temporary, &change.path)
            .with_context(|| format!("cannot replace {}", change.path.display()))
        {
            restore_written(&written, &backups);
            remove_staged(&staged);
            return Err(error);
        }
        written.push(change.path.clone());
        #[cfg(unix)]
        if let Err(error) = fs::set_permissions(&change.path, fs::Permissions::from_mode(0o600)) {
            restore_written(&written, &backups);
            remove_staged(&staged);
            return Err(error).with_context(|| {
                format!(
                    "cannot set private permissions on {}",
                    change.path.display()
                )
            });
        }
    }

    Ok(ApplyReceipt {
        written,
        backups: backups.into_iter().map(|(_, path)| path).collect(),
    })
}

fn remove_staged(staged: &[(&FileChange, PathBuf)]) {
    for (_, path) in staged {
        let _ = fs::remove_file(path);
    }
}

fn restore_written(written: &[PathBuf], backups: &[(PathBuf, PathBuf)]) {
    for path in written.iter().rev() {
        if let Some((_, backup)) = backups.iter().find(|(target, _)| target == path) {
            let _ = fs::copy(backup, path);
        } else {
            let _ = fs::remove_file(path);
        }
    }
}

fn handler_timeout(event: &str) -> u64 {
    if event == "SessionEnd" { 3 } else { 5 }
}

fn event_matcher(provider: Provider, event: &str) -> Option<&'static str> {
    match (provider, event) {
        (Provider::Claude, "Notification") => Some(CLAUDE_NOTIFICATION_MATCHER),
        (Provider::Codex, "PreToolUse") => Some("^request_user_input$"),
        (Provider::Claude, "PreToolUse") => Some("^AskUserQuestion$"),
        _ => None,
    }
}

fn handler_command(executable: &Path, provider: Provider) -> String {
    [
        executable.to_string_lossy().as_ref(),
        "hook",
        "--provider",
        provider.as_str(),
    ]
    .into_iter()
    .map(shell_quote)
    .collect::<Vec<_>>()
    .join(" ")
}

fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_@%+=:,./-".contains(&byte))
    {
        return value.into();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn contains_handler(group: &Value, provider: Provider, event: &str, executable: &Path) -> bool {
    let Some(group) = group.as_object() else {
        return false;
    };
    let matcher = group.get("matcher").and_then(Value::as_str);
    match event_matcher(provider, event) {
        Some(expected) if matcher != Some(expected) => return false,
        None if matcher.is_some_and(|value| !value.is_empty() && value != "*") => return false,
        _ => {}
    }
    group
        .get("hooks")
        .and_then(Value::as_array)
        .is_some_and(|handlers| {
            handlers.iter().any(|handler| {
                handler.get("type").and_then(Value::as_str) == Some("command")
                    && handler.get("command").and_then(Value::as_str)
                        == Some(handler_command(executable, provider).as_str())
                    && handler.get("timeout").and_then(Value::as_u64)
                        == Some(handler_timeout(event))
            })
        })
}

fn is_pika_group(group: &Value, provider: Provider) -> bool {
    group
        .get("hooks")
        .and_then(Value::as_array)
        .is_some_and(|handlers| {
            handlers.iter().any(|handler| {
                let command = handler
                    .get("command")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let Ok(argv) = shell_words::split(command) else {
                    return false;
                };
                let native = argv.first().is_some_and(|program| {
                    Path::new(program)
                        .file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name == "pika" || name == "pikamux")
                });
                let python_module = argv.windows(2).any(|pair| pair == ["-m", "pikamux"]);
                let provider_flag = argv
                    .windows(2)
                    .any(|pair| pair == ["--provider", provider.as_str()]);
                (native || python_module) && argv.iter().any(|item| item == "hook") && provider_flag
            })
        })
}

fn parse_object(path: &Path, content: &str) -> Result<Value> {
    let value: Value = serde_json::from_str(content)
        .with_context(|| format!("cannot safely merge invalid JSON at {}", path.display()))?;
    if !value.is_object() {
        bail!("cannot safely merge non-object JSON at {}", path.display());
    }
    Ok(value)
}

fn read_optional_text(path: &Path) -> Result<String> {
    match fs::read_to_string(path) {
        Ok(value) => Ok(value),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(error).with_context(|| format!("cannot read {}", path.display())),
    }
}

fn pretty_object_preserving_top_order(before: &str, object: &Map<String, Value>) -> Result<String> {
    let mut keys = top_level_key_order(before);
    for key in object.keys() {
        if !keys.contains(key) {
            keys.push(key.clone());
        }
    }
    keys.retain(|key| object.contains_key(key));
    let mut output = String::from("{\n");
    for (index, key) in keys.iter().enumerate() {
        let encoded_key = serde_json::to_string(key)?;
        let encoded_value = serde_json::to_string_pretty(&object[key])?;
        let indented = encoded_value.replace('\n', "\n  ");
        output.push_str(&format!("  {encoded_key}: {indented}"));
        if index + 1 != keys.len() {
            output.push(',');
        }
        output.push('\n');
    }
    output.push_str("}\n");
    Ok(output)
}

fn top_level_key_order(content: &str) -> Vec<String> {
    let bytes = content.as_bytes();
    let mut keys = Vec::new();
    let (mut index, mut depth) = (0, 0_i32);
    let mut in_string = false;
    let mut escaped = false;
    while index < bytes.len() {
        let byte = bytes[index];
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            index += 1;
            continue;
        }
        match byte {
            b'{' | b'[' => depth += 1,
            b'}' | b']' => depth -= 1,
            b'"' if depth == 1 => {
                let start = index;
                index += 1;
                let mut local_escape = false;
                while index < bytes.len() {
                    if local_escape {
                        local_escape = false;
                    } else if bytes[index] == b'\\' {
                        local_escape = true;
                    } else if bytes[index] == b'"' {
                        break;
                    }
                    index += 1;
                }
                if index < bytes.len() {
                    let token = &content[start..=index];
                    let mut probe = index + 1;
                    while probe < bytes.len() && bytes[probe].is_ascii_whitespace() {
                        probe += 1;
                    }
                    if bytes.get(probe) == Some(&b':')
                        && let Ok(key) = serde_json::from_str::<String>(token)
                    {
                        keys.push(key);
                    }
                }
            }
            b'"' => in_string = true,
            _ => {}
        }
        index += 1;
    }
    keys
}

fn reject_symlink_target(path: &Path) -> Result<()> {
    for candidate in [
        Some(path),
        path.parent(),
        path.parent().and_then(Path::parent),
    ]
    .into_iter()
    .flatten()
    {
        match fs::symlink_metadata(candidate) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                bail!(
                    "setup target crosses externally managed symlink {}; run Pika setup again",
                    candidate.display()
                )
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn validate_stamp(stamp: &str) -> Result<()> {
    if stamp.is_empty()
        || !stamp
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        bail!("invalid backup stamp");
    }
    Ok(())
}

fn validate_executable(executable: &Path) -> Result<()> {
    if !executable.is_absolute() {
        bail!("Pika hook executable must be an absolute path");
    }
    if executable.to_string_lossy().chars().any(char::is_control) {
        bail!("Pika hook executable path contains a control character");
    }
    Ok(())
}
