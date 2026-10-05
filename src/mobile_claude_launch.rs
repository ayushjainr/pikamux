//! Launch-local experimental Claude channel configuration; no provider files are changed.
use super::{Binding, binding_path, private, publish};
use crate::{core::Pika, model::Provider, process};
use anyhow::{Context, Result, ensure};
use serde_json::{Map, Value, json};
use std::{
    fs,
    io::Read,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::Path,
    process::{Child, Command},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

pub(crate) fn launch(pika: &Pika, token: &str, argv: &[String]) -> Result<i32> {
    let (pending, mut binding, config, _lock) = prepare_launch(pika, token, argv)?;
    // MCP/hook children wait for the one native-generation publication before writing their own fields.
    publish(&pika.paths, &binding)?;
    let interrupts = Interrupts::install()?;
    require_launch_context(pika, &pending)?;
    let mut child = OwnedChild(
        Command::new(&config[0])
            .args(&config[1..])
            .current_dir(&pending.cwd)
            .spawn()?,
        false,
    );
    let generation = process::process_generation(i64::from(child.0.id()))
        .context("Native Claude generation unavailable")?;
    binding.native_pid = generation.pid;
    binding.native_start = generation.start_time;
    publish(&pika.paths, &binding)?;
    eprintln!(
        "Experimental Claude phone connection: continue this conversation only. Native channel consent and tool permissions remain in the terminal; delivery stays unconfirmed until an attested Claude reply arrives."
    );
    await_native(pika, token, &mut child, &interrupts)
}

fn prepare_launch(
    pika: &Pika,
    token: &str,
    argv: &[String],
) -> Result<(crate::store::PendingLaunch, Binding, Vec<String>, fs::File)> {
    let pending = pika
        .store
        .get_pending(token)?
        .context("Reserved Claude launch unavailable")?;
    ensure!(
        pending.provider == Provider::Claude,
        "Wrong provider launch reservation"
    );
    require_launch_context(pika, &pending)?;
    let thread = pending
        .expected_session_id
        .as_deref()
        .context("Exact Claude conversation missing")?;
    let token_id = uuid::Uuid::parse_str(token)?;
    validate_argv(argv, thread)?;
    let path = binding_path(&pika.paths, thread)?;
    let parent = path
        .parent()
        .context("Private Claude launch directory missing")?;
    let lock = reserve_directory(pika, parent, token)?;
    let server_name = format!("pika_{}", token_id.simple());
    let config = configure(argv, Path::new(&pending.cwd), token, &server_name, parent)?;
    let binding = Binding {
        version: 1,
        token: token.into(),
        thread: thread.into(),
        cwd: pending.cwd.clone(),
        native_pid: 0,
        native_start: 0,
        mcp_pid: 0,
        mcp_start: 0,
        endpoint: super::endpoint_path(&pika.paths, token)?
            .to_string_lossy()
            .into_owned(),
        server_name,
        ready: false,
    };
    Ok((pending, binding, config, lock))
}

fn reserve_directory(pika: &Pika, parent: &Path, token: &str) -> Result<fs::File> {
    create_private_directory(parent)?;
    ensure!(
        fs::canonicalize(parent)? == fs::canonicalize(&pika.paths.state_dir)?.join("claude-shared"),
        "Private launch directory alias rejected"
    );
    use fs2::FileExt;
    let path = parent.join(format!("{token}.launch.lock"));
    let lock = private::open_lock(&path)?;
    lock.try_lock_exclusive()
        .context("This experimental native launch already has an owner")?;
    private::validate_lock(&path, &lock)?;
    Ok(lock)
}

fn require_launch_context(pika: &Pika, pending: &crate::store::PendingLaunch) -> Result<()> {
    let tree = require_reserved_pane(pika, pending)?;
    let observation = process::observe();
    let records = observation
        .require_complete("verify an unused Claude launch pane")
        .map_err(anyhow::Error::msg)?;
    ensure!(
        !tree.iter().any(|pid| records
            .get(pid)
            .is_some_and(|p| p.provider() == Some(Provider::Claude))),
        "Reserved pane already has a native Claude owner"
    );
    Ok(())
}

fn require_reserved_pane(pika: &Pika, pending: &crate::store::PendingLaunch) -> Result<Vec<i64>> {
    let current = pika
        .store
        .get_pending(&pending.launch_token)?
        .context("Native launch reservation disappeared")?;
    ensure!(
        current.created_at == pending.created_at
            && current.expected_session_id == pending.expected_session_id
            && current.tmux_pane == pending.tmux_pane,
        "Native launch reservation changed"
    );
    let pane_id = pending
        .tmux_pane
        .as_deref()
        .context("Reserved native pane missing")?;
    let pane = pika
        .tmux
        .list_panes()?
        .into_iter()
        .find(|p| p.pane_id == pane_id)
        .context("Reserved native pane disappeared")?;
    ensure!(
        !pane.dead
            && pane.pika_provider == Some(Provider::Claude)
            && pane.pika_launch_token.as_deref() == Some(&pending.launch_token)
            && pane.pika_session_id == pending.expected_session_id,
        "Reserved native pane identity changed"
    );
    let observation = process::observe();
    let records = observation
        .require_complete("verify the original Claude launch pane")
        .map_err(anyhow::Error::msg)?;
    let tree = process::process_tree(pane.pane_pid, records);
    ensure!(
        tree.contains(&i64::from(std::process::id())),
        "Experimental launcher is outside its reserved native pane"
    );
    Ok(tree)
}

fn create_private_directory(path: &Path) -> Result<()> {
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => (),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (),
        Err(e) => return Err(e.into()),
    }
    private::private_directory(path)
}

