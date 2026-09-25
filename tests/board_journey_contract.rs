//! Exercise the actual Unix executable through a disposable terminal, not just
//! BoardAction enums. No provider, real SSH endpoint, or user tmux server is used.
#![cfg(unix)]

use pikamux::{
    model::{ObservationKind, Status, StatusObservation},
    store::Store,
};
use std::{
    fs::{self, File},
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{fs::PermissionsExt, process::CommandExt},
    },
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

struct BoardProcess {
    child: Child,
    terminal: File,
    output: String,
    root: tempfile::TempDir,
    real_tmux: bool,
}

impl Drop for BoardProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if self.real_tmux {
            let _ = Command::new(self.root.path().join("bin/tmux"))
                .env_clear()
                .env("HOME", self.root.path().join("home"))
                .env("TMUX_TMPDIR", self.root.path().join("sockets"))
                .args(["-L", "board-journey", "kill-server"])
                .output();
        }
    }
}

impl BoardProcess {
    fn start() -> Self {
        // Keep the Unix socket path below the platform's address-length limit.
        let root = tempfile::Builder::new()
            .prefix("pika-board-")
            .tempdir_in("/tmp")
            .unwrap();
        for dir in [
            "bin", "home", "config", "state", "cache", "data", "codex", "claude", "oc", "tmp",
            "sockets",
        ] {
            fs::create_dir_all(root.path().join(dir)).unwrap();
        }
        for name in [
            "tmux",
            "codex",
            "claude",
            "opencode",
            "ssh",
            "curl",
            "tailscale",
        ] {
            let path = root.path().join("bin").join(name);
            fs::write(
                &path,
                if name == "tmux" {
                    "#!/bin/sh\nprintf 'no server running\\n' >&2\nexit 1\n"
                } else {
                    "#!/bin/sh\nexit 97\n"
                },
            )
            .unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let store = Store::at(root.path().join("state/pika.db"));
        store.initialize().unwrap();
        let db = rusqlite::Connection::open(store.path()).unwrap();
        db.execute(
            "INSERT INTO sessions(provider,session_id,name,status,unread,managed,source,created_at,updated_at,last_event_at,last_activity_at) VALUES ('codex','aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa','audit_saved','READY',1,1,'fixture',1,1,1,1)",
            [],
        ).unwrap();
        db.execute(
            "INSERT INTO sessions(provider,session_id,name,status,unread,managed,source,created_at,updated_at,last_event_at,last_activity_at) VALUES ('codex','bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb','Generated provider title','READY',1,0,'external',1,1,1,1)",
            [],
        ).unwrap();
        drop(db);
        // The board may reconcile before the first key. Seed authoritative
        // lifecycle evidence, not only the compatibility-cache unread column.
        store
            .record_status_observation(
                pikamux::model::Provider::Codex,
                "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
                &StatusObservation {
                    kind: ObservationKind::Lifecycle,
                    status: Status::Ready,
                    unread: true,
                    attention_reason: Some("completed".into()),
                    error: None,
                    observed_at: 1.0,
                    source: "fixture-completion".into(),
                },
            )
            .unwrap();
        let (mut master, mut slave) = (-1, -1);
        let mut size = libc::winsize {
            ws_row: 32,
            ws_col: 140,
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
        let mut command = Command::new(env!("CARGO_BIN_EXE_pika"));
        command
            .env_clear()
            // Keep only the coverage destination, not caller credentials/state.
            .envs(std::env::var_os("LLVM_PROFILE_FILE").map(|value| ("LLVM_PROFILE_FILE", value)))
            .current_dir(root.path().join("home"))
            .env("HOME", root.path().join("home"))
            .env("XDG_CONFIG_HOME", root.path().join("config"))
            .env("XDG_STATE_HOME", root.path().join("state"))
            .env("XDG_CACHE_HOME", root.path().join("cache"))
            .env("XDG_DATA_HOME", root.path().join("data"))
            .env("PIKA_CONFIG_HOME", root.path().join("config/pika"))
            .env("PIKA_STATE_HOME", root.path().join("state"))
            .env("PIKA_DB_PATH", store.path())
            .env("CODEX_HOME", root.path().join("codex"))
            .env("CLAUDE_CONFIG_DIR", root.path().join("claude"))
            .env("OPENCODE_DATA_HOME", root.path().join("oc"))
            .env("OPENCODE_CONFIG_DIR", root.path().join("config/oc"))
            .env("TMPDIR", root.path().join("tmp"))
            .env("TMUX_TMPDIR", root.path().join("sockets"))
            .env("PIKA_TMUX_SOCKET", "board-journey")
            .env("PIKA_UPDATE_CHECK", "0")
            // Linux /bin/sh is often dash, which does not optimize a final
            // external command into exec as some other shells do.
            .env("SHELL", "/bin/sh")
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", root.path().join("bin").display()),
            )
            .env("TERM", "xterm-256color")
            .stdin(Stdio::from(child_terminal.try_clone().unwrap()))
            .stdout(Stdio::from(child_terminal.try_clone().unwrap()))
            .stderr(Stdio::from(child_terminal.try_clone().unwrap()));
        command.process_group(0);
        let child = command.spawn().unwrap();
        drop(child_terminal);
        let mut board = Self {
            child,
            terminal,
            output: String::new(),
            root,
            real_tmux: false,
        };
        board.await_text("audit_saved");
        board
    }

    fn send(&mut self, keys: &[u8]) {
        self.output.clear();
        self.terminal.write_all(keys).unwrap();
    }

    fn endpoint(&self, args: &[&str]) -> Command {
        let root = self.root.path();
        let mut command = Command::new(env!("CARGO_BIN_EXE_pika"));
        command
            .env_clear()
            .envs(std::env::var_os("LLVM_PROFILE_FILE").map(|value| ("LLVM_PROFILE_FILE", value)))
            .args(args)
            .env("HOME", root.join("home"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_STATE_HOME", root.join("state"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env("PIKA_CONFIG_HOME", root.join("config/pika"))
            .env("PIKA_STATE_HOME", root.join("state"))
            .env("PIKA_DB_PATH", root.join("state/pika.db"))
            .env("CODEX_HOME", root.join("codex"))
            .env("CLAUDE_CONFIG_DIR", root.join("claude"))
            .env("OPENCODE_DATA_HOME", root.join("oc"))
            .env("OPENCODE_CONFIG_DIR", root.join("config/oc"))
            .env("TMPDIR", root.join("tmp"))
            .env("TMUX_TMPDIR", root.join("sockets"))
            .env("PIKA_TMUX_SOCKET", "board-journey")
            .env("PIKA_UPDATE_CHECK", "0")
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", root.join("bin").display()),
            );
        command
    }

    fn tmux(&self, args: &[&str]) -> String {
        let output = Command::new(self.root.path().join("bin/tmux"))
            .env_clear()
            .env("HOME", self.root.path().join("home"))
            .env("TMUX_TMPDIR", self.root.path().join("sockets"))
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", self.root.path().join("bin").display()),
            )
            .args(["-L", "board-journey"])
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    fn await_text(&mut self, expected: &str) {
        let deadline = Instant::now() + Duration::from_secs(6);
        loop {
            let mut bytes = [0; 32768];
            match self.terminal.read(&mut bytes) {
                Ok(n) => self.output.push_str(&String::from_utf8_lossy(&bytes[..n])),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => panic!("terminal read: {e}; output: {}", self.output),
            }
            if self.output.contains(expected) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "missing {expected:?}; tmux: {}; output: {}",
                fs::read_to_string(self.root.path().join("tmux-trace")).unwrap_or_default(),
                self.output
            );
            assert!(
                self.child.try_wait().unwrap().is_none(),
                "board exited before {expected:?}: {}",
                self.output
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn finish(&mut self) {
        self.send(b"q");
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            // Drain pending frames while waiting: a real terminal consumes
            // output continuously, and a full PTY must not manufacture a hang.
            let mut bytes = [0; 32768];
            if let Ok(count) = self.terminal.read(&mut bytes) {
                self.output
                    .push_str(&String::from_utf8_lossy(&bytes[..count]));
            }
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success());
                // Drain bytes written between the last read and process exit.
                while let Ok(count) = self.terminal.read(&mut bytes) {
                    if count == 0 {
                        break;
                    }
                    self.output
                        .push_str(&String::from_utf8_lossy(&bytes[..count]));
                }
                break;
            }
            assert!(Instant::now() < deadline, "board did not quit");
            thread::sleep(Duration::from_millis(10));
        }
        let mut flags = std::mem::MaybeUninit::<libc::termios>::uninit();
        assert_eq!(
            unsafe { libc::tcgetattr(self.terminal.as_raw_fd(), flags.as_mut_ptr()) },
            0
        );
        assert_ne!(
            unsafe { flags.assume_init() }.c_lflag & libc::ICANON,
            0,
            "terminal left raw"
        );
        assert!(
            self.output.contains("\x1b[?1006l"),
            "SGR mouse reporting left enabled"
        );
        assert!(
            self.output.contains("\x1b[?1000l"),
            "click reporting left enabled"
        );
    }
}

#[test]
fn remote_board_feed_verifies_node_bounds_frames_and_clears_on_disconnect() {
    let board = BoardProcess::start();
    let store = Store::at(board.root.path().join("state/pika.db"));
    let node = store.ensure_local_node_id().unwrap();
    let trace = board.root.path().join("feed-trace");
    fs::write(
        board.root.path().join("bin/tmux"),
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {}\nexit 0\n",
            shell_words::quote(trace.to_str().unwrap())
        ),
    )
    .unwrap();
    let token = "dddddddddddddddddddddddddddddddd";
    let mut feed = board
        .endpoint(&["_board-feed", "--expected-node-id", &node, "--token", token])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = feed.stdin.take().unwrap();
    let wait = |expected: &str| {
        let deadline = Instant::now() + Duration::from_secs(3);
        while !fs::read_to_string(&trace)
            .unwrap_or_default()
            .contains(expected)
        {
            assert!(Instant::now() < deadline, "missing {expected}");
            thread::sleep(Duration::from_millis(10));
        }
    };
    // Fragmented frames and multiple frames in one write are both supported.
    input.write_all(b"1,2").unwrap();
    input.write_all(b",4,4,0\n2,1,4,4,0\n").unwrap();
    wait("2 need you");
    assert!(fs::read_to_string(&trace).unwrap().contains("1 need you"));
    input.write_all(b"v2|2,1,4,4,0|ts_quality @rs2a\n").unwrap();
    wait("↑ ts_quality @rs2a");
    drop(input);
    assert!(feed.wait().unwrap().success());
    wait(&format!("set-option -gu @pika_feed_{token}"));

