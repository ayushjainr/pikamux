#![cfg(unix)]

use pikamux::update::{artifact_name, native_target, sha256_file};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn assert_bootstrap_signal_cleans_probe_group(signal: i32, expected_exit: i32) {
    let temp = tempfile::tempdir().unwrap();
    let bundle = temp.path().join("bundle");
    let payload = temp.path().join("payload");
    fs::create_dir(&bundle).unwrap();
    fs::create_dir(&payload).unwrap();
    let version = env!("CARGO_PKG_VERSION");
    let target = native_target().unwrap();
    let probe_pids = temp.path().join("probe-pids");
    let fake = payload.join("pika");
    fs::write(
        &fake,
        r#"#!/bin/sh
case "$*" in
  --version) echo "pika $PIKA_TEST_VERSION" ;;
  --help)
    sleep 30 &
    printf '%s %s\n' "$$" "$!" > "$PIKA_TEST_PROBE_PIDS"
    wait
    ;;
  'skill show') exit 0 ;;
  *) exit 2 ;;
esac
"#,
    )
    .unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).unwrap();
    fs::copy("LICENSE", payload.join("LICENSE")).unwrap();
    fs::copy("THIRD_PARTY.md", payload.join("THIRD_PARTY.md")).unwrap();

    let archive_name = artifact_name(version, target).unwrap();
    let archive = bundle.join(&archive_name);
    assert!(
        Command::new("tar")
            .args(["-czf"])
            .arg(&archive)
            .args(["-C"])
            .arg(&payload)
            .args(["LICENSE", "THIRD_PARTY.md", "pika"])
            .status()
            .unwrap()
            .success()
    );
    let checksum = sha256_file(&archive).unwrap();
    fs::write(
        bundle.join(format!("{archive_name}.sha256")),
        format!("{checksum}\n"),
    )
    .unwrap();
    fs::write(bundle.join("pika-version"), format!("{version}\n")).unwrap();
    fs::write(bundle.join("pika-native-release.json"), "{}\n").unwrap();

    let mut installer = Command::new("/bin/bash")
        .arg("scripts/install.sh")
        .arg("--bundle")
        .arg(&bundle)
        .arg("--root")
        .arg(temp.path().join("managed"))
        .arg("--bin-dir")
        .arg(temp.path().join("bin"))
        .arg("--no-setup")
        .env("PIKA_TEST_VERSION", version)
        .env("PIKA_TEST_PROBE_PIDS", &probe_pids)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let start_deadline = Instant::now() + Duration::from_secs(5);
    while !probe_pids.exists() && Instant::now() < start_deadline {
        assert!(
            installer.try_wait().unwrap().is_none(),
            "installer exited before its candidate probe started"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(probe_pids.exists(), "candidate probe did not start");
    let pids = fs::read_to_string(&probe_pids)
        .unwrap()
        .split_whitespace()
        .map(|value| value.parse::<i32>().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(pids.len(), 2);

    assert_eq!(unsafe { libc::kill(installer.id() as i32, signal) }, 0);
    let exit_deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(status) = installer.try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < exit_deadline,
            "installer did not handle signal {signal}"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(status.code(), Some(expected_exit));

    let reap_deadline = Instant::now() + Duration::from_secs(3);
    for pid in pids {
        while unsafe { libc::kill(pid, 0) } == 0 && Instant::now() < reap_deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(
            unsafe { libc::kill(pid, 0) },
            -1,
            "candidate probe process {pid} survived signal {signal}"
        );
    }
}

#[test]
fn shell_bootstrap_sigint_kills_and_reaps_active_probe_group() {
    assert_bootstrap_signal_cleans_probe_group(libc::SIGINT, 130);
}

#[test]
fn shell_bootstrap_sigterm_kills_and_reaps_active_probe_group() {
    assert_bootstrap_signal_cleans_probe_group(libc::SIGTERM, 143);
}