fn validate_argv(argv: &[String], thread: &str) -> Result<()> {
    ensure!(!argv.is_empty(), "Native executable missing");
    ensure!(
        uuid::Uuid::parse_str(thread)?.to_string() == thread,
        "Canonical Claude conversation required"
    );
    let mut identity = false;
    for (i, arg) in argv.iter().enumerate().skip(1) {
        let option = arg.split('=').next().unwrap_or(arg);
        ensure!(
            !matches!(
                option,
                "--print"
                    | "-p"
                    | "--bg"
                    | "--background"
                    | "--fork-session"
                    | "--cloud"
                    | "--continue"
                    | "-c"
            ),
            "Experimental phone connection requires an exact foreground native conversation"
        );
        if matches!(arg.as_str(), "--session-id" | "--resume" | "-r") {
            ensure!(
                argv.get(i + 1).is_some_and(|v| v == thread),
                "Native launch targets another conversation"
            );
            identity = true;
        }
        if matches!(option, "--session-id" | "--resume" | "-r") && arg.contains('=') {
            ensure!(
                arg.split_once('=')
                    .is_some_and(|(_, value)| value == thread),
                "Native launch targets another conversation"
            );
            identity = true;
        }
    }
    ensure!(identity, "Native launch has no exact conversation identity");
    Ok(())
}

fn configure(
    argv: &[String],
    cwd: &Path,
    token: &str,
    server: &str,
    parent: &Path,
) -> Result<Vec<String>> {
    let (mut argv, mcp_inputs) = extract_option(argv, "--mcp-config", true)?;
    let (rest, settings_inputs) = extract_option(&argv, "--settings", false)?;
    argv = rest;
    ensure!(
        settings_inputs.len() <= 1,
        "Multiple native settings overrides cannot be safely merged"
    );
    let mut mcp = json!({"mcpServers":{}});
    for input in mcp_inputs {
        merge_mcp(&mut mcp, load_json(&input, cwd)?)?;
    }
    let executable = std::env::current_exe()?;
    let servers = mcp["mcpServers"]
        .as_object_mut()
        .context("MCP servers must be an object")?;
    ensure!(
        !servers.contains_key(server),
        "Experimental channel server name collision"
    );
    servers.insert(
        server.into(),
        json!({"command":executable,"args":["_claude-channel","--launch-token",token]}),
    );
    let mut settings = settings_inputs
        .first()
        .map(|s| load_json(s, cwd))
        .transpose()?
        .unwrap_or_else(|| json!({}));
    append_hooks(&mut settings, &executable, token, server)?;
    let mcp_path = parent.join(format!("{token}.mcp.json"));
    let settings_path = parent.join(format!("{token}.settings.json"));
    private::write_private(&mcp_path, &serde_json::to_vec(&mcp)?)?;
    private::write_private(&settings_path, &serde_json::to_vec(&settings)?)?;
    argv.extend([
        "--mcp-config".into(),
        mcp_path.to_string_lossy().into_owned(),
        "--settings".into(),
        settings_path.to_string_lossy().into_owned(),
    ]);
    add_development_channel(argv, server)
}

