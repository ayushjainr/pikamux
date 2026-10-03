//! Pika-only workspace for the actual native Codex TUI. Provider state stays in place.
use anyhow::{Context, Result, bail};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use std::{
    collections::BTreeMap,
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};
use toml_edit::{DocumentMut, Item, Table, value};

pub(crate) struct LaunchProfile {
    pub cwd: PathBuf,
    pub environment: BTreeMap<String, String>,
    pub argv: Vec<String>,
}

/// Prove the selected native runtime honors the final scoped configuration.
/// This renders synthetic prompt input only: no thread, transcript or model call.
pub(crate) fn validate_provider(selected_provider: &Path, profile: &LaunchProfile) -> Result<()> {
    validate_provider_bounded(
        selected_provider,
        profile,
        Duration::from_secs(10),
        1024 * 1024,
    )
}

fn validate_provider_bounded(
    provider: &Path,
    profile: &LaunchProfile,
    timeout: Duration,
    limit: u64,
) -> Result<()> {
    let provider = provider
        .canonicalize()
        .context("Native provider executable is unavailable")?;
    let mut child = spawn_capability_probe(&provider, profile)?;
    let stdout = child
        .stdout
        .take()
        .context("Missing native capability output")?;
    let stderr = child
        .stderr
        .take()
        .context("Missing native capability diagnostics")?;
    let output = capture_stream(stdout, limit);
    let diagnostics = capture_stream(stderr, limit);
    let start = Instant::now();
    let status = wait_capability_probe(&mut child, start, timeout)?;
    wait_captures(&child, &output, &diagnostics, start, timeout)?;
    let output = output
        .join()
        .map_err(|_| anyhow::anyhow!("Native capability output failed"))??;
    let diagnostics = diagnostics
        .join()
        .map_err(|_| anyhow::anyhow!("Native capability diagnostics failed"))??;
    if output.len() as u64 > limit || diagnostics.len() as u64 > limit {
        bail!("Native provider capability output exceeded its bound; no conversation was launched");
    }
    if !status.success() {
        bail!(
            "Native provider does not support the scoped capability check; no conversation was launched"
        );
    }
    validate_capabilities(&output, &provider, profile)
}

fn spawn_capability_probe(provider: &Path, profile: &LaunchProfile) -> Result<std::process::Child> {
    let mut command = Command::new(provider);
    command
        .args(&profile.argv)
        .args([
            "debug",
            "prompt-input",
            "Pika native sandbox capability check; no model turn.",
        ])
        .current_dir(&profile.cwd)
        .envs(&profile.environment)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    command
        .spawn()
        .context("Native provider capability check could not start")
}

type Capture = std::thread::JoinHandle<std::io::Result<Vec<u8>>>;
fn capture_stream(stream: impl Read + Send + 'static, limit: u64) -> Capture {
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        stream
            .take(limit + 1)
            .read_to_end(&mut bytes)
            .map(|_| bytes)
    })
}

fn kill_probe_group(child: &std::process::Child) {
    #[cfg(unix)]
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
}

fn wait_capability_probe(
    child: &mut std::process::Child,
    start: Instant,
    timeout: Duration,
) -> Result<std::process::ExitStatus> {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) if start.elapsed() < timeout => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                kill_probe_group(child);
                let _ = child.kill();
                let _ = child.wait();
                bail!("Native provider capability check timed out; no conversation was launched");
            }
        }
    }
}

fn wait_captures(
    child: &std::process::Child,
    output: &Capture,
    diagnostics: &Capture,
    start: Instant,
    timeout: Duration,
) -> Result<()> {
    while (!output.is_finished() || !diagnostics.is_finished()) && start.elapsed() < timeout {
        std::thread::sleep(Duration::from_millis(2));
    }
    // Do not wait on pipes inherited by a misbehaving provider's descendants.
    if !output.is_finished() || !diagnostics.is_finished() {
        kill_probe_group(child);
        bail!(
            "Native provider capability check did not close its bounded output; no conversation was launched"
        );
    }
    Ok(())
}

fn validate_capabilities(output: &[u8], provider: &Path, profile: &LaunchProfile) -> Result<()> {
    let messages: serde_json::Value = serde_json::from_slice(output)
        .context("Native capability response is not structured prompt input")?;
    let messages = messages
        .as_array()
        .context("Native capability response is not prompt input")?;
    let context = capability_context(messages)?;
    let filesystem = capability_filesystem(context)?;
    validate_filesystem_capabilities(filesystem, provider, profile)?;
    validate_network_capabilities(messages)
}

fn role_texts<'a>(
    messages: &'a [serde_json::Value],
    role: &'a str,
) -> impl Iterator<Item = &'a str> {
    messages
        .iter()
        .filter(move |message| message["role"] == role)
        .flat_map(|message| message["content"].as_array().into_iter().flatten())
        .filter_map(|content| content["text"].as_str())
}