    let wrong_token = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";
    let wrong = board
        .endpoint(&[
            "_board-feed",
            "--expected-node-id",
            "00000000-0000-4000-8000-000000000000",
            "--token",
            wrong_token,
        ])
        .output()
        .unwrap();
    assert!(!wrong.status.success());
    assert!(String::from_utf8_lossy(&wrong.stderr).contains("NODE IDENTITY CHANGED"));
    assert!(!fs::read_to_string(&trace).unwrap().contains(wrong_token));

    let mut bad = board
        .endpoint(&["_board-feed", "--expected-node-id", &node, "--token", token])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    bad.stdin.take().unwrap().write_all(&[b'1'; 513]).unwrap();
    assert!(!bad.wait().unwrap().success());
    assert!(
        !fs::read_to_string(&trace)
            .unwrap()
            .contains("11111111111111111111111111111111111111111")
    );
}

#[test]
fn unverified_terminal_requires_choice_and_never_relaunches_or_acknowledges() {
    let real_tmux = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|path| path.join("tmux"))
        .find(|path| path.is_file())
        .map(|path| fs::canonicalize(path).unwrap());
    let Some(real_tmux) = real_tmux else {
        eprintln!("tmux unavailable; unverified terminal journey was not exercised");
        return;
    };
    let mut board = BoardProcess::start();
    fs::write(
        board.root.path().join("bin/tmux"),
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {}\nexec {} -f /dev/null \"$@\"\n",
            shell_words::quote(board.root.path().join("tmux-trace").to_str().unwrap()),
            shell_words::quote(real_tmux.to_str().unwrap())
        ),
    )
    .unwrap();
    board.real_tmux = true;
    let launches = board.root.path().join("launches");
    let provider = board.root.path().join("bin/codex");
    fs::write(
        &provider,
        format!(
            "#!/bin/sh\n[ \"$*\" = 'app-server --stdio' ] && exit 97\nprintf '%s\\n' \"$*\" >> {}\nprintf 'EXISTING FAKE AGENT\\n'\nwhile :; do sleep 1; done\n",
            shell_words::quote(launches.to_str().unwrap())
        ),
    )
    .unwrap();
    // Simulate an existing provider picker: a genuine process, but no UUID
    // in its arguments. Neither tags nor liveness prove a conversation.
    board.tmux(&[
        "new-session",
        "-d",
        "-s",
        "pika-c-unverified",
        "-c",
        board.root.path().join("home").to_str().unwrap(),
        &format!(
            "exec {} resume",
            shell_words::quote(provider.to_str().unwrap())
        ),
    ]);
    for (key, value) in [
        ("@pika_provider", "codex"),
        ("@pika_session_id", "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"),
        ("@pika_name", "audit_saved"),
        ("@pika_launch_token", "fixture-existing-terminal"),
    ] {
        board.tmux(&["set-option", "-p", "-t", "pika-c-unverified", key, value]);
    }
    board.tmux(&["bind-key", "-T", "root", "F12", "detach-client"]);
    let before = board.tmux(&[
        "display-message",
        "-p",
        "-t",
        "pika-c-unverified",
        "#{pane_pid}",
    ]);
    board.send(b"/audit\r");
    board.await_text("FILTER audit");
    board.send(b"\r");
    board.await_text("Open existing terminal");
    board.send(b"\r"); // Cancel is the default, never an implicit attachment.
    board.await_text("FILTER audit");
    assert!(board.tmux(&["list-clients"]).trim().is_empty());
    board.send(b"\r");
    board.await_text("Open existing terminal");
    board.send(b"\x1b[B\r");
    board.await_text("UNVERIFIED TERMINAL");
    assert!(!board.output.contains("CONTINUITY PROVEN"));
    board.send(b"\x1b[24~");
    board.await_text("FILTER audit");
    assert_eq!(
        board.tmux(&[
            "display-message",
            "-p",
            "-t",
            "pika-c-unverified",
            "#{pane_pid}"
        ]),
        before
    );
    assert_eq!(fs::read_to_string(launches).unwrap(), "resume\n");
    let store = Store::at(board.root.path().join("state/pika.db"));
    let session = store
        .get_session(
            pikamux::model::Provider::Codex,
            "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
        )
        .unwrap()
        .unwrap();
    assert!(
        session.unread,
        "unverified terminal acknowledged the conversation"
    );
    assert!(
        store
            .get_recovery_owner(session.provider, &session.session_id)
            .unwrap()
            .is_none()
    );
    board.send(b"q");
    assert!(board.child.wait().unwrap().success());
}

