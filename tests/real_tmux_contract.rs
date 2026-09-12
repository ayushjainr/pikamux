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
    process::Command,
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
}