fn capability_context(messages: &[serde_json::Value]) -> Result<&str> {
    let texts: Vec<&str> = role_texts(messages, "user")
        .filter(|text| text.starts_with("<environment_context>"))
        .collect();
    if texts.len() != 1 {
        bail!("Native provider did not report one exact environment capability context");
    }
    Ok(texts[0])
}

fn capability_filesystem(context: &str) -> Result<&str> {
    if !context.contains("<permission_profile type=\"managed\">")
        || !context.contains("<file_system type=\"restricted\">")
    {
        bail!("Native provider did not enforce the scoped permission profile");
    }
    context
        .split("<file_system type=\"restricted\">")
        .nth(1)
        .and_then(|text| text.split_once("</file_system>").map(|(body, _)| body))
        .context("Native filesystem capability is incomplete")
}

struct RuntimeReadScope<'a> {
    provider: &'a Path,
    workspace: &'a Path,
    canonical_workspace: PathBuf,
    home: PathBuf,
    bundled_shell: PathBuf,
}

impl RuntimeReadScope<'_> {
    fn allows_workspace(&self, path: &Path) -> bool {
        [self.workspace, self.canonical_workspace.as_path()].contains(&path)
    }
    fn allows_read(&self, path: &Path) -> bool {
        [
            self.provider,
            self.bundled_shell.as_path(),
            self.workspace,
            self.canonical_workspace.as_path(),
        ]
        .contains(&path)
            || path.starts_with(self.home.join("tmp/arg0"))
    }
}

#[derive(Default)]
struct FilesystemRequirements {
    root_denied: bool,
    temp_denied: bool,
    slash_temp_denied: bool,
    workspace_writable: bool,
}

fn validate_filesystem_capabilities(
    filesystem: &str,
    provider: &Path,
    profile: &LaunchProfile,
) -> Result<()> {
    let cwd = profile.cwd.canonicalize()?;
    let home = PathBuf::from(
        profile
            .environment
            .get("CODEX_HOME")
            .context("Native profile has no provider home")?,
    )
    .canonicalize()?;
    let bundled_shell = provider
        .parent()
        .and_then(Path::parent)
        .context("Native provider has no runtime parent")?
        .join("codex-resources/zsh/bin/zsh");
    let scope = RuntimeReadScope {
        provider,
        workspace: &profile.cwd,
        canonical_workspace: cwd,
        home,
        bundled_shell,
    };
    let mut required = FilesystemRequirements::default();
    for entry in filesystem.split("<entry ").skip(1) {
        observe_filesystem_entry(entry, &scope, &mut required)?;
    }
    if !required.root_denied
        || !required.temp_denied
        || !required.slash_temp_denied
        || !required.workspace_writable
    {
        bail!(
            "Native provider did not enforce exact workspace, root/temp, network and approval restrictions"
        );
    }
    Ok(())
}

fn observe_filesystem_entry(
    entry: &str,
    scope: &RuntimeReadScope<'_>,
    required: &mut FilesystemRequirements,
) -> Result<()> {
    let (attributes, body) = entry
        .split_once('>')
        .context("Native permission entry is malformed")?;
    let (body, _) = body
        .split_once("</entry>")
        .context("Native permission entry is incomplete")?;
    if attributes.contains("access=\"deny\"") && attributes.contains("escalatable=\"false\"") {
        required.root_denied |= body == "<special>:root</special>";
        required.temp_denied |= body == "<special>:tmpdir</special>";
        required.slash_temp_denied |= body == "<special>:slash_tmp</special>";
    } else if attributes.contains("access=\"write\"") {
        let path = permission_path(body)?;
        if !scope.allows_workspace(&path) {
            bail!("Native provider allows writes outside the private assistant workspace");
        }
        required.workspace_writable = true;
    } else if attributes.contains("access=\"read\"") {
        if body == "<special>:minimal</special>" {
            return Ok(());
        }
        let path = permission_path(body)?;
        if !scope.allows_read(&path) {
            bail!(
                "Native provider permits an unrelated filesystem read outside its scoped runtime"
            );
        }
    } else {
        bail!("Native provider returned an unsupported filesystem permission");
    }
    Ok(())
}

fn validate_network_capabilities(messages: &[serde_json::Value]) -> Result<()> {
    let permissions = role_texts(messages, "developer")
        .find(|text| text.starts_with("<permissions instructions>"))
        .context("Native provider omitted its effective network/approval capability")?;
    if !permissions.contains("Network access is restricted.")
        || !permissions.contains("Approval policy is currently never.")
    {
        bail!(
            "Native provider did not enforce exact workspace, root/temp, network and approval restrictions"
        );
    }
    Ok(())
}