#[test]
fn exact_open_detach_and_reopen_return_to_the_same_filtered_board() {
    let real_tmux = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|path| path.join("tmux"))
        .find(|path| path.is_file())
        .map(|path| fs::canonicalize(path).unwrap());
    let Some(real_tmux) = real_tmux else {
        eprintln!("tmux unavailable; real board handoff was not exercised");
        return;
    };
    let mut board = BoardProcess::start();
    fs::write(
        board.root.path().join("bin/tmux"),
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {}\nexec {} -f /dev/null \"$@\"\n",
            shell_words::quote(board.root.path().join("tmux-trace").to_str().unwrap()),
            shell_words::quote(real_tmux.to_str().unwrap())
        ),
    )
    .unwrap();
    board.real_tmux = true;
    fs::write(board.root.path().join("bin/codex"),
        "#!/bin/sh\ntest \"$1\" = resume || exit 97\nprintf '%s\\n' 'FAKE AGENT READY' '• Queued follow-up inputs' '  ? 1 question' '' '› Ask Codex to do anything' '' 'gpt-6-astra medium · ~/fixture · weekly 58% left'\nwhile :; do sleep 1; done\n"
    ).unwrap();
    board.send(b"/audit\r");
    board.await_text("FILTER audit");
    let mut original_pane = None;
    // Exercise both user-facing routes, not tmux's prefix sequence.
    for return_keys in [
        b"\x1b[24~".as_slice(),
        b"\x1b[<0;3;32M\x1b[<0;3;32m".as_slice(),
    ] {
        board.send(b"\r");
        board.await_text("CONTINUITY PROVEN");
        board.await_text("need you");
        for label in ["need you", "working", "ready", "parked"] {
            assert!(
                board.output.contains(label),
                "missing return-strip count {label}"
            );
        }
        let clients = board.tmux(&[
            "show-options",
            "-v",
            "-t",
            "pika-c-aaaaaaaaaa",
            "@pika_board_clients",
        ]);
        let clients: serde_json::Value = serde_json::from_str(&clients).unwrap();
        let token = clients.as_object().unwrap().values().next_back().unwrap()[1]
            .as_str()
            .unwrap();
        let option = format!("@pika_feed_{token}");
        // Even if a sender dies without cleanup, tmux evaluates the lease at
        // render time. Frozen values must never masquerade as live counts.
        board.tmux(&[
            "set-option",
            "-g",
            &option,
            "#{?#{<=:%s,1},99 need you,Board disconnected}",
        ]);
        assert_eq!(
            board
                .tmux(&["display-message", "-p", &format!("#{{T:{option}}}")])
                .trim(),
            "Board disconnected"
        );
        // The board is hidden, not closed. A real store commit must update the
        // attached agent's status row without another key or another inventory.
        let store = Store::at(board.root.path().join("state/pika.db"));
        for (status, expected) in [
            (Status::NeedsYou, "1 need you"),
            (Status::OpenTwice, "open twice"),
            (Status::Working, "1 working"),
        ] {
            board.output.clear();
            store
                .record_status_observation(
                    pikamux::model::Provider::Codex,
                    "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
                    &StatusObservation {
                        kind: ObservationKind::Lifecycle,
                        status,
                        unread: false,
                        attention_reason: Some("fixture live change".into()),
                        error: None,
                        observed_at: SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap()
                            .as_secs_f64(),
                        source: "fixture:live".into(),
                    },
                )
                .unwrap();
            let mut projected = store
                .get_session(
                    pikamux::model::Provider::Codex,
                    "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
                )
                .unwrap()
                .unwrap();
            projected.status = status;
            projected.unread = false;
            store.upsert_session(&projected, true).unwrap();
            board.await_text(expected);
            if status == Status::NeedsYou {
                // Non-UTF-8 tmux clients may replace the arrow glyph; the name
                // must still reach the actual terminal, and the format keeps it.
                board.await_text("audit_saved");
                let value = board.tmux(&["show-options", "-gqv", &option]);
                let narrow = board.tmux(&[
                    "display-message",
                    "-p",
                    &value.replace("#{client_width}", "72"),
                ]);
                assert!(
                    narrow.contains("audit_s") && !narrow.contains("audit_saved"),
                    "name did not shorten: {narrow}"
                );
                let tiny = board.tmux(&[
                    "display-message",
                    "-p",
                    &value.replace("#{client_width}", "60"),
                ]);
                assert!(!tiny.contains("audit"), "name displaced navigation: {tiny}");
            } else if status == Status::OpenTwice {
                board.await_text("audit_saved");
                let value = board.tmux(&["show-options", "-gqv", &option]);
                // Non-UTF-8 tmux replaces glyphs with underscores. The typed
                // reason and exact name still have to reach the real terminal.
                assert!(
                    value.contains("audit_saved") && value.contains("open twice"),
                    "{value}"
                );
                assert!(!value.contains("↑ audit_saved"));
                let narrow = board.tmux(&[
                    "display-message",
                    "-p",
                    &value.replace("#{client_width}", "87"),
                ]);
                assert!(
                    narrow.contains("open twice"),
                    "warning lost on narrow client: {narrow}"
                );
                assert!(narrow.contains("audit_s"));
            } else {
                let rendered = board.tmux(&["display-message", "-p", &format!("#{{T:{option}}}")]);
                assert!(
                    !rendered.contains("audit"),
                    "resolved request still visible: {rendered}"
                );
            }
            assert!(board.child.try_wait().unwrap().is_none());
        }
        // The actual configured bottom-bar key opens Files without leaving
        // this attachment or sending any input to the fake provider.
        let wide = return_keys == b"\x1b[24~";
        if wide {
            board.send(b"\x1b[20~"); // F9
        } else {
            // Resize the client terminal, not just its tmux window. A manually
            // undersized window leaves padding and platform-dependent mouse
            // coordinates that do not represent a user resizing their terminal.
            let size = libc::winsize {
                ws_row: 32,
                ws_col: 120,
                ws_xpixel: 0,
                ws_ypixel: 0,
            };
            assert_eq!(
                unsafe { libc::ioctl(board.terminal.as_raw_fd(), libc::TIOCSWINSZ, &size) },
                0
            );
            // This test PTY has no controlling-terminal foreground group, so
            // deliver the resize signal to Pika's outer terminal bridge. It
            // propagates the dimensions to the attached tmux client's PTY.
            assert_eq!(
                unsafe { libc::kill(board.child.id() as i32, libc::SIGWINCH) },
                0
            );
            let deadline = Instant::now() + Duration::from_secs(3);
            while board
                .tmux(&["display-message", "-p", "#{window_width}"])
                .trim()
                != "120"
            {
                let mut bytes = [0; 32768];
                while let Ok(n) = board.terminal.read(&mut bytes) {
                    if n == 0 {
                        break;
                    }
                }
                assert!(
                    Instant::now() < deadline,
                    "tmux did not observe resized client: {}",
                    board.tmux(&["list-clients", "-F", "#{client_width}x#{client_height}"])
                );
                thread::sleep(Duration::from_millis(20));
            }
            board.send(b"\x1b[<0;116;32M\x1b[<0;116;32m"); // Files click
        }
        board.await_text("q close");
        let viewer = board
            .tmux(&["display-message", "-p", "#{pane_id}"])
            .trim()
            .to_owned();
        let origin = board
            .tmux(&["show-options", "-pqv", "-t", &viewer, "@pika_files_source"])
            .trim()
            .to_owned();
        assert!(origin.starts_with('%'), "not a Files companion: {viewer}");
        let pid = board.tmux(&["display-message", "-p", "-t", &origin, "#{pane_pid}"]);
        let axis = if wide { "#{pane_left}" } else { "#{pane_top}" };
        let viewer_axis: usize = board
            .tmux(&["display-message", "-p", "-t", &viewer, axis])
            .trim()
            .parse()
            .unwrap();
        let origin_axis: usize = board
            .tmux(&["display-message", "-p", "-t", &origin, axis])
            .trim()
            .parse()
            .unwrap();
        assert!(
            if wide {
                viewer_axis < origin_axis
            } else {
                viewer_axis > origin_axis
            },
            "companion opened on wrong side"
        );
        assert_eq!(
            board.tmux(&["list-panes", "-t", &origin]).lines().count(),
            2
        );
        let reopen = board
            .endpoint(&["_files-open", "--pane", &origin])
            .output()
            .unwrap();
        assert!(
            reopen.status.success(),
            "{}",
            String::from_utf8_lossy(&reopen.stderr)
        );
        assert_eq!(
            board.tmux(&["list-panes", "-t", &origin]).lines().count(),
            2
        );
        assert!(
            board
                .tmux(&["show-options", "-pqv", "-t", &viewer, "@pika_provider"])
                .trim()
                .is_empty()
        );
        let summary = board.tmux(&[
            "display-message",
            "-p",
            "-t",
            &viewer,
            "#{E:@pika_board_summary}",
        ]);
        assert!(
            summary.contains("working"),
            "companion lost live feed: {summary}"
        );
        if wide {
            // Files captures mouse input, but the outer border must remain
            // tmux-owned and resize the companion without touching the agent.
            let before: u16 = board
                .tmux(&["display-message", "-p", "-t", &viewer, "#{pane_width}"])
                .trim()
                .parse()
                .unwrap();
            let border = before + 1; // SGR coordinates are one based; pane starts at zero.
            board.send(format!("\x1b[<0;{border};8M").as_bytes());
            board.send(
                format!(
                    "\x1b[<32;{};8M\x1b[<32;{};8M\x1b[<0;{};8m",
                    border + 3,
                    border + 6,
                    border + 6
                )
                .as_bytes(),
            );
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                let mut bytes = [0; 32768];
                while let Ok(n) = board.terminal.read(&mut bytes) {
                    if n == 0 {
                        break;
                    }
                }
                let after: u16 = board
                    .tmux(&["display-message", "-p", "-t", &viewer, "#{pane_width}"])
                    .trim()
                    .parse()
                    .unwrap();
                if after > before {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "outer tmux border did not resize the viewer"
                );
                thread::sleep(Duration::from_millis(20));
            }
        }
        board.send(b"q");
        let deadline = Instant::now() + Duration::from_secs(5);
        while board.tmux(&["list-panes", "-t", &origin]).lines().count() != 1 {
            let mut bytes = [0; 32768];
            while let Ok(n) = board.terminal.read(&mut bytes) {
                if n == 0 {
                    break;
                }
            }
            assert!(
                Instant::now() < deadline,
                "companion did not close: {}\n{}",
                board.tmux(&[
                    "list-panes",
                    "-t",
                    &origin,
                    "-F",
                    "#{pane_id}:#{pane_active}:#{pane_current_command}:#{pane_dead}"
                ]),
                board.tmux(&["capture-pane", "-p", "-t", &viewer])
            );
            thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(
            board.tmux(&["display-message", "-p", "-t", &origin, "#{pane_pid}"]),
            pid
        );
        if !wide {
            // tmux 3.4 groups any two button-1 clicks within 300 ms,
            // even at opposite ends of the status bar. This journey tests
            // distinct Files/return clicks, not its SecondClick gesture.
            thread::sleep(Duration::from_millis(350));
        }
        board.send(return_keys);
        board.await_text("FILTER audit");
        assert!(board.child.try_wait().unwrap().is_none());
        let store = Store::at(board.root.path().join("state/pika.db"));
        let mut session = store
            .get_session(
                pikamux::model::Provider::Codex,
                "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
            )
            .unwrap()
            .unwrap();
        assert!(session.tmux_pane.is_some());
        if let Some(pane) = &original_pane {
            assert_eq!(&session.tmux_pane, pane, "reopen created another home");
        } else {
            // The earlier exact opening acknowledged the original completion.
            // Publish a new authoritative lifecycle observation, not only an
            // unread compatibility-cache bit that reconciliation may restore.
            let completed_at = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs_f64();
            store
                .record_status_observation(
                    session.provider,
                    &session.session_id,
                    &StatusObservation {
                        kind: ObservationKind::Lifecycle,
                        status: Status::Ready,
                        unread: true,
                        attention_reason: Some("completed".into()),
                        error: None,
                        observed_at: completed_at,
                        source: "fixture:Stop".into(),
                    },
                )
                .unwrap();
            session.status = Status::Ready;
            session.unread = true;
            session.last_event_at = completed_at;
            store.upsert_session(&session, true).unwrap();
            board.send(b"r");
            board.await_text("Pane output");
            board.await_text("FAKE AGENT READY");
            assert!(board.output.contains("Queued follow-up inputs"));
            assert!(!board.output.contains("Ask Codex to do anything"));
            assert!(!board.output.contains("weekly 58% left"));
            // Native pane and raw capture still contain the provider's UI.
            let raw = board.tmux(&[
                "capture-pane",
                "-p",
                "-t",
                session.tmux_pane.as_deref().unwrap(),
            ]);
            assert!(raw.contains("Ask Codex to do anything"));
            assert!(raw.contains("weekly 58% left"));
            board.send(b"p");
            board.await_text("PEEK");
            board.await_text("FAKE AGENT READY");
            assert!(board.output.contains("Queued follow-up inputs"));
            assert!(!board.output.contains("Ask Codex to do anything"));
            assert!(!board.output.contains("weekly 58% left"));
            board.send(b"\x1b");
            board.await_text("FILTER audit");
            assert!(
                store
                    .get_session(session.provider, &session.session_id)
                    .unwrap()
                    .unwrap()
                    .unread,
                "automatic preview acknowledged the result"
            );
            let completion = store
                .status_observations(session.provider, &session.session_id)
                .unwrap()
                .into_iter()
                .find(|observation| observation.kind == ObservationKind::Lifecycle)
                .unwrap();
            assert_eq!(completion.observed_at, completed_at);
            assert_eq!(completion.status, Status::Ready);
            assert!(
                completion.unread,
                "automatic preview acknowledged the authoritative completion"
            );
            original_pane = Some(session.tmux_pane);
        }
    }
    // Preserve a user's F12 binding; the strip must advertise and use F11.
    board.tmux(&[
        "bind-key",
        "-T",
        "root",
        "F12",
        "display-message",
        "my-custom-F12",
    ]);
    board.send(b"\r");
    board.await_text("F11");
    board.send(b"\x1b[23~");
    board.await_text("FILTER audit");
    assert!(
        board
            .tmux(&["list-keys", "-T", "root"])
            .contains("my-custom-F12")
    );

    // For an existing tmux client, return to its previous session rather than
    // detaching the whole terminal. No agent is killed by either route.
    board.tmux(&[
        "new-session",
        "-d",
        "-s",
        "origin",
        "printf 'ORIGINAL BOARD\\n'; exec sleep 30",
    ]);
    board.send(b"\r");
    board.await_text("F11");
    let target = original_pane.flatten().unwrap();
    board.tmux(&["switch-client", "-t", "origin"]);
    board.tmux(&["switch-client", "-t", &target]);
    board.send(b"\x1b[23~");
    board.await_text("ORIGINAL BOARD");
    // Only fixture teardown uses tmux's conventional detach; the user-facing
    // return above has already landed on the originating session.
    board.tmux(&["detach-client"]);
    board.await_text("FILTER audit");
    board.finish();
}

