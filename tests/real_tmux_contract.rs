#![cfg(unix)]

use pikamux::{
    config::Config,
    core::{OpenTarget, Pika},
    model::{Provider, Session, Status},
    paths::Paths,
    store::Store,
    tmux::Tmux,
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
    let child = match Command::new("script")
        .args([
            "-q",
            transcript.to_str().unwrap(),
            "tmux",
            "-L",
            &socket,
            "attach-session",
            "-t",
            &pane,
        ])
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
                "#{client_pid}\t#{pane_id}",
            ])
            .output()
            .unwrap();
        if output.status.success()
            && String::from_utf8_lossy(&output.stdout).lines().any(|line| {
                line.split_once('\t').is_some_and(|(pid, selected)| {
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