fn permission_path(body: &str) -> Result<PathBuf> {
    let path = body
        .strip_prefix("<path>")
        .and_then(|text| text.strip_suffix("</path>"))
        .context("Native provider reported an unsupported filesystem path capability")?;
    Ok(PathBuf::from(
        path.replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&apos;", "'")
            .replace("&amp;", "&"),
    ))
}

const SKILLS: &[(&str, &str)] = &[
    (
        "pika-control",
        include_str!("../pika-native-skills/pika-control/SKILL.md"),
    ),
    (
        "pika-self-awareness",
        include_str!("../pika-skills/self-awareness/SKILL.md"),
    ),
    (
        "pika-memory",
        include_str!("../pika-native-skills/pika-memory/SKILL.md"),
    ),
    (
        "pika-reflection",
        include_str!("../pika-skills/reflection/SKILL.md"),
    ),
    (
        "pika-consolidation",
        include_str!("../pika-skills/consolidation/SKILL.md"),
    ),
    (
        "pika-user-feedback",
        include_str!("../pika-native-skills/pika-user-feedback/SKILL.md"),
    ),
    (
        "pika-small-council-grasp",
        include_str!("../pika-native-skills/pika-small-council-grasp/SKILL.md"),
    ),
];

pub(crate) fn prepare(
    root: &Path,
    profile_id: &str,
    scope: &str,
    executable: &Path,
    selected_provider: &Path,
) -> Result<LaunchProfile> {
    crate::assistant_host::verify_existing_profile(root, profile_id)?;
    crate::assistant::scope(scope)?;
    if !executable.is_absolute() {
        bail!("Native Pika tools need an absolute executable");
    }
    let root_text = root
        .to_str()
        .context("Assistant profile path is not UTF-8")?;
    let executable_text = executable
        .to_str()
        .context("Assistant executable is not UTF-8")?;
    let cwd = install_workspace(root)?;
    let arguments = vec![
        "--profile-root".to_owned(),
        root_text.to_owned(),
        "--expected-profile-id".to_owned(),
        profile_id.to_owned(),
        "--scope".to_owned(),
        scope.to_owned(),
    ];
    let provider_home = root.join("provider-home");
    // No credential reads, copies, symlinks, or replacement provider state.
    crate::assistant_storage::directory(&provider_home)?;
    let provider = selected_provider
        .canonicalize()
        .context("Native provider executable is unavailable")?;
    let provider_text = provider
        .to_str()
        .context("Native provider executable is not UTF-8")?;
    let permissions = scoped_permissions(provider_text);
    configure(&provider_home, executable_text, &arguments, &permissions)?;
    let environment = BTreeMap::from([
        (
            "CODEX_HOME".into(),
            provider_home
                .to_str()
                .context("Provider home is not UTF-8")?
                .into(),
        ),
        ("PIKA_ASSISTANT_PROFILE_ROOT".into(), root_text.into()),
        ("PIKA_ASSISTANT_PROFILE_ID".into(), profile_id.into()),
        ("PIKA_ASSISTANT_SCOPE".into(), scope.into()),
    ]);
    Ok(LaunchProfile {
        argv: vec![
            "--profile".into(),
            "pika-assistant".into(),
            "--ask-for-approval".into(),
            "never".into(),
            "--config".into(),
            "default_permissions=\"pika-source-scoped\"".into(),
            "--config".into(),
            format!("permissions.pika-source-scoped={permissions}"),
            // Native presentation only: never expose the internal profile
            // directory in the assistant footer or terminal window title.
            "--config".into(),
            "tui.status_line=[\"model-with-reasoning\",\"context-remaining\"]".into(),
            "--config".into(),
            "tui.terminal_title=[\"app-name\"]".into(),
            "--model".into(),
            "gpt-6-luna".into(),
            "--cd".into(),
            cwd.to_str()
                .context("Native assistant workspace is not UTF-8")?
                .into(),
        ],
        cwd,
        environment,
    })
}

fn install_workspace(root: &Path) -> Result<PathBuf> {
    let cwd = root.join("native-assistant");
    crate::assistant_storage::directory(&cwd)?;
    install(
        &cwd.join("AGENTS.md"),
        include_str!("../pika-native-skills/AGENTS.md"),
    )?;
    for (name, body) in SKILLS {
        install(
            &cwd.join(".agents/skills").join(name).join("SKILL.md"),
            body,
        )?;
    }
    Ok(cwd)
}

