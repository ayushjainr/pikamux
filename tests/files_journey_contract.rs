//! Real terminal input, disposable files only. No provider or fleet calls.
#![cfg(unix)]
use std::{
    fs::{self, File},
    io::{Read, Write},
    os::fd::{AsRawFd, FromRawFd},
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
    fn start(width: u16, height: u16) -> Self {
        Self::start_with_color(width, height, false)
    }

    fn start_with_color(width: u16, height: u16, color: bool) -> Self {
        let root = tempfile::tempdir().unwrap();
        for dir in [
            "home", "config", "state", "cache", "data", "tmp", "project", "codex", "claude", "oc",
        ] {
            fs::create_dir_all(root.path().join(dir)).unwrap();
        }
        fs::write(
            root.path().join("project/a.rs"),
            "fn untouched_agent() {}\nFILE_PREVIEW_MARKER\n\x1b]52;c;SECRET\x07\n",
        )
        .unwrap();
        fs::write(
            root.path().join("project/z.md"),
            format!("# Rich preview\n\nThis has **bold** and `code`.\n\n{} WRAPPED_TAIL\n\n```rust\n    let exact_spacing = 1;\n```\n", "words ".repeat(22)),
        ).unwrap();
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
        let mut command = Command::new(env!("CARGO_BIN_EXE_pika"));
        command
            .env_clear()
            .args(["_files-view", "--project"])
            .arg(root.path().join("project"));
        for (key, dir) in [
            ("HOME", "home"),
            ("XDG_CONFIG_HOME", "config"),
            ("XDG_STATE_HOME", "state"),
            ("XDG_CACHE_HOME", "cache"),
            ("XDG_DATA_HOME", "data"),
            ("TMPDIR", "tmp"),
            ("PIKA_CONFIG_HOME", "config"),
            ("PIKA_STATE_HOME", "state"),
            ("CODEX_HOME", "codex"),
            ("CLAUDE_CONFIG_DIR", "claude"),
            ("OPENCODE_DATA_HOME", "oc"),
            ("OPENCODE_CONFIG_DIR", "oc"),
        ] {
            command.env(key, root.path().join(dir));
        }
        command
            .env("PATH", "/usr/bin:/bin")
            .env("TERM", "xterm-256color")
            .env("PIKA_UPDATE_CHECK", "0")
            .stdin(Stdio::from(child_terminal.try_clone().unwrap()))
            .stdout(Stdio::from(child_terminal.try_clone().unwrap()))
            .stderr(Stdio::from(child_terminal));
        if !color {
            command.env("NO_COLOR", "1");
        }
        let child = command.spawn().unwrap();
        Self {
            root,
            child,
            terminal,
            output: String::new(),
        }
    }
    fn drain(&mut self) {
        let mut bytes = [0; 32768];
        while let Ok(n) = self.terminal.read(&mut bytes) {
            if n == 0 {
                break;
            }
            self.output.push_str(&String::from_utf8_lossy(&bytes[..n]));
        }
    }
    fn wait_text(&mut self, text: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            self.drain();
            if self.output.contains(text) {
                return;
            }
            assert!(
                self.child.try_wait().unwrap().is_none(),
                "viewer exited: {}",
                self.output
            );
            assert!(Instant::now() < deadline, "missing {text}: {}", self.output);
            thread::sleep(Duration::from_millis(10));
        }
    }
    fn send(&mut self, bytes: &[u8]) {
        self.output.clear();
        self.terminal.write_all(bytes).unwrap();
    }
    fn finish(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            self.drain();
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success(), "{}", self.output);
                assert!(self.output.contains("\x1b[?1049l"), "screen not restored");
                assert!(
                    self.output.contains("\x1b[?1006l"),
                    "SGR mouse reporting left enabled"
                );
                let mut attrs = unsafe { std::mem::zeroed::<libc::termios>() };
                assert_eq!(
                    unsafe { libc::tcgetattr(self.terminal.as_raw_fd(), &mut attrs) },
                    0
                );
                assert_ne!(attrs.c_lflag & libc::ICANON, 0, "typing mode leaked");
                assert!(!self.root.path().join("state/pika.db").exists());
                return;
            }
            assert!(Instant::now() < deadline, "viewer did not exit");
            thread::sleep(Duration::from_millis(10));
        }
    }
}
impl Drop for Journey {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn files_keyboard_preview_is_inert_and_restores_terminal() {
    let mut j = Journey::start(120, 32);
    j.wait_text("a.rs");
    // The one-shot Git result is a legitimate update, not an idle repaint.
    j.wait_text("Git status unavailable");
    j.send(b"\r");
    j.wait_text("FILE_PREVIEW_MARKER");
    j.wait_text("\x1b[?2026l");
    assert!(
        !j.output.contains("\x1b]52"),
        "file content executed terminal command"
    );
    j.drain();
    j.output.clear();
    thread::sleep(Duration::from_millis(350));
    j.drain();
    assert!(j.output.is_empty(), "idle viewer repaints: {:?}", j.output);
    j.send(b"u");
    j.wait_text("project");
    j.send(b"b");
    j.wait_text("a.rs");
    j.send(b"q");
    j.finish();
}

#[test]
fn small_files_viewer_always_has_an_exit() {
    let mut j = Journey::start(32, 8);
    j.wait_text("Files");
    j.send(b"\x1b");
    j.finish();
}

#[test]
fn tree_can_be_hidden_and_restored_from_the_keyboard() {
    let mut j = Journey::start(120, 24);
    j.wait_text("a.rs");
    j.wait_text("Git status unavailable");
    j.send(b"\r");
    j.wait_text("FILE_PREVIEW_MARKER");
    j.send(b"t");
    j.wait_text("t show tree");
    j.wait_text("\x1b[?2026l");
    assert!(
        !j.output.contains("z.md"),
        "hidden tree still occupies the screen"
    );
    assert!(j.output.contains("FILE_PREVIEW_MARKER"));
    j.send(b"t");
    j.wait_text("t hide tree");
    j.wait_text("z.md");
    j.send(b"q");
    j.finish();
}

#[test]
fn markdown_and_wrap_toggles_work_in_a_real_narrow_terminal() {
    let mut j = Journey::start(60, 24);
    j.wait_text("z.md");
    j.wait_text("Git status unavailable");
    j.send(b"j\r");
    j.wait_text("Rendered");
    j.wait_text("WRAPPED_TAIL");
    j.wait_text("\x1b[?2026l");
    assert!(!j.output.contains("**bold**"));
    assert!(!j.output.contains("\x1b[38;"), "NO_COLOR ignored");
    j.send(b"m");
    j.wait_text("Source");
    j.wait_text("**bold**");
    j.send(b"w");
    j.wait_text("wrap off");
    j.wait_text("\x1b[?2026l");
    assert!(
        !j.output.contains("WRAPPED_TAIL"),
        "source did not stop wrapping"
    );
    j.send(b"w");
    j.wait_text("WRAPPED_TAIL");
    j.send(b"m");
    j.wait_text("Rendered");
    j.send(b"q");
    j.finish();
    assert!(
        fs::read_to_string(j.root.path().join("project/z.md"))
            .unwrap()
            .contains("**bold**")
    );
}

#[test]
fn files_refuses_redirected_input_without_opening_state() {
    let root = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_pika"))
        .env_clear()
        .env("HOME", root.path())
        .env("PATH", "/usr/bin:/bin")
        .args(["_files-view", "--project"])
        .arg(root.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("interactive terminal"));
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 0);
}

