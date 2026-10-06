use super::{Binding, binding_path, transport};
use crate::{core::Pika, model::Provider, process};
use anyhow::{Context, Result, bail, ensure};
use serde_json::json;
use std::{
    fs,
    io::{BufRead, BufReader, Read},
    net::TcpListener,
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

#[path = "mobile_opencode_capability.rs"]
mod capability;

struct OwnedChild(Child, bool, bool);
impl OwnedChild {
    fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        if self.1 && !self.2 {
            let mut info = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
            // Peek without reaping so server descendants can be stopped while
            // the owned group leader's PID is still exclusively reserved.
            let result = unsafe {
                libc::waitid(
                    libc::P_PID,
                    self.0.id(),
                    &mut info,
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            };
            if result != 0 {
                return Err(std::io::Error::last_os_error());
            };
            if info.si_signo != 0 {
                stop_group(self.0.id());
            }
        }
        let status = self.0.try_wait()?;
        self.2 |= status.is_some();
        Ok(status)
    }
}

fn stop_group(pid: u32) {
    if let Ok(pid) = i32::try_from(pid) {
        unsafe { libc::kill(-pid, libc::SIGTERM) };
        std::thread::sleep(Duration::from_millis(250));
        unsafe { libc::kill(-pid, libc::SIGKILL) };
    }
}
impl Drop for OwnedChild {
    fn drop(&mut self) {
        if self.2 {
            return;
        };
        let pid = i32::try_from(self.0.id()).unwrap_or(0);
        if pid > 0 {
            // Child is unreaped and exclusively owned, so its PID cannot be reused.
            // Only the server creates a fresh group; the TUI retains terminal control.
            unsafe { libc::kill(if self.1 { -pid } else { pid }, libc::SIGTERM) };
        }
        // Do not call try_wait before the group escalation: it would reap the
        // group leader and invalidate our exclusive unreaped PID guarantee.
        std::thread::sleep(Duration::from_millis(250));
        if self.1 && pid > 0 {
            unsafe { libc::kill(-pid, libc::SIGKILL) };
        } else {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}

struct Interrupts {
    value: Arc<AtomicUsize>,
    registrations: Vec<signal_hook::SigId>,
}
impl Interrupts {
    fn install() -> Result<Self> {
        let mut guard = Self {
            value: Arc::new(AtomicUsize::new(0)),
            registrations: Vec::new(),
        };
        for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
            guard.registrations.push(signal_hook::flag::register_usize(
                signal,
                Arc::clone(&guard.value),
                signal as usize,
            )?);
        }
        Ok(guard)
    }
}
impl Drop for Interrupts {
    fn drop(&mut self) {
        for id in self.registrations.drain(..) {
            signal_hook::low_level::unregister(id);
        }
    }
}

pub(crate) fn launch(pika: &Pika, token: &str, argv: &[String]) -> Result<i32> {
    let interrupts = Interrupts::install()?;
    let pending = pika
        .store
        .get_pending(token)?
        .context("Reserved native launch is unavailable")?;
    ensure!(
        pending.provider == Provider::Opencode,
        "Wrong provider launch reservation"
    );
    let executable = argv.first().context("Native executable missing")?;
    let options = launch_options(argv)?;
    let startup = startup_lock(pika, &interrupts)?;
    let (mut server, mut binding) = start_server(&pending, executable, &options)?;
    transport::bootstrap(&binding, &|| {
        ensure!(
            interrupts.value.load(Ordering::Relaxed) == 0,
            "Native startup cancelled"
        );
        ensure!(
            super::alive(binding.server_pid, binding.server_start),
            "Native server generation changed during startup"
        );
        Ok(())
    })?;
    ensure!(
        interrupts.value.load(Ordering::Relaxed) == 0,
        "Native startup cancelled"
    );
    select_session(&mut binding, &pending)?;
    let mut native = start_native(executable, &options, &mut binding)?;
    certify(pika, &binding)?;
    drop(startup);
    await_native(&mut native, &mut server, &interrupts)
}

fn startup_lock(pika: &Pika, interrupts: &Interrupts) -> Result<fs::File> {
    use std::os::unix::fs::DirBuilderExt;
    let parent = pika.paths.state_dir.join("opencode-shared");
    if !parent.exists() {
        match fs::DirBuilder::new().mode(0o700).create(&parent) {
            Ok(()) => (),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => (),
            Err(error) => return Err(error.into()),
        }
    }
    ensure!(
        fs::canonicalize(&parent)?
            == fs::canonicalize(&pika.paths.state_dir)?.join("opencode-shared"),
        "Native startup lock parent alias rejected"
    );
    let path = parent.join("startup.lock");
    let file = super::private::open_lock(&path)?;
    wait_startup_lock(&file, &interrupts.value, Duration::from_secs(30))?;
    super::private::validate_lock(&path, &file)?;
    Ok(file)
}

fn wait_startup_lock(file: &fs::File, signal: &AtomicUsize, budget: Duration) -> Result<()> {
    use fs2::FileExt;
    let deadline = Instant::now() + budget;
    loop {
        ensure!(
            signal.load(Ordering::Relaxed) == 0,
            "Native startup cancelled"
        );
        ensure!(Instant::now() < deadline, "Native startup lock timed out");
        match file.try_lock_exclusive() {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20))
            }
            Err(error) => return Err(error.into()),
        }
    }
}

