#![cfg(unix)]

use pikamux::{model::Provider, tmux::Tmux};
use std::{
    collections::BTreeMap,
    io::{IsTerminal, Write},
    process::Command,
    thread,
    time::{Duration, Instant},
};

struct PrivateServer(String);

impl Drop for PrivateServer {
    fn drop(&mut self) {
        let _ = Command::new("tmux")
            .args(["-L", &self.0, "kill-server"])
            .output();
    }
}

// A provider-side detector, not a Pika environment-value assertion. The
// supports-color family used by Claude recognizes COLORTERM=truecolor before
// falling back to TERM patterns; tmux-direct alone matches neither fallback.
// Real supports-color is also exercised separately without a model call.
#[test]
#[ignore = "subprocess fake provider; only run inside a disposable PTY"]
fn color_provider_fixture() {
    if std::env::var("PIKA_COLOR_FIXTURE").as_deref() != Ok("1") {
        return;
    }
    assert!(std::io::stdout().is_terminal());
    if std::env::var("COLORTERM").as_deref() == Ok("truecolor") {
        print!("\x1b[38;2;215;119;87mThinking orange\x1b[0m\r\n");
    } else {
        print!("Thinking orange\r\n");
    }
    std::io::stdout().flush().unwrap();
    thread::sleep(Duration::from_secs(20));
}

#[test]
fn managed_harness_keeps_rgb_despite_a_stale_color_hint() {
    let temp = tempfile::tempdir().unwrap();
    let socket = format!("pika-color-{}", std::process::id());
    let _server = PrivateServer(socket.clone());
    // Start from a realistic SSH environment on a private, config-free server.
    let started = Command::new("tmux")
        .args([
            "-L",
            &socket,
            "-f",
            "/dev/null",
            "new-session",
            "-d",
            "-s",
            "seed",
            "sleep 30",
        ])
        .env_remove("COLORTERM")
        .env_remove("FORCE_COLOR")
        .env_remove("TERM_PROGRAM")
        .env("TERM", "xterm-256color")
        .current_dir(temp.path())
        .output()
        .unwrap();
    assert!(started.status.success(), "{started:?}");
    // Recent tmux versions may insert COLORTERM themselves. Reproduce an
    // older/long-lived server's blank capability hint instead of accidentally
    // letting the test host repair the launcher bug for us.
    assert!(
        Command::new("tmux")
            .args(["-L", &socket, "set-environment", "-g", "COLORTERM", ""])
            .status()
            .unwrap()
            .success()
    );
    let tmux = Tmux::with_executable("tmux", Some(socket));
    for provider in [
        Provider::Claude,
        Provider::Codex,
        Provider::Opencode,
        Provider::Muse,
    ] {
        let name = format!("pika-c-color-{provider}");
        let pane = tmux
            .create_agent_session(
                &name,
                temp.path().to_str().unwrap(),
                provider,
                &[
                    std::env::current_exe()
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                    "--exact".into(),
                    "color_provider_fixture".into(),
                    "--ignored".into(),
                    "--nocapture".into(),
                ],
                &BTreeMap::from([
                    ("PIKA_COLOR_FIXTURE".into(), "1".into()),
                    // Explicitly emulate the older tmux launch environment; new
                    // tmux versions insert truecolor even after set-environment.
                    ("COLORTERM".into(), "".into()),
                ]),
                None,
                "color fixture",
                Some("color-fixture-launch"),
            )
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let captured = loop {
            let output = tmux.capture(&pane.pane_id, 30).unwrap();
            if output.contains("Thinking orange") {
                break output;
            }
            assert!(Instant::now() < deadline, "{provider}: {output:?}");
            thread::sleep(Duration::from_millis(20));
        };
        assert!(
            captured.contains("38;2;215;119;87m"),
            "{provider} lost orange: {captured:?}"
        );
    }
}
