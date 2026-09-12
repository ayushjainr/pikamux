use crate::model::Provider;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::path::Path;
use std::time::Duration;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessRecord {
    pub pid: i64,
    pub parent_pid: Option<i64>,
    pub start_time: u64,
    pub argv: Vec<String>,
}

impl ProcessRecord {
    pub fn provider(&self) -> Option<Provider> {
        process_kind(&self.argv)
    }
}

pub fn process_kind(argv: &[String]) -> Option<Provider> {
    argv.iter().take(4).find_map(|value| {
        let name = Path::new(value)
            .file_name()
            .and_then(OsStr::to_str)
            .unwrap_or(value)
            .to_ascii_lowercase();
        match name.as_str() {
            "codex" | "codex.js" => Some(Provider::Codex),
            "claude" | "claude-code" => Some(Provider::Claude),
            "opencode" | "opencode.js" => Some(Provider::Opencode),
            _ => None,
        }
    })
}

pub fn snapshot() -> BTreeMap<i64, ProcessRecord> {
    platform::snapshot()
}

pub fn process_start_time(pid: i64) -> Option<u64> {
    platform::read(pid).map(|record| record.start_time)
}

pub fn process_alive(pid: i64, generation: Option<u64>) -> bool {
    platform::read(pid)
        .is_some_and(|record| generation.is_none_or(|expected| record.start_time == expected))
}

pub const fn can_terminate_generation() -> bool {
    cfg!(target_os = "linux")
}

