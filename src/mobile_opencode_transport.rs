use super::Binding;
use anyhow::{Context, Result, bail, ensure};
use base64::Engine;
use serde_json::Value;
use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpStream},
    time::{Duration, Instant},
};

pub(super) fn request(
    binding: &Binding,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> Result<Value> {
    request_checked(
        binding,
        method,
        path,
        body,
        Duration::from_secs(10),
        &|| Ok(()),
    )
}

pub(super) fn bootstrap(binding: &Binding, check: &dyn Fn() -> Result<()>) -> Result<()> {
    // Cold native plugin dependency materialization was measured at 12.334s.
    // Warmup is read-only; session creation and ordinary RPCs retain 10s/no replay.
    request_checked(
        binding,
        "GET",
        "/session?limit=1",
        None,
        Duration::from_secs(30),
        check,
    )?;
    Ok(())
}

fn request_checked(
    binding: &Binding,
    method: &str,
    path: &str,
    body: Option<Value>,
    budget: Duration,
    check: &dyn Fn() -> Result<()>,
) -> Result<Value> {
    check()?;
    let address: SocketAddr = ([127, 0, 0, 1], binding.port).into();
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(2))?;
    stream.set_read_timeout(Some(Duration::from_millis(100)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    let request_prefix = request_prefix(&mut stream, binding, method)?;
    let data = body.map(|v| v.to_string()).unwrap_or_default();
    ensure!(data.len() <= 128 * 1024, "Native request too large");
    let auth =
        base64::engine::general_purpose::STANDARD.encode(format!("opencode:{}", binding.password));
    write!(
        stream,
        "{request_prefix}{path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Basic {auth}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{data}",
        binding.port,
        data.len()
    )?;
    let (value, cursor) = read_response(&mut stream, budget, check)?;
    check()?;
    ensure!(
        super::alive(binding.server_pid, binding.server_start),
        "Native server generation changed"
    );
    if method == "GET" && path.contains("/message?") {
        Ok(serde_json::json!({"data":value,"nativeNextCursor":cursor}))
    } else {
        Ok(value)
    }
}

fn request_prefix(stream: &mut TcpStream, binding: &Binding, method: &str) -> Result<String> {
    ensure!(
        matches!(method, "GET" | "POST"),
        "Unsupported native HTTP method"
    );
    #[cfg(target_os = "linux")]
    {
        // Native Bun uses TCP_DEFER_ACCEPT: no accepted inode exists until data
        // arrives. Only this fixed method token (never path, auth or content)
        // may precede the unchanged exact accepted-socket ownership proof.
        ensure!(
            super::alive(binding.server_pid, binding.server_start),
            "Native server generation unavailable"
        );
        stream.write_all(format!("{method} ").as_bytes())?;
        stream.flush()?;
        prove_peer(binding, stream)?;
        Ok(String::new())
    }
    #[cfg(not(target_os = "linux"))]
    {
        prove_peer(binding, stream)?;
        Ok(format!("{method} "))
    }
}

fn read_response(
    stream: &mut TcpStream,
    budget: Duration,
    check: &dyn Fn() -> Result<()>,
) -> Result<(Value, Option<String>)> {
    let deadline = Instant::now() + budget;
    let mut bytes = Vec::new();
    loop {
        check()?;
        ensure!(Instant::now() < deadline, "Native API response timed out");
        let mut chunk = [0; 8192];
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => bytes.extend_from_slice(&chunk[..n]),
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::Interrupted
                ) =>
            {
                continue;
            }
            Err(e) => return Err(e.into()),
        }
        ensure!(
            bytes.len() <= 16 * 1024 * 1024,
            "Native API response too large"
        );
        if response_complete(&bytes)? {
            break;
        }
    }
    let (status, head, body) = split_response(&bytes)?;
    ensure!(
        (200..300).contains(&status),
        "Native API rejected HTTP {status}"
    );
    if status == 204 {
        return Ok((Value::Null, None));
    };
    let body = if head
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        decode_chunked(body)?
    } else {
        body.to_vec()
    };
    let cursor = head.lines().find_map(|line| {
        line.split_once(':')
            .filter(|(name, _)| name.eq_ignore_ascii_case("x-next-cursor"))
            .map(|(_, value)| value.trim().to_owned())
    });
    ensure!(
        cursor.as_ref().is_none_or(|value| value.len() <= 2048
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_-+=/".contains(&byte))),
        "Invalid native history cursor header"
    );
    Ok((serde_json::from_slice(&body)?, cursor))
}