fn scoped_permissions(provider: &str) -> toml_edit::InlineTable {
    let mut filesystem = toml_edit::InlineTable::new();
    for (path, access) in [
        (":root", "deny"),
        (":minimal", "read"),
        (":tmpdir", "deny"),
        (":slash_tmp", "deny"),
        (provider, "read"),
    ] {
        filesystem.insert(path, access.into());
    }
    let mut workspace = toml_edit::InlineTable::new();
    workspace.insert(".", "write".into());
    filesystem.insert(":workspace_roots", workspace.into());
    let mut network = toml_edit::InlineTable::new();
    network.insert("enabled", false.into());
    network.insert("dangerously_allow_all_unix_sockets", false.into());
    let mut permissions = toml_edit::InlineTable::new();
    permissions.insert("extends", ":workspace".into());
    permissions.insert("filesystem", filesystem.into());
    permissions.insert("network", network.into());
    permissions
}

fn install(path: &Path, text: &str) -> Result<()> {
    crate::assistant_storage::directory(path.parent().context("Missing private parent")?)?;
    crate::assistant_storage::file(path)?;
    if fs::read(path)? == text.as_bytes() {
        return Ok(());
    }
    let parent = path.parent().context("Missing private parent")?;
    let temporary = parent.join(format!(".pika-native-{}", uuid::Uuid::new_v4()));
    crate::assistant_storage::file(&temporary)?;
    let result = install_atomically(path, &temporary, text);
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result?;
    Ok(())
}

fn install_atomically(path: &Path, temporary: &Path, text: &str) -> Result<()> {
    let mut file = fs::OpenOptions::new().write(true).open(temporary)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    fs::rename(temporary, path)?;
    fs::File::open(path.parent().context("Missing private parent")?)?.sync_all()?;
    Ok(())
}

fn configure(
    provider_home: &Path,
    executable: &str,
    arguments: &[String],
    permissions: &toml_edit::InlineTable,
) -> Result<()> {
    let config_path = provider_home.join("pika-assistant.config.toml");
    crate::assistant_storage::directory(config_path.parent().unwrap())?;
    let mut config = load_native_config(&config_path)?;
    let mut server = Table::new();
    server["command"] = value(executable);
    let mut args = toml_edit::Array::new();
    args.push("_assistant-native-tools");
    for argument in arguments {
        args.push(argument.as_str());
    }
    server["args"] = value(args);
    server["enabled"] = value(true);
    server["required"] = value(true);
    // The private Pika adapter enforces scope, sources and explicit authority
    // itself. A second provider approval gate would reject even ordinary recall
    // under our noninteractive policy. This applies to Pika, not other servers.
    server["default_tools_approval_mode"] = value("approve");
    // Codex filters inherited environment for stdio MCP children. Forward
    // only the exact native admission generation and board/registry paths;
    // otherwise the adapter exits before advertising its tools.
    let mut environment = toml_edit::Array::new();
    for name in [
        "PIKA_ASSISTANT_NATIVE_LAUNCH_TOKEN",
        "PIKA_ASSISTANT_BOARD_DB_PATH",
        "PIKA_DB_PATH",
        "PIKA_STATE_HOME",
        "PIKA_CONFIG_HOME",
    ] {
        environment.push(name);
    }
    server["env_vars"] = value(environment);
    config["mcp_servers"]["pika"] = Item::Table(server);
    config.remove("sandbox_mode");
    config["approval_policy"] = value("never");
    config["default_permissions"] = value("pika-source-scoped");
    config["permissions"]["pika-source-scoped"] = value(permissions.clone());
    install(&config_path, &config.to_string())?;
    configure_hooks(provider_home, executable, arguments)
}

fn load_native_config(path: &Path) -> Result<DocumentMut> {
    if !path.try_exists()? {
        return Ok(DocumentMut::new());
    }
    crate::assistant_storage::existing_file(path)?;
    Ok(fs::read_to_string(path)?.parse()?)
}

fn configure_hooks(provider_home: &Path, executable: &str, arguments: &[String]) -> Result<()> {
    let hook_path = provider_home.join("hooks.json");
    let mut hooks = load_hook_config(&hook_path)?;
    let events = hooks
        .get_mut("hooks")
        .and_then(serde_json::Value::as_object_mut)
        .context("Invalid native hook configuration")?;
    let command = shell_words::join(
        std::iter::once(executable)
            .chain(std::iter::once("_assistant-native-hook"))
            .chain(arguments.iter().map(String::as_str)),
    );
    for event in [
        "SessionStart",
        "UserPromptSubmit",
        "PreCompact",
        "PostCompact",
        "Stop",
        "Interrupt",
    ] {
        let entries = events
            .entry(event)
            .or_insert_with(|| serde_json::json!([]))
            .as_array_mut()
            .context("Invalid native hook event")?;
        retain_unrelated_handlers(entries);
        let event_command = format!("{command} --event {}", shell_words::quote(event));
        entries.push(
            serde_json::json!({"hooks": [{"type": "command", "command": event_command, "timeout": if event == "Interrupt" { 3 } else { 15 }}]}),
        );
    }
    install(&hook_path, &serde_json::to_string_pretty(&hooks)?)?;
    Ok(())
}