fn start_server(
    pending: &crate::store::PendingLaunch,
    executable: &str,
    options: &[String],
) -> Result<(OwnedChild, Binding)> {
    let password = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let mut command = Command::new(executable);
    // Native --port 0 defaults to 4096 rather than allocating an ephemeral port.
    // Reserve an OS-selected loopback port until immediately before spawning;
    // any bind race fails closed (reported port and accepted PID are verified).
    let reservation = TcpListener::bind(("127.0.0.1", 0))?;
    let selected_port = reservation.local_addr()?.port();
    command
        .args(options)
        .args(["serve", "--hostname", "127.0.0.1", "--port"])
        .arg(selected_port.to_string())
        .current_dir(&pending.cwd)
        .env("OPENCODE_SERVER_PASSWORD", &password)
        .env("OPENCODE_SERVER_USERNAME", "opencode")
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    advisory_environment(&mut command);
    use std::os::unix::process::CommandExt;
    command.process_group(0);
    drop(reservation);
    let mut server = OwnedChild(command.spawn()?, true, false);
    let server_generation = process::process_generation(i64::from(server.0.id()))
        .context("Native server generation unavailable")?;
    let port = server_port(&mut server)?;
    ensure!(
        port == selected_port,
        "Native listener differs from reserved port"
    );
    let supervisor = process::process_generation(i64::from(std::process::id()))
        .context("Native supervisor generation unavailable")?;
    let binding = Binding {
        version: 1,
        token: pending.launch_token.clone(),
        thread: String::new(),
        cwd: fs::canonicalize(&pending.cwd)?
            .to_string_lossy()
            .into_owned(),
        server_pid: server_generation.pid,
        server_start: server_generation.start_time,
        native_pid: 0,
        native_start: 0,
        supervisor_pid: supervisor.pid,
        supervisor_start: supervisor.start_time,
        port,
        password,
    };
    Ok((server, binding))
}

fn select_session(binding: &mut Binding, pending: &crate::store::PendingLaunch) -> Result<()> {
    let session = if let Some(thread) = pending.expected_session_id.as_deref() {
        transport::request(binding, "GET", &format!("/session/{thread}"), None)?
    } else {
        transport::request(
            binding,
            "POST",
            "/session",
            Some(json!({"title":pending.name})),
        )?
    };
    binding.thread = session["id"]
        .as_str()
        .context("Native exact session ID missing")?
        .to_owned();
    if let Some(expected) = pending.expected_session_id.as_deref() {
        ensure!(
            binding.thread == expected,
            "Native cold resume identity mismatch"
        )
    };
    ensure!(
        session["directory"]
            .as_str()
            .map(fs::canonicalize)
            .transpose()?
            == Some(fs::canonicalize(&binding.cwd)?),
        "Native created session directory mismatch"
    );
    Ok(())
}

