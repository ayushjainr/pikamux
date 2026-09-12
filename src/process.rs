use crate::model::Provider;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::path::Path;
use std::time::Duration;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ObservationState {
    Complete,
    Partial(Vec<String>),
    Error(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessObservation {
    pub processes: BTreeMap<i64, ProcessRecord>,
    pub state: ObservationState,
}

impl ProcessObservation {
    pub fn complete(processes: BTreeMap<i64, ProcessRecord>) -> Self {
        Self {
            processes,
            state: ObservationState::Complete,
        }
    }

    pub fn partial(processes: BTreeMap<i64, ProcessRecord>, issues: Vec<String>) -> Self {
        Self {
            processes,
            state: ObservationState::Partial(issues),
        }
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self {
            processes: BTreeMap::new(),
            state: ObservationState::Error(message.into()),
        }
    }

    pub fn require_complete(
        &self,
        operation: &str,
    ) -> Result<&BTreeMap<i64, ProcessRecord>, String> {
        match &self.state {
            ObservationState::Complete => Ok(&self.processes),
            ObservationState::Partial(issues) => Err(format!(
                "Pika refused to {operation}: process identity observation was partial ({}). No provider was launched and no stored ownership was cleared.",
                issues.join("; ")
            )),
            ObservationState::Error(error) => Err(format!(
                "Pika refused to {operation}: process identity could not be observed ({error}). No provider was launched and no stored ownership was cleared."
            )),
        }
    }
}

impl std::ops::Deref for ProcessObservation {
    type Target = BTreeMap<i64, ProcessRecord>;

    fn deref(&self) -> &Self::Target {
        &self.processes
    }
}

fn collect_observation(
    pids: Result<Vec<i64>, String>,
    mut read: impl FnMut(i64) -> Result<Option<ProcessRecord>, String>,
) -> ProcessObservation {
    let pids = match pids {
        Ok(pids) => pids,
        Err(error) => return ProcessObservation::error(error),
    };
    let mut processes = BTreeMap::new();
    let mut issues = Vec::new();
    for pid in pids {
        match read(pid) {
            Ok(Some(record)) => {
                processes.insert(pid, record);
            }
            Ok(None) => {}
            Err(error) => issues.push(format!("PID {pid}: {error}")),
        }
    }
    if issues.is_empty() {
        ProcessObservation::complete(processes)
    } else {
        ProcessObservation::partial(processes, issues)
    }
}

#[cfg(any(target_os = "linux", test))]
fn relevant_process_owner(owner_uid: u32, current_uid: u32) -> bool {
    owner_uid == current_uid
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessRecord {
    pub pid: i64,
    pub parent_pid: Option<i64>,
    pub start_time: u64,
    pub argv: Vec<String>,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ProcessGeneration {
    pub pid: i64,
    pub start_time: u64,
}

impl ProcessRecord {
    pub fn provider(&self) -> Option<Provider> {
        process_kind(&self.argv)
    }

    pub fn generation(&self) -> ProcessGeneration {
        ProcessGeneration {
            pid: self.pid,
            start_time: self.start_time,
        }
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

pub fn observe() -> ProcessObservation {
    platform::snapshot()
}

/// Compatibility view for read-only callers that do not make ownership
/// decisions. Identity-changing paths must use `observe` and require complete
/// evidence instead of treating observation failure as an empty machine.
pub fn snapshot() -> BTreeMap<i64, ProcessRecord> {
    observe().processes
}

pub fn process_start_time(pid: i64) -> Option<u64> {
    platform::read(pid).map(|record| record.start_time)
}

pub fn process_generation(pid: i64) -> Option<ProcessGeneration> {
    platform::read(pid).map(|record| record.generation())
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

/// Return a process tree only when the pane/root PID is still the exact
/// generation captured by the caller. A reusable numeric PID is never an
/// ancestry root for an identity-changing action.
pub fn process_tree_generation(
    root: ProcessGeneration,
    processes: &BTreeMap<i64, ProcessRecord>,
) -> Vec<i64> {
    if processes.get(&root.pid).map(ProcessRecord::generation) != Some(root) {
        return Vec::new();
    }
    process_tree(root.pid, processes)
}

fn ancestry_path(
    root: ProcessGeneration,
    descendant: ProcessGeneration,
    processes: &BTreeMap<i64, ProcessRecord>,
) -> Option<Vec<ProcessRecord>> {
    let mut current = descendant.pid;
    let mut seen = BTreeSet::new();
    let mut path = Vec::new();
    loop {
        if !seen.insert(current) {
            return None;
        }
        let record = processes.get(&current)?;
        if current == descendant.pid && record.generation() != descendant {
            return None;
        }
        path.push(record.clone());
        if current == root.pid {
            return (record.generation() == root).then_some(path);
        }
        current = record.parent_pid?;
    }
}

fn revalidate_ancestry_with(
    root: ProcessGeneration,
    descendant: ProcessGeneration,
    processes: &BTreeMap<i64, ProcessRecord>,
    mut read: impl FnMut(i64) -> Option<ProcessRecord>,
) -> Result<(), String> {
    let path = ancestry_path(root, descendant, processes).ok_or_else(|| {
        "the provider is no longer descended from the exact pane generation".to_owned()
    })?;
    for expected in path {
        let current = read(expected.pid)
            .ok_or_else(|| format!("process {} disappeared during ancestry proof", expected.pid))?;
        if current.start_time != expected.start_time {
            return Err(format!(
                "process {} changed generation during ancestry proof",
                expected.pid
            ));
        }
        // The pane root's parent is outside the ownership chain. Reparenting
        // a descendant changes membership; reparenting the root itself does
        // not move the provider outside that root.
        if expected.pid != root.pid && current.parent_pid != expected.parent_pid {
            return Err(format!(
                "process {} changed parent during ancestry proof",
                expected.pid
            ));
        }
    }
    Ok(())
}

/// Re-read every `(pid, start_time, parent_pid)` edge from the exact provider
/// to the pane root. This must be called immediately before an exact action;
/// a process reparent, root exit/reuse, or descendant reuse fails closed.
pub fn revalidate_ancestry(
    root: ProcessGeneration,
    descendant: ProcessGeneration,
    processes: &BTreeMap<i64, ProcessRecord>,
) -> Result<(), String> {
    revalidate_ancestry_with(root, descendant, processes, platform::read)
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
    use super::{ProcessObservation, ProcessRecord};
    use std::{fs, io::ErrorKind, os::unix::fs::MetadataExt, path::Path};

    pub fn snapshot() -> ProcessObservation {
        let entries = match fs::read_dir("/proc") {
            Ok(entries) => entries,
            Err(error) => {
                return ProcessObservation::error(format!("cannot enumerate /proc: {error}"));
            }
        };
        let mut pids = Vec::new();
        let mut entry_issues = Vec::new();
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    entry_issues.push(format!("cannot read /proc entry: {error}"));
                    continue;
                }
            };
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|value| value.parse::<i64>().ok())
            else {
                continue;
            };
            let metadata = match entry.metadata() {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == ErrorKind::NotFound => continue,
                Err(error) => {
                    entry_issues.push(format!("PID {pid}: cannot identify owner: {error}"));
                    continue;
                }
            };
            // `/proc` lists the whole host. Foreign cmdlines are irrelevant to
            // this user's conversations and are commonly unreadable under
            // hidepid; only an unreadable same-user entry is uncertainty.
            if !super::relevant_process_owner(metadata.uid(), unsafe { libc::getuid() }) {
                continue;
            }
            pids.push(pid);
        }
        let mut observed = super::collect_observation(Ok(pids), read_result);
        if !entry_issues.is_empty() {
            if let super::ObservationState::Partial(issues) = &mut observed.state {
                issues.extend(entry_issues);
            } else {
                observed.state = super::ObservationState::Partial(entry_issues);
            }
        }
        observed
    }

    pub fn read(pid: i64) -> Option<ProcessRecord> {
        read_result(pid).ok().flatten()
    }

    fn read_result(pid: i64) -> Result<Option<ProcessRecord>, String> {
        if pid <= 0 {
            return Ok(None);
        }
        let root = Path::new("/proc").join(pid.to_string());
        let stat = match fs::read_to_string(root.join("stat")) {
            Ok(stat) => stat,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(format!("cannot read stat: {error}")),
        };
        let tail = stat
            .get(stat.rfind(')').ok_or("malformed stat")? + 2..)
            .ok_or("malformed stat")?;
        let fields: Vec<_> = tail.split_whitespace().collect();
        let parsed_parent = fields
            .get(1)
            .ok_or("stat has no parent PID")?
            .parse::<i64>()
            .map_err(|_| "invalid parent PID")?;
        let parent_pid = (parsed_parent > 0).then_some(parsed_parent);
        let start_time = fields
            .get(19)
            .ok_or("stat has no start time")?
            .parse()
            .map_err(|_| "invalid start time")?;
        let raw = match fs::read(root.join("cmdline")) {
            Ok(raw) => raw,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(format!("cannot read command line: {error}")),
        };
        let argv = raw
            .split(|byte| *byte == 0)
            .filter(|value| !value.is_empty())
            .map(|value| String::from_utf8_lossy(value).into_owned())
            .collect();
        // Re-read the generation after argv so PID exit/reuse during the
        // observation cannot turn two different processes into one record.
        let verified = match fs::read_to_string(root.join("stat")) {
            Ok(stat) => stat,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(format!("cannot verify stat: {error}")),
        };
        let verified_tail = verified
            .get(verified.rfind(')').ok_or("malformed verified stat")? + 2..)
            .ok_or("malformed verified stat")?;
        let verified_start: u64 = verified_tail
            .split_whitespace()
            .nth(19)
            .ok_or("verified stat has no start time")?
            .parse()
            .map_err(|_| "invalid verified start time")?;
        if start_time != verified_start {
            return Ok(None);
        }
        Ok(Some(ProcessRecord {
            pid,
            parent_pid,
            start_time,
            argv,
        }))
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::{ProcessObservation, ProcessRecord};
    use libproc::libproc::{bsd_info::BSDInfo, proc_pid::pidinfo};
    use libproc::processes::{ProcFilter, pids_by_type};

    pub fn snapshot() -> ProcessObservation {
        let pids = match pids_by_type(ProcFilter::ByUID {
            // SAFETY: getuid has no preconditions and does not mutate memory.
            uid: unsafe { libc::getuid() },
        }) {
            Ok(pids) => pids,
            Err(error) => {
                return ProcessObservation::error(format!("cannot enumerate processes: {error}"));
            }
        };
        super::collect_observation(Ok(pids.into_iter().map(i64::from).collect()), read_result)
    }

    pub fn read(pid: i64) -> Option<ProcessRecord> {
        read_result(pid).ok().flatten()
    }

    fn read_result(pid: i64) -> Result<Option<ProcessRecord>, String> {
        let Some(pid32) = i32::try_from(pid).ok().filter(|value| *value > 0) else {
            return Ok(None);
        };
        let info = match pidinfo::<BSDInfo>(pid32, 0) {
            Ok(info) => info,
            Err(error) if process_info_error_is_missing(&error) => return Ok(None),
            Err(_error) if !process_exists(pid32) => return Ok(None),
            Err(error) => return Err(format!("cannot read process info: {error}")),
        };
        // A same-user check is part of identity evidence; denied/foreign reads
        // become absence rather than a guess from a title or PID.
        let current_uid = unsafe { libc::getuid() };
        if i64::from(info.pbi_pid) != pid {
            return Err("process identity changed during observation".into());
        }
        if info.pbi_ruid != current_uid {
            return Ok(None);
        }
        // Login/session supervisors can retain the user's real UID while
        // running with another effective UID. They cannot be a user-owned
        // provider client and macOS correctly denies their argv; exclude them
        // before that denial is classified as incomplete same-user evidence.
        if info.pbi_uid != current_uid {
            return Ok(None);
        }
        if info.pbi_status == super::MACOS_ZOMBIE_STATUS {
            return Ok(None);
        }
        let start_time = info
            .pbi_start_tvsec
            .checked_mul(1_000_000)
            .and_then(|value| value.checked_add(info.pbi_start_tvusec))
            .ok_or_else(|| "invalid process start time".to_owned())?;
        let mut argv = None;
        for attempt in 0..3 {
            if let Some(arguments) = process_arguments(pid32) {
                argv = Some(arguments);
                break;
            }
            match pidinfo::<BSDInfo>(pid32, 0) {
                Err(error) if process_info_error_is_missing(&error) => return Ok(None),
                Err(_) if !process_exists(pid32) => return Ok(None),
                Err(error) => {
                    return Err(format!("cannot verify unreadable process info: {error}"));
                }
                Ok(current)
                    if super::mac_unreadable_process_is_transient(
                        pid,
                        start_time,
                        Some((
                            i64::from(current.pbi_pid),
                            current
                                .pbi_start_tvsec
                                .checked_mul(1_000_000)
                                .and_then(|value| value.checked_add(current.pbi_start_tvusec))
                                .unwrap_or_default(),
                            current.pbi_status,
                        )),
                    ) =>
                {
                    return Ok(None);
                }
                Ok(_) if attempt < 2 => std::thread::sleep(std::time::Duration::from_millis(2)),
                Ok(_) => return Err("cannot read process command line after bounded retry".into()),
            }
        }
        let argv = argv.ok_or_else(|| "cannot read process command line".to_owned())?;
        // Re-read after argv so exit/reuse during observation fails closed.
        let verified = match pidinfo::<BSDInfo>(pid32, 0) {
            Ok(info) => info,
            Err(error) if process_info_error_is_missing(&error) => return Ok(None),
            Err(_) if !process_exists(pid32) => return Ok(None),
            Err(error) => return Err(format!("cannot verify process info: {error}")),
        };
        let verified_start = verified
            .pbi_start_tvsec
            .checked_mul(1_000_000)
            .and_then(|value| value.checked_add(verified.pbi_start_tvusec))
            .ok_or_else(|| "invalid verified process start time".to_owned())?;
        if start_time != verified_start || verified.pbi_ruid != info.pbi_ruid {
            return Ok(None);
        }
        Ok(Some(ProcessRecord {
            pid,
            parent_pid: (info.pbi_ppid > 0).then(|| i64::from(info.pbi_ppid)),
            start_time,
            argv,
        }))
    }

    fn process_exists(pid: i32) -> bool {
        // SAFETY: signal zero performs an existence/permission check only.
        let result = unsafe { libc::kill(pid, 0) };
        result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }

    fn process_info_error_is_missing(error: &str) -> bool {
        // libproc returns its errno as a formatted string. ESRCH is definitive
        // evidence that the enumerated generation vanished; a later `kill(0)`
        // success may already refer to a reused PID and must not turn that
        // ordinary race into a permanently partial host observation.
        error.contains("errno = 3,")
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

// Darwin's proc status value for an unreaped zombie. Kept as a numeric wire
// value so this race-classification helper remains testable on every host.
const MACOS_ZOMBIE_STATUS: u32 = 5;

fn mac_unreadable_process_is_transient(
    original_pid: i64,
    original_start: u64,
    reread: Option<(i64, u64, u32)>,
) -> bool {
    match reread {
        None => true,
        Some((pid, start, status)) => {
            status == MACOS_ZOMBIE_STATUS || pid != original_pid || start != original_start
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod platform {
    use super::{ProcessObservation, ProcessRecord};
    use std::collections::BTreeMap;
    pub fn snapshot() -> ProcessObservation {
        ProcessObservation::error("process identity observation is unsupported on this platform")
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

    #[test]
    fn generation_bound_tree_rejects_reused_root_pid() {
        let records = BTreeMap::from([
            (1, record(1, None, &["zsh"])),
            (2, record(2, Some(1), &["codex", "resume", "uuid"])),
        ]);
        assert_eq!(
            process_tree_generation(records[&1].generation(), &records),
            vec![1, 2]
        );
        assert!(
            process_tree_generation(
                ProcessGeneration {
                    pid: 1,
                    start_time: 999,
                },
                &records,
            )
            .is_empty()
        );
    }

    #[test]
    fn ancestry_revalidation_rejects_ppid_change_and_generation_reuse() {
        let records = BTreeMap::from([
            (1, record(1, None, &["zsh"])),
            (2, record(2, Some(1), &["sh"])),
            (3, record(3, Some(2), &["codex", "resume", "uuid"])),
        ]);
        let root = records[&1].generation();
        let provider = records[&3].generation();
        revalidate_ancestry_with(root, provider, &records, |pid| records.get(&pid).cloned())
            .unwrap();

        let error = revalidate_ancestry_with(root, provider, &records, |pid| {
            let mut current = records.get(&pid)?.clone();
            if pid == 3 {
                current.parent_pid = Some(1);
            }
            Some(current)
        })
        .unwrap_err();
        assert!(error.contains("changed parent"));

        let error = revalidate_ancestry_with(root, provider, &records, |pid| {
            let mut current = records.get(&pid)?.clone();
            if pid == 1 {
                current.start_time += 1;
            }
            Some(current)
        })
        .unwrap_err();
        assert!(error.contains("changed generation"));
    }

    #[test]
    fn enumeration_failure_is_not_an_empty_complete_snapshot() {
        let observed = collect_observation(Err("enumeration denied".into()), |_| Ok(None));
        assert_eq!(
            observed.state,
            ObservationState::Error("enumeration denied".into())
        );
        assert!(observed.processes.is_empty());
        assert!(
            observed
                .require_complete("resume the conversation")
                .unwrap_err()
                .contains("No provider was launched")
        );
    }

    #[test]
    fn one_denied_pid_makes_the_whole_identity_snapshot_partial() {
        let observed = collect_observation(Ok(vec![1, 2, 3]), |pid| match pid {
            1 => Ok(Some(record(1, None, &["codex", "resume", "uuid"]))),
            2 => Err("permission denied".into()),
            _ => Ok(None), // A process that vanished during enumeration is benign.
        });
        assert_eq!(observed.processes.len(), 1);
        assert!(matches!(observed.state, ObservationState::Partial(_)));
        assert!(observed.require_complete("reconcile ownership").is_err());
    }

    #[test]
    fn macos_unreadable_pid_race_skips_only_absent_zombie_or_reused_processes() {
        assert!(mac_unreadable_process_is_transient(42, 100, None));
        assert!(mac_unreadable_process_is_transient(
            42,
            100,
            Some((42, 100, MACOS_ZOMBIE_STATUS))
        ));
        assert!(mac_unreadable_process_is_transient(
            42,
            100,
            Some((42, 101, 2))
        ));
        assert!(!mac_unreadable_process_is_transient(
            42,
            100,
            Some((42, 100, 2))
        ));
    }

    #[test]
    fn a_later_complete_observation_recovers_after_partial_evidence() {
        let partial = collect_observation(Ok(vec![1]), |_| Err("permission denied".into()));
        assert!(partial.require_complete("open").is_err());

        let recovered = collect_observation(Ok(vec![1]), |pid| {
            Ok(Some(record(pid, None, &["codex", "resume", "uuid"])))
        });
        assert_eq!(recovered.state, ObservationState::Complete);
        assert_eq!(recovered.processes.keys().copied().collect::<Vec<_>>(), [1]);
        assert!(recovered.require_complete("open").is_ok());
    }

    #[test]
    fn foreign_processes_are_outside_the_identity_scan() {
        assert!(relevant_process_owner(501, 501));
        assert!(!relevant_process_owner(502, 501));
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
