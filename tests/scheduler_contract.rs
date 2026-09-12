use pikamux::scheduler::{
    LAUNCHD_NAME, SERVICE_NAME, SchedulePlatform, ScheduleRequest, TIMER_NAME,
    default_unit_directory, install_schedule, schedule_changes,
};
use std::{fs, path::Path};

fn executable(root: &Path) -> std::path::PathBuf {
    let path = root.join("bin/pika & friends");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, "fixture").unwrap();
    path
}

#[test]
fn platform_directories_are_explicit_and_host_independent() {
    let home = Path::new("/Users/tester");
    assert_eq!(
        default_unit_directory(SchedulePlatform::Macos, home, None),
        home.join("Library/LaunchAgents")
    );
    assert_eq!(
        default_unit_directory(
            SchedulePlatform::Linux,
            home,
            Some(Path::new("/test/config"))
        ),
        Path::new("/test/config/systemd/user")
    );
}

#[test]
fn linux_schedule_is_finite_quota_aware_and_escaped() {
    let temp = tempfile::tempdir().unwrap();
    let binary = executable(temp.path());
    let root = temp.path().join("systemd/user");
    let request = ScheduleRequest {
        platform: SchedulePlatform::Linux,
        unit_directory: &root,
        pika_executable: &binary,
        runtime_path: "/path with tools:/percent%/quote\"",
    };
    let changes = schedule_changes(&request).unwrap();
    assert_eq!(changes.len(), 2);
    assert!(changes.iter().all(|change| !change.path.exists()));
    let service = changes
        .iter()
        .find(|change| change.path.ends_with(SERVICE_NAME))
        .unwrap();
    let timer = changes
        .iter()
        .find(|change| change.path.ends_with(TIMER_NAME))
        .unwrap();
    assert!(service.after.contains("Type=oneshot"));
    assert!(service.after.contains("expert refresh --due --json"));
    assert!(service.after.contains("pika & friends\" expert"));
    assert!(service.after.contains("percent%%"));
    assert!(service.after.contains("quote\\\""));
    assert!(timer.after.contains("OnUnitActiveSec=10min"));
    assert!(timer.after.contains("RandomizedDelaySec=2min"));
    assert!(timer.after.contains("Persistent=true"));
    assert!(!timer.after.contains("ExecStart"));
}

#[test]
fn mac_schedule_preserves_argv_boundaries_and_has_no_eager_run() {
    let temp = tempfile::tempdir().unwrap();
    let binary = executable(temp.path());
    let root = temp.path().join("LaunchAgents");
    let request = ScheduleRequest {
        platform: SchedulePlatform::Macos,
        unit_directory: &root,
        pika_executable: &binary,
        runtime_path: "/path & tools:/usr/bin",
    };
    let changes = schedule_changes(&request).unwrap();
    assert_eq!(changes.len(), 1);
    assert!(changes[0].path.ends_with(LAUNCHD_NAME));
    let plist = &changes[0].after;
    assert!(plist.contains("<string>expert</string><string>refresh</string>"));
    assert!(plist.contains("<string>--due</string><string>--json</string>"));
    assert!(plist.contains("/path &amp; tools:/usr/bin"));
    assert!(plist.contains("pika &amp; friends"));
    assert!(plist.contains("<key>StartInterval</key><integer>600</integer>"));
    assert!(!plist.contains("RunAtLoad"));
    assert!(!plist.contains("KeepAlive"));
}

#[test]
fn install_is_idempotent_and_never_activates_services() {
    let temp = tempfile::tempdir().unwrap();
    let binary = executable(temp.path());
    let root = temp.path().join("systemd/user");
    let request = ScheduleRequest {
        platform: SchedulePlatform::Linux,
        unit_directory: &root,
        pika_executable: &binary,
        runtime_path: "/usr/bin:/bin",
    };
    let first = install_schedule(&request, "one").unwrap();
    assert_eq!(first.written.len(), 2);
    assert!(first.backups.is_empty());
    let second = install_schedule(&request, "two").unwrap();
    assert!(second.written.is_empty());
    assert!(second.backups.is_empty());
    assert!(root.join(SERVICE_NAME).is_file());
    assert!(root.join(TIMER_NAME).is_file());
}

#[test]
fn unsafe_or_relative_schedule_inputs_fail_before_writes() {
    let temp = tempfile::tempdir().unwrap();
    for (binary, runtime) in [
        (Path::new("relative/pika"), "/usr/bin"),
        (
            Path::new("/absolute/pika"),
            "/usr/bin\nExecStart=/bin/false",
        ),
    ] {
        let request = ScheduleRequest {
            platform: SchedulePlatform::Linux,
            unit_directory: temp.path(),
            pika_executable: binary,
            runtime_path: runtime,
        };
        assert!(schedule_changes(&request).is_err());
    }
    assert!(fs::read_dir(temp.path()).unwrap().next().is_none());
}