fn start_native(executable: &str, options: &[String], binding: &mut Binding) -> Result<OwnedChild> {
    let mut tui = Command::new(executable);
    tui.args(options)
        .args([
            "attach",
            &format!("http://127.0.0.1:{}", binding.port),
            "--session",
            &binding.thread,
            "--dir",
            &binding.cwd,
        ])
        .current_dir(&binding.cwd)
        .env("OPENCODE_SERVER_PASSWORD", &binding.password)
        .env("OPENCODE_SERVER_USERNAME", "opencode");
    let native = OwnedChild(tui.spawn()?, false, false);
    let generation = process::process_generation(i64::from(native.0.id()))
        .context("Native terminal generation unavailable")?;
    binding.native_pid = generation.pid;
    binding.native_start = generation.start_time;
    ensure!(
        super::pair_processes(binding),
        "Native server and terminal pair proof failed"
    );
    Ok(native)
}

fn certify(pika: &Pika, binding: &Binding) -> Result<()> {
    let token = &binding.token;
    prepare_home(pika, binding)?;
    ensure!(
        pika.store
            .bind_launch(token, Provider::Opencode, &binding.thread)?,
        "Native launch identity reservation changed"
    );
    ensure!(
        pika.store.observe_launched_generation(
            token,
            binding.native_pid,
            i64::try_from(binding.native_start)?
        )?,
        "Native generation reservation changed"
    );
    pika.store.reconcile_transaction(|ledger| {
        let pending = ledger
            .get_pending(token)?
            .context("Native launch reservation disappeared")?;
        require_pending_context(binding, &pending)?;
        admit_managed(ledger, binding, &pending)
    })?;
    Ok(())
}

fn prepare_home(pika: &Pika, binding: &Binding) -> Result<()> {
    let token = &binding.token;
    let pending = pika
        .store
        .get_pending(token)?
        .context("Native launch reservation disappeared before pane binding")?;
    require_pending_context(binding, &pending)?;
    ensure!(
        !pika
            .store
            .is_untracked(Provider::Opencode, &binding.thread)?,
        "Native conversation was explicitly untracked before pane binding"
    );
    ensure!(
        super::pair_processes(binding),
        "Original native pair changed before pane binding"
    );
    pika.tmux.tag_pane(
        pending
            .tmux_pane
            .as_deref()
            .context("Native launch pane missing")?,
        Some(Provider::Opencode),
        Some(&binding.thread),
        Some(&pending.name),
        Some(token),
    )?;
    publish(pika, binding)?;
    Ok(())
}

fn admit_managed(
    ledger: &crate::store::ReconcileLedger<'_>,
    binding: &Binding,
    pending: &crate::store::PendingLaunch,
) -> Result<()> {
    let session = managed_session(
        binding,
        pending,
        ledger.get_session(Provider::Opencode, &binding.thread)?,
    );
    ensure!(
        !ledger.is_untracked(Provider::Opencode, &binding.thread)?,
        "Original native conversation was explicitly untracked during launch"
    );
    ensure!(
        ledger.upsert_session(&session, true)?,
        "Managed native session admission failed"
    );
    ensure!(
        ledger.certify_launch(
            &binding.token,
            Provider::Opencode,
            &binding.thread,
            binding.native_pid,
            i64::try_from(binding.native_start)?
        )?,
        "Native owner certificate rejected"
    );
    Ok(())
}

fn require_pending_context(binding: &Binding, pending: &crate::store::PendingLaunch) -> Result<()> {
    ensure!(
        pending.provider == Provider::Opencode
            && fs::canonicalize(&pending.cwd)? == fs::canonicalize(&binding.cwd)?,
        "Native launch context changed"
    );
    ensure!(
        pending.tmux_pane.is_some()
            && pending.tmux_session.is_some()
            && pending.tmux_pane.as_deref() == std::env::var("TMUX_PANE").ok().as_deref(),
        "Native terminal pane mismatch"
    );
    Ok(())
}

