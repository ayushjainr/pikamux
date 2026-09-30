//! Real-keyboard setup journeys, with fake providers/services and disposable state.
#![cfg(unix)]
use std::{
    fs::{self, File},
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{fs::PermissionsExt, process::CommandExt},
    },
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

struct Journey {
    root: tempfile::TempDir,
    child: Child,
    terminal: File,
    output: String,
}

impl Journey {
    fn start(args: &[&str], width: u16, height: u16) -> Self {
        Self::start_with_io(args, width, height, true, true, Some("xterm-256color"))
    }

    fn start_with_io(
        args: &[&str],
        width: u16,
        height: u16,
        input_tty: bool,
        output_tty: bool,
        term: Option<&str>,
    ) -> Self {
        Self::start_prepared(args, width, height, input_tty, output_tty, term, |_| {})
    }

    fn start_prepared(
        args: &[&str],
        width: u16,
        height: u16,
        input_tty: bool,
        output_tty: bool,
        term: Option<&str>,
        prepare: impl FnOnce(&std::path::Path),
    ) -> Self {
        let root = tempfile::Builder::new()
            .prefix("pika-onboard-")
            .tempdir_in("/tmp")
            .unwrap();
        for dir in [
            "home",
            "bin",
            "config",
            "state",
            "data",
            "cache",
            "tmp",
            "codex",
            "claude",
            "oc",
            "oc-config",
            "sockets",
        ] {
            fs::create_dir_all(root.path().join(dir)).unwrap();
        }
        for (name, version) in [
            ("codex", "codex-cli 0.154.0"),
            ("claude", "claude 1.0.0"),
            ("opencode", "opencode 1.18.21"),
        ] {
            executable(
                &root.path().join("bin").join(name),
                &format!(
                    "#!/bin/sh\nif [ \"$1\" = --version ]; then printf '%s\\n' '{version}'; exit 0; fi\nprintf 'unexpected provider call\\n' >> \"$HOME/provider-calls\"\nexit 97\n"
                ),
            );
        }
        for name in ["ssh", "curl", "tailscale", "tmux"] {
            executable(&root.path().join("bin").join(name), "#!/bin/sh\nexit 97\n");
        }
        for name in ["launchctl", "systemctl"] {
            executable(&root.path().join("bin").join(name), "#!/bin/sh\nexit 0\n");
        }
        fs::write(
            root.path().join("claude/settings.json"),
            "{\"theme\":\"existing-theme\"}\n",
        )
        .unwrap();
        prepare(root.path());
        let (mut master, mut slave) = (-1, -1);
        let mut size = libc::winsize {
            ws_col: width,
            ws_row: height,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &raw mut size,
                )
            },
            0
        );
        let terminal = unsafe { File::from_raw_fd(master) };
        let child_terminal = unsafe { File::from_raw_fd(slave) };
        let flags = unsafe { libc::fcntl(master, libc::F_GETFL) };
        assert_eq!(
            unsafe { libc::fcntl(master, libc::F_SETFL, flags | libc::O_NONBLOCK) },
            0
        );
        let windows_fixture = args == ["windows-fixture"];
        let mut command = Command::new(if windows_fixture {
            std::env::current_exe().unwrap()
        } else {
            env!("CARGO_BIN_EXE_pika").into()
        });
        command.env_clear();
        if windows_fixture {
            command.args([
                "--exact",
                "windows_screen_fixture_entry",
                "--ignored",
                "--nocapture",
            ]);
            command.env("PIKA_TEST_WINDOWS_SCREEN", "1");
        } else {
            command.args(args);
        }
        for (key, dir) in [
            ("HOME", "home"),
            ("XDG_CONFIG_HOME", "config"),
            ("XDG_STATE_HOME", "state"),
            ("XDG_CACHE_HOME", "cache"),
            ("XDG_DATA_HOME", "data"),
            ("PIKA_CONFIG_HOME", "config/pika"),
            ("PIKA_STATE_HOME", "state/pika"),
            ("CODEX_HOME", "codex"),
            ("CLAUDE_CONFIG_DIR", "claude"),
            ("OPENCODE_DATA_HOME", "oc"),
            ("OPENCODE_CONFIG_DIR", "oc-config"),
            ("TMPDIR", "tmp"),
            ("TMUX_TMPDIR", "sockets"),
        ] {
            command.env(key, root.path().join(dir));
        }
        command
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", root.path().join("bin").display()),
            )
            .env("NO_COLOR", "1")
            .env("PIKA_UPDATE_CHECK", "0")
            .env("PIKA_TMUX_SOCKET", "onboarding-test")
            .stdin(if input_tty {
                Stdio::from(child_terminal.try_clone().unwrap())
            } else {
                Stdio::null()
            })
            .stdout(if output_tty {
                Stdio::from(child_terminal.try_clone().unwrap())
            } else {
                Stdio::from(File::create(root.path().join("stdout.txt")).unwrap())
            })
            .stderr(Stdio::from(child_terminal.try_clone().unwrap()));
        if let Some(term) = term {
            command.env("TERM", term);
        }
        command.process_group(0);
        let child = command.spawn().unwrap();
        Self {
            root,
            child,
            terminal,
            output: String::new(),
        }
    }

    fn send(&mut self, keys: &[u8]) {
        self.output.clear();
        self.terminal.write_all(keys).unwrap();
    }

    fn drain(&mut self) {
        let mut buf = [0_u8; 32768];
        while let Ok(n) = self.terminal.read(&mut buf) {
            if n == 0 {
                break;
            }
            self.output.push_str(&String::from_utf8_lossy(&buf[..n]));
        }
    }

    fn await_text(&mut self, text: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            self.drain();
            if self.output.contains(text) {
                return;
            }
            assert!(
                self.child.try_wait().unwrap().is_none(),
                "exited: {}",
                self.output
            );
            assert!(Instant::now() < deadline, "missing {text}: {}", self.output);
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn finish(&mut self) {
        self.finish_with_code(0, true);
    }

    fn finish_with_code(&mut self, expected_code: i32, alternate_screen: bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            self.drain();
            if let Some(status) = self.child.try_wait().unwrap() {
                self.drain();
                self.output.push_str(
                    &fs::read_to_string(self.root.path().join("stdout.txt")).unwrap_or_default(),
                );
                assert_eq!(status.code(), Some(expected_code), "{}", self.output);
                if alternate_screen {
                    assert!(
                        self.output.contains("\x1b[?1049l"),
                        "alternate screen not restored: {}",
                        self.output
                    );
                } else {
                    assert!(
                        !self.output.contains("\x1b[?1049h"),
                        "noninteractive command entered full screen: {}",
                        self.output
                    );
                }
                assert!(
                    !self.output.contains("open terminal failed"),
                    "{}",
                    self.output
                );
                let mut attrs = unsafe { std::mem::zeroed::<libc::termios>() };
                assert_eq!(
                    unsafe { libc::tcgetattr(self.terminal.as_raw_fd(), &mut attrs) },
                    0
                );
                assert_ne!(attrs.c_lflag & libc::ICANON, 0, "raw typing mode leaked");
                return;
            }
            assert!(Instant::now() < deadline, "did not finish: {}", self.output);
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn assert_untouched(&self) {
        assert!(!self.root.path().join("config/pika/config.json").exists());
        assert_eq!(
            fs::read_to_string(self.root.path().join("claude/settings.json")).unwrap(),
            "{\"theme\":\"existing-theme\"}\n"
        );
        assert!(!self.root.path().join("home/provider-calls").exists());
    }
}

impl Drop for Journey {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn executable(path: &std::path::Path, contents: &str) {
    fs::write(path, contents).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

#[test]
fn first_pika_is_a_quiet_consent_screen_and_escape_preserves_settings() {
    let mut j = Journey::start(&[], 100, 32);
    j.await_text("Keep your board up to date");
    assert!(
        j.output
            .contains("Scheduled expert-card refresh may use model quota")
    );
    assert!(!j.output.contains("+++"));
    assert!(!j.output.contains("Commissioning status"));
    assert!(!j.output.contains("38;"), "NO_COLOR ignored");
    j.send(b"\x1b");
    j.finish();
    j.assert_untouched();
}

#[test]
fn redirected_input_or_output_uses_finite_commands_without_taking_over_the_terminal() {
    // Three independent noninteractive routes, not just both streams piped.
    // Keep stderr on the PTY so a subprocess's raw terminal error is observed.
    for (input_tty, output_tty) in [(false, true), (true, false), (false, false)] {
        let mut board =
            Journey::start_with_io(&[], 100, 32, input_tty, output_tty, Some("xterm-256color"));
        board.finish_with_code(0, false);
        assert!(
            board.output.contains("No conversations are tracked yet"),
            "{}",
            board.output
        );
        assert!(!board.output.contains("Enable updates"), "{}", board.output);
        board.assert_untouched();

        let mut setup = Journey::start_with_io(
            &["setup", "--dry-run", "--no-machines", "--no-import"],
            100,
            32,
            input_tty,
            output_tty,
            Some("xterm-256color"),
        );
        setup.finish_with_code(0, false);
        assert!(setup.output.contains("Dry run only"), "{}", setup.output);
        setup.assert_untouched();
    }
}

#[test]
fn setup_without_terminal_input_requires_explicit_consent_and_preserves_settings() {
    let mut setup = Journey::start_with_io(
        &["setup", "--no-machines", "--no-import"],
        100,
        32,
        false,
        true,
        Some("xterm-256color"),
    );
    setup.finish_with_code(2, false);
    assert!(
        setup.output.contains("pika setup --yes"),
        "{}",
        setup.output
    );
    setup.assert_untouched();
}

#[test]
fn dumb_terminal_setup_uses_a_plain_prompt_and_can_be_declined() {
    let mut setup = Journey::start_with_io(
        &["setup", "--no-machines", "--no-import"],
        100,
        32,
        true,
        true,
        Some("dumb"),
    );
    setup.await_text("Apply these changes?");
    setup.send(b"n\n");
    setup.finish_with_code(0, false);
    assert!(
        setup.output.contains("No changes applied"),
        "{}",
        setup.output
    );
    setup.assert_untouched();
}

#[test]
fn missing_term_setup_works_on_a_pty_without_a_controlling_terminal() {
    // openpty + process_group intentionally does not acquire a controlling
    // terminal. An SSH-like PTY on stdin/stdout is sufficient for setup.
    let mut setup = Journey::start_with_io(
        &["setup", "--no-machines", "--no-import"],
        100,
        32,
        true,
        true,
        None,
    );
    setup.await_text("Enable updates");
    setup.send(b"\x1b");
    setup.finish();
    setup.assert_untouched();
}

#[test]
fn detailed_diff_is_available_before_approval_and_cancel_writes_nothing() {
    let mut j = Journey::start(&["setup", "--no-machines", "--no-import"], 100, 32);
    j.await_text("Enable updates");
    j.send(b"\x1b[B\r");
    j.await_text("Changes to your settings");
    j.await_text("+++ ");
    j.send(b"\x1b");
    j.await_text("Enable updates");
    j.send(b"\x1b");
    j.finish();
    j.assert_untouched();
}

#[test]
fn approved_setup_preserves_settings_opens_board_and_never_interviews() {
    let mut j = Journey::start(&["setup", "--no-machines", "--no-import"], 100, 32);
    j.await_text("Enable updates");
    j.send(b"\r");
    j.await_text("Open board");
    assert!(!j.output.contains("Commissioning status"));
    assert!(!j.output.contains("Backup:"));
    assert!(j.output.contains("connection notice"));
    assert!(
        !j.root.path().join("home/provider-calls").exists(),
        "setup must not start a consultation"
    );
    j.send(b"\r");
    j.await_text("PIKA");
    // Wait for the board's own controls before sending its quit key.
    j.await_text("q leave");
    j.send(b"q");
    j.finish();
    assert!(j.root.path().join("config/pika/config.json").exists());
    assert!(
        fs::read_to_string(j.root.path().join("claude/settings.json"))
            .unwrap()
            .contains("existing-theme")
    );
}

#[test]
fn tiny_terminal_cannot_approve_an_invisible_choice() {
    let mut j = Journey::start(&["setup", "--no-machines", "--no-import"], 30, 8);
    j.await_text("Enlarge");
    j.send(b"\r");
    thread::sleep(Duration::from_millis(100));
    j.assert_untouched();
    j.send(b"\x1b");
    j.finish();
    j.assert_untouched();
}

#[test]
fn setup_finish_connects_saved_machines_without_repeating_provider_setup() {
    let mut j = Journey::start(&["setup", "--no-machines", "--no-import"], 100, 32);
    j.await_text("Enable updates");
    fs::create_dir_all(j.root.path().join("home/.ssh")).unwrap();
    fs::write(
        j.root.path().join("home/.ssh/config"),
        "Host lab\n  HostName lab.invalid\n",
    )
    .unwrap();
    executable(
        &j.root.path().join("bin/ssh"),
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$HOME/ssh-calls\"\nexit 97\n",
    );
    j.send(b"\r");
    j.await_text("Connect another machine");
    let hooks_before = fs::read(j.root.path().join("claude/settings.json")).unwrap();
    j.send(b"\x1b[B\r");
    j.await_text("Connect your machines");
    assert!(j.output.contains("lab"));
    assert!(!j.root.path().join("home/ssh-calls").exists());
    j.send(b" \r");
    j.await_text("Connection unavailable");
    assert!(j.output.contains("Retry connection"));
    j.send(b"\r");
    j.await_text("Open board");
    assert!(
        fs::read_to_string(j.root.path().join("home/ssh-calls"))
            .unwrap()
            .contains("lab")
    );
    assert!(!j.output.contains("Enable updates"));
    assert_eq!(
        fs::read(j.root.path().join("claude/settings.json")).unwrap(),
        hooks_before
    );
    j.send(b"\x1b[B\x1b[B\r");
    j.await_text("lab was not connected");
    j.send(b"\x1b");
    j.await_text("Open board");
    j.send(b"\x1b");
    j.finish();
}

#[test]
fn machine_address_entry_can_be_cancelled_without_connecting() {
    let mut j = Journey::start(&["setup", "--no-machines", "--no-import"], 100, 32);
    j.await_text("Enable updates");
    executable(
        &j.root.path().join("bin/ssh"),
        "#!/bin/sh\ntouch \"$HOME/ssh-calls\"\nexit 97\n",
    );
    j.send(b"\r");
    j.await_text("Connect another machine");
    j.send(b"\x1b[B\r");
    j.await_text("Enter a machine address");
    j.send(b" \r");
    j.await_text("Machine address");
    j.send(b"user@example.invalid\x1b");
    j.await_text("Open board");
    assert!(!j.root.path().join("home/ssh-calls").exists());
    j.send(b"\x1b");
    j.finish();
}

#[test]
fn machine_connection_needs_verified_snapshot_and_explicit_retry() {
    use serde_json::json;
    let mut j = Journey::start(&["setup", "--no-machines", "--no-import"], 100, 32);
    j.await_text("Enable updates");
    fs::create_dir_all(j.root.path().join("home/.ssh")).unwrap();
    fs::write(
        j.root.path().join("home/.ssh/config"),
        "Host lab\n  HostName lab.invalid\n",
    )
    .unwrap();
    let node = "99999999-9999-4999-8999-999999999999";
    let hello = json!({"type":"hello", "protocol":pikamux::fleet::PROTOCOL_NAME,"version":pikamux::fleet::PROTOCOL_VERSION,"node_id":node,"machine":"lab","package_version":pikamux::VERSION,"capabilities":pikamux::fleet::CAPABILITIES});
    let snapshot = json!({"type":"snapshot","protocol":pikamux::fleet::PROTOCOL_NAME,"version":pikamux::fleet::PROTOCOL_VERSION,"node_id":node,"machine":"lab","captured_at":1,"sessions":[],"profiles":[],"cards":[]});
    executable(
        &j.root.path().join("bin/ssh"),
        &format!(
            r#"#!/bin/sh
IFS= read -r request
printf '%s\n' "$request" >> "$HOME/ssh-requests"
case "$request" in
  *hello*) printf '%s\n' '{hello}' ;;
  *snapshot*)
    if [ -f "$HOME/retry-ready" ]; then printf '%s\n' '{snapshot}'; else printf '%s\n' '{{"incomplete":true}}'; fi ;;
  *) exit 97 ;;
esac
"#
        ),
    );
    j.send(b"\r");
    j.await_text("Connect another machine");
    j.send(b"\x1b[B\r");
    j.await_text("lab");
    j.send(b" \r");
    j.await_text("Connection unavailable");
    let store = pikamux::store::Store::at(j.root.path().join("state/pika/pika.db"));
    assert!(
        store.list_nodes().unwrap().is_empty(),
        "SSH hello alone is not a connected machine"
    );
    assert_eq!(
        fs::read_to_string(j.root.path().join("home/ssh-requests"))
            .unwrap()
            .lines()
            .count(),
        2
    );
    fs::write(j.root.path().join("home/retry-ready"), "ready").unwrap();
    j.send(b"\x1b[B\r");
    j.await_text("Machines connected");
    assert!(j.output.contains("lab"));
    assert_eq!(store.list_nodes().unwrap().len(), 1);
    assert!(store.has_remote_snapshot(node).unwrap());
    assert_eq!(
        fs::read_to_string(j.root.path().join("home/ssh-requests"))
            .unwrap()
            .lines()
            .count(),
        4
    );
    j.send(b"\x1b");
    j.await_text("Open board");
    j.send(b"\x1b");
    j.finish();
    assert!(!j.root.path().join("home/provider-calls").exists());
}