#[test]
fn failed_open_returns_to_filtered_board_without_replaying_the_action() {
    let mut board = BoardProcess::start();
    board.send(b"/audit\r");
    board.await_text("FILTER audit");
    board.send(b"\r");
    board.await_text("OPEN NEEDS ATTENTION");
    assert!(
        board
            .output
            .contains("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa")
    );
    assert!(board.child.try_wait().unwrap().is_none());
    board.send(b"\x1b");
    board.await_text("FILTER audit");
    board.finish();
}

#[test]
fn add_named_native_conversation_without_setup_preserves_unread_and_cancel() {
    let mut board = BoardProcess::start();
    let identity = "cccccccc-cccc-4ccc-8ccc-cccccccccccc";
    let generated_claude = "dddddddd-dddd-4ddd-8ddd-dddddddddddd";
    let generated_codex = "eeeeeeee-eeee-4eee-8eee-eeeeeeeeeeee";
    fs::create_dir_all(board.root.path().join("claude/sessions")).unwrap();
    fs::create_dir_all(board.root.path().join("claude/projects/p")).unwrap();
    let claude_session = board
        .root
        .path()
        .join(format!("claude/sessions/{identity}.json"));
    fs::write(
        &claude_session,
        serde_json::to_vec(&serde_json::json!({
            "kind":"interactive", "sessionId":identity, "name":"native_renamed",
            "nameSource":"custom", "cwd":"/project", "updatedAt":50
        }))
        .unwrap(),
    )
    .unwrap();
    let transcript = board
        .root
        .path()
        .join(format!("claude/projects/p/{identity}.jsonl"));
    fs::write(
        &transcript,
        format!(
            "{}\n",
            serde_json::json!({
                "sessionId":identity, "isSidechain":false, "entrypoint":"cli"
            })
        ),
    )
    .unwrap();
    let generated_claude_session = board
        .root
        .path()
        .join(format!("claude/sessions/{generated_claude}.json"));
    fs::write(
        &generated_claude_session,
        serde_json::to_vec(&serde_json::json!({
            "kind":"interactive", "sessionId":generated_claude,
            "name":"Generated Claude title", "nameSource":"derived",
            "cwd":"/project", "updatedAt":90
        }))
        .unwrap(),
    )
    .unwrap();
    let generated_claude_transcript = board
        .root
        .path()
        .join(format!("claude/projects/p/{generated_claude}.jsonl"));
    fs::write(
        &generated_claude_transcript,
        format!(
            "{}\n",
            serde_json::json!({
                "type":"ai-title", "aiTitle":"Generated Claude title"
            })
        ),
    )
    .unwrap();
    let codex_transcript = board
        .root
        .path()
        .join(format!("codex/{generated_codex}.jsonl"));
    fs::write(
        &codex_transcript,
        format!(
            "{}\n",
            serde_json::json!({"type":"session_meta","payload":{}})
        ),
    )
    .unwrap();
    let db = rusqlite::Connection::open(board.root.path().join("codex/state_1.sqlite")).unwrap();
    db.execute_batch("CREATE TABLE threads(id TEXT PRIMARY KEY,name TEXT,cwd TEXT,rollout_path TEXT,created_at INTEGER,updated_at INTEGER,archived INTEGER)").unwrap();
    db.execute(
        "INSERT INTO threads VALUES(?1,'Generated Codex title','/project',?2,1,90,0)",
        rusqlite::params![generated_codex, codex_transcript.to_string_lossy()],
    )
    .unwrap();
    drop(db);
    let store = Store::at(board.root.path().join("state/pika.db"));
    let db = rusqlite::Connection::open(store.path()).unwrap();
    db.execute(
        "INSERT INTO sessions(provider,session_id,name,status,unread,managed,source,created_at,updated_at,last_event_at,last_activity_at) VALUES ('claude',?1,'native_renamed','READY',1,0,'external',1,1,1,1)",
        [identity],
    )
    .unwrap();
    drop(db);
    store
        .untrack_session(pikamux::model::Provider::Claude, identity)
        .unwrap();
    // A new provider event can arrive while the identity remains untracked.
    // Seed that unread projection after the tombstone so the explicit restore
    // must not acknowledge or replace it.
    let db = rusqlite::Connection::open(store.path()).unwrap();
    db.execute(
        "UPDATE sessions SET status='READY',unread=1,last_event_at=50,last_activity_at=50 WHERE provider='claude' AND session_id=?",
        [identity],
    )
    .unwrap();
    drop(db);
    assert!(
        store
            .is_untracked(pikamux::model::Provider::Claude, identity)
            .unwrap()
    );
    board.send(b"+");
    board.await_text("native_renamed");
    assert!(!board.output.contains("Generated Claude title"));
    assert!(!board.output.contains("Generated Codex title"));
    board.send(b"native_renamed\r");
    // The UUID is already visible in the candidate list. Differential rendering
    // need not emit that unchanged line again when confirmation opens.
    board.await_text("Confirm add");
    board.send(b"\x1b");
    board.await_text("candidate(s)");
    assert!(
        !store
            .is_watched(pikamux::model::Provider::Claude, identity)
            .unwrap()
    );
    assert!(
        store
            .is_untracked(pikamux::model::Provider::Claude, identity)
            .unwrap()
    );
    board.send(b"\r");
    board.await_text("Confirm add");
    board.send(b"\r");
    board.await_text("Added to board");
    assert!(
        store
            .is_watched(pikamux::model::Provider::Claude, identity)
            .unwrap()
    );
    assert!(
        !store
            .is_untracked(pikamux::model::Provider::Claude, identity)
            .unwrap()
    );
    assert!(
        store
            .get_session(pikamux::model::Provider::Claude, identity)
            .unwrap()
            .unwrap()
            .unread
    );
    assert!(!board.root.path().join("config/pika/config.json").exists());
    assert!(!board.root.path().join("codex/hooks.json").exists());
    assert!(!board.root.path().join("claude/settings.json").exists());
    board.send(b"\x1b");
    board.await_text("+ add");
    board.finish();
}