/// Request a graceful stop through a Linux pidfd pinned to one process
/// generation. Pika never falls back to a reusable numeric PID and never
/// escalates to SIGKILL.
#[cfg(target_os = "linux")]
pub fn terminate_generation(pid: i64, generation: u64, timeout: Duration) -> Result<(), String> {
    if process_start_time(pid) != Some(generation) {
        return Err("the outside PID generation changed; no signal was sent".into());
    }
    let pid32 = i32::try_from(pid).map_err(|_| "invalid outside process identity")?;
    // SAFETY: pidfd_open receives a validated positive PID and no pointers.
    let descriptor = unsafe { libc::syscall(libc::SYS_pidfd_open, pid32, 0) as i32 };
    if descriptor < 0 {
        return Err(format!(
            "this Linux kernel could not pin the outside process: {}",
            std::io::Error::last_os_error()
        ));
    }
    struct Descriptor(i32);
    impl Drop for Descriptor {
        fn drop(&mut self) {
            // SAFETY: this descriptor was returned by pidfd_open and is owned
            // solely by this guard.
            unsafe { libc::close(self.0) };
        }
    }
    let descriptor = Descriptor(descriptor);
    if process_start_time(pid) != Some(generation) {
        return Err("the outside PID generation changed; no signal was sent".into());
    }
    // SAFETY: pidfd_send_signal takes the owned pidfd, a standard signal, a
    // null siginfo pointer, and zero flags.
    let sent = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            descriptor.0,
            libc::SIGTERM,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    };
    if sent != 0 {
        return Err(format!(
            "the graceful stop request failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    let mut ready = libc::pollfd {
        fd: descriptor.0,
        events: libc::POLLIN,
        revents: 0,
    };
    let timeout_ms = timeout.as_millis().clamp(100, i32::MAX as u128) as i32;
    // SAFETY: ready points to one initialized pollfd for the duration of poll.
    let polled = unsafe { libc::poll(&mut ready, 1, timeout_ms) };
    if polled < 0 {
        return Err(format!(
            "waiting for the graceful stop failed: {}",
            std::io::Error::last_os_error()
        ));
    }
    if polled == 0 || process_alive(pid, Some(generation)) {
        return Err(format!(
            "PID {pid} did not exit after {}s; Pika did not force-kill it",
            timeout.as_secs_f64()
        ));
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn terminate_generation(_pid: i64, _generation: u64, _timeout: Duration) -> Result<(), String> {
    Err("this platform cannot safely signal a pinned PID generation; no signal was sent".into())
}

pub fn process_tree(root: i64, processes: &BTreeMap<i64, ProcessRecord>) -> Vec<i64> {
    if !processes.contains_key(&root) {
        return Vec::new();
    }
    let mut children: BTreeMap<i64, Vec<i64>> = BTreeMap::new();
    for record in processes.values() {
        if let Some(parent) = record.parent_pid {
            children.entry(parent).or_default().push(record.pid);
        }
    }
    let mut seen = BTreeSet::new();
    let mut stack = vec![root];
    let mut output = Vec::new();
    while let Some(pid) = stack.pop() {
        if !seen.insert(pid) || !processes.contains_key(&pid) {
            continue;
        }
        output.push(pid);
        if let Some(values) = children.get(&pid) {
            stack.extend(values.iter().rev());
        }
    }
    output
}

pub fn provider_process(
    root: i64,
    provider: Option<Provider>,
    processes: &BTreeMap<i64, ProcessRecord>,
) -> Option<i64> {
    process_tree(root, processes).into_iter().rfind(|pid| {
        processes
            .get(pid)
            .and_then(ProcessRecord::provider)
            .is_some_and(|found| provider.is_none_or(|expected| found == expected))
    })
}

/// Walk toward the terminal-owning provider process that launched a hook.
/// The nearest matching ancestor is the lease owner; the hook subprocess is
/// never recorded as durable identity.
pub fn provider_ancestor(
    start: i64,
    provider: Provider,
    processes: &BTreeMap<i64, ProcessRecord>,
) -> Option<i64> {
    let mut current = Some(start);
    let mut seen = BTreeSet::new();
    while let Some(pid) = current {
        if !seen.insert(pid) {
            return None;
        }
        let record = processes.get(&pid)?;
        if record.provider() == Some(provider) {
            return Some(pid);
        }
        current = record.parent_pid;
    }
    None
}

pub fn find_session_processes(
    identity: &str,
    provider: Provider,
    processes: &BTreeMap<i64, ProcessRecord>,
) -> Vec<i64> {
    let matches: BTreeSet<i64> = processes
        .values()
        .filter(|record| record.provider() == Some(provider))
        .filter(|record| record.argv.iter().any(|argument| argument == identity))
        .map(|record| record.pid)
        .collect();
    canonical_identity_pids(&matches, processes)
}

fn canonical_identity_pids(
    matches: &BTreeSet<i64>,
    processes: &BTreeMap<i64, ProcessRecord>,
) -> Vec<i64> {
    let launcher_aliases: BTreeSet<i64> = matches
        .iter()
        .filter_map(|pid| processes.get(pid).and_then(|record| record.parent_pid))
        .filter(|parent| matches.contains(parent))
        .collect();
    matches.difference(&launcher_aliases).copied().collect()
}

pub fn shared_provider_process(record: &ProcessRecord, provider: Provider) -> bool {
    if record.provider() != Some(provider) {
        return false;
    }
    match provider {
        Provider::Opencode => true,
        Provider::Codex => record.argv.iter().any(|value| value == "app-server"),
        Provider::Claude => false,
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use super::ProcessRecord;
    use std::{collections::BTreeMap, fs, path::Path};

    pub fn snapshot() -> BTreeMap<i64, ProcessRecord> {
        let Ok(entries) = fs::read_dir("/proc") else {
            return BTreeMap::new();
        };
        entries
            .flatten()
            .filter_map(|entry| entry.file_name().to_str()?.parse::<i64>().ok())
            .filter_map(|pid| read(pid).map(|record| (pid, record)))
            .collect()
    }

    pub fn read(pid: i64) -> Option<ProcessRecord> {
        if pid <= 0 {
            return None;
        }
        let root = Path::new("/proc").join(pid.to_string());
        let stat = fs::read_to_string(root.join("stat")).ok()?;
        let tail = stat.get(stat.rfind(')')? + 2..)?;
        let fields: Vec<_> = tail.split_whitespace().collect();
        let parent_pid = fields
            .get(1)?
            .parse::<i64>()
            .ok()
            .filter(|value| *value > 0);
        let start_time = fields.get(19)?.parse().ok()?;
        let raw = fs::read(root.join("cmdline")).ok()?;
        let argv = raw
            .split(|byte| *byte == 0)
            .filter(|value| !value.is_empty())
            .map(|value| String::from_utf8_lossy(value).into_owned())
            .collect();
        Some(ProcessRecord {
            pid,
            parent_pid,
            start_time,
            argv,
        })
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::ProcessRecord;
    use libproc::libproc::{bsd_info::BSDInfo, proc_pid::pidinfo};
    use libproc::processes::{ProcFilter, pids_by_type};
    use std::collections::BTreeMap;

    pub fn snapshot() -> BTreeMap<i64, ProcessRecord> {
        pids_by_type(ProcFilter::ByRealUID {
            // SAFETY: getuid has no preconditions and does not mutate memory.
            ruid: unsafe { libc::getuid() },
        })
        .unwrap_or_default()
        .into_iter()
        .filter_map(|pid| read(i64::from(pid)).map(|record| (record.pid, record)))
        .collect()
    }

    pub fn read(pid: i64) -> Option<ProcessRecord> {
        let pid32 = i32::try_from(pid).ok().filter(|value| *value > 0)?;
        let info = pidinfo::<BSDInfo>(pid32, 0).ok()?;
        // A same-user check is part of identity evidence; denied/foreign reads
        // become absence rather than a guess from a title or PID.
        if info.pbi_ruid != unsafe { libc::getuid() } || i64::from(info.pbi_pid) != pid {
            return None;
        }
        let argv = process_arguments(pid32)?;
        // Re-read after argv so exit/reuse during observation fails closed.
        let verified = pidinfo::<BSDInfo>(pid32, 0).ok()?;
        let start_time = info
            .pbi_start_tvsec
            .checked_mul(1_000_000)?
            .checked_add(info.pbi_start_tvusec)?;
        let verified_start = verified
            .pbi_start_tvsec
            .checked_mul(1_000_000)?
            .checked_add(verified.pbi_start_tvusec)?;
        if start_time != verified_start || verified.pbi_ruid != info.pbi_ruid {
            return None;
        }
        Some(ProcessRecord {
            pid,
            parent_pid: (info.pbi_ppid > 0).then(|| i64::from(info.pbi_ppid)),
            start_time,
            argv,
        })
    }

    fn process_arguments(pid: i32) -> Option<Vec<String>> {
        let argument_max = unsafe { libc::sysconf(libc::_SC_ARG_MAX) };
        if argument_max <= 0 {
            return None;
        }
        let mut buffer = vec![0_u8; usize::try_from(argument_max).ok()?];
        let mut size = buffer.len();
        let mut mib = [libc::CTL_KERN, 49, pid]; // KERN_PROCARGS2
        let result = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                mib.len() as u32,
                buffer.as_mut_ptr().cast(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        if result != 0 || size < std::mem::size_of::<i32>() {
            return None;
        }
        buffer.truncate(size);
        let count = i32::from_ne_bytes(buffer.get(..4)?.try_into().ok()?);
        if !(0..=65_536).contains(&count) {
            return None;
        }
        let mut cursor = 4;
        cursor += buffer.get(cursor..)?.iter().position(|value| *value == 0)?;
        while buffer.get(cursor) == Some(&0) {
            cursor += 1;
        }
        let mut output = Vec::with_capacity(count as usize);
        while output.len() < count as usize && cursor < buffer.len() {
            let end = cursor + buffer.get(cursor..)?.iter().position(|value| *value == 0)?;
            output.push(String::from_utf8_lossy(&buffer[cursor..end]).into_owned());
            cursor = end + 1;
            while buffer.get(cursor) == Some(&0) {
                cursor += 1;
            }
        }
        (output.len() == count as usize).then_some(output)
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod platform {
    use super::ProcessRecord;
    use std::collections::BTreeMap;
    pub fn snapshot() -> BTreeMap<i64, ProcessRecord> {
        BTreeMap::new()
    }
    pub fn read(_pid: i64) -> Option<ProcessRecord> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    use std::{process::Command, time::Duration};

    fn record(pid: i64, parent: Option<i64>, argv: &[&str]) -> ProcessRecord {
        ProcessRecord {
            pid,
            parent_pid: parent,
            start_time: pid as u64,
            argv: argv.iter().map(|value| (*value).to_owned()).collect(),
        }
    }

    #[test]
    fn launcher_and_native_child_are_one_identity() {
        let records = BTreeMap::from([
            (1, record(1, None, &["node", "codex", "resume", "uuid"])),
            (2, record(2, Some(1), &["codex", "resume", "uuid"])),
        ]);
        assert_eq!(
            find_session_processes("uuid", Provider::Codex, &records),
            vec![2]
        );
    }

    #[test]
    fn distinct_trees_remain_distinct() {
        let records = BTreeMap::from([
            (1, record(1, None, &["codex", "resume", "uuid"])),
            (2, record(2, None, &["codex", "resume", "uuid"])),
        ]);
        assert_eq!(
            find_session_processes("uuid", Provider::Codex, &records),
            vec![1, 2]
        );
    }

    #[test]
    fn hook_process_resolves_to_nearest_provider_ancestor() {
        let records = BTreeMap::from([
            (1, record(1, None, &["codex", "resume", "uuid"])),
            (2, record(2, Some(1), &["sh", "-c", "pika hook"])),
            (3, record(3, Some(2), &["pika", "hook"])),
        ]);
        assert_eq!(provider_ancestor(3, Provider::Codex, &records), Some(1));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn graceful_stop_is_pinned_to_the_observed_generation() {
        let mut child = Command::new("sleep").arg("30").spawn().unwrap();
        let pid = i64::from(child.id());
        let generation = process_start_time(pid).unwrap();
        terminate_generation(pid, generation, Duration::from_secs(2)).unwrap();
        assert!(child.wait().unwrap().code().is_none());
        assert!(!process_alive(pid, Some(generation)));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_generation_mismatch_never_signals_the_process() {
        let mut child = Command::new("sleep").arg("30").spawn().unwrap();
        let pid = i64::from(child.id());
        let generation = process_start_time(pid).unwrap();
        let error = terminate_generation(pid, generation.saturating_add(1), Duration::from_secs(1))
            .unwrap_err();
        assert!(error.contains("generation changed"));
        assert!(process_alive(pid, Some(generation)));
        child.kill().unwrap();
        child.wait().unwrap();
    }
}