fn add_development_channel(argv: Vec<String>, server: &str) -> Result<Vec<String>> {
    let (mut argv, mut development) =
        extract_option(&argv, "--dangerously-load-development-channels", true)?;
    development.push(format!("server:{server}"));
    argv.push("--dangerously-load-development-channels".into());
    argv.extend(development);
    Ok(argv)
}

fn extract_option(argv: &[String], option: &str, many: bool) -> Result<(Vec<String>, Vec<String>)> {
    let mut kept = Vec::new();
    let mut inputs = Vec::new();
    let mut i = 0;
    while i < argv.len() {
        if argv[i] == option {
            i += 1;
            ensure!(
                i < argv.len() && !argv[i].starts_with('-'),
                "Native option {option} is missing its value"
            );
            inputs.push(argv[i].clone());
            i += 1;
            while many && i < argv.len() && !argv[i].starts_with('-') {
                inputs.push(argv[i].clone());
                i += 1;
            }
        } else if let Some(value) = argv[i].strip_prefix(&format!("{option}=")) {
            ensure!(
                !value.is_empty(),
                "Native option {option} is missing its value"
            );
            inputs.push(value.into());
            i += 1;
        } else {
            kept.push(argv[i].clone());
            i += 1;
        }
    }
    Ok((kept, inputs))
}

fn load_json(input: &str, cwd: &Path) -> Result<Value> {
    let bytes = if input.trim_start().starts_with('{') {
        ensure!(
            input.len() <= 8192,
            "Native inline configuration exceeds limit"
        );
        input.as_bytes().to_vec()
    } else {
        let path = cwd.join(input);
        let file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&path)?;
        ensure!(
            file.metadata()?.is_file(),
            "Native configuration must be a regular file"
        );
        let mut bytes = Vec::new();
        file.take(8193).read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() <= 8192,
            "Native launch configuration exceeds limit"
        );
        bytes
    };
    let value: Value = serde_json::from_slice(&bytes)?;
    ensure!(value.is_object(), "Native configuration must be an object");
    Ok(value)
}

fn merge_mcp(target: &mut Value, source: Value) -> Result<()> {
    let input = source
        .as_object()
        .context("MCP configuration must be an object")?;
    ensure!(
        input.keys().all(|k| k == "mcpServers"),
        "Unsupported MCP configuration fields"
    );
    let servers = input
        .get("mcpServers")
        .and_then(Value::as_object)
        .context("MCP servers must be an object")?;
    let output = target["mcpServers"]
        .as_object_mut()
        .context("MCP servers must be an object")?;
    for (name, value) in servers {
        ensure!(!output.contains_key(name), "MCP server collision: {name}");
        output.insert(name.clone(), value.clone());
    }
    Ok(())
}

fn append_hooks(settings: &mut Value, executable: &Path, token: &str, server: &str) -> Result<()> {
    ensure!(
        settings.get("disableAllHooks") != Some(&Value::Bool(true)),
        "Native hooks are disabled; experimental mobile connection unavailable"
    );
    let object = settings
        .as_object_mut()
        .context("Native settings must be an object")?;
    let hooks = object
        .entry("hooks")
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .context("Native hooks must be an object")?;
    let command = |name: &str| {
        format!(
            "{} {} --launch-token {}",
            shell_words::quote(&executable.to_string_lossy()),
            name,
            shell_words::quote(token)
        )
    };
    for (event, matcher, helper) in [
        (
            "PreToolUse",
            format!("mcp__{server}__(fetch_message|reply)"),
            "_claude-attest",
        ),
        ("SessionStart", String::new(), "_claude-session"),
        ("SessionEnd", String::new(), "_claude-session"),
    ] {
        let entries = hooks
            .entry(event)
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .context("Native hook entries must be an array")?;
        ensure!(
            !entries
                .iter()
                .any(|e| e["hooks"]
                    .as_array()
                    .is_some_and(|h| h.iter().any(|v| v["command"].as_str().is_some_and(|c| c
                        .contains("_claude-attest")
                        || c.contains("_claude-session"))))),
            "Existing experimental hook collision"
        );
        entries.push(
            json!({"matcher":matcher,"hooks":[{"type":"command","command":command(helper)}]}),
        );
    }
    Ok(())
}