#[test]
fn board_excludes_unconfirmed_provider_titles_without_deleting_the_record() {
    let mut board = BoardProcess::start();
    assert!(!board.output.contains("Generated provider title"));
    let store = Store::at(board.root.path().join("state/pika.db"));
    let external = store
        .get_session(
            pikamux::model::Provider::Codex,
            "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb",
        )
        .unwrap()
        .unwrap();
    assert!(external.unread);
    assert_eq!(store.list_unconfirmed_sessions().unwrap().len(), 1);
    board.finish();
}

#[test]
fn saved_thread_peek_explains_unavailable_inside_board_and_preserves_unread() {
    let mut board = BoardProcess::start();
    board.send(b"p");
    board.await_text("No live Pika pane");
    assert!(
        !board.output.contains("\x1b[2J"),
        "peek refresh cleared the whole terminal"
    );
    assert!(
        board.child.try_wait().unwrap().is_none(),
        "peek must stay in the board"
    );
    let db = rusqlite::Connection::open(board.root.path().join("state/pika.db")).unwrap();
    let unread: bool = db
        .query_row(
            "SELECT unread FROM sessions WHERE name='audit_saved'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(unread, "peek acknowledged unseen work");
    let store = Store::at(board.root.path().join("state/pika.db"));
    assert!(
        store
            .status_observations(
                pikamux::model::Provider::Codex,
                "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
            )
            .unwrap()
            .into_iter()
            .find(|o| o.kind == ObservationKind::Lifecycle)
            .unwrap()
            .unread,
        "peek acknowledged the authoritative completion"
    );
    board.send(b"\x1b");
    board.await_text("audit_saved");
    board.finish();
}

#[test]
fn help_and_usage_are_real_board_actions_with_a_return_path() {
    let mut board = BoardProcess::start();
    board.send(b"?");
    board.await_text("PIKA KEYS");
    assert!(
        !board.output.contains("\x1b[2J"),
        "help refresh cleared the whole terminal"
    );
    board.send(b"\x1b");
    board.await_text("audit_saved");
    board.send(b"u");
    board.await_text("CUMULATIVE USAGE");
    assert!(
        !board.output.contains("\x1b[2J"),
        "usage refresh cleared the whole terminal"
    );
    board.finish();
}

#[test]
fn unwatch_can_be_cancelled_and_then_confirmed_without_leaving_the_board() {
    let mut board = BoardProcess::start();
    board.send(b"x");
    board.await_text("Stop watching audit_saved");
    board.send(b"\x1b");
    board.await_text("audit_saved");
    let store = Store::at(board.root.path().join("state/pika.db"));
    let id = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    assert!(
        !store
            .is_untracked(pikamux::model::Provider::Codex, id)
            .unwrap()
    );
    board.send(b"x");
    board.await_text("Stop watching audit_saved");
    board.send(b"\r");
    board.await_text("Stopped watching audit_saved");
    assert!(
        store
            .is_untracked(pikamux::model::Provider::Codex, id)
            .unwrap()
    );
    assert!(board.child.try_wait().unwrap().is_none());
    board.finish();
}

#[test]
fn overdue_launch_can_be_hidden_without_deleting_recovery_or_untracking_a_conversation() {
    use pikamux::{model::Provider, store::PendingLaunch};
    let mut board = BoardProcess::start();
    let store = Store::at(board.root.path().join("state/pika.db"));
    let pending = PendingLaunch {
        launch_token: "fixture-stuck-launch".into(),
        provider: Provider::Codex,
        name: "stuck_launch".into(),
        cwd: board
            .root
            .path()
            .join("home")
            .to_string_lossy()
            .into_owned(),
        tmux_session: None,
        tmux_pane: None,
        expected_session_id: Some("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa".into()),
        root_pid: None,
        root_pid_start: None,
        preexisting_session_ids: None,
        candidate_session_id: None,
        candidate_observed_at: None,
        created_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs_f64()
            - 3600.0,
    };
    store.add_pending(&pending).unwrap();
    board.send(b"/stuck\r");
    board.await_text("Pika has not confirmed this launch");
    board.send(b"x");
    board.await_text("Hide launch entry for stuck_launch");
    board.send(b"\x1b");
    board.await_text("stuck_launch");
    assert_eq!(store.list_visible_pending().unwrap().len(), 1);
    board.send(b"x");
    board.await_text("Hide launch entry for stuck_launch");
    board.send(b"\r");
    board.await_text("Launch entry hidden");
    assert!(store.list_visible_pending().unwrap().is_empty());
    assert_eq!(
        store.get_pending(&pending.launch_token).unwrap(),
        Some(pending)
    );
    assert!(
        !store
            .is_untracked(Provider::Codex, "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa")
            .unwrap()
    );
    assert!(board.child.try_wait().unwrap().is_none());
    board.finish();
}

#[test]
fn client_update_exit_keeps_one_reopenable_startup_home_and_an_actionable_board_row() {
    use pikamux::model::Provider;
    let real_tmux = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|path| path.join("tmux"))
        .find(|path| path.is_file())
        .map(|path| fs::canonicalize(path).unwrap());
    let Some(real_tmux) = real_tmux else {
        eprintln!("tmux unavailable; client updater exit journey was not exercised");
        return;
    };
    let mut board = BoardProcess::start();
    fs::write(
        board.root.path().join("bin/tmux"),
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {}\nexec {} -f /dev/null \"$@\"\n",
            shell_words::quote(board.root.path().join("tmux-trace").to_str().unwrap()),
            shell_words::quote(real_tmux.to_str().unwrap())
        ),
    )
    .unwrap();
    board.real_tmux = true;
    let store = Store::at(board.root.path().join("state/pika.db"));
    for (provider, code) in [(Provider::Codex, 0), (Provider::Claude, 1)] {
        let launches = board.root.path().join(format!("{provider}-launches"));
        fs::write(board.root.path().join("bin").join(provider.as_str()), format!(
            "#!/bin/sh\n[ \"$*\" = 'app-server --stdio' ] && exit 97\nprintf launched\\n >> {}\nprintf 'Updater finished; restart the client.\\n'\nexit {code}\n",
            shell_words::quote(launches.to_str().unwrap())
        )).unwrap();
        let name = format!("updating_{provider}");
        // Exercise the actual executable/wrapper callback, not a simulated
        // store write. The noninteractive caller cannot attach, but the new
        // private pane must survive and remain openable from the real board.
        let mut launch = board
            .endpoint(&[
                "new",
                &name,
                "--agent",
                provider.as_str(),
                "--cwd",
                board.root.path().join("home").to_str().unwrap(),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(12);
        while launch.try_wait().unwrap().is_none() {
            if Instant::now() >= deadline {
                let _ = launch.kill();
                let _ = launch.wait();
                panic!("new client did not return after updater exit");
            }
            thread::sleep(Duration::from_millis(20));
        }
        let launch_output = launch.wait_with_output().unwrap();
        let pending = store
            .list_pending()
            .unwrap()
            .into_iter()
            .find(|pending| pending.name == name)
            .unwrap_or_else(|| {
                panic!(
                    "missing {provider} pending launch: stdout={} stderr={}",
                    String::from_utf8_lossy(&launch_output.stdout),
                    String::from_utf8_lossy(&launch_output.stderr)
                )
            });
        let deadline = Instant::now() + Duration::from_secs(6);
        let exit = loop {
            if let Some(exit) = store.get_pending_exit(&pending.launch_token).unwrap() {
                break exit;
            }
            assert!(
                Instant::now() < deadline,
                "missing wrapper exit receipt for {provider}"
            );
            thread::sleep(Duration::from_millis(20));
        };
        assert_eq!(exit.code, code);
        assert_eq!(exit.provider, provider);
        let retry = board
            .endpoint(&[&name])
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(!String::from_utf8_lossy(&retry.stderr).contains("already starting"));
        let retry_trace = fs::read_to_string(board.root.path().join("tmux-trace")).unwrap();
        assert_eq!(
            retry_trace
                .lines()
                .filter(|line| line.contains(" new-session "))
                .count(),
            if provider == Provider::Codex { 1 } else { 2 }
        );
        board.send(format!("/{name}\r").as_bytes());
        board.await_text(&format!("{provider} exited during startup"));
        board.send(b"\r");
        board.await_text("STARTUP EXITED");
        board.send(b"\x1b[24~"); // F12 returns without killing the retained shell.
        board.await_text(&format!("{provider} exited during startup"));
        assert_eq!(
            fs::read_to_string(&launches)
                .unwrap()
                .matches("launched")
                .count(),
            1
        );
        assert_eq!(
            store.get_pending(&pending.launch_token).unwrap(),
            Some(pending.clone())
        );
        board.send(b"x");
        board.await_text(&format!("Hide launch entry for {name}"));
        board.send(b"\x1b");
        board.await_text(&format!("{provider} exited during startup"));
        assert!(
            store
                .list_visible_pending()
                .unwrap()
                .iter()
                .any(|row| row.launch_token == pending.launch_token)
        );
        board.send(b"x");
        board.await_text(&format!("Hide launch entry for {name}"));
        board.send(b"\r");
        board.await_text("Launch entry hidden");
        assert!(
            !store
                .list_visible_pending()
                .unwrap()
                .iter()
                .any(|row| row.launch_token == pending.launch_token)
        );
        assert!(
            board
                .tmux(&["list-panes", "-a", "-F", "#{pane_id}"])
                .lines()
                .any(|pane| Some(pane) == pending.tmux_pane.as_deref())
        );
        assert_eq!(
            store.get_pending(&pending.launch_token).unwrap(),
            Some(pending)
        );
        assert!(
            !store
                .list_sessions()
                .unwrap()
                .iter()
                .any(|session| session.name.as_deref() == Some(&name))
        );
        assert!(
            store
                .get_session(Provider::Codex, "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa")
                .unwrap()
                .unwrap()
                .unread
        );
        board.send(b"\x1b");
        board.await_text("No conversations match this filter");
        board.send(b"\x1b");
        board.await_text("audit_saved");
    }
    board.finish();
}
