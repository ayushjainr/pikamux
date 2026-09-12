//! Platform schedule definitions for quota-aware expert refresh.
//!
//! Rendering and installation are deliberately separate from activation. This
//! keeps setup previewable and makes tests incapable of starting a real job.

use crate::setup::{ApplyReceipt, FileChange, apply_changes};
use anyhow::{Context, Result, bail};
use std::{
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

pub const SERVICE_NAME: &str = "pika-expert-refresh.service";
pub const TIMER_NAME: &str = "pika-expert-refresh.timer";
pub const LAUNCHD_LABEL: &str = "io.pikamux.expert-refresh";
pub const LAUNCHD_NAME: &str = "io.pikamux.expert-refresh.plist";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SchedulePlatform {
    Macos,
    Linux,
}

impl SchedulePlatform {
    pub fn current() -> Option<Self> {
        match std::env::consts::OS {
            "macos" => Some(Self::Macos),
            "linux" => Some(Self::Linux),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ScheduleRequest<'a> {
    pub platform: SchedulePlatform,
    /// Explicit target directory. Callers may point this at a test root.
    pub unit_directory: &'a Path,
    /// Absolute path to the immutable native Pika executable.
    pub pika_executable: &'a Path,
    pub runtime_path: &'a str,
}

pub fn default_unit_directory(
    platform: SchedulePlatform,
    home: &Path,
    xdg_config_home: Option<&Path>,
) -> PathBuf {
    match platform {
        SchedulePlatform::Macos => home.join("Library/LaunchAgents"),
        SchedulePlatform::Linux => xdg_config_home
            .map(Path::to_owned)
            .unwrap_or_else(|| home.join(".config"))
            .join("systemd/user"),
    }
}

/// Return previewable file changes. No directory is created and no service is
/// activated while rendering.
pub fn schedule_changes(request: &ScheduleRequest<'_>) -> Result<Vec<FileChange>> {
    validate_request(request)?;
    let rendered = match request.platform {
        SchedulePlatform::Macos => vec![(
            request.unit_directory.join(LAUNCHD_NAME),
            launchd_plist(request.pika_executable, request.runtime_path),
        )],
        SchedulePlatform::Linux => vec![
            (
                request.unit_directory.join(SERVICE_NAME),
                systemd_service(request.pika_executable, request.runtime_path),
            ),
            (
                request.unit_directory.join(TIMER_NAME),
                systemd_timer().to_owned(),
            ),
        ],
    };
    rendered
        .into_iter()
        .map(|(path, after)| {
            let before = match fs::read_to_string(&path) {
                Ok(value) => value,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
                Err(error) => {
                    return Err(error).with_context(|| format!("cannot read {}", path.display()));
                }
            };
            Ok(FileChange {
                path,
                before,
                after,
                notice: Some("activated only after the reviewed setup batch is approved".into()),
            })
        })
        .collect()
}

/// Install only the supplied, previously rendered schedule roots. This never
/// invokes systemctl or launchctl.
pub fn install_schedule(request: &ScheduleRequest<'_>, backup_stamp: &str) -> Result<ApplyReceipt> {
    apply_changes(&schedule_changes(request)?, backup_stamp)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActivationReceipt {
    pub platform: SchedulePlatform,
    pub active: bool,
    pub detail: String,
}

/// Activate a schedule that has already been written by the reviewed setup
/// batch. Every subprocess is bounded; existing launchd jobs are left running
/// instead of being torn down during setup.
pub fn activate_schedule(request: &ScheduleRequest<'_>) -> Result<ActivationReceipt> {
    activate_schedule_with(request, "systemctl", "launchctl")
}

fn activate_schedule_with(
    request: &ScheduleRequest<'_>,
    systemctl: &str,
    launchctl: &str,
) -> Result<ActivationReceipt> {
    validate_request(request)?;
    match request.platform {
        SchedulePlatform::Linux => {
            run_bounded(
                systemctl,
                &["--user", "daemon-reload"],
                Duration::from_secs(10),
            )?;
            run_bounded(
                systemctl,
                &["--user", "enable", "--now", TIMER_NAME],
                Duration::from_secs(10),
            )?;
            Ok(ActivationReceipt {
                platform: request.platform,
                active: true,
                detail: "10-minute quota-aware expert refresh timer active".into(),
            })
        }
        SchedulePlatform::Macos => {
            #[cfg(unix)]
            let uid = unsafe { libc::geteuid() };
            #[cfg(not(unix))]
            let uid = 0;
            let domain = format!("gui/{uid}");
            let service = format!("{domain}/{LAUNCHD_LABEL}");
            if command_succeeds(
                launchctl,
                &[OsString::from("print"), OsString::from(&service)],
                Duration::from_secs(5),
            )? {
                return Ok(ActivationReceipt {
                    platform: request.platform,
                    active: true,
                    detail: "existing agent left running; saved changes take effect at next desktop login".into(),
                });
            }
            run_bounded_os(
                launchctl,
                &[OsString::from("enable"), OsString::from(&service)],
                Duration::from_secs(10),
            )?;
            run_bounded_os(
                launchctl,
                &[
                    OsString::from("bootstrap"),
                    OsString::from(&domain),
                    request.unit_directory.join(LAUNCHD_NAME).into_os_string(),
                ],
                Duration::from_secs(10),
            )?;
            run_bounded_os(
                launchctl,
                &[OsString::from("print"), OsString::from(&service)],
                Duration::from_secs(10),
            )?;
            Ok(ActivationReceipt {
                platform: request.platform,
                active: true,
                detail: "10-minute quota-aware expert refresh agent active".into(),
            })
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn fresh_launchd_activation_bootstraps_before_verification_and_is_idempotent() {
        let temp = tempfile::tempdir().unwrap();
        let launchctl = temp.path().join("launchctl");
        let log = temp.path().join("calls");
        fs::write(
            &launchctl,
            "#!/bin/sh\nbase=$(dirname \"$0\")\nprintf '%s\\n' \"$*\" >> \"$base/calls\"\nif [ \"$1\" = print ] && [ ! -f \"$base/active\" ]; then exit 1; fi\nif [ \"$1\" = bootstrap ]; then : > \"$base/active\"; fi\n",
        )
        .unwrap();
        fs::set_permissions(&launchctl, fs::Permissions::from_mode(0o700)).unwrap();
        let request = ScheduleRequest {
            platform: SchedulePlatform::Macos,
            unit_directory: temp.path(),
            pika_executable: Path::new("/opt/pika/bin/pika"),
            runtime_path: "/usr/bin:/bin",
        };
        let executable = launchctl.to_str().unwrap();

        let first = activate_schedule_with(&request, "unused-systemctl", executable).unwrap();
        assert!(first.active);
        let first_calls = fs::read_to_string(&log).unwrap();
        let commands = first_calls.lines().collect::<Vec<_>>();
        assert_eq!(commands.len(), 4);
        assert!(commands[0].starts_with("print "));
        assert!(commands[1].starts_with("enable "));
        assert!(commands[2].starts_with("bootstrap "));
        assert!(commands[3].starts_with("print "));

        let second = activate_schedule_with(&request, "unused-systemctl", executable).unwrap();
        assert!(second.detail.contains("existing agent"));
        assert_eq!(fs::read_to_string(log).unwrap().lines().count(), 5);
    }
}

fn run_bounded(program: &str, arguments: &[&str], timeout: Duration) -> Result<()> {
    let arguments = arguments.iter().map(OsString::from).collect::<Vec<_>>();
    run_bounded_os(program, &arguments, timeout)
}

fn command_succeeds(program: &str, arguments: &[OsString], timeout: Duration) -> Result<bool> {
    Ok(run_command(program, arguments, timeout)?.success())
}

fn run_bounded_os(program: &str, arguments: &[OsString], timeout: Duration) -> Result<()> {
    let status = run_command(program, arguments, timeout)?;
    if !status.success() {
        bail!("{program} exited with {status}")
    }
    Ok(())
}

fn run_command(
    program: &str,
    arguments: &[OsString],
    timeout: Duration,
) -> Result<std::process::ExitStatus> {
    let mut child = Command::new(program)
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("cannot start {program}"))?;
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child
            .try_wait()
            .with_context(|| format!("cannot wait for {program}"))?
        {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("{program} timed out after {}s", timeout.as_secs_f64())
        }
        thread::sleep(Duration::from_millis(25));
    }
}

fn validate_request(request: &ScheduleRequest<'_>) -> Result<()> {
    if !request.pika_executable.is_absolute() {
        bail!("scheduled Pika executable must be an absolute path")
    }
    for (label, value) in [
        ("Pika executable", request.pika_executable.to_string_lossy()),
        ("runtime PATH", request.runtime_path.into()),
    ] {
        if value.contains(['\0', '\n', '\r']) {
            bail!("{label} contains an unsupported control character")
        }
    }
    Ok(())
}

fn systemd_service(executable: &Path, runtime_path: &str) -> String {
    format!(
        "[Unit]\n\
         Description=Refresh one stale Pika expert card per provider\n\n\
         [Service]\n\
         Type=oneshot\n\
         Environment=\"PATH={}\"\n\
         ExecStart={} expert refresh --due --json\n\
         Nice=10\n\
         TimeoutStartSec=30min\n",
        systemd_quoted(runtime_path),
        systemd_argument(&executable.to_string_lossy()),
    )
}

fn systemd_timer() -> &'static str {
    "[Unit]\n\
     Description=Check whether Pika expert cards are due for refresh\n\n\
     [Timer]\n\
     OnBootSec=10min\n\
     OnUnitActiveSec=10min\n\
     RandomizedDelaySec=2min\n\
     Persistent=true\n\n\
     [Install]\n\
     WantedBy=timers.target\n"
}

fn systemd_quoted(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%")
}

fn systemd_argument(value: &str) -> String {
    format!("\"{}\"", systemd_quoted(value).replace('$', "$$"))
}

fn launchd_plist(executable: &Path, runtime_path: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
           <key>Label</key><string>{}</string>\n\
           <key>ProgramArguments</key>\n\
           <array>\n\
             <string>{}</string>\n\
             <string>expert</string><string>refresh</string>\n\
             <string>--due</string><string>--json</string>\n\
           </array>\n\
           <key>EnvironmentVariables</key>\n\
           <dict><key>PATH</key><string>{}</string></dict>\n\
           <key>StartInterval</key><integer>600</integer>\n\
           <key>ProcessType</key><string>Background</string>\n\
         </dict>\n\
         </plist>\n",
        xml(LAUNCHD_LABEL),
        xml(&executable.to_string_lossy()),
        xml(runtime_path),
    )
}

fn xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}