fn managed_session(
    binding: &Binding,
    pending: &crate::store::PendingLaunch,
    existing: Option<crate::model::Session>,
) -> crate::model::Session {
    use crate::model::{Session, Status};
    let mut session = existing.unwrap_or_else(|| Session {
        provider: Provider::Opencode,
        session_id: binding.thread.clone(),
        name: Some(pending.name.clone()),
        cwd: Some(binding.cwd.clone()),
        branch: None,
        transcript_path: None,
        tmux_session: None,
        tmux_pane: None,
        root_pid: None,
        status: Status::Starting,
        unread: false,
        model: None,
        source: "private-shared-provider".into(),
        managed: true,
        error: None,
        attention_reason: None,
        created_at: pending.created_at,
        updated_at: pending.created_at,
        last_event_at: pending.created_at,
        last_activity_at: pending.created_at,
        live: true,
        attached: false,
        home_state: "starting".into(),
        cpu_percent: None,
        rss_kb: None,
        input_tokens: None,
        output_tokens: None,
        cached_input_tokens: None,
        cache_write_tokens: None,
        total_tokens: None,
        estimated_cost_usd: None,
        active_thread_id: None,
    });
    session.tmux_session = pending.tmux_session.clone();
    session.tmux_pane = pending.tmux_pane.clone();
    session.root_pid = Some(binding.native_pid);
    session.source = "private-shared-provider".into();
    session.managed = true;
    session.live = true;
    session.home_state = "exact".into();
    session
}

fn await_native(
    native: &mut OwnedChild,
    server: &mut OwnedChild,
    interrupts: &Interrupts,
) -> Result<i32> {
    let status = loop {
        if let Some(status) = native.try_wait()? {
            break status;
        };
        ensure!(
            server.try_wait()?.is_none(),
            "Original native server exited; terminal shared connection unavailable"
        );
        let signal = interrupts.value.load(Ordering::Relaxed);
        if signal != 0 {
            return Ok(128 + i32::try_from(signal)?);
        };
        std::thread::sleep(Duration::from_millis(50));
    };
    // The endpoint ceases with its original terminal. Its certificate remains stale,
    // deliberately preventing fallback adoption or accidental replay after exit.
    Ok(status.code().unwrap_or(128))
}

fn launch_options(argv: &[String]) -> Result<Vec<String>> {
    let mut options = Vec::new();
    let mut index = 1;
    while index < argv.len() {
        match argv[index].as_str() {
            "--pure" => options.push(argv[index].clone()),
            "--session" | "-s" => {
                index += 1;
                ensure!(index < argv.len(), "Session flag missing value");
            }
            "--model" | "-m" | "--agent" => bail!(
                "Explicit OpenCode model/agent launch options need verified attach translation"
            ),
            other => bail!("Unverified OpenCode shared launch option: {other}"),
        }
        index += 1;
    }
    Ok(options)
}

pub(crate) fn supports_launch(argv: &[String]) -> bool {
    argv.first()
        .is_some_and(|executable| launch_options(argv).is_ok() && capability::verified(executable))
}

fn advisory_environment(command: &mut Command) {
    for (key, _) in std::env::vars_os() {
        let name = key.to_string_lossy();
        if name.starts_with("PIKA_")
            && (name.contains("LAUNCH")
                || name.contains("OWNER")
                || name.contains("SESSION")
                || name.contains("NAME")
                || name.contains("PANE"))
            || name == "TMUX_PANE"
            || name == "TMUX"
        {
            command.env_remove(key);
        }
    }
}

fn server_port(server: &mut OwnedChild) -> Result<u16> {
    let stdout = server
        .0
        .stdout
        .take()
        .context("Native server readiness channel unavailable")?;
    let (send, receive) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        for _ in 0..100 {
            let mut bytes = Vec::new();
            let Ok(size) = (&mut reader).take(4096).read_until(b'\n', &mut bytes) else {
                break;
            };
            if size == 0 || bytes.last() != Some(&b'\n') {
                break;
            };
            let Ok(line) = std::str::from_utf8(&bytes) else {
                break;
            };
            if let Some(port) = line
                .strip_prefix("opencode server listening on http://127.0.0.1:")
                .and_then(|v| v.trim().parse::<u16>().ok())
            {
                let _ = send.send(port);
                break;
            }
        }
        let _ = std::io::copy(&mut reader, &mut std::io::sink());
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Ok(port) = receive.recv_timeout(Duration::from_millis(100)) {
            ensure!(port != 0, "Invalid native listener");
            return Ok(port);
        };
        ensure!(
            server.try_wait()?.is_none(),
            "Native shared server exited before readiness"
        );
    }
    bail!("Native shared server did not become ready")
}

