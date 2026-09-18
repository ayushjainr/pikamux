#![cfg(unix)]

use pikamux::{
    config::Config,
    core::{OpenTarget, Pika},
    model::{Provider, Session, Status},
    paths::Paths,
    process,
    store::Store,
    tmux::{ReceiptDelivery, Tmux},
};
use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::PermissionsExt,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

struct IsolatedTmux(String);

fn recorded_terminal(transcript: &std::path::Path, args: &[&str]) -> Command {
    let mut command = Command::new("script");
    #[cfg(target_os = "linux")]
    command
        .args(["-q", "-e", "-c", &shell_words::join(args)])
        .arg(transcript);
    #[cfg(not(target_os = "linux"))]
    command.arg("-q").arg(transcript).args(args);
    command
}

impl Drop for IsolatedTmux {
    fn drop(&mut self) {
        let _ = Command::new("tmux")
            .args(["-L", &self.0, "kill-server"])
            .status();
    }
}

struct AttachedClient {
    child: Child,
    socket: String,
}

impl Drop for AttachedClient {
    fn drop(&mut self) {
        let _ = Command::new("tmux")
            .args(["-L", &self.socket, "kill-server"])
            .status();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn paths(root: &std::path::Path) -> Paths {
    Paths {
        config_dir: root.join("config"),
        state_dir: root.join("state"),
        config: root.join("config/config.json"),
        database: root.join("state/pika.db"),
        codex_home: root.join("codex-home"),
        claude_home: root.join("claude-home"),
        opencode_data_home: root.join("opencode-data"),
        opencode_config_home: root.join("opencode-config"),
    }
}

fn saved(provider: Provider, identity: &str, name: &str, cwd: &std::path::Path) -> Session {
    Session {
        provider,
        session_id: identity.into(),
        name: Some(name.into()),
        cwd: Some(cwd.display().to_string()),
        branch: None,
        transcript_path: None,
        tmux_session: None,
        tmux_pane: None,
        root_pid: None,
        status: Status::Parked,
        unread: false,
        model: None,
        source: "fixture".into(),
        managed: true,
        error: None,
        attention_reason: None,
        created_at: 1.0,
        updated_at: 1.0,
        last_event_at: 1.0,
        last_activity_at: 1.0,
        live: false,
        attached: false,
        home_state: "missing".into(),
        cpu_percent: None,
        rss_kb: None,
        input_tokens: None,
        output_tokens: None,
        cached_input_tokens: None,
        cache_write_tokens: None,
        total_tokens: None,
        estimated_cost_usd: None,
        active_thread_id: None,
    }
}

fn wait_for_provider_identity(provider: Provider, identity: &str) {
    let deadline = Instant::now() + Duration::from_secs(4);
    loop {
        let observed = process::observe();
        if observed
            .require_complete("wait for the fake provider identity")
            .ok()
            .is_some_and(|processes| {
                process::find_session_processes(identity, provider, processes).len() == 1
            })
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "fake {provider} identity {identity} did not become observable"
        );
        thread::sleep(Duration::from_millis(25));
    }
}

fn fake_provider_pika(
    temp: &tempfile::TempDir,
    socket: &str,
    executable: &std::path::Path,
) -> Pika {
    let paths = paths(temp.path());
    let store = Store::from_paths(&paths);
    store.initialize().unwrap();
    let mut config = Config {
        provider_executables: BTreeMap::from([(
            "claude".to_owned(),
            executable.to_string_lossy().into_owned(),
        )]),
        ..Config::default()
    };
    config.alerts = "none".into();
    Pika::with_components(
        paths,
        config,
        store,
        Tmux::with_executable("tmux", Some(socket.to_owned())),
    )
}

fn wait_for_exact_session(pika: &Pika, identity: &str) -> Session {
    let deadline = Instant::now() + Duration::from_secs(4);
    loop {
        let session = pika
            .reconcile_local()
            .unwrap()
            .sessions
            .into_iter()
            .find(|session| session.provider == Provider::Claude && session.session_id == identity)
            .unwrap();
        if session.home_state == "exact" {
            return session;
        }
        assert!(
            Instant::now() < deadline,
            "fake provider never became exact: {session:#?}"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn real_isolated_tmux_reopens_unique_uuid_owner_beside_inert_stale_tag() {
    if Command::new("tmux").arg("-V").output().is_err() {
        eprintln!("tmux unavailable; exact ownership integration not exercised");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let socket = format!("pika-rust-stale-tag-{}", std::process::id());
    let _guard = IsolatedTmux(socket.clone());
    let claude = temp.path().join("claude");
    fs::write(
        &claude,
        "#!/bin/sh\nprintf 'unique UUID owner ready\\n'\nwhile :; do sleep 1; done\n",
    )
    .unwrap();
    fs::set_permissions(&claude, fs::Permissions::from_mode(0o700)).unwrap();

    let identity = "01a03a1f-a350-74b0-9e6f-902784af83e5";
    let pika = fake_provider_pika(&temp, &socket, &claude);
    pika.store
        .upsert_session(
            &saved(Provider::Claude, identity, "stale_tag", temp.path()),
            false,
        )
        .unwrap();
    assert_eq!(
        pika.open_session(
            pika.store
                .get_session(Provider::Claude, identity)
                .unwrap()
                .unwrap(),
            false,
        )
        .unwrap()
        .kind,
        "RESUMED EXACT"
    );
    wait_for_provider_identity(Provider::Claude, identity);
    let exact = wait_for_exact_session(&pika, identity);
    let exact_pane = exact.tmux_pane.clone().unwrap();

    assert!(
        Command::new("tmux")
            .args([
                "-L",
                &socket,
                "new-session",
                "-d",
                "-s",
                "stale-shell",
                "sleep 30",
            ])
            .status()
            .unwrap()
            .success()
    );
    let tmux = Tmux::with_executable("tmux", Some(socket.clone()));
    let stale = tmux.get_pane("stale-shell").unwrap().unwrap();
    for (key, value) in [
        ("@pika_provider", "claude"),
        ("@pika_session_id", identity),
        ("@pika_name", "stale_tag"),
    ] {
        assert!(
            Command::new("tmux")
                .args([
                    "-L",
                    &socket,
                    "set-option",
                    "-p",
                    "-t",
                    &stale.pane_id,
                    key,
                    value,
                ])
                .status()
                .unwrap()
                .success()
        );
    }

    let reconciled = pika
        .reconcile_local()
        .unwrap()
        .sessions
        .into_iter()
        .find(|session| session.provider == Provider::Claude && session.session_id == identity)
        .unwrap();
    assert_eq!(reconciled.home_state, "exact");
    assert_eq!(reconciled.tmux_pane.as_deref(), Some(exact_pane.as_str()));
    assert_ne!(reconciled.status, Status::OpenTwice);

    let reopened = pika.open_session(reconciled.clone(), false).unwrap();
    assert_eq!(reopened.kind, "ATTACHED LIVE");
    let output_deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let output = pika.capture_exact(&reconciled, 20).unwrap();
        if output.contains("unique UUID owner ready") {
            break;
        }
        assert!(Instant::now() < output_deadline, "{output:?}");
        thread::sleep(Duration::from_millis(25));
    }

    let panes = tmux.list_panes().unwrap();
    let same_uuid: Vec<_> = panes
        .iter()
        .filter(|pane| pane.pika_session_id.as_deref() == Some(identity))
        .collect();
    assert_eq!(
        same_uuid.len(),
        2,
        "the inert stale tag was deleted or duplicated"
    );
    let stale_after = tmux.get_pane(&stale.pane_id).unwrap().unwrap();
    assert_eq!(stale_after.current_command, "sleep");
    assert_eq!(stale_after.pika_provider, Some(Provider::Claude));
    assert_eq!(stale_after.pika_session_id.as_deref(), Some(identity));
    let observation = process::observe();
    let processes = observation
        .require_complete("count unique fake provider owner")
        .unwrap();
    assert_eq!(
        process::find_session_processes(identity, Provider::Claude, processes),
        vec![reconciled.root_pid.unwrap()]
    );
}

#[test]
fn real_isolated_tmux_blocks_unproven_competing_live_uuid_pane() {
    if Command::new("tmux").arg("-V").output().is_err() {
        eprintln!("tmux unavailable; competing ownership integration not exercised");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let socket = format!("pika-rust-competing-{}", std::process::id());
    let _guard = IsolatedTmux(socket.clone());
    let claude = temp.path().join("claude");
    fs::write(
        &claude,
        "#!/bin/sh\nprintf 'live UUID %s\\n' \"$3\"\nwhile :; do sleep 1; done\n",
    )
    .unwrap();
    fs::set_permissions(&claude, fs::Permissions::from_mode(0o700)).unwrap();
    let identity = "11111111-1111-4111-8111-111111111111";
    let competitor = "22222222-2222-4222-8222-222222222222";
    let pika = fake_provider_pika(&temp, &socket, &claude);
    pika.store
        .upsert_session(
            &saved(Provider::Claude, identity, "competing", temp.path()),
            false,
        )
        .unwrap();
    let session = pika
        .store
        .get_session(Provider::Claude, identity)
        .unwrap()
        .unwrap();
    pika.open_session(session, false).unwrap();
    wait_for_provider_identity(Provider::Claude, identity);

    assert!(
        Command::new("tmux")
            .args([
                "-L",
                &socket,
                "new-session",
                "-d",
                "-s",
                "competitor",
                &format!("{} resume {}", claude.display(), competitor),
            ])
            .status()
            .unwrap()
            .success()
    );
    wait_for_provider_identity(Provider::Claude, competitor);
    let tmux = Tmux::with_executable("tmux", Some(socket.clone()));
    let competitor_pane = tmux.get_pane("competitor").unwrap().unwrap();
    for (key, value) in [
        ("@pika_provider", "claude"),
        ("@pika_session_id", identity),
        ("@pika_name", "competing"),
    ] {
        assert!(
            Command::new("tmux")
                .args([
                    "-L",
                    &socket,
                    "set-option",
                    "-p",
                    "-t",
                    &competitor_pane.pane_id,
                    key,
                    value,
                ])
                .status()
                .unwrap()
                .success()
        );
    }
    let current = pika
        .store
        .get_session(Provider::Claude, identity)
        .unwrap()
        .unwrap();
    let error = pika.capture_exact(&current, 20).unwrap_err().to_string();
    assert!(
        error.contains("unverified live")
            || error.contains("one exact pane")
            || error.contains("found 2"),
        "{error}"
    );
    assert!(pika.open_session(current, false).is_err());

    let observation = process::observe();
    let processes = observation
        .require_complete("prove competing UUID clients")
        .unwrap();
    assert_eq!(
        process::find_session_processes(identity, Provider::Claude, processes).len(),
        1
    );
    assert_eq!(
        process::find_session_processes(competitor, Provider::Claude, processes).len(),
        1
    );
    let retained = tmux.get_pane(&competitor_pane.pane_id).unwrap().unwrap();
    assert_eq!(retained.pika_session_id.as_deref(), Some(identity));
}

#[test]
fn real_isolated_tmux_nested_provider_helper_does_not_steal_uuid_owner() {
    if Command::new("tmux").arg("-V").output().is_err() {
        eprintln!("tmux unavailable; nested-helper integration not exercised");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let socket = format!("pika-rust-nested-helper-{}", std::process::id());
    let _guard = IsolatedTmux(socket.clone());
    let helper = temp.path().join("nested/claude");
    fs::create_dir_all(helper.parent().unwrap()).unwrap();
    fs::write(&helper, "#!/bin/sh\nwhile :; do sleep 1; done\n").unwrap();
    fs::set_permissions(&helper, fs::Permissions::from_mode(0o700)).unwrap();
    let claude = temp.path().join("claude");
    fs::write(
        &claude,
        format!(
        "#!/bin/sh\n{} helper-without-uuid >/dev/null 2>&1 &\nwhile :; do printf 'UUID owner output\\n'; sleep 0.1; done\n",
            helper.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&claude, fs::Permissions::from_mode(0o700)).unwrap();
    let identity = "33333333-3333-4333-8333-333333333333";
    let pika = fake_provider_pika(&temp, &socket, &claude);
    pika.store
        .upsert_session(
            &saved(Provider::Claude, identity, "nested", temp.path()),
            false,
        )
        .unwrap();
    pika.open_session(
        pika.store
            .get_session(Provider::Claude, identity)
            .unwrap()
            .unwrap(),
        false,
    )
    .unwrap();
    wait_for_provider_identity(Provider::Claude, identity);
    let helper_argv = helper.to_string_lossy().into_owned();
    let helper_deadline = Instant::now() + Duration::from_secs(2);
    let owner_pid = loop {
        let observation = process::observe();
        if let Ok(processes) = observation.require_complete("observe nested provider helper") {
            let owners = process::find_session_processes(identity, Provider::Claude, processes);
            if owners.len() == 1 {
                let tree = process::process_tree(owners[0], processes);
                if tree.iter().any(|pid| {
                    processes
                        .get(pid)
                        .is_some_and(|record| record.argv.iter().any(|arg| arg == &helper_argv))
                }) {
                    break owners[0];
                }
            }
        }
        assert!(
            Instant::now() < helper_deadline,
            "nested helper {helper_argv:?} never appeared beneath the UUID owner"
        );
        thread::sleep(Duration::from_millis(25));
    };
    let session = pika
        .store
        .get_session(Provider::Claude, identity)
        .unwrap()
        .unwrap();
    let binding = pika
        .exact_pane_binding(&session, session.tmux_pane.as_deref())
        .unwrap();
    assert_eq!(binding.provider_pid, owner_pid);
    let observation = process::observe();
    let processes = observation
        .require_complete("prove nested helper ownership")
        .unwrap();
    let owners = process::find_session_processes(identity, Provider::Claude, processes);
    assert_eq!(owners, vec![binding.provider_pid]);
    let output_deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let output = pika.capture_exact(&session, 20).unwrap();
        if output.contains("UUID owner output") {
            break;
        }
        assert!(Instant::now() < output_deadline, "{output:?}");
        thread::sleep(Duration::from_millis(25));
    }
}

#[test]
fn real_isolated_tmux_attach_observes_receipt_after_proven_commit() {
    if Command::new("tmux").arg("-V").output().is_err()
        || Command::new("script").arg("--help").output().is_err()
    {
        eprintln!("tmux/script unavailable; exact receipt integration not exercised");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let socket = format!("pika-rust-receipt-{}", std::process::id());
    let _guard = IsolatedTmux(socket.clone());
    let committed = temp.path().join("committed");
    // Keep the pane alive until the handoff is proven, with a finite watchdog.
    // Runner speed must not determine whether the receipt can be observed.
    let pane_command = format!(
        "n=0; while test ! -f {} && test $n -lt 300; do n=$((n + 1)); sleep 0.1; done; sleep 0.3",
        shell_words::quote(committed.to_str().unwrap())
    );
    assert!(
        Command::new("tmux")
            .args([
                "-L",
                &socket,
                "new-session",
                "-d",
                "-s",
                "pika-c-receipt",
                &pane_command
            ])
            .status()
            .unwrap()
            .success()
    );
    let tmux = Tmux::with_executable("tmux", Some(socket.clone()));
    let untagged = tmux.get_pane("pika-c-receipt").unwrap().unwrap();
    tmux.tag_pane_if_unchanged(
        &untagged,
        Some(Provider::Codex),
        Some("44444444-4444-4444-8444-444444444444"),
        Some("receipt_test"),
        None,
    )
    .unwrap();
    let pane = tmux.get_pane("pika-c-receipt").unwrap().unwrap();
    let transcript = temp.path().join("receipt.out");
    let trace = temp.path().join("tmux.trace");
    let adapter = temp.path().join("tmux-receipt-adapter");
    fs::write(
        &adapter,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$$ $*\" >> {trace}\ncase \"$*\" in *list-clients*) tmux \"$@\" > {output}; status=$?; cat {output}; cat {output} >> {trace}; exit $status;; *) exec tmux \"$@\";; esac\n",
            trace = shell_words::quote(trace.to_str().unwrap()),
            output = shell_words::quote(temp.path().join("clients.out").to_str().unwrap()),
        ),
    )
    .unwrap();
    fs::set_permissions(&adapter, fs::Permissions::from_mode(0o700)).unwrap();
    let output = recorded_terminal(
        &transcript,
        &[
            std::env::current_exe().unwrap().to_str().unwrap(),
            "--exact",
            "real_isolated_tmux_receipt_helper",
            "--nocapture",
        ],
    )
    .env("TERM", "xterm-256color")
    .env("PIKA_RECEIPT_TEST_SOCKET", &socket)
    .env("PIKA_RECEIPT_TEST_PANE", &pane.pane_id)
    .env("PIKA_RECEIPT_TEST_COMMITTED", &committed)
    .env("PIKA_RECEIPT_TEST_TMUX", &adapter)
    .output()
    .unwrap();
    assert!(
        output.status.success(),
        "stdout={} stderr={} transcript={} trace={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
        fs::read_to_string(&transcript).unwrap_or_default(),
        fs::read_to_string(&trace).unwrap_or_default()
    );
    assert_eq!(fs::read_to_string(&committed).unwrap(), "after-receipt");
    let transcript = fs::read_to_string(transcript).unwrap();
    assert!(
        transcript.contains("CONTINUITY PROVEN - receipt_test - ATTACHED LIVE"),
        "{transcript:?}"
    );
}

#[test]
fn real_isolated_tmux_receipt_helper() {
    let Ok(socket) = std::env::var("PIKA_RECEIPT_TEST_SOCKET") else {
        return;
    };
    let pane_id = std::env::var("PIKA_RECEIPT_TEST_PANE").unwrap();
    let committed = std::path::PathBuf::from(std::env::var("PIKA_RECEIPT_TEST_COMMITTED").unwrap());
    let adapter = std::env::var("PIKA_RECEIPT_TEST_TMUX").unwrap();
    let tmux = Tmux::with_executable(adapter, Some(socket));
    let pane = tmux.get_pane(&pane_id).unwrap().unwrap();
    let handoff = tmux
        .attach_exact_with_observed_receipt(
            &pane,
            // Keep the fixture ASCII: a C-locale terminal can render a Unicode
            // separator through ACS escape sequences instead of literal UTF-8.
            "CONTINUITY PROVEN - receipt_test - ATTACHED LIVE",
            || fs::write(&committed, "after-receipt").map_err(Into::into),
        )
        .unwrap();
    assert_eq!(handoff.exit_code, 0);
    assert_eq!(handoff.delivery, Some(ReceiptDelivery::TmuxClient));
}

#[test]
fn real_isolated_tmux_list_clients_proves_the_exact_selected_pane() {
    if Command::new("tmux").arg("-V").output().is_err() {
        eprintln!("tmux unavailable; isolated client format integration not exercised");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let socket = format!("pika-rust-client-proof-{}", std::process::id());
    let _server = IsolatedTmux(socket.clone());
    assert!(
        Command::new("tmux")
            .args([
                "-L",
                &socket,
                "new-session",
                "-d",
                "-s",
                "proof",
                "sleep 30"
            ])
            .status()
            .unwrap()
            .success()
    );
    let pane = Command::new("tmux")
        .args([
            "-L",
            &socket,
            "display-message",
            "-p",
            "-t",
            "proof",
            "#{pane_id}",
        ])
        .output()
        .unwrap();
    assert!(pane.status.success());
    let pane = String::from_utf8(pane.stdout).unwrap().trim().to_owned();

    let transcript = temp.path().join("script.out");
    let child = match recorded_terminal(
        &transcript,
        &["tmux", "-L", &socket, "attach-session", "-t", &pane],
    )
    .env("TERM", "xterm-256color")
    .stdin(Stdio::piped())
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .spawn()
    {
        Ok(child) => child,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("script unavailable; isolated client format integration not exercised");
            return;
        }
        Err(error) => panic!("cannot start isolated pseudo-terminal: {error}"),
    };
    // Keeping script's input pipe open keeps its pseudo-terminal client alive
    // while the server-side client inventory is inspected.
    let mut client = AttachedClient { child, socket };
    let _input = client.child.stdin.take().unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let output = Command::new("tmux")
            .args([
                "-L",
                &client.socket,
                "list-clients",
                "-F",
                r"#{client_pid}\037#{pane_id}",
            ])
            .output()
            .unwrap();
        if output.status.success()
            && String::from_utf8_lossy(&output.stdout).lines().any(|line| {
                line.split_once(r"\037").is_some_and(|(pid, selected)| {
                    pid.parse::<i32>().is_ok_and(|pid| pid > 0) && selected == pane
                })
            })
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "tmux never exposed client_pid with its exact selected pane: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn real_isolated_tmux_resumes_all_providers_and_reuses_each_exact_home() {
    if Command::new("tmux").arg("-V").output().is_err() {
        eprintln!("tmux unavailable; isolated host integration not exercised");
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let socket = format!("pika-rust-test-{}", std::process::id());
    let _guard = IsolatedTmux(socket.clone());
    let bin = temp.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    let mut executables = BTreeMap::new();
    for provider in Provider::ALL {
        let fake = bin.join(provider.as_str());
        fs::write(
            &fake,
            format!(
                "#!/bin/sh\nprintf 'fake {} ready\\n'\nwhile :; do sleep 1; done\n",
                provider.as_str()
            ),
        )
        .unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).unwrap();
        executables.insert(
            provider.as_str().to_owned(),
            fake.to_string_lossy().into_owned(),
        );
    }

    let paths = paths(temp.path());
    let store = Store::from_paths(&paths);
    store.initialize().unwrap();
    let fixtures = [
        (
            Provider::Codex,
            "44444444-4444-4444-8444-444444444444",
            "tmux_codex",
        ),
        (
            Provider::Claude,
            "55555555-5555-4555-8555-555555555555",
            "tmux_claude",
        ),
        (Provider::Opencode, "ses_444444444444", "tmux_opencode"),
    ];
    for (provider, identity, name) in fixtures {
        store
            .upsert_session(&saved(provider, identity, name, temp.path()), false)
            .unwrap();
    }
    let mut config = Config {
        provider_executables: executables,
        ..Config::default()
    };
    config.alerts = "none".into();
    let pika = Pika::with_components(
        paths,
        config,
        store,
        Tmux::with_executable("tmux", Some(socket)),
    );

    for (index, (provider, identity, name)) in fixtures.into_iter().enumerate() {
        let session = pika.store.get_session(provider, identity).unwrap().unwrap();
        let first = pika.open_session(session, false).unwrap();
        assert_eq!(first.kind, "RESUMED EXACT");
        assert!(matches!(first.target, OpenTarget::Session(_)));

        let deadline = Instant::now() + Duration::from_secs(4);
        let exact = loop {
            let current = pika
                .reconcile_local()
                .unwrap()
                .sessions
                .into_iter()
                .find(|session| session.provider == provider && session.session_id == identity)
                .unwrap();
            if current.home_state == "exact" {
                break current;
            }
            if Instant::now() >= deadline {
                let panes = pika.tmux.list_panes().unwrap();
                let processes = pikamux::process::snapshot();
                let evidence: Vec<_> = panes
                    .iter()
                    .map(|pane| {
                        let tree = pikamux::process::process_tree(pane.pane_pid, &processes);
                        let records: Vec<_> = tree
                            .iter()
                            .filter_map(|pid| processes.get(pid))
                            .cloned()
                            .collect();
                        (pane, records)
                    })
                    .collect();
                panic!("fake provider never became exact: {current:#?} {evidence:#?}");
            }
            thread::sleep(Duration::from_millis(50));
        };
        let pane = pika
            .tmux
            .get_pane(exact.tmux_pane.as_deref().unwrap())
            .unwrap()
            .unwrap();
        assert_eq!(pane.pika_provider, Some(provider));
        assert_eq!(pane.pika_session_id.as_deref(), Some(identity));
        assert_eq!(pane.pika_name.as_deref(), Some(name));
        let output_deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if pika
                .tmux
                .capture(&pane.pane_id, 20)
                .unwrap()
                .contains(&format!("fake {} ready", provider.as_str()))
            {
                break;
            }
            assert!(
                Instant::now() < output_deadline,
                "fake provider output never reached its exact pane"
            );
            thread::sleep(Duration::from_millis(25));
        }

        let second = pika.open_session(exact, false).unwrap();
        assert_eq!(second.kind, "ATTACHED LIVE");
        assert_eq!(pika.tmux.list_panes().unwrap().len(), index + 1);
    }

    let fresh = pika
        .new_session("fresh_claude", Provider::Claude, false)
        .unwrap();
    assert_eq!(fresh.kind, "NEW HOME");
    let OpenTarget::Pending(pending) = fresh.target else {
        panic!("new conversation did not produce a pending exact home")
    };
    assert_eq!(pending.name, "fresh_claude");
    assert!(pending.expected_session_id.is_some());
    let pane = pika
        .tmux
        .get_pane(pending.tmux_pane.as_deref().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(pane.pika_provider, Some(Provider::Claude));
    assert_eq!(pane.pika_name.as_deref(), Some("fresh_claude"));
    assert_eq!(
        pane.pika_launch_token.as_deref(),
        Some(pending.launch_token.as_str())
    );
    assert!(
        pika.new_session("fresh_claude", Provider::Claude, false)
            .is_err(),
        "a repeated daily command must not create a second pending home"
    );
    assert_eq!(
        pika.store
            .list_pending()
            .unwrap()
            .iter()
            .filter(|item| item.name == "fresh_claude")
            .count(),
        0,
        "an immediately observable UUID-bearing provider is certified instead of left pending"
    );
    let expected = pending.expected_session_id.as_deref().unwrap();
    let tracked = pika
        .store
        .get_session(Provider::Claude, expected)
        .unwrap()
        .expect("the exact new Claude identity should be tracked immediately");
    assert_eq!(tracked.name.as_deref(), Some("fresh_claude"));
    assert_eq!(tracked.tmux_pane, pending.tmux_pane);

    // Exercise the actual new-process argv for every provider. Codex/OpenCode
    // issue their UUID only after startup, so simulate their lifecycle hook's
    // independently observed PID generation and exact pane certification.
    pika.config.write(&pika.paths).unwrap();
    for provider in Provider::ALL {
        let current = if provider == Provider::Claude {
            tracked.clone()
        } else {
            let name = format!("fresh_{provider}");
            let receipt = pika.new_session(&name, provider, false).unwrap();
            let OpenTarget::Pending(pending) = receipt.target else {
                panic!("new provider did not retain its launch identity")
            };
            let identity = if provider == Provider::Codex {
                "66666666-6666-4666-8666-666666666666"
            } else {
                "ses_freshfixture"
            };
            let deadline = Instant::now() + Duration::from_secs(4);
            let (pane, pid, generation) = loop {
                let pane = pika
                    .tmux
                    .get_pane(pending.tmux_pane.as_deref().unwrap())
                    .unwrap()
                    .unwrap();
                let processes = pikamux::process::observe();
                let processes = processes
                    .require_complete("certify isolated fake provider")
                    .unwrap();
                if let Some(pid) =
                    pikamux::process::provider_process(pane.pane_pid, Some(provider), processes)
                {
                    break (pane, pid, processes[&pid].start_time as i64);
                }
                assert!(Instant::now() < deadline, "fake new provider did not start");
                thread::sleep(Duration::from_millis(25));
            };
            let payload = pikamux::hooks::parse_hook_payload(
                serde_json::to_vec(&serde_json::json!({"session_id":identity,"hook_event_name":"SessionStart","cwd":pending.cwd})).unwrap().as_slice(),
                provider,
            ).unwrap();
            let mut context = pikamux::hooks::HookContext::at(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs_f64(),
            );
            context.expected_provider = Some(provider);
            context.desired_name = Some(name.clone());
            context.launch_token = Some(pending.launch_token.clone());
            context.owner_token = format!("isolated-{provider}");
            context.owner_pid = Some(pid);
            context.owner_start_time = Some(generation);
            context.pane_id = Some(pane.pane_id.clone());
            context.pane_session = Some(pane.session_name.clone());
            context.exact_home_verified = true;
            let hook =
                pikamux::hooks::handle_hook(&pika.store, provider, &payload, &context).unwrap();
            let tag = hook
                .tag_request
                .expect("new UUID must tag its exact launch pane");
            pika.tmux
                .tag_pane(
                    &pane.pane_id,
                    Some(provider),
                    Some(identity),
                    Some(&name),
                    Some(&pending.launch_token),
                )
                .unwrap();
            pika.store
                .finalize_pending_pane(
                    &pending.launch_token,
                    &pane.session_name,
                    &pane.pane_id,
                    Some(pid),
                    Some(generation),
                )
                .unwrap();
            assert!(
                pikamux::hooks::certify_hook_home(
                    &pika.store,
                    &pending.launch_token,
                    &tag,
                    pid,
                    generation
                )
                .unwrap()
            );
            pika.store.get_session(provider, identity).unwrap().unwrap()
        };
        assert_eq!(
            pika.open_session(current.clone(), false).unwrap().kind,
            "ATTACHED LIVE"
        );
        assert!(
            pika.capture_exact(&current, 20)
                .unwrap()
                .contains(&format!("fake {provider} ready"))
        );
        let publish = Command::new(env!("CARGO_BIN_EXE_pika"))
            .args([
                "expert",
                "publish",
                "--scope",
                "Exact new-home expertise",
                "--now",
                "Verifying return and peek",
                "--topic",
                "identity",
            ])
            .env("HOME", temp.path())
            .env("XDG_CONFIG_HOME", temp.path().join("xdg-config"))
            .env("XDG_STATE_HOME", temp.path().join("xdg-state"))
            .env("XDG_DATA_HOME", temp.path().join("xdg-data"))
            .env("PIKA_CONFIG_HOME", &pika.paths.config_dir)
            .env("PIKA_STATE_HOME", &pika.paths.state_dir)
            .env("PIKA_DB_PATH", &pika.paths.database)
            .env("CODEX_HOME", &pika.paths.codex_home)
            .env("CLAUDE_CONFIG_DIR", &pika.paths.claude_home)
            .env("OPENCODE_DATA_HOME", &pika.paths.opencode_data_home)
            .env("OPENCODE_CONFIG_DIR", &pika.paths.opencode_config_home)
            .env("PIKA_TMUX_SOCKET", &_guard.0)
            .env("PIKA_PROVIDER", provider.as_str())
            .env("PIKA_SESSION_ID", &current.session_id)
            .env("TMUX_PANE", current.tmux_pane.as_deref().unwrap())
            .output()
            .unwrap();
        assert!(
            publish.status.success(),
            "new {provider} expert publication failed: {}",
            String::from_utf8_lossy(&publish.stderr)
        );
        assert!(
            pika.store
                .get_stored_expert_profile(provider, &current.session_id)
                .unwrap()
                .is_some()
        );
    }

    // A closed Claude history has no live registry and legitimately no cwd.
    // Exact resume keeps its UUID and uses the caller's directory only for that
    // absent value; it does not require manual adoption or name cleanup.
    let history_id = "77777777-7777-4777-8777-777777777777";
    let history = pika
        .paths
        .claude_home
        .join(format!("projects/history/{history_id}.jsonl"));
    fs::create_dir_all(history.parent().unwrap()).unwrap();
    fs::write(
        &history,
        "{\"type\":\"custom-title\",\"customTitle\":\"history_only\"}\n",
    )
    .unwrap();
    let discovered = pika.resolve_local("history_only").unwrap().remove(0);
    assert!(discovered.cwd.is_none());
    let resumed = pika.open_session(discovered, false).unwrap();
    let OpenTarget::Session(resumed) = resumed.target else {
        panic!("history resume lost exact identity")
    };
    assert_eq!(resumed.session_id, history_id);
    assert_eq!(
        resumed.cwd.as_deref(),
        Some(std::env::current_dir().unwrap().to_str().unwrap())
    );
}