#[test]
fn assistant_first_use_is_native_and_unsupported_provider_launches_nothing() {
    let mut j = Journey::start_prepared(
        &["pika"],
        100,
        32,
        true,
        true,
        Some("xterm-256color"),
        |root| {
            native_probe_fixture(&root.join("bin/codex"));
        },
    );
    j.finish_with_code(1, false);
    assert!(j.output.contains("capability"), "{}", j.output);
    assert!(!j.output.contains("Enter to connect Pika"));
    assert!(!j.output.contains("Connect with Codex"));
    assert!(!j.output.contains("Sign in to Codex"));
    let profile = j.root.path().join("state/pika/assistant");
    let probe = fs::read_to_string(profile.join("provider-home/probe-context")).unwrap();
    assert!(probe.contains("debug prompt-input"));
    assert!(probe.contains("--model gpt-6-luna"));
    assert!(probe.contains("--profile pika-assistant"));
    assert!(!profile.join("provider-home/paid-turn").exists());
    assert!(!profile.join("native-registry/pika.db").exists());
    assert!(
        !j.root
            .path()
            .join("state/pika/assistant-startup/selection.json")
            .exists()
    );
    assert!(!j.root.path().join("codex/auth.json").exists());
    let db = rusqlite::Connection::open(profile.join("memory.sqlite")).unwrap();
    let messages: i64 = db
        .query_row("SELECT count(*) FROM memory_records", [], |row| row.get(0))
        .unwrap();
    assert_eq!(
        messages, 0,
        "provider capability inspection is not a human message"
    );
}