#[test]
fn code_highlighting_renders_and_respects_no_color() {
    for color in [true, false] {
        let mut j = Journey::start_with_color(120, 24, color);
        j.wait_text("a.rs");
        j.wait_text("Git status unavailable");
        j.send(b"\r");
        j.wait_text("FILE_PREVIEW_MARKER");
        j.wait_text("\x1b[?2026l");
        let keyword = format!(
            "{}fn",
            crossterm::style::SetForegroundColor(crossterm::style::Color::Magenta)
        );
        assert_eq!(j.output.contains(&keyword), color, "keyword color mismatch");
        if !color {
            assert!(!j.output.contains("\x1b[38;"), "NO_COLOR ignored");
        }
        assert!(
            !j.output.contains("\x1b]52"),
            "source content executed terminal controls"
        );
        j.send(b"q");
        j.finish();
        assert!(
            fs::read_to_string(j.root.path().join("project/a.rs"))
                .unwrap()
                .starts_with("fn untouched_agent() {}")
        );
    }
}

#[test]
fn mouse_file_wheel_and_divider_drag_restore_mouse_reporting() {
    let mut j = Journey::start(120, 32);
    fs::write(
        j.root.path().join("project/scroll.txt"),
        (0..40).map(|n| format!("ROW_{n:02}\n")).collect::<String>(),
    )
    .unwrap();
    j.wait_text("a.rs");
    assert!(
        j.output.contains("\x1b[?1000h"),
        "click reporting not enabled"
    );
    assert!(
        j.output.contains("\x1b[?1006h"),
        "SGR mouse reporting not enabled"
    );

    // Refresh discovers the fixture added by this test without changing the
    // shared Journey fixture ordering.
    j.send(b"r");
    j.wait_text("scroll.txt");
    j.send(b"j\r");
    j.wait_text("ROW_00");
    j.send(b"\x1b[<65;65;8M"); // SGR wheel down, file pane (1-based x=65).
    j.wait_text("ROW_03");
    assert!(
        !j.output.contains("ROW_00"),
        "file wheel moved fewer than three rows"
    );
    j.send(b"\x1b[<64;65;8M"); // SGR wheel up.
    j.wait_text("ROW_00");

    // The default divider is x=40 (zero-based). Drag its one-based x=41
    // handle to one-based x=55, producing divider x=54 and file x=56.
    j.send(b"\x1b[<0;41;8M\x1b[<32;55;8M\x1b[<0;55;8m");
    j.wait_text("\x1b[2;55H");
    j.send(b"q");
    j.finish();
    assert!(
        j.output.contains("\x1b[?1000l"),
        "click reporting left enabled"
    );
    assert!(
        j.output.contains("\x1b[?1006l"),
        "SGR mouse reporting left enabled"
    );
}

#[test]
fn mouse_tree_wheel_scrolls_viewport_without_opening_file() {
    let mut j = Journey::start(120, 32);
    fs::write(
        j.root.path().join("project/scroll.txt"),
        (0..40).map(|n| format!("ROW_{n:02}\n")).collect::<String>(),
    )
    .unwrap();
    for n in 0..36 {
        fs::write(
            j.root.path().join(format!("project/tree_{n:02}.txt")),
            format!("TREE_FILE_{n:02}\n"),
        )
        .unwrap();
    }
    j.wait_text("a.rs");
    j.send(b"r");
    j.wait_text("tree_20.txt");
    j.send(b"j\r");
    j.wait_text("ROW_00");
    j.send(b"\x1b[<65;10;8M"); // SGR wheel down over the tree.
    j.wait_text("tree_27.txt");
    j.wait_text("\x1b[?2026l");
    assert!(!j.output.contains("a.rs"), "tree viewport did not move");
    assert!(
        j.output.contains("ROW_00"),
        "tree wheel scrolled the file instead"
    );
    assert!(!j.output.contains("TREE_FILE_"), "tree wheel opened a file");
    j.send(b"q");
    j.finish();
}