fn load_hook_config(path: &Path) -> Result<serde_json::Value> {
    if !path.try_exists()? {
        return Ok(serde_json::json!({"hooks": {}}));
    }
    crate::assistant_storage::existing_file(path)?;
    Ok(serde_json::from_str(&fs::read_to_string(path)?)?)
}

fn retain_unrelated_handlers(entries: &mut Vec<serde_json::Value>) {
    for entry in entries.iter_mut() {
        if let Some(handlers) = entry
            .get_mut("hooks")
            .and_then(serde_json::Value::as_array_mut)
        {
            handlers.retain(|handler| {
                !handler
                    .get("command")
                    .and_then(serde_json::Value::as_str)
                    .and_then(|text| shell_words::split(text).ok())
                    .is_some_and(|argv| {
                        argv.get(1)
                            .is_some_and(|arg| arg == "_assistant-native-hook")
                    })
            });
        }
    }
    entries.retain(|entry| {
        !entry
            .get("hooks")
            .and_then(serde_json::Value::as_array)
            .is_some_and(Vec::is_empty)
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (tempfile::TempDir, PathBuf, String) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("private");
        let memory = crate::assistant_memory::Store::open(root.join("memory.sqlite")).unwrap();
        let id = memory.profile_id().to_owned();
        (temp, root, id)
    }

    #[cfg(unix)]
    #[test]
    fn unsupported_error_timeout_and_oversized_provider_never_fall_back() {
        use std::os::unix::fs::PermissionsExt;
        for (script, expected) in [
            ("printf '[]'", "exact environment"),
            ("exit 9", "does not support"),
            ("exec /bin/sleep 5", "timed out"),
            ("/bin/sleep 5 & exit 0", "did not close"),
            ("/usr/bin/head -c 2048 /dev/zero", "exceeded"),
        ] {
            let (_temp, root, id) = fixture();
            let provider = root.join("fake-codex");
            install(&provider, &format!("#!/bin/sh\n{script}\n")).unwrap();
            fs::set_permissions(&provider, fs::Permissions::from_mode(0o700)).unwrap();
            let profile =
                prepare(&root, &id, "personal", Path::new("/tmp/pika"), &provider).unwrap();
            let error =
                validate_provider_bounded(&provider, &profile, Duration::from_millis(200), 1024)
                    .unwrap_err();
            assert!(error.to_string().contains(expected), "{error}");
            assert!(!root.join("native-binding.json").exists());
            assert!(!root.join("native-registry").exists());
        }
    }
    #[test]
    fn profile_is_isolated_and_preserves_provider_config_and_credentials() {
        let (temp, root, id) = fixture();
        install(&root.join("provider-home/config.toml"), "model = 'old'\n").unwrap();
        install(
            &root.join("provider-home/auth.json"),
            "synthetic-not-a-credential",
        )
        .unwrap();
        let launch = prepare(
            &root,
            &id,
            "personal",
            Path::new("/tmp/pika binary"),
            Path::new("/usr/bin/false"),
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(root.join("provider-home/config.toml")).unwrap(),
            "model = 'old'\n"
        );
        assert_eq!(
            fs::read_to_string(root.join("provider-home/auth.json")).unwrap(),
            "synthetic-not-a-credential"
        );
        assert!(launch.argv.contains(&"gpt-6-luna".to_owned()));
        assert!(!launch.argv.contains(&"--sandbox".to_owned()));
        assert!(!launch.argv.iter().any(|arg| arg.contains("bypass")));
        assert!(
            launch
                .argv
                .windows(2)
                .any(|pair| pair == ["--ask-for-approval", "never"])
        );
        assert!(
            launch
                .argv
                .contains(&"default_permissions=\"pika-source-scoped\"".to_owned())
        );
        let pinned = launch
            .argv
            .iter()
            .find(|arg| arg.starts_with("permissions.pika-source-scoped="))
            .unwrap();
        assert!(pinned.contains("\":root\" = \"deny\""));
        assert!(pinned.contains("\":tmpdir\" = \"deny\""));
        assert_eq!(
            launch.environment["CODEX_HOME"],
            root.join("provider-home").to_str().unwrap()
        );
        assert_eq!(
            fs::read_dir(launch.cwd.join(".agents/skills"))
                .unwrap()
                .count(),
            7
        );
        assert!(!root.join("provider-home/skills").exists());
        assert!(!temp.path().join(".agents").exists());
        let config: DocumentMut =
            fs::read_to_string(root.join("provider-home/pika-assistant.config.toml"))
                .unwrap()
                .parse()
                .unwrap();
        let server = &config["mcp_servers"]["pika"];
        assert_eq!(server["required"].as_bool(), Some(true));
        assert_eq!(
            server["default_tools_approval_mode"].as_str(),
            Some("approve")
        );
        assert_eq!(
            server["env_vars"]
                .as_array()
                .unwrap()
                .iter()
                .map(|item| item.as_str().unwrap())
                .collect::<Vec<_>>(),
            [
                "PIKA_ASSISTANT_NATIVE_LAUNCH_TOKEN",
                "PIKA_ASSISTANT_BOARD_DB_PATH",
                "PIKA_DB_PATH",
                "PIKA_STATE_HOME",
                "PIKA_CONFIG_HOME"
            ]
        );
    }
    #[test]
    fn native_footer_overrides_exclude_paths_without_changing_private_settings() {
        let (_temp, root, id) = fixture();
        let base = "[tui]\ntheme='existing-custom-theme'\nstatus_line=['current-dir','project-root']\nterminal_title=['project','thread']\n";
        let private = "[tui]\ntheme='private-custom-theme'\nstatus_line=['current-dir']\nterminal_title=['project']\n";
        install(&root.join("provider-home/config.toml"), base).unwrap();
        install(
            &root.join("provider-home/pika-assistant.config.toml"),
            private,
        )
        .unwrap();
        install(
            &root.join("provider-home/auth.json"),
            "synthetic-auth-bytes",
        )
        .unwrap();
        let launch = prepare(
            &root,
            &id,
            "personal",
            Path::new("/usr/bin/false"),
            Path::new("/usr/bin/false"),
        )
        .unwrap();
        let mut overrides = DocumentMut::new();
        for pair in launch.argv.windows(2).filter(|pair| pair[0] == "--config") {
            let document: DocumentMut = pair[1].parse().unwrap();
            if let Some(tui) = document.get("tui") {
                for (key, value) in tui.as_table().unwrap().iter() {
                    overrides["tui"][key] = value.clone();
                }
            }
        }
        assert_eq!(
            overrides["tui"]["status_line"]
                .as_array()
                .unwrap()
                .iter()
                .map(|item| item.as_str().unwrap())
                .collect::<Vec<_>>(),
            ["model-with-reasoning", "context-remaining"]
        );
        assert_eq!(
            overrides["tui"]["terminal_title"]
                .as_array()
                .unwrap()
                .iter()
                .map(|item| item.as_str().unwrap())
                .collect::<Vec<_>>(),
            ["app-name"]
        );
        assert_eq!(
            fs::read_to_string(root.join("provider-home/config.toml")).unwrap(),
            base
        );
        assert_eq!(
            fs::read_to_string(root.join("provider-home/auth.json")).unwrap(),
            "synthetic-auth-bytes"
        );
        let preserved: DocumentMut =
            fs::read_to_string(root.join("provider-home/pika-assistant.config.toml"))
                .unwrap()
                .parse()
                .unwrap();
        assert_eq!(
            preserved["tui"]["theme"].as_str(),
            Some("private-custom-theme")
        );
        assert_eq!(
            preserved["tui"]["status_line"][0].as_str(),
            Some("current-dir")
        );
    }
    #[test]
    fn mismatched_identity_creates_nothing() {
        let (_temp, root, _id) = fixture();
        assert!(
            prepare(
                &root,
                &uuid::Uuid::new_v4().to_string(),
                "personal",
                Path::new("/tmp/pika"),
                Path::new("/usr/bin/false")
            )
            .is_err()
        );
        assert!(!root.join("native-assistant").exists());
    }
    #[test]
    fn existing_native_configuration_is_preserved_and_hook_argv_is_quoted() {
        let (_temp, root, id) = fixture();
        let cwd = root.join("provider-home");
        install(
            &cwd.join("pika-assistant.config.toml"),
            "# keep\nmodel = 'custom'\n[mcp_servers.other]\ncommand = 'other'\n",
        )
        .unwrap();
        install(&cwd.join("hooks.json"), r#"{"hooks":{"UserPromptSubmit":[{"hooks":[{"type":"command","command":"unrelated"}]}]}}"#).unwrap();
        let executable = Path::new("/tmp/a 'quote' $(touch no) pika");
        let provider = root.join("a 'quote' $(touch no) provider");
        install(&provider, "synthetic provider, never executed").unwrap();
        prepare(&root, &id, "personal", executable, &provider).unwrap();
        prepare(&root, &id, "personal", executable, &provider).unwrap();
        let config = fs::read_to_string(cwd.join("pika-assistant.config.toml"))
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        assert_eq!(config["model"].as_str(), Some("custom"));
        assert_eq!(
            config["mcp_servers"]["other"]["command"].as_str(),
            Some("other")
        );
        let hooks: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(cwd.join("hooks.json")).unwrap()).unwrap();
        let entries = hooks["hooks"]["UserPromptSubmit"].as_array().unwrap();
        assert_eq!(entries.len(), 2);
        let argv = shell_words::split(entries[1]["hooks"][0]["command"].as_str().unwrap()).unwrap();
        assert_eq!(argv[0], executable.to_str().unwrap());
        assert_eq!(argv[1], "_assistant-native-hook");
        assert_eq!(argv[3], root.to_str().unwrap());
        assert_eq!(argv[5], id);
        assert_eq!(argv[7], "personal");
        assert_eq!(argv[8], "--event");
        assert_eq!(argv[9], "UserPromptSubmit");
        assert_eq!(config["approval_policy"].as_str(), Some("never"));
        assert_eq!(
            config["permissions"]["pika-source-scoped"]["filesystem"]
                [provider.canonicalize().unwrap().to_str().unwrap()]
            .as_str(),
            Some("read")
        );
        assert_eq!(
            config["permissions"]["pika-source-scoped"]["filesystem"][":root"].as_str(),
            Some("deny")
        );
        for event in [
            "SessionStart",
            "UserPromptSubmit",
            "PreCompact",
            "PostCompact",
            "Stop",
            "Interrupt",
        ] {
            let handler = hooks["hooks"][event].as_array().unwrap().last().unwrap();
            let argv =
                shell_words::split(handler["hooks"][0]["command"].as_str().unwrap()).unwrap();
            assert_eq!(argv[8], "--event");
            assert_eq!(argv[9], event);
        }
    }

    /// Explicit no-model diagnostic against a locally selected native Codex.
    /// Never creates a thread, reads a real credential, or dispatches a model turn.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "set PIKA_NATIVE_CODEX to run the native protocol-only sandbox contract"]
    fn configured_native_sandbox_denies_temporary_authority_and_socket_access() {
        use std::{
            io::{BufRead, BufReader},
            os::unix::net::UnixListener,
            process::{Command, Stdio},
        };
        let codex = std::env::var_os("PIKA_NATIVE_CODEX")
            .expect("Explicit native diagnostic executable required");
        let codex = PathBuf::from(codex).canonicalize().unwrap();
        let fixture_parent = std::env::var_os("PIKA_NATIVE_PROBE_PARENT")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/tmp"));
        assert!(fixture_parent.is_absolute());
        let temp = tempfile::Builder::new()
            .prefix("pika-native-")
            .tempdir_in(fixture_parent)
            .unwrap();
        let root = temp.path().join("private");
        let memory = crate::assistant_memory::Store::open(root.join("memory.sqlite")).unwrap();
        let pika = PathBuf::from(
            std::env::var_os("PIKA_NATIVE_PIKA")
                .unwrap_or_else(|| assert_cmd::cargo::cargo_bin("pika").into_os_string()),
        )
        .canonicalize()
        .unwrap();
        let mut launch = prepare(&root, memory.profile_id(), "personal", &pika, &codex).unwrap();
        // Required-server startup must use the real adapter. Catalog loading
        // needs its launch token but performs no authority/tool request.
        launch.environment.insert(
            "PIKA_ASSISTANT_NATIVE_LAUNCH_TOKEN".into(),
            uuid::Uuid::new_v4().to_string(),
        );
        install(
            &root.join("provider-home/config.toml"),
            "sandbox_mode = 'danger-full-access'\napproval_policy = 'on-request'\n[sandbox_workspace_write]\nwritable_roots = ['/tmp']\nnetwork_access = true\n",
        )
        .unwrap();
        install(
            &root.join("provider-home/auth.json"),
            r#"{"auth_mode":"apikey","OPENAI_API_KEY":"synthetic-test-api-key"}"#,
        )
        .unwrap();
        let original_config = fs::read(root.join("provider-home/config.toml")).unwrap();
        let original_auth = fs::read(root.join("provider-home/auth.json")).unwrap();
        let home = temp.path().join("home");
        crate::assistant_storage::directory(&home).unwrap();
        for (key, path) in [
            ("HOME", home.clone()),
            ("XDG_CONFIG_HOME", home.join("config")),
            ("XDG_STATE_HOME", home.join("state")),
            ("XDG_DATA_HOME", home.join("data")),
            ("TMPDIR", root.clone()),
        ] {
            launch
                .environment
                .insert(key.into(), path.to_str().unwrap().into());
        }
        let auth_status = Command::new(&codex)
            // Authentication is host-owned; this diagnostic subcommand does
            // not accept TUI --profile or create a conversation/model call.
            .args(["--config", "cli_auth_credentials_store=\"file\""])
            .args(["login", "status"])
            .current_dir(&launch.cwd)
            .envs(&launch.environment)
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join("config"))
            .env("XDG_STATE_HOME", home.join("state"))
            .env("TMPDIR", &root)
            .env_remove("OPENAI_API_KEY")
            .env_remove("CODEX_API_KEY")
            .output()
            .unwrap();
        assert!(
            auth_status.status.success(),
            "synthetic provider auth must remain available outside tool sandbox: {}",
            String::from_utf8_lossy(&auth_status.stderr)
        );
        install(&launch.cwd.join("work.txt"), "synthetic workspace").unwrap();
        let listener = UnixListener::bind(root.join("view-probe.sock")).unwrap();
        let probe = format!(
            r#"root={root}
typeset -A results
for name path in authority "$root/memory.sqlite" provider "$root/provider-home/config.toml" workspace "$root/native-assistant/work.txt"; do
 if (: >> "$path") 2>/dev/null; then results[$name]=writable; else results[$name]=denied; fi
done
for name path in authority_read "$root/memory.sqlite" provider_read "$root/provider-home/config.toml"; do
 if /bin/cat "$path" >/dev/null 2>&1; then results[$name]=readable; else results[$name]=denied; fi
done
zmodload zsh/net/socket || exit 92
if zsocket "$root/view-probe.sock" 2>/dev/null; then results[socket]=connected; else results[socket]=denied; fi
printf '{{"authority":"%s","provider":"%s","authority_read":"%s","provider_read":"%s","workspace":"%s","socket":"%s"}}\n' $results[authority] $results[provider] $results[authority_read] $results[provider_read] $results[workspace] $results[socket]"#,
            root = shell_words::quote(root.to_str().unwrap())
        );
        // The exact TUI flags are supported by prompt-input; app-server itself
        // does not accept --profile, so replay only its explicit security config.
        validate_provider(&codex, &launch).unwrap();
        let mut execution_args = vec!["-c".to_owned(), "approval_policy=\"never\"".to_owned()];
        for pair in launch.argv.windows(2).filter(|pair| pair[0] == "--config") {
            execution_args.extend([pair[0].clone(), pair[1].clone()]);
        }
        let mut child = Command::new(codex)
            .args(execution_args)
            .arg("app-server")
            .current_dir(&launch.cwd)
            .envs(&launch.environment)
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join("config"))
            .env("XDG_DATA_HOME", home.join("data"))
            .env("XDG_STATE_HOME", home.join("state"))
            .env("TMPDIR", &root)
            .env_remove("OPENAI_API_KEY")
            .env_remove("CODEX_API_KEY")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut input = child.stdin.take().unwrap();
        let mut output = BufReader::new(child.stdout.take().unwrap());
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            writeln!(input, "{}", serde_json::json!({"id":1,"method":"initialize","params":{"clientInfo":{"name":"pika-sandbox-contract","version":"1"},"capabilities":{"experimentalApi":true}}})).unwrap();
            input.flush().unwrap();
            let receive = |output: &mut BufReader<std::process::ChildStdout>, id| loop {
                let mut line = String::new();
                assert!(
                    output.read_line(&mut line).unwrap() > 0,
                    "Native diagnostic exited"
                );
                let value: serde_json::Value = serde_json::from_str(&line).unwrap();
                if value["id"] == id {
                    break value;
                }
            };
            assert!(receive(&mut output, 1).get("error").is_none());
            writeln!(
                input,
                "{}",
                serde_json::json!({"method":"initialized","params":{}})
            )
            .unwrap();
            writeln!(input, "{}", serde_json::json!({"id":2,"method":"command/exec","params":{"command":["/bin/zsh","-f","-c",probe],"cwd":launch.cwd,"timeoutMs":5000}})).unwrap();
            input.flush().unwrap();
            let reply = receive(&mut output, 2);
            assert_eq!(reply["result"]["exitCode"], 0, "{reply}");
            let status: serde_json::Value =
                serde_json::from_str(reply["result"]["stdout"].as_str().unwrap()).unwrap();
            assert_eq!(
                status,
                serde_json::json!({"authority":"denied","provider":"denied","authority_read":"denied","provider_read":"denied","workspace":"writable","socket":"denied"})
            );
        }));
        let _ = child.kill();
        let _ = child.wait();
        drop(listener);
        assert_eq!(
            fs::read(root.join("provider-home/config.toml")).unwrap(),
            original_config
        );
        assert_eq!(
            fs::read(root.join("provider-home/auth.json")).unwrap(),
            original_auth
        );
        if let Err(error) = result {
            std::panic::resume_unwind(error);
        }
    }
}
