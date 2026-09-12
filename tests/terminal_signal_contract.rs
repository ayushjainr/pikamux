#![cfg(unix)]

use std::{
    fs::{self, File},
    os::{fd::FromRawFd, unix::process::CommandExt},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

struct BridgeProcess(Child);

impl Drop for BridgeProcess {
    fn drop(&mut self) {
        let pid = self.0.id() as libc::pid_t;
        unsafe {
            libc::kill(pid, libc::SIGCONT);
            libc::kill(pid, libc::SIGTERM);
        }
        for _ in 0..50 {
            if self.0.try_wait().ok().flatten().is_some() {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn terminal_flags(fd: libc::c_int) -> libc::tcflag_t {
    let mut mode = std::mem::MaybeUninit::<libc::termios>::uninit();
    assert_eq!(unsafe { libc::tcgetattr(fd, mode.as_mut_ptr()) }, 0);
    unsafe { mode.assume_init() }.c_lflag
}

fn wait_for(deadline: Instant, mut condition: impl FnMut() -> bool, message: &str) {
    while !condition() {
        assert!(Instant::now() < deadline, "{message}");
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn real_terminal_bridge_forwards_signals_and_restores_caller_tty() {
    let temp = tempfile::tempdir().unwrap();
    let signal_log = temp.path().join("signals");
    let child_pid_file = temp.path().join("child.pid");
    let child_script = temp.path().join("child.sh");
    fs::write(
        &child_script,
        format!(
            "#!/bin/sh\ntrap 'printf winch\\n >> {signals}' WINCH\ntrap 'printf tstp\\n >> {signals}' TSTP\ntrap 'printf cont\\n >> {signals}' CONT\ntrap 'printf term\\n >> {signals}; exit 0' TERM\nprintf '%s' \"$$\" > {pid}\nwhile :; do :; done\n",
            signals = shell_words::quote(&signal_log.to_string_lossy()),
            pid = shell_words::quote(&child_pid_file.to_string_lossy()),
        ),
    )
    .unwrap();
    let mut permissions = fs::metadata(&child_script).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o700);
    fs::set_permissions(&child_script, permissions).unwrap();

    let mut master_fd = -1;
    let mut slave_fd = -1;
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master_fd,
                &mut slave_fd,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        },
        0
    );
    let _master = unsafe { File::from_raw_fd(master_fd) };
    let slave = unsafe { File::from_raw_fd(slave_fd) };
    let mut baseline = std::mem::MaybeUninit::<libc::termios>::uninit();
    assert_eq!(
        unsafe { libc::tcgetattr(slave_fd, baseline.as_mut_ptr()) },
        0
    );
    let mut baseline = unsafe { baseline.assume_init() };
    baseline.c_lflag |= libc::ICANON | libc::ECHO;
    assert_eq!(
        unsafe { libc::tcsetattr(slave_fd, libc::TCSANOW, &baseline) },
        0
    );

    let duplicate = |fd| {
        let duplicate = unsafe { libc::dup(fd) };
        assert!(duplicate >= 0);
        unsafe { File::from_raw_fd(duplicate) }
    };
    let child = Command::new(env!("CARGO_BIN_EXE_pika"))
        .args([
            "_terminal-bridge",
            "--foreground",
            "255,255,255",
            "--background",
            "0,0,0",
            "--",
            child_script.to_str().unwrap(),
        ])
        .env("TERM", "xterm-256color")
        .stdin(Stdio::from(duplicate(slave_fd)))
        .stdout(Stdio::from(duplicate(slave_fd)))
        .stderr(Stdio::from(duplicate(slave_fd)))
        .process_group(0)
        .spawn()
        .unwrap();
    let mut bridge = BridgeProcess(child);
    let bridge_pid = bridge.0.id() as libc::pid_t;
    let deadline = Instant::now() + Duration::from_secs(5);
    wait_for(
        deadline,
        || child_pid_file.exists(),
        "bridge child never started",
    );
    wait_for(
        deadline,
        || terminal_flags(slave_fd) & (libc::ICANON | libc::ECHO) == 0,
        "bridge never entered raw terminal mode",
    );

    assert_eq!(unsafe { libc::kill(bridge_pid, libc::SIGWINCH) }, 0);
    wait_for(
        Instant::now() + Duration::from_secs(3),
        || {
            fs::read_to_string(&signal_log)
                .unwrap_or_default()
                .contains("winch")
        },
        "SIGWINCH was not forwarded to the PTY child",
    );

    assert_eq!(unsafe { libc::kill(bridge_pid, libc::SIGTSTP) }, 0);
    let mut stopped = false;
    wait_for(
        Instant::now() + Duration::from_secs(3),
        || {
            let mut status = 0;
            let waited =
                unsafe { libc::waitpid(bridge_pid, &mut status, libc::WUNTRACED | libc::WNOHANG) };
            if waited == bridge_pid && libc::WIFSTOPPED(status) {
                stopped = true;
            }
            stopped
        },
        "terminal bridge did not suspend itself",
    );
    assert_ne!(terminal_flags(slave_fd) & libc::ICANON, 0);
    assert_ne!(terminal_flags(slave_fd) & libc::ECHO, 0);
    assert_eq!(unsafe { libc::kill(bridge_pid, libc::SIGCONT) }, 0);
    wait_for(
        Instant::now() + Duration::from_secs(3),
        || {
            let signals = fs::read_to_string(&signal_log).unwrap_or_default();
            signals.contains("tstp") && signals.contains("cont")
        },
        "suspend/continue signals were not forwarded to the PTY child",
    );
    wait_for(
        Instant::now() + Duration::from_secs(3),
        || terminal_flags(slave_fd) & (libc::ICANON | libc::ECHO) == 0,
        "terminal bridge did not re-enter raw mode after continue",
    );

    assert_eq!(unsafe { libc::kill(bridge_pid, libc::SIGTERM) }, 0);
    let termination_deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(status) = bridge.0.try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < termination_deadline,
            "terminal bridge ignored SIGTERM; child signals: {:?}; tty flags: {:#x}; process state: {}",
            fs::read_to_string(&signal_log).unwrap_or_default(),
            terminal_flags(slave_fd),
            String::from_utf8_lossy(
                &Command::new("ps")
                    .args([
                        "-o",
                        "pid,state,ppid,pgid,command",
                        "-p",
                        &format!(
                            "{},{}",
                            bridge_pid,
                            fs::read_to_string(&child_pid_file).unwrap_or_default()
                        ),
                    ])
                    .output()
                    .map(|output| output.stdout)
                    .unwrap_or_default()
            )
        );
        thread::sleep(Duration::from_millis(10));
    };
    assert!(
        status.success(),
        "forwarded SIGTERM should let the fixture exit cleanly"
    );
    assert!(
        fs::read_to_string(&signal_log)
            .unwrap_or_default()
            .contains("term"),
        "PTY child did not receive SIGTERM"
    );
    assert_ne!(terminal_flags(slave_fd) & libc::ICANON, 0);
    assert_ne!(terminal_flags(slave_fd) & libc::ECHO, 0);

    let child_pid: libc::pid_t = fs::read_to_string(child_pid_file).unwrap().parse().unwrap();
    assert_eq!(unsafe { libc::kill(child_pid, 0) }, -1);
    drop(slave);
}