struct OwnedChild(Child, bool);
impl Drop for OwnedChild {
    fn drop(&mut self) {
        if self.1 {
            return;
        }
        // The child remains unreaped, so this owned PID cannot have been reused.
        unsafe {
            libc::kill(self.0.id() as i32, libc::SIGTERM);
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if self.0.try_wait().ok().flatten().is_some() {
                self.1 = true;
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
struct Interrupts {
    value: Arc<AtomicUsize>,
    registrations: Vec<signal_hook::SigId>,
}
impl Interrupts {
    fn install() -> Result<Self> {
        let mut result = Self {
            value: Arc::new(AtomicUsize::new(0)),
            registrations: vec![],
        };
        for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
            result.registrations.push(signal_hook::flag::register_usize(
                signal,
                Arc::clone(&result.value),
                signal as usize,
            )?);
        }
        Ok(result)
    }
}
impl Drop for Interrupts {
    fn drop(&mut self) {
        for id in self.registrations.drain(..) {
            signal_hook::low_level::unregister(id);
        }
    }
}

fn await_native(
    pika: &Pika,
    token: &str,
    child: &mut OwnedChild,
    signals: &Interrupts,
) -> Result<i32> {
    let mut admitted = false;
    let mut next_check = Instant::now();
    loop {
        if let Some(status) = child.0.try_wait()? {
            child.1 = true;
            return Ok(status.code().unwrap_or(1));
        }
        let signal = signals.value.swap(0, Ordering::Relaxed);
        // Ctrl-C belongs to native Claude's foreground terminal; do not turn its first interrupt into exit.
        if signal != 0 && signal != libc::SIGINT as usize {
            return Ok(128 + signal as i32);
        }
        if !admitted && Instant::now() >= next_check {
            next_check = Instant::now() + Duration::from_millis(250);
            if super::channel::ready_to_admit(pika, token).unwrap_or(false) {
                admitted = certify_started(pika, token)
                    .and_then(|()| super::mark_ready(pika, token))
                    .is_ok();
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn certify_started(pika: &Pika, token: &str) -> Result<()> {
    let binding = super::token_binding(pika, token)?;
    ensure!(
        super::native_matches(&binding),
        "Native generation changed before admission"
    );
    let Some(pending) = pika.store.get_pending(token)? else {
        let owner = pika
            .store
            .get_recovery_owner(Provider::Claude, &binding.thread)?
            .context("Original native owner unavailable")?;
        ensure!(
            owner.pid == binding.native_pid
                && owner.start_time == i64::try_from(binding.native_start)?
                && owner.launch_token == token,
            "Native owner changed"
        );
        return Ok(());
    };
    ensure!(
        require_reserved_pane(pika, &pending)?.contains(&binding.native_pid),
        "Native owner left original pane"
    );
    ensure!(
        pika.store.observe_launched_generation(
            token,
            binding.native_pid,
            i64::try_from(binding.native_start)?
        )?,
        "Native reservation changed"
    );
    pika.store
        .reconcile_transaction(|ledger| admit_started(ledger, &binding, &pending))
}

fn admit_started(
    ledger: &crate::store::ReconcileLedger<'_>,
    binding: &Binding,
    original: &crate::store::PendingLaunch,
) -> Result<()> {
    let pending = ledger
        .get_pending(&binding.token)?
        .context("Native reservation disappeared")?;
    ensure!(
        pending.created_at == original.created_at
            && pending.expected_session_id.as_deref() == Some(&binding.thread)
            && pending.tmux_pane == original.tmux_pane
            && pending.cwd == binding.cwd
            && pending.provider == Provider::Claude,
        "Native reservation changed during admission"
    );
    ensure!(
        !ledger.is_untracked(Provider::Claude, &binding.thread)?,
        "Native conversation explicitly untracked"
    );
    let mut session = ledger
        .get_session(Provider::Claude, &binding.thread)?
        .unwrap_or_else(|| crate::core::session_from_pending(&pending));
    session.root_pid = Some(binding.native_pid);
    session.tmux_pane = pending.tmux_pane;
    session.tmux_session = pending.tmux_session;
    session.status = crate::model::Status::Ready;
    session.source = "claude-native-session-start".into();
    session.home_state = "open".into();
    session.attention_reason = None;
    session.error = None;
    session.live = true;
    session.managed = true;
    ensure!(
        ledger.upsert_session(&session, true)?,
        "Native session admission rejected"
    );
    ensure!(
        ledger.certify_launch(
            &binding.token,
            Provider::Claude,
            &binding.thread,
            binding.native_pid,
            i64::try_from(binding.native_start)?
        )?,
        "Native owner certification rejected"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn add_only_hooks_keep_permissions_and_existing_commands() {
        let mut settings = json!({"permissions":{"deny":["Bash"]},"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"existing"}]}]}});
        append_hooks(
            &mut settings,
            Path::new("/tmp/pika with spaces"),
            "token",
            "pika_name",
        )
        .unwrap();
        assert_eq!(settings["permissions"], json!({"deny":["Bash"]}));
        assert_eq!(
            settings["hooks"]["PreToolUse"][0]["hooks"][0]["command"],
            "existing"
        );
        assert!(
            settings["hooks"]["PreToolUse"][1]["hooks"][0]["command"]
                .as_str()
                .unwrap()
                .starts_with("'/tmp/pika with spaces'")
        );
        assert!(settings.get("allowedTools").is_none());
        assert!(append_hooks(&mut settings, Path::new("pika"), "token", "pika_name").is_err());
    }
    #[test]
    fn mcp_collision_and_noninteractive_identity_fail_closed() {
        let mut mcp = json!({"mcpServers":{"existing":{"command":"unchanged"}}});
        merge_mcp(
            &mut mcp,
            json!({"mcpServers":{"other":{"command":"other"}}}),
        )
        .unwrap();
        assert_eq!(mcp["mcpServers"]["existing"]["command"], "unchanged");
        assert!(merge_mcp(&mut mcp, json!({"mcpServers":{"existing":{}}})).is_err());
        let id = uuid::Uuid::new_v4().to_string();
        assert!(validate_argv(&["claude".into(), "--session-id".into(), id.clone()], &id).is_ok());
        assert!(
            validate_argv(
                &[
                    "claude".into(),
                    "--resume".into(),
                    id.clone(),
                    "--bg".into()
                ],
                &id
            )
            .is_err()
        );
        assert!(
            validate_argv(
                &[
                    "claude".into(),
                    "--session-id".into(),
                    id.clone(),
                    "--resume=another".into()
                ],
                &id
            )
            .is_err()
        );
    }

    #[test]
    fn launch_local_files_preserve_native_configuration_and_no_pregrants() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let token = uuid::Uuid::new_v4().to_string();
        let thread = uuid::Uuid::new_v4().to_string();
        let server = format!("pika_{}", uuid::Uuid::parse_str(&token).unwrap().simple());
        let settings = json!({"permissions":{"deny":["Bash"]},"hooks":{"SessionStart":[{"matcher":"resume","hooks":[{"type":"command","command":"original hook"}]}]}}).to_string();
        let mcp =
            json!({"mcpServers":{"original":{"command":"original-server","args":["literal arg"]}}})
                .to_string();
        fs::write(temp.path().join("original-settings.json"), &settings).unwrap();
        fs::write(temp.path().join("original-mcp.json"), &mcp).unwrap();
        let argv = vec![
            "claude".into(),
            "--session-id".into(),
            thread,
            "--mcp-config".into(),
            "original-mcp.json".into(),
            "--settings".into(),
            "original-settings.json".into(),
            "--dangerously-load-development-channels".into(),
            "plugin:original@marketplace".into(),
        ];
        let configured = configure(&argv, temp.path(), &token, &server, temp.path()).unwrap();
        assert!(!configured.iter().any(|a| matches!(
            a.as_str(),
            "--allowedTools" | "--permission-mode" | "--strict-mcp-config" | "--debug-file"
        )));
        assert!(configured.contains(&"plugin:original@marketplace".into()));
        assert!(configured.contains(&format!("server:{server}")));
        let generated_mcp: Value = serde_json::from_slice(
            &fs::read(temp.path().join(format!("{token}.mcp.json"))).unwrap(),
        )
        .unwrap();
        assert_eq!(
            generated_mcp["mcpServers"]["original"],
            serde_json::from_str::<Value>(&mcp).unwrap()["mcpServers"]["original"]
        );
        let generated_settings: Value = serde_json::from_slice(
            &fs::read(temp.path().join(format!("{token}.settings.json"))).unwrap(),
        )
        .unwrap();
        assert_eq!(generated_settings["permissions"], json!({"deny":["Bash"]}));
        assert_eq!(
            generated_settings["hooks"]["SessionStart"][0]["hooks"][0]["command"],
            "original hook"
        );
        assert_eq!(
            fs::read_to_string(temp.path().join("original-settings.json")).unwrap(),
            settings
        );
        assert_eq!(
            fs::read_to_string(temp.path().join("original-mcp.json")).unwrap(),
            mcp
        );
        assert_eq!(
            fs::metadata(temp.path().join(format!("{token}.settings.json")))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}
