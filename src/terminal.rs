use anyhow::{Context, Result, bail};
use std::{
    collections::BTreeMap,
    io::{self, IsTerminal, Write},
    process::Command,
    time::{Duration, Instant},
};

pub const FOREGROUND_ENV: &str = "PIKA_TERMINAL_FOREGROUND";
pub const BACKGROUND_ENV: &str = "PIKA_TERMINAL_BACKGROUND";
pub const WINDOWS_TERMINAL_DA2: &[u8] = b"\x1b[>0;10;1c";
const PALETTE_QUERY: &[u8] = b"\x1b]10;?\x1b\\\x1b]11;?\x1b\\";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Palette {
    pub foreground: [u8; 3],
    pub background: [u8; 3],
}

impl Palette {
    pub fn environment(self) -> BTreeMap<String, String> {
        BTreeMap::from([
            (FOREGROUND_ENV.into(), encode_color(self.foreground)),
            (BACKGROUND_ENV.into(), encode_color(self.background)),
        ])
    }
}

pub fn encode_color(color: [u8; 3]) -> String {
    format!("{},{},{}", color[0], color[1], color[2])
}

pub fn decode_color(value: &str) -> Option<[u8; 3]> {
    let values: Vec<u8> = value
        .split(',')
        .map(str::parse)
        .collect::<std::result::Result<_, _>>()
        .ok()?;
    (values.len() == 3).then(|| [values[0], values[1], values[2]])
}

pub fn palette_from_environment() -> Option<Palette> {
    Some(Palette {
        foreground: decode_color(&std::env::var(FOREGROUND_ENV).ok()?)?,
        background: decode_color(&std::env::var(BACKGROUND_ENV).ok()?)?,
    })
}

pub fn parse_palette_response(data: &[u8]) -> Option<Palette> {
    let foreground = parse_osc_color(data, 10)?;
    let background = parse_osc_color(data, 11)?;
    Some(Palette {
        foreground,
        background,
    })
}

fn parse_osc_color(data: &[u8], slot: u8) -> Option<[u8; 3]> {
    let prefix = format!("\x1b]{slot};rgb:");
    let start = find_bytes(data, prefix.as_bytes())? + prefix.len();
    let tail = data.get(start..)?;
    let end = tail
        .windows(2)
        .position(|value| value == b"\x1b\\")
        .or_else(|| tail.iter().position(|value| *value == 7))?;
    let mut output = [0_u8; 3];
    let parts: Vec<_> = tail.get(..end)?.split(|value| *value == b'/').collect();
    if parts.len() < 3 {
        return None;
    }
    for (index, part) in parts[..3].iter().enumerate() {
        if !matches!(part.len(), 2 | 4) || !part.iter().all(u8::is_ascii_hexdigit) {
            return None;
        }
        let text = std::str::from_utf8(part).ok()?;
        let value = u16::from_str_radix(text, 16).ok()?;
        output[index] = if part.len() == 2 {
            value as u8
        } else {
            (value / 257) as u8
        };
    }
    Some(output)
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|value| value == needle)
}

pub struct ExactInputFilter {
    patterns: Vec<Vec<u8>>,
    minimum_prefix: usize,
    pending: Vec<u8>,
}

impl ExactInputFilter {
    pub fn new(patterns: &[&[u8]]) -> Self {
        Self {
            patterns: patterns.iter().map(|value| value.to_vec()).collect(),
            minimum_prefix: 3,
            pending: Vec::new(),
        }
    }

    pub fn feed(&mut self, data: &[u8]) -> Vec<u8> {
        self.pending.extend_from_slice(data);
        let mut visible = Vec::new();
        while !self.pending.is_empty() {
            let found = self
                .patterns
                .iter()
                .filter_map(|pattern| {
                    find_bytes(&self.pending, pattern).map(|at| (at, pattern.len()))
                })
                .min_by_key(|(at, _)| *at);
            if let Some((at, length)) = found {
                visible.extend(self.pending.drain(..at));
                self.pending.drain(..length);
                continue;
            }
            let mut keep = 0;
            for pattern in &self.patterns {
                for size in (self.minimum_prefix..pattern.len().min(self.pending.len() + 1)).rev() {
                    if self.pending.ends_with(&pattern[..size]) {
                        keep = keep.max(size);
                        break;
                    }
                }
            }
            let emit = self.pending.len() - keep;
            visible.extend(self.pending.drain(..emit));
            break;
        }
        visible
    }

    pub fn finish(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.pending)
    }
}

pub struct ColorQueryFilter {
    patterns: Vec<(Vec<u8>, Vec<u8>)>,
    pending: Vec<u8>,
}

