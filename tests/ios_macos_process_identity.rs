#![cfg(target_os = "macos")]

/// Opt-in read-only probe of a child PID verified by the disposable SSH harness.
/// This makes no classification decision and never enumerates user processes.
#[test]
#[ignore = "requires an explicitly identified, disposable SSH child"]
fn owned_protected_ssh_child_kernel_path() {
    use libproc::libproc::{
        bsd_info::BSDInfo,
        proc_pid::{pidinfo, pidpath},
    };
    use std::os::unix::process::CommandExt;
    let pid: i32 = std::env::var("PIKA_IOS_OWNED_SSH_CHILD_PID")
        .expect("Exact disposable SSH child PID is required")
        .parse()
        .unwrap();
    assert!(pid > 0);
    let before = pidinfo::<BSDInfo>(pid, 0).expect("Kernel BSD identity");
    assert_eq!(before.pbi_ruid, unsafe { libc::getuid() });
    assert_eq!(before.pbi_uid, unsafe { libc::geteuid() });
    let path = pidpath(pid);
    let argv_record = pikamux::process::process_record(i64::from(pid));
    let after = pidinfo::<BSDInfo>(pid, 0).expect("Rechecked kernel BSD identity");
    assert_eq!(before.pbi_pid, after.pbi_pid);
    assert_eq!(before.pbi_ruid, after.pbi_ruid);
    assert_eq!(before.pbi_uid, after.pbi_uid);
    assert_eq!(before.pbi_start_tvsec, after.pbi_start_tvsec);
    assert_eq!(before.pbi_start_tvusec, after.pbi_start_tvusec);
    eprintln!(
        "Owned SSH child {pid}: kernel path={path:?}; argv-readable={}; UID={}; start={}.{}",
        argv_record.is_some(),
        before.pbi_uid,
        before.pbi_start_tvsec,
        before.pbi_start_tvusec
    );
    assert!(matches!(
        path.expect("Kernel executable path access must succeed")
            .as_str(),
        "/usr/sbin/sshd" | "/usr/libexec/sshd-session"
    ));
    assert!(
        argv_record.is_none(),
        "Probe requires the protected-argv case"
    );
    let mut fake_provider = std::process::Command::new("/bin/sleep")
        .arg0("codex")
        .arg("30")
        .spawn()
        .unwrap();
    let observation = pikamux::process::observe();
    let retained = observation
        .processes
        .contains_key(&i64::from(fake_provider.id()));
    let issues = match &observation.state {
        pikamux::process::ObservationState::Partial(issues) => issues.as_slice(),
        _ => &[],
    };
    let excluded = !observation.processes.contains_key(&i64::from(pid))
        && !issues
            .iter()
            .any(|issue| issue.contains(&format!("PID {pid}:")));
    fake_provider.kill().unwrap();
    fake_provider.wait().unwrap();
    assert!(
        retained,
        "Independent readable fake provider must remain enumerated"
    );
    assert!(
        excluded,
        "Exact trusted SSH supervisor must not make inventory partial"
    );
}