#[test]
fn assistant_finite_saved_allowance_is_preserved_without_login_or_launch() {
    let mut j = Journey::start_prepared(
        &["pika"],
        100,
        32,
        true,
        true,
        Some("xterm-256color"),
        |root| {
            save_native_selection(root, &root.join("bin/codex"), 7);
        },
    );
    j.finish_with_code(1, false);
    assert!(j.output.contains("per-model-call ceiling"), "{}", j.output);
    let selection: serde_json::Value = serde_json::from_slice(
        &fs::read(
            j.root
                .path()
                .join("state/pika/assistant-startup/selection.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(selection["max_calls"], 7);
    assert!(!j.root.path().join("home/provider-calls").exists());
    assert!(
        !j.root
            .path()
            .join("state/pika/assistant/native-binding.json")
            .exists()
    );
    assert!(
        !j.root
            .path()
            .join("state/pika/assistant/provider-home/auth.json")
            .exists()
    );
    assert!(!j.output.contains("Sign in to Codex"));
}

#[test]
fn assistant_native_entry_keeps_exact_saved_provider_profile_and_authentication() {
    let mut j = Journey::start_prepared(
        &["pika"],
        100,
        32,
        true,
        true,
        Some("xterm-256color"),
        |root| {
            let pinned = root.join("bin/pinned-codex");
            native_probe_fixture(&pinned);
            save_native_selection(root, &pinned, i64::MAX as u64);
            let home = root.join("state/pika/assistant/provider-home");
            fs::create_dir_all(&home).unwrap();
            fs::set_permissions(&home, fs::Permissions::from_mode(0o700)).unwrap();
            fs::write(
                home.join("auth.json"),
                r#"{"OPENAI_API_KEY":"synthetic-private-auth"}"#,
            )
            .unwrap();
            fs::write(
                home.join("config.toml"),
                "theme = 'existing-private-theme'\nsandbox_mode = 'danger-full-access'\n",
            )
            .unwrap();
            for name in ["auth.json", "config.toml"] {
                fs::set_permissions(home.join(name), fs::Permissions::from_mode(0o600)).unwrap();
            }
            executable(
                &root.join("bin/codex"),
                "#!/bin/sh\nprintf decoy > \"$HOME/provider-decoy\"\nexit 97\n",
            );
        },
    );
    j.finish_with_code(1, false);
    let root = j.root.path().join("state/pika/assistant");
    let selection: serde_json::Value = serde_json::from_slice(
        &fs::read(
            j.root
                .path()
                .join("state/pika/assistant-startup/selection.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(selection["profile_root"], root.to_str().unwrap());
    assert_eq!(
        selection["executable"],
        j.root.path().join("bin/pinned-codex").to_str().unwrap()
    );
    let context = fs::read_to_string(root.join("provider-home/probe-context")).unwrap();
    assert!(context.contains(root.join("provider-home").to_str().unwrap()));
    assert!(context.contains(root.join("native-assistant").to_str().unwrap()));
    assert!(context.contains("--ask-for-approval never"));
    assert!(!j.root.path().join("home/provider-decoy").exists());
    assert!(!j.root.path().join("home/provider-calls").exists());
    assert!(!root.join("provider-home/paid-turn").exists());
    assert_eq!(
        fs::read_to_string(root.join("provider-home/auth.json")).unwrap(),
        r#"{"OPENAI_API_KEY":"synthetic-private-auth"}"#
    );
    assert_eq!(
        fs::read_to_string(root.join("provider-home/config.toml")).unwrap(),
        "theme = 'existing-private-theme'\nsandbox_mode = 'danger-full-access'\n"
    );
    assert!(!j.root.path().join("codex/auth.json").exists());
    assert!(!root.join("native-registry/pika.db").exists());
    let binding: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("native-binding.json")).unwrap()).unwrap();
    assert_eq!(binding["profile_id"], selection["profile_id"]);
    assert_eq!(binding["scope"], selection["scope"]);
    assert_eq!(binding["provider_executable"], selection["executable"]);
    let db = rusqlite::Connection::open(root.join("memory.sqlite")).unwrap();
    let messages: i64 = db
        .query_row("SELECT count(*) FROM memory_records", [], |row| row.get(0))
        .unwrap();
    assert_eq!(messages, 0);
}

fn save_native_selection(root: &std::path::Path, executable: &std::path::Path, max_calls: u64) {
    let profile = root.join("state/pika/assistant");
    let memory = pikamux::assistant_memory::Store::open(profile.join("memory.sqlite")).unwrap();
    let selection = root.join("state/pika/assistant-startup/selection.json");
    fs::create_dir_all(selection.parent().unwrap()).unwrap();
    fs::set_permissions(
        selection.parent().unwrap(),
        fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    fs::write(
        &selection,
        serde_json::to_vec(&serde_json::json!({
            "profile_root":profile, "profile_id":memory.profile_id(),
            "scope":"personal", "executable":executable, "max_calls":max_calls
        }))
        .unwrap(),
    )
    .unwrap();
    fs::set_permissions(selection, fs::Permissions::from_mode(0o600)).unwrap();
}

fn native_probe_fixture(path: &std::path::Path) {
    executable(
        path,
        r#"#!/bin/sh
case " $* " in
  *" debug prompt-input "*)
    printf '%s\n%s\n%s\n' "$CODEX_HOME" "$PWD" "$*" > "$CODEX_HOME/probe-context"
    printf '[]\n'
    exit 0 ;;
esac
printf unexpected > "$CODEX_HOME/paid-turn"
exit 97
"#,
    );
}

#[test]
fn choosing_work_keeps_identity_explicit_and_does_not_import_everything() {
    let mut j = Journey::start(&["setup", "--no-machines"], 100, 32);
    j.await_text("Enable updates");
    let sessions = j.root.path().join("claude/sessions");
    fs::create_dir_all(&sessions).unwrap();
    for (id, name) in [
        ("20000000-0000-4000-8000-000000000001", "returns_tracker"),
        ("20000000-0000-4000-8000-000000000002", "strategy_dashboard"),
    ] {
        fs::write(sessions.join(format!("{id}.json")), serde_json::to_vec(&serde_json::json!({"kind":"interactive", "sessionId":id, "name":name, "nameSource":"custom", "cwd":"/project", "updatedAt":100})).unwrap()).unwrap();
    }
    j.send(b"\r");
    j.await_text("Choose the work you want to see");
    assert!(j.output.contains("returns_tracker"));
    assert!(j.output.contains("strategy_dashboard"));
    j.send(b" \r");
    j.await_text("Anything else to bring in?");
    j.send(b"\r");
    j.await_text("Open board");
    assert!(j.output.contains("1 conversation(s)"));
    j.send(b"\x1b");
    j.finish();
    let store = pikamux::store::Store::at(j.root.path().join("state/pika/pika.db"));
    assert_eq!(store.list_sessions().unwrap().len(), 1);
    assert!(!j.root.path().join("home/provider-calls").exists());
}

#[test]
#[ignore = "subprocess fixture entrypoint"]
fn windows_screen_fixture_entry() {
    if std::env::var("PIKA_TEST_WINDOWS_SCREEN").as_deref() != Ok("1") {
        return;
    }
    use pikamux::client_bridge::{ClientConfig, LoopbackEndpoint};
    use pikamux::client_cli::{ClientBridgeOptions, ClientCliRuntime};
    use serde_json::{Value, json};
    struct Fake {
        config: ClientConfig,
        calls: Vec<String>,
        opened: usize,
    }
    impl ClientCliRuntime for Fake {
        fn styled_setup(&self) -> bool {
            true
        }
        fn interactive(&self) -> bool {
            true
        }
        fn discover_hosts(&mut self) -> anyhow::Result<Vec<String>> {
            Ok(vec![
                "saved-mac".into(),
                "offline-linux".into(),
                "unselected".into(),
            ])
        }
        fn load_config(&mut self) -> anyhow::Result<ClientConfig> {
            Ok(self.config.clone())
        }
        fn save_config(&mut self, config: &ClientConfig) -> anyhow::Result<()> {
            self.config = config.clone();
            Ok(())
        }
        fn client_label(&mut self) -> anyhow::Result<String> {
            Ok("fixture-windows".into())
        }
        fn pairing_token(&mut self) -> anyhow::Result<String> {
            Ok("a".repeat(64))
        }
        fn ssh_json(
            &mut self,
            target: &str,
            _args: &[String],
            payload: &Value,
            _exe: &str,
            _timeout: Duration,
        ) -> anyhow::Result<Value> {
            self.calls.push(target.into());
            if target == "offline-linux" {
                anyhow::bail!("SSH connection timed out (fixture)");
            }
            let node = "99999999-9999-4999-8999-999999999999";
            if payload["op"] == "hello" {
                Ok(
                    json!({"type":"hello", "protocol":pikamux::fleet::PROTOCOL_NAME,"version":pikamux::fleet::PROTOCOL_VERSION,"node_id":node,"machine":"saved-mac","package_version":pikamux::VERSION,"capabilities":pikamux::fleet::CAPABILITIES}),
                )
            } else {
                Ok(
                    json!({"type":"paired","protocol":pikamux::client_bridge::BRIDGE_PROTOCOL,"version":pikamux::client_bridge::BRIDGE_VERSION,"node_id":node,"client_id":payload["client_id"],"port":payload["port"]}),
                )
            }
        }
        fn open_fleet_board(&mut self, config: &ClientConfig) -> anyhow::Result<i32> {
            self.opened = config.nodes.len();
            Ok(0)
        }
        fn bridge_running(&mut self, _: &LoopbackEndpoint) -> bool {
            panic!("setup must not start a bridge")
        }
        fn start_bridge(&mut self, _: &ClientBridgeOptions) -> anyhow::Result<()> {
            panic!("setup must not start a bridge")
        }
        fn serve_bridge(&mut self, _: &ClientBridgeOptions) -> anyhow::Result<()> {
            panic!("setup must not start a bridge")
        }
    }
    let mut fake = Fake {
        config: ClientConfig::empty(),
        calls: Vec::new(),
        opened: 0,
    };
    let code = pikamux::client_cli::run_with(
        ["pika"],
        &mut fake,
        &mut std::io::stdout(),
        &mut std::io::stderr(),
    )
    .unwrap();
    assert_eq!(code, 0);
    fs::write(
        std::path::Path::new(&std::env::var("HOME").unwrap()).join("client-evidence.json"),
        serde_json::to_vec(
            &json!({"calls":fake.calls,"paired":fake.config.nodes.len(),"opened":fake.opened}),
        )
        .unwrap(),
    )
    .unwrap();
}

#[test]
fn windows_screen_pairs_only_selected_hosts_and_keeps_success_after_failure() {
    let mut j = Journey::start(&["windows-fixture"], 100, 32);
    j.await_text("Choose saved connections");
    j.send(b"\r");
    j.await_text("Choose your machines");
    j.send(b" \x1b[B \r");
    j.await_text("Connection unavailable");
    j.send(b"\r");
    j.await_text("Some connections need attention");
    assert!(j.output.contains("1 saved connection(s)"));
    assert!(!j.output.contains("PAIRED"));
    j.send(b"\r");
    j.finish();
    let evidence: serde_json::Value =
        serde_json::from_slice(&fs::read(j.root.path().join("home/client-evidence.json")).unwrap())
            .unwrap();
    assert_eq!(
        evidence["calls"],
        serde_json::json!(["saved-mac", "saved-mac", "offline-linux"])
    );
    assert_eq!(evidence["paired"], 1);
    assert_eq!(evidence["opened"], 1);
}

#[test]
fn windows_screen_cancel_never_contacts_a_host() {
    let mut j = Journey::start(&["windows-fixture"], 100, 32);
    j.await_text("Choose saved connections");
    j.send(b"\x1b");
    j.finish();
    let evidence: serde_json::Value =
        serde_json::from_slice(&fs::read(j.root.path().join("home/client-evidence.json")).unwrap())
            .unwrap();
    assert_eq!(evidence["calls"], serde_json::json!([]));
    assert_eq!(evidence["paired"], 0);
}
