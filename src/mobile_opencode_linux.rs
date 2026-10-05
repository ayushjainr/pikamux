//! Prove the accepted TCP peer belongs to the certified server before sending credentials.
use anyhow::{Result, ensure};
use std::{collections::HashSet, fs, io::Read, os::unix::fs::MetadataExt};

pub(super) fn peer_owned(pid: i64, client: u16, server: u16) -> Result<bool> {
    ensure!(pid > 0, "Invalid native server process");
    let root = format!("/proc/{pid}");
    ensure!(
        fs::metadata(&root)?.uid() == unsafe { libc::geteuid() },
        "Unsafe native server process owner"
    );
    let sockets = socket_inodes(&root)?;
    let mut table = String::new();
    fs::File::open(format!("{root}/net/tcp"))?
        .take(4 * 1024 * 1024 + 1)
        .read_to_string(&mut table)?;
    ensure!(
        table.len() <= 4 * 1024 * 1024,
        "Native socket table too large"
    );
    // /proc exposes addresses as host-order hexadecimal words.
    let loopback = format!("{:08X}", u32::from_ne_bytes([127, 0, 0, 1]));
    let local = format!("{loopback}:{server:04X}");
    let remote = format!("{loopback}:{client:04X}");
    Ok(table.lines().skip(1).any(|line| {
        let fields: Vec<_> = line.split_whitespace().take(10).collect();
        fields.len() == 10
            && fields[1] == local
            && fields[2] == remote
            && fields[3] == "01"
            && fields[7].parse::<u32>().ok() == Some(unsafe { libc::geteuid() })
            && fields[9]
                .parse::<u64>()
                .is_ok_and(|inode| sockets.contains(&inode))
    }))
}

fn socket_inodes(root: &str) -> Result<HashSet<u64>> {
    let mut sockets = HashSet::new();
    for (index, entry) in fs::read_dir(format!("{root}/fd"))?.enumerate() {
        ensure!(index < 8192, "Native server has too many descriptors");
        let entry = entry?;
        let target = match fs::read_link(entry.path()) {
            Ok(target) => target,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        if let Some(inode) = target
            .to_str()
            .and_then(|s| s.strip_prefix("socket:["))
            .and_then(|s| s.strip_suffix(']'))
            .and_then(|s| s.parse().ok())
        {
            sockets.insert(inode);
        }
    }
    Ok(sockets)
}

pub(super) fn peer_diagnostic(pid: i64, client: u16, server: u16) -> Result<String> {
    let root = format!("/proc/{pid}");
    let sockets = socket_inodes(&root)?;
    let mut table = String::new();
    fs::File::open(format!("{root}/net/tcp"))?
        .take(4 * 1024 * 1024 + 1)
        .read_to_string(&mut table)?;
    ensure!(
        table.len() <= 4 * 1024 * 1024,
        "Native socket table too large"
    );
    let loopback = format!("{:08X}", u32::from_ne_bytes([127, 0, 0, 1]));
    let local = format!("{loopback}:{server:04X}");
    let remote = format!("{loopback}:{client:04X}");
    let mut matched = Vec::new();
    for line in table.lines().skip(1) {
        let fields: Vec<_> = line.split_whitespace().take(10).collect();
        if fields.len() == 10 && fields[1] == local && fields[2] == remote {
            let owned = fields[9]
                .parse::<u64>()
                .is_ok_and(|inode| sockets.contains(&inode));
            matched.push(format!(
                "state={},uid={},inode={},ownedFd={owned}",
                fields[3], fields[7], fields[9]
            ));
        }
    }
    Ok(format!(
        "pid={pid},clientPort={client},serverPort={server},matches={}",
        matched.join(";")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{TcpListener, TcpStream};

    #[test]
    fn accepted_socket_requires_exact_peer_and_process() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let client = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let (accepted, _) = listener.accept().unwrap();
        let client_port = client.local_addr().unwrap().port();
        let pid = i64::from(std::process::id());
        assert!(peer_owned(pid, client_port, port).unwrap());
        assert!(!peer_owned(pid, client_port, port.wrapping_add(1)).unwrap());
        assert!(peer_owned(-1, client_port, port).is_err());
        drop(accepted);
        assert!(!peer_owned(pid, client_port, port).unwrap());
    }
}