fn publish(pika: &Pika, binding: &Binding) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    let path = binding_path(&pika.paths, &binding.thread)?;
    let parent = path
        .parent()
        .context("Private certificate parent missing")?;
    if !parent.exists() {
        fs::DirBuilder::new().mode(0o700).create(parent)?
    }
    ensure!(
        fs::canonicalize(parent)?
            == fs::canonicalize(&pika.paths.state_dir)?.join("opencode-shared"),
        "Native certificate parent alias rejected"
    );
    super::private::write_private(&path, &serde_json::to_vec(binding)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{PendingLaunch, Store};

    #[test]
    fn startup_lock_is_bounded_cancelled_and_released_by_close() {
        use fs2::FileExt;
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let path = temp.path().join("startup.lock");
        let owner = super::super::private::open_lock(&path).unwrap();
        let follower = super::super::private::open_lock(&path).unwrap();
        owner.try_lock_exclusive().unwrap();
        assert!(
            wait_startup_lock(&follower, &AtomicUsize::new(1), Duration::from_secs(1)).is_err()
        );
        assert!(
            wait_startup_lock(&follower, &AtomicUsize::new(0), Duration::from_millis(30)).is_err()
        );
        drop(owner);
        wait_startup_lock(&follower, &AtomicUsize::new(0), Duration::from_secs(1)).unwrap();
        super::super::private::validate_lock(&path, &follower).unwrap();
    }

    fn fixture() -> (tempfile::TempDir, Store, Binding, PendingLaunch) {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::at(temp.path().join("synthetic.db"));
        let pending = PendingLaunch {
            launch_token: "fixture-launch".into(),
            provider: Provider::Opencode,
            name: "Synthetic original".into(),
            cwd: temp.path().to_string_lossy().into_owned(),
            tmux_session: Some("synthetic-pane".into()),
            tmux_pane: Some("%99".into()),
            expected_session_id: Some("ses_fixture".into()),
            root_pid: Some(99),
            root_pid_start: Some(123),
            preexisting_session_ids: None,
            candidate_session_id: None,
            candidate_observed_at: None,
            created_at: 1.0,
        };
        let binding = Binding {
            version: 1,
            token: pending.launch_token.clone(),
            thread: "ses_fixture".into(),
            cwd: pending.cwd.clone(),
            server_pid: 98,
            server_start: 122,
            native_pid: 99,
            native_start: 123,
            supervisor_pid: 97,
            supervisor_start: 121,
            port: 12345,
            password: "synthetic-only".into(),
        };
        assert!(store.add_pending(&pending).unwrap());
        assert!(
            store
                .bind_launch(&binding.token, Provider::Opencode, &binding.thread)
                .unwrap()
        );
        (temp, store, binding, pending)
    }

    #[test]
    fn session_admission_and_owner_certificate_commit_together() {
        let (_temp, store, binding, pending) = fixture();
        store
            .reconcile_transaction(|ledger| admit_managed(ledger, &binding, &pending))
            .unwrap();
        let session = store
            .get_session(Provider::Opencode, &binding.thread)
            .unwrap()
            .unwrap();
        assert_eq!(session.name.as_deref(), Some("Synthetic original"));
        assert_eq!(session.tmux_pane.as_deref(), Some("%99"));
        assert!(
            store
                .get_recovery_owner(Provider::Opencode, &binding.thread)
                .unwrap()
                .is_some()
        );
        assert!(store.get_pending(&binding.token).unwrap().is_none());
    }

    #[test]
    fn explicit_untrack_before_native_bind_preserves_recovery_reservation() {
        let (_temp, store, binding, pending) = fixture();
        store
            .untrack_session(Provider::Opencode, &binding.thread)
            .unwrap();
        assert!(
            store
                .reconcile_transaction(|ledger| admit_managed(ledger, &binding, &pending))
                .is_err()
        );
        assert!(
            store
                .get_session(Provider::Opencode, &binding.thread)
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .get_recovery_owner(Provider::Opencode, &binding.thread)
                .unwrap()
                .is_none()
        );
        assert!(store.get_pending(&binding.token).unwrap().is_some());
    }
}