fn split_response(bytes: &[u8]) -> Result<(u16, &str, &[u8])> {
    let split = bytes
        .windows(4)
        .position(|v| v == b"\r\n\r\n")
        .context("Malformed native HTTP response")?;
    let head = std::str::from_utf8(&bytes[..split])?;
    let status = head
        .lines()
        .next()
        .and_then(|v| v.split_whitespace().nth(1))
        .context("Missing native HTTP status")?
        .parse()?;
    Ok((status, head, &bytes[split + 4..]))
}

fn response_complete(bytes: &[u8]) -> Result<bool> {
    if !bytes.windows(4).any(|v| v == b"\r\n\r\n") {
        return Ok(false);
    };
    let (status, head, body) = split_response(bytes)?;
    if status == 204 {
        return Ok(true);
    };
    if let Some(length) = head.lines().find_map(|line| {
        line.to_ascii_lowercase()
            .strip_prefix("content-length:")
            .map(|s| s.trim().to_owned())
    }) {
        let length = length.parse::<usize>()?;
        ensure!(
            length <= 16 * 1024 * 1024,
            "Native API content length too large"
        );
        return Ok(body.len() >= length);
    }
    Ok(head
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
        && body.ends_with(b"\r\n0\r\n\r\n"))
}

fn decode_chunked(mut encoded: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let pos = encoded
            .windows(2)
            .position(|v| v == b"\r\n")
            .context("Invalid native chunk header")?;
        let size = usize::from_str_radix(
            std::str::from_utf8(&encoded[..pos])?
                .split(';')
                .next()
                .unwrap_or(""),
            16,
        )?;
        encoded = &encoded[pos + 2..];
        if size == 0 {
            return Ok(out);
        };
        ensure!(
            size <= encoded.len().saturating_sub(2) && out.len() + size <= 16 * 1024 * 1024,
            "Invalid native chunk size"
        );
        out.extend_from_slice(&encoded[..size]);
        ensure!(
            &encoded[size..size + 2] == b"\r\n",
            "Invalid native chunk boundary"
        );
        encoded = &encoded[size + 2..];
    }
}