impl ColorQueryFilter {
    pub fn new(palette: Palette) -> Self {
        let mut patterns = Vec::new();
        for (slot, color) in [(10, palette.foreground), (11, palette.background)] {
            let reply = format!(
                "\x1b]{slot};rgb:{:04x}/{:04x}/{:04x}\x1b\\",
                u16::from(color[0]) * 257,
                u16::from(color[1]) * 257,
                u16::from(color[2]) * 257
            )
            .into_bytes();
            patterns.push((format!("\x1b]{slot};?\x1b\\").into_bytes(), reply.clone()));
            patterns.push((format!("\x1b]{slot};?\x07").into_bytes(), reply));
        }
        Self {
            patterns,
            pending: Vec::new(),
        }
    }

    pub fn feed(&mut self, data: &[u8]) -> (Vec<u8>, Vec<Vec<u8>>) {
        self.pending.extend_from_slice(data);
        let mut visible = Vec::new();
        let mut replies = Vec::new();
        while !self.pending.is_empty() {
            let found = self
                .patterns
                .iter()
                .enumerate()
                .filter_map(|(index, (pattern, _))| {
                    find_bytes(&self.pending, pattern).map(|at| (at, index))
                })
                .min_by_key(|(at, _)| *at);
            if let Some((at, index)) = found {
                visible.extend(self.pending.drain(..at));
                let length = self.patterns[index].0.len();
                self.pending.drain(..length);
                replies.push(self.patterns[index].1.clone());
                continue;
            }
            let keep = self
                .patterns
                .iter()
                .map(|(pattern, _)| longest_suffix_prefix(&self.pending, pattern))
                .max()
                .unwrap_or(0);
            let emit = self.pending.len() - keep;
            visible.extend(self.pending.drain(..emit));
            break;
        }
        (visible, replies)
    }

    pub fn finish(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.pending)
    }
}

fn longest_suffix_prefix(value: &[u8], pattern: &[u8]) -> usize {
    (1..pattern.len().min(value.len() + 1))
        .rev()
        .find(|size| value.ends_with(&pattern[..*size]))
        .unwrap_or(0)
}

#[cfg(unix)]
pub fn probe_palette(timeout: Duration) -> Option<Palette> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return None;
    }
    let input = libc::STDIN_FILENO;
    let mut previous = std::mem::MaybeUninit::<libc::termios>::uninit();
    if unsafe { libc::tcgetattr(input, previous.as_mut_ptr()) } != 0 {
        return None;
    }
    let previous = unsafe { previous.assume_init() };
    let mut raw = previous;
    unsafe { libc::cfmakeraw(&mut raw) };
    if unsafe { libc::tcsetattr(input, libc::TCSANOW, &raw) } != 0 {
        return None;
    }
    struct Restore(libc::termios);
    impl Drop for Restore {
        fn drop(&mut self) {
            unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSADRAIN, &self.0) };
        }
    }
    let _restore = Restore(previous);
    io::stdout().write_all(PALETTE_QUERY).ok()?;
    io::stdout().flush().ok()?;
    let deadline = Instant::now() + timeout;
    let mut data = Vec::new();
    while Instant::now() < deadline && data.len() < 1024 {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let mut poll = libc::pollfd {
            fd: input,
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe {
            libc::poll(
                &mut poll,
                1,
                remaining.as_millis().min(i32::MAX as u128) as i32,
            )
        };
        if ready <= 0 {
            break;
        }
        let mut buffer = [0_u8; 256];
        let read = unsafe { libc::read(input, buffer.as_mut_ptr().cast(), buffer.len()) };
        if read <= 0 {
            break;
        }
        data.extend_from_slice(&buffer[..read as usize]);
        if let Some(palette) = parse_palette_response(&data) {
            return Some(palette);
        }
    }
    None
}

#[cfg(not(unix))]
pub fn probe_palette(_timeout: Duration) -> Option<Palette> {
    None
}

pub fn palette_environment() -> BTreeMap<String, String> {
    palette_from_environment()
        .or_else(|| probe_palette(Duration::from_millis(120)))
        .map(Palette::environment)
        .unwrap_or_default()
}

/// Run an interactive child behind a private PTY. Codex gets exact OSC 10/11
/// replies; tmux attach gets the exact Windows Terminal DA2 reply removed.
#[cfg(unix)]
pub fn run_pty_bridge(
    argv: &[String],
    palette: Option<Palette>,
    suppress_da2: bool,
) -> Result<i32> {
    run_pty_bridge_with_started(argv, palette, suppress_da2, || Ok(()))
}

