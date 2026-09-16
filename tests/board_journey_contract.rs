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
            } else {
                let rendered = board.tmux(&["display-message", "-p", &format!("#{{T:{option}}}")]);
                assert!(
                    !rendered.contains("audit"),
                    "resolved request still visible: {rendered}"
                );
            }
            assert!(board.child.try_wait().unwrap().is_none());
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