fn prove_peer(binding: &Binding, stream: &TcpStream) -> Result<()> {
    ensure!(
        super::alive(binding.server_pid, binding.server_start),
        "Native server generation unavailable"
    );
    let local = stream.local_addr()?;
    let started = Instant::now();
    // Native Linux accept was measured at 752ms while bounded /proc scans took
    // at most 49ms. This is an ownership-readiness budget, not an RPC retry.
    #[cfg(target_os = "linux")]
    let deadline = started + Duration::from_secs(2);
    #[cfg(not(target_os = "linux"))]
    let deadline = started + Duration::from_millis(500);
    while Instant::now() < deadline {
        let owned = peer_owned(binding.server_pid, local.port(), binding.port)?;
        if owned {
            ensure!(
                super::alive(binding.server_pid, binding.server_start),
                "Native server generation changed"
            );
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    // The final bounded kernel read can straddle acceptance at the deadline.
    // Never reject a subsequently observed exact peer merely because the prior
    // fd/table snapshots preceded accept; the unchanged generation fence applies.
    if peer_owned(binding.server_pid, local.port(), binding.port)? {
        ensure!(
            super::alive(binding.server_pid, binding.server_start),
            "Native server generation changed"
        );
        return Ok(());
    }
    #[cfg(target_os = "linux")]
    {
        bail!(
            "Connected socket is not owned by the certified native server (elapsedMs={},generationMatches={}, {})",
            started.elapsed().as_millis(),
            super::alive(binding.server_pid, binding.server_start),
            linux::peer_diagnostic(binding.server_pid, local.port(), binding.port)
                .unwrap_or_else(|_| "kernel evidence unavailable".into())
        );
    }
    #[cfg(not(target_os = "linux"))]
    bail!("Connected socket is not owned by the certified native server")
}

#[cfg(target_os = "macos")]
fn peer_owned(pid: i64, client: u16, server: u16) -> Result<bool> {
    use libproc::libproc::{
        bsd_info::BSDInfo,
        file_info::{ListFDs, pidfdinfo},
        net_info::SocketFDInfo,
        proc_pid::{listpidinfo, pidinfo},
    };
    let pid = i32::try_from(pid)?;
    let info = pidinfo::<BSDInfo>(pid, 0).map_err(anyhow::Error::msg)?;
    ensure!(
        info.pbi_uid == unsafe { libc::geteuid() } && info.pbi_nfiles <= 8192,
        "Unsafe native server descriptor owner"
    );
    let fds = listpidinfo::<ListFDs>(pid, info.pbi_nfiles as usize).map_err(anyhow::Error::msg)?;
    for fd in fds.iter().filter(|fd| fd.proc_fdtype == 2) {
        let Ok(socket) = pidfdinfo::<SocketFDInfo>(pid, fd.proc_fd) else {
            continue;
        };
        if socket.psi.soi_kind != 2 {
            continue;
        };
        // libproc's discriminant confirms the TCP union; vflag confirms IPv4 addresses.
        let tcp = unsafe { socket.psi.soi_proto.pri_tcp };
        let ip = tcp.tcpsi_ini;
        if tcp.tcpsi_state != 4 || ip.insi_vflag & 1 == 0 {
            continue;
        };
        let local = unsafe { ip.insi_laddr.ina_46.i46a_addr4.s_addr };
        let foreign = unsafe { ip.insi_faddr.ina_46.i46a_addr4.s_addr };
        if u16::from_be(ip.insi_lport as u16) == server
            && u16::from_be(ip.insi_fport as u16) == client
            && u32::from_be(local) == 0x7f000001
            && u32::from_be(foreign) == 0x7f000001
        {
            return Ok(true);
        };
    }
    Ok(false)
}

#[cfg(target_os = "linux")]
#[path = "mobile_opencode_linux.rs"]
mod linux;

#[cfg(target_os = "linux")]
fn peer_owned(pid: i64, client: u16, server: u16) -> Result<bool> {
    linux::peer_owned(pid, client, server)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn peer_owned(_pid: i64, _client: u16, _server: u16) -> Result<bool> {
    bail!("Native socket ownership proof unavailable on this platform")
}

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
mod tests {
    use super::*;
    use std::{net::TcpListener, sync::mpsc};

    fn binding(port: u16, pid: i64) -> Binding {
        let generation = crate::process::process_generation(pid).unwrap();
        Binding {
            version: 1,
            token: "fixture".into(),
            thread: "ses_fixture".into(),
            cwd: "/synthetic".into(),
            server_pid: pid,
            server_start: generation.start_time,
            native_pid: 0,
            native_start: 0,
            supervisor_pid: 0,
            supervisor_start: 0,
            port,
            password: "disposable-secret".into(),
        }
    }

    #[test]
    fn cancelled_bootstrap_never_connects() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let own = binding(
            listener.local_addr().unwrap().port(),
            i64::from(std::process::id()),
        );
        assert!(bootstrap(&own, &|| bail!("cancelled")).is_err());
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }

    fn server() -> (u16, mpsc::Receiver<Vec<u8>>, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (send, receive) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut bytes = Vec::new();
            while bytes.len() < 4096 && !bytes.windows(4).any(|part| part == b"\r\n\r\n") {
                let mut chunk = [0; 512];
                let n = socket.read(&mut chunk).unwrap_or(0);
                if n == 0 {
                    break;
                };
                bytes.extend_from_slice(&chunk[..n]);
            }
            let any = !bytes.is_empty();
            send.send(bytes).unwrap();
            if any {
                let _ = socket.write_all(
                    b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
            }
        });
        (port, receive, worker)
    }

    #[test]
    fn actual_socket_owner_accepts_only_exact_connected_peer_before_auth() {
        let (port, received, worker) = server();
        let own = binding(port, i64::from(std::process::id()));
        assert_eq!(request(&own, "GET", "/fixture", None).unwrap(), Value::Null);
        assert!(
            String::from_utf8(received.recv().unwrap())
                .unwrap()
                .contains("Authorization: Basic")
        );
        worker.join().unwrap();
        let (port, received, worker) = server();
        let unrelated = binding(port, i64::from(unsafe { libc::getppid() }));
        assert!(request(&unrelated, "GET", "/fixture", None).is_err());
        let observed = received.recv().unwrap();
        #[cfg(target_os = "linux")]
        assert_eq!(
            observed, b"GET ",
            "Unrelated owner received private request bytes"
        );
        #[cfg(not(target_os = "linux"))]
        assert!(observed.is_empty(), "Credentials reached unrelated owner");
        worker.join().unwrap();
        assert!(!peer_owned(i64::from(std::process::id()), 12345, 54321).unwrap());
    }
}