/// Run an interactive child and invoke `on_started` exactly once after the
/// terminal client completes successfully. Process survival is not proof that
/// tmux accepted the attach, so every non-zero exit remains fail-closed.
#[cfg(unix)]
pub fn run_pty_bridge_with_started<F>(
    argv: &[String],
    palette: Option<Palette>,
    suppress_da2: bool,
    on_started: F,
) -> Result<i32>
where
    F: FnOnce() -> Result<()>,
{
    use std::os::unix::process::CommandExt;
    if argv.is_empty() {
        bail!("missing terminal bridge command");
    }
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        let mut child = Command::new(&argv[0]).args(&argv[1..]).spawn()?;
        let status = child.wait()?;
        if status.success() {
            on_started()?;
        }
        return Ok(status.code().unwrap_or(1));
    }
    let mut master = 0;
    let child = unsafe {
        libc::forkpty(
            &mut master,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if child < 0 {
        return Err(io::Error::last_os_error()).context("cannot create private terminal");
    }
    if child == 0 {
        let error = Command::new(&argv[0]).args(&argv[1..]).exec();
        eprintln!("pika: cannot start {}: {error}", argv[0]);
        unsafe { libc::_exit(127) };
    }
    let code = bridge_parent(child, master, palette, suppress_da2)?;
    if code == 0 {
        on_started()?;
    }
    Ok(code)
}

#[cfg(not(unix))]
pub fn run_pty_bridge(
    argv: &[String],
    _palette: Option<Palette>,
    _suppress_da2: bool,
) -> Result<i32> {
    run_pty_bridge_with_started(argv, None, false, || Ok(()))
}

#[cfg(not(unix))]
pub fn run_pty_bridge_with_started<F>(
    argv: &[String],
    _palette: Option<Palette>,
    _suppress_da2: bool,
    on_started: F,
) -> Result<i32>
where
    F: FnOnce() -> Result<()>,
{
    if argv.is_empty() {
        bail!("missing terminal bridge command");
    }
    let mut child = Command::new(&argv[0]).args(&argv[1..]).spawn()?;
    let status = child.wait()?;
    if status.success() {
        on_started()?;
    }
    Ok(status.code().unwrap_or(1))
}

#[cfg(unix)]
fn bridge_parent(
    child: libc::pid_t,
    master: libc::c_int,
    palette: Option<Palette>,
    suppress_da2: bool,
) -> Result<i32> {
    let input = libc::STDIN_FILENO;
    let output = libc::STDOUT_FILENO;
    let mut prior = std::mem::MaybeUninit::<libc::termios>::uninit();
    if unsafe { libc::tcgetattr(input, prior.as_mut_ptr()) } != 0 {
        bail!("cannot read terminal mode");
    }
    let prior = unsafe { prior.assume_init() };
    let mut raw = prior;
    unsafe { libc::cfmakeraw(&mut raw) };
    if unsafe { libc::tcsetattr(input, libc::TCSANOW, &raw) } != 0 {
        bail!("cannot enter raw terminal mode");
    }
    struct Guard {
        mode: libc::termios,
        master: libc::c_int,
    }
    impl Drop for Guard {
        fn drop(&mut self) {
            unsafe {
                libc::tcsetattr(libc::STDIN_FILENO, libc::TCSADRAIN, &self.mode);
                libc::close(self.master);
            }
        }
    }
    let _guard = Guard {
        mode: prior,
        master,
    };
    let mut color_filter = palette.map(ColorQueryFilter::new);
    let mut input_filter = suppress_da2.then(|| ExactInputFilter::new(&[WINDOWS_TERMINAL_DA2]));
    let mut status = None;
    loop {
        copy_terminal_size(input, master);
        let mut descriptors = [
            libc::pollfd {
                fd: input,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: master,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let polled = unsafe { libc::poll(descriptors.as_mut_ptr(), 2, 100) };
        if polled < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return Err(io::Error::last_os_error()).context("terminal bridge poll failed");
        }
        if descriptors[0].revents & libc::POLLIN != 0 {
            let mut buffer = [0_u8; 4096];
            let count = unsafe { libc::read(input, buffer.as_mut_ptr().cast(), buffer.len()) };
            if count > 0 {
                let visible = input_filter.as_mut().map_or_else(
                    || buffer[..count as usize].to_vec(),
                    |filter| filter.feed(&buffer[..count as usize]),
                );
                write_fd(master, &visible)?;
            }
        }
        if descriptors[1].revents & (libc::POLLIN | libc::POLLHUP) != 0 {
            let mut buffer = [0_u8; 65_536];
            let count = unsafe { libc::read(master, buffer.as_mut_ptr().cast(), buffer.len()) };
            if count > 0 {
                if let Some(filter) = &mut color_filter {
                    let (visible, replies) = filter.feed(&buffer[..count as usize]);
                    write_fd(output, &visible)?;
                    for reply in replies {
                        write_fd(master, &reply)?;
                    }
                } else {
                    write_fd(output, &buffer[..count as usize])?;
                }
            } else if count == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EIO) {
                break;
            }
        }
        let mut candidate = 0;
        let waited = unsafe { libc::waitpid(child, &mut candidate, libc::WNOHANG) };
        if waited == child {
            status = Some(candidate);
            if descriptors[1].revents == 0 {
                break;
            }
        }
    }
    if let Some(filter) = &mut color_filter {
        write_fd(output, &filter.finish())?;
    }
    let raw_status = if let Some(status) = status {
        status
    } else {
        let mut value = 0;
        unsafe { libc::waitpid(child, &mut value, 0) };
        value
    };
    Ok(wait_exit_code(raw_status))
}

#[cfg(unix)]
fn copy_terminal_size(source: libc::c_int, target: libc::c_int) {
    let mut size = std::mem::MaybeUninit::<libc::winsize>::uninit();
    if unsafe { libc::ioctl(source, libc::TIOCGWINSZ, size.as_mut_ptr()) } == 0 {
        let size = unsafe { size.assume_init() };
        unsafe { libc::ioctl(target, libc::TIOCSWINSZ, &size) };
    }
}

#[cfg(unix)]
fn write_fd(fd: libc::c_int, mut data: &[u8]) -> Result<()> {
    while !data.is_empty() {
        let count = unsafe { libc::write(fd, data.as_ptr().cast(), data.len()) };
        if count < 0 {
            return Err(io::Error::last_os_error()).context("terminal bridge write failed");
        }
        data = &data[count as usize..];
    }
    Ok(())
}

#[cfg(unix)]
fn wait_exit_code(status: libc::c_int) -> i32 {
    if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else if libc::WIFSIGNALED(status) {
        128 + libc::WTERMSIG(status)
    } else {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_palette_in_any_order_and_precision() {
        let data = b"noise\x1b]11;rgb:2222/2121/3333\x1b\\\x1b]10;rgb:dd/cc/bb\x07";
        assert_eq!(
            parse_palette_response(data),
            Some(Palette {
                foreground: [221, 204, 187],
                background: [34, 33, 51]
            })
        );
    }

    #[test]
    fn exact_filter_removes_fragmented_da2_only() {
        let mut filter = ExactInputFilter::new(&[WINDOWS_TERMINAL_DA2]);
        let mut output = filter.feed(b"before\x1b[>");
        output.extend(filter.feed(b"0;10;"));
        output.extend(filter.feed(b"1cafter"));
        output.extend(filter.finish());
        assert_eq!(output, b"beforeafter");
    }

    #[test]
    fn color_filter_answers_queries_without_showing_them() {
        let mut filter = ColorQueryFilter::new(Palette {
            foreground: [221, 204, 187],
            background: [34, 33, 51],
        });
        let (first, none) = filter.feed(b"before\x1b]10;");
        let (second, replies) = filter.feed(b"?\x1b\\middle\x1b]11;?\x1b\\after");
        assert!(none.is_empty());
        assert_eq!(
            [first, second, filter.finish()].concat(),
            b"beforemiddleafter"
        );
        assert_eq!(replies.len(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn attach_callback_records_only_successful_terminal_completion() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };

        let failed_calls = Arc::new(AtomicUsize::new(0));
        let failed_counter = Arc::clone(&failed_calls);
        let failed = run_pty_bridge_with_started(
            &["sh".into(), "-c".into(), "exit 17".into()],
            None,
            false,
            move || {
                failed_counter.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(failed, 17);
        assert_eq!(failed_calls.load(Ordering::SeqCst), 0);

        let interrupted_calls = Arc::new(AtomicUsize::new(0));
        let interrupted_counter = Arc::clone(&interrupted_calls);
        let interrupted = run_pty_bridge_with_started(
            &["sh".into(), "-c".into(), "sleep 0.25; exit 130".into()],
            None,
            false,
            move || {
                interrupted_counter.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(interrupted, 130);
        assert_eq!(interrupted_calls.load(Ordering::SeqCst), 0);

        let successful_calls = Arc::new(AtomicUsize::new(0));
        let successful_counter = Arc::clone(&successful_calls);
        let successful = run_pty_bridge_with_started(
            &["sh".into(), "-c".into(), "sleep 0.25; exit 0".into()],
            None,
            false,
            move || {
                successful_counter.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(successful, 0);
        assert_eq!(successful_calls.load(Ordering::SeqCst), 1);
    }
}
