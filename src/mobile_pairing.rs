//! Foreground, single-phone SSH enrollment. The existing SSH service remains
//! the only durable transport; no provider or permanent listener is started.
use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rustls::{ServerConfig, ServerConnection, StreamOwned, pki_types::PrivatePkcs8KeyDer};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    io::{Read, Write},
    net::{IpAddr, SocketAddr, TcpListener, TcpStream},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use subtle::ConstantTimeEq;

#[derive(clap::Args, Debug)]
pub(crate) struct Args {
    #[arg(long)]
    pub address: Option<IpAddr>,
    #[arg(long, default_value_t = 22)]
    pub ssh_port: u16,
    #[arg(long)]
    pub username: Option<String>,
    #[arg(long, default_value = default_host_key())]
    pub host_key: PathBuf,
    #[arg(long)]
    pub authorized_keys: Option<PathBuf>,
    #[arg(long, default_value_t = 300)]
    pub expires: u64,
    #[arg(long, hide = true)]
    pub json: bool,
}
impl Default for Args {
    fn default() -> Self {
        Self {
            address: None,
            ssh_port: 22,
            username: None,
            host_key: default_host_key().into(),
            authorized_keys: None,
            expires: 300,
            json: false,
        }
    }
}
fn default_host_key() -> &'static str {
    if cfg!(target_os = "macos") {
        "/private/etc/ssh/ssh_host_ed25519_key.pub"
    } else {
        "/etc/ssh/ssh_host_ed25519_key.pub"
    }
}

#[derive(Serialize, Clone)]
struct Descriptor {
    v: u8,
    address: IpAddr,
    ssh_port: u16,
    username: String,
    node_id: String,
    ssh_host_key: String,
    pair_port: u16,
    tls_sha256: String,
    token: String,
    expires_at: i64,
}
struct Bootstrap {
    listener: TcpListener,
    tls: Arc<ServerConfig>,
    descriptor: Descriptor,
    uri: String,
    token: [u8; 32],
    deadline: Instant,
    grant: crate::mobile_pairing_keys::Grant,
    cancelled: Arc<AtomicBool>,
    #[cfg(test)]
    drop_receipt: bool,
    #[cfg(test)]
    stop_path: Option<PathBuf>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Claim {
    token: String,
    public_key: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Fetch {
    token: String,
}
#[derive(Debug, thiserror::Error)]
#[error(
    "SSH enrollment could not be confirmed: {0}. Keep this phone key and check pinned SSH before retrying."
)]
struct EnrollmentFailure(anyhow::Error);

pub(crate) fn run(pika: &crate::core::Pika, args: Args) -> Result<i32> {
    let screen = crate::onboarding::Screen::new(!args.json)?;
    run_on_screen(pika, args, &screen)
}

pub(crate) fn run_on_screen(
    pika: &crate::core::Pika,
    args: Args,
    screen: &crate::onboarding::Screen,
) -> Result<i32> {
    if !screen.active() && !args.json {
        bail!("Phone pairing needs an interactive terminal");
    }
    let json = args.json;
    let bootstrap = Bootstrap::prepare(pika, args)?;
    if json {
        println!(
            "{}",
            serde_json::json!({"uri":bootstrap.uri,"descriptor":bootstrap.descriptor})
        );
    } else {
        crate::mobile_pairing_ui::draw(&bootstrap.uri, bootstrap.deadline)?;
    }
    bootstrap.serve(!json)
}

impl Bootstrap {
    fn prepare(pika: &crate::core::Pika, args: Args) -> Result<Self> {
        let address = validated_address(&args)?;
        let username = validated_username(args.username)?;
        let ssh_host_key =
            crate::mobile_pairing_keys::verified_host_key(&args.host_key, address, args.ssh_port)?;
        let path = args.authorized_keys.unwrap_or(
            directories::BaseDirs::new()
                .context("Cannot determine SSH home")?
                .home_dir()
                .join(".ssh/authorized_keys"),
        );
        let executable = pairing_executable()?;
        let grant = crate::mobile_pairing_keys::Grant::prepare(path, executable)?;
        let (tls, digest) = tls_config(address, args.expires)?;
        let listener = TcpListener::bind(SocketAddr::new(address, 0))?;
        listener.set_nonblocking(true)?;
        let mut token = [0; 32];
        getrandom::fill(&mut token)
            .map_err(|error| anyhow::anyhow!("Secure pairing randomness unavailable: {error}"))?;
        let expires_at = time::OffsetDateTime::now_utc().unix_timestamp() + args.expires as i64;
        let descriptor = Descriptor {
            v: 1,
            address,
            ssh_port: args.ssh_port,
            username,
            node_id: pika.store.ensure_local_node_id()?,
            ssh_host_key,
            pair_port: listener.local_addr()?.port(),
            tls_sha256: hex(&digest),
            token: URL_SAFE_NO_PAD.encode(token),
            expires_at,
        };
        let mut fragment = Vec::from(digest);
        fragment.extend(token);
        let uri = format!(
            "pika://pair/v1/{}#{}",
            listener.local_addr()?,
            URL_SAFE_NO_PAD.encode(fragment)
        );
        Ok(Self {
            listener,
            tls,
            descriptor,
            uri,
            token,
            deadline: Instant::now() + Duration::from_secs(args.expires),
            grant,
            cancelled: Arc::new(AtomicBool::new(false)),
            #[cfg(test)]
            drop_receipt: false,
            #[cfg(test)]
            stop_path: None,
        })
    }
    fn serve(self, interactive: bool) -> Result<i32> {
        let mut redraw = Instant::now();
        let mut dimensions = if interactive {
            crossterm::terminal::size()?
        } else {
            (0, 0)
        };
        while Instant::now() < self.deadline {
            if self.should_cancel(interactive)? {
                return Ok(0);
            }
            if interactive && redraw.elapsed() >= Duration::from_secs(1) {
                crate::mobile_pairing_ui::refresh(&self.uri, self.deadline, &mut dimensions)?;
                redraw = Instant::now();
            }
            match self.listener.accept() {
                Ok((stream, _)) => match self.handle(stream, interactive) {
                    Ok(true) => {
                        if interactive {
                            crate::mobile_pairing_ui::paired()?;
                        }
                        return Ok(0);
                    }
                    Ok(false) => {}
                    Err(error) if error.downcast_ref::<EnrollmentFailure>().is_some() => {
                        return Err(error);
                    }
                    Err(_) => {} // Untrusted malformed or disconnected clients never reveal the capability.
                },
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(25))
                }
                Err(error) => return Err(error.into()),
            }
        }
        bail!("Pairing expired. No new phone was connected; start a fresh QR when ready.")
    }
    fn should_cancel(&self, interactive: bool) -> Result<bool> {
        #[cfg(test)]
        if self.stop_path.as_ref().is_some_and(|path| path.exists()) {
            return Ok(true);
        }
        Ok(self.cancelled.load(Ordering::Relaxed)
            || interactive && crate::mobile_pairing_ui::cancelled()?)
    }
    fn handle(&self, stream: TcpStream, interactive: bool) -> Result<bool> {
        let deadline = self.deadline.min(Instant::now() + Duration::from_secs(3));
        let stream = crate::mobile_pairing_io::DeadlineIo::new(
            stream,
            deadline,
            self.cancelled.clone(),
            interactive,
        );
        let mut tls = StreamOwned::new(ServerConnection::new(self.tls.clone())?, stream);
        let (path, body) = read_request(&mut tls)?;
        #[cfg(test)]
        println!("PAIRING_HTTP path={path}");
        if Instant::now() >= self.deadline {
            respond(&mut tls, 410, &serde_json::json!({"error":"expired"}))?;
            return Ok(false);
        }
        match path.as_str() {
            "/descriptor" => {
                let fetch: Fetch = serde_json::from_slice(&body)?;
                if !self.authenticated(&fetch.token) {
                    respond(&mut tls, 403, &serde_json::json!({"error":"rejected"}))?;
                    return Ok(false);
                }
                respond(&mut tls, 200, &serde_json::to_value(&self.descriptor)?)?;
                Ok(false)
            }
            "/pair" => self.claim(&body, &mut tls, deadline, interactive),
            _ => {
                respond(&mut tls, 404, &serde_json::json!({"error":"unavailable"}))?;
                Ok(false)
            }
        }
    }
    fn claim(
        &self,
        body: &[u8],
        output: &mut impl Write,
        deadline: Instant,
        interactive: bool,
    ) -> Result<bool> {
        let claim: Claim = serde_json::from_slice(body)?;
        if !self.authenticated(&claim.token) {
            respond(output, 403, &serde_json::json!({"error":"rejected"}))?;
            return Ok(false);
        }
        let key = crate::mobile_pairing_keys::public_key(&claim.public_key)?;
        // A valid claim consumes this foreground capability before any grant
        // side effect: every grant error is terminal, including post-write errors.
        self.grant
            .authorize(&key, deadline, &self.cancelled, interactive)
            .map_err(EnrollmentFailure)?;
        #[cfg(test)]
        if self.drop_receipt {
            return Ok(true);
        }
        // Lost receipts recover only via this same key on pinned SSH.
        let _ = respond(
            output,
            200,
            &serde_json::json!({"v":1,"node_id":self.descriptor.node_id,"public_key":key,"ssh_host_key":self.descriptor.ssh_host_key,"state":"paired"}),
        );
        Ok(true)
    }
    fn authenticated(&self, token: &str) -> bool {
        URL_SAFE_NO_PAD.decode(token).ok().is_some_and(|bytes| {
            bytes.len() == 32 && bool::from(bytes.as_slice().ct_eq(&self.token))
        })
    }
}

fn private_address(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(ip) => {
            ip.is_loopback()
                || ip.is_private()
                || (ip.octets()[0] == 100 && ip.octets()[1] & 0xc0 == 0x40)
        }
        IpAddr::V6(ip) => ip.is_loopback() || ip.segments()[0] & 0xfe00 == 0xfc00,
    }
}
fn validated_address(args: &Args) -> Result<IpAddr> {
    if args.ssh_port == 0 || !(30..=300).contains(&args.expires) {
        bail!("Choose a valid SSH port and a pairing window of 30–300 seconds");
    }
    let address = args.address.map(Ok).unwrap_or_else(tailscale_address)?;
    if !private_address(address) {
        bail!("Pairing binds only a private network address, never a wildcard or public interface");
    }
    Ok(address)
}
fn tailscale_address() -> Result<IpAddr> {
    use std::process::{Command, Stdio};
    let spawn = |program: &str| {
        Command::new(program)
            .args(["ip", "-4"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
    };
    let mut child = spawn("tailscale").or_else(|error| {
        if error.kind() == std::io::ErrorKind::NotFound { spawn("/Applications/Tailscale.app/Contents/MacOS/Tailscale") } else { Err(error) }
    }).context("Tailscale address discovery is unavailable; choose a private address with pika pair --address")?;
    let deadline = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "Tailscale address discovery timed out; choose a private address with pika pair --address"
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let mut output = String::new();
    child
        .stdout
        .take()
        .context("Tailscale address output unavailable")?
        .take(256)
        .read_to_string(&mut output)?;
    if !status.success() {
        bail!("Tailscale is not ready; choose a reachable private address");
    }
    Ok(output.trim().parse()?)
}
fn current_username() -> String {
    std::process::Command::new("/usr/bin/id")
        .arg("-un")
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .unwrap_or_default()
        .trim()
        .to_owned()
}
fn validated_username(requested: Option<String>) -> Result<String> {
    let current = current_username();
    let username = requested.unwrap_or_else(|| current.clone());
    if username != current {
        bail!("Pairing must use the current computer user's SSH account");
    }
    if username.is_empty()
        || username.len() > 64
        || !username
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(&byte))
    {
        bail!("A valid current SSH username is required");
    }
    Ok(username)
}
fn pairing_executable() -> Result<PathBuf> {
    pairing_executable_at(&std::env::current_exe()?)
}
fn pairing_executable_at(path: &std::path::Path) -> Result<PathBuf> {
    let executable = path.canonicalize()?;
    if let Ok(managed) = crate::update::discover_managed_install(&executable) {
        let launcher = managed.bin_dir.join("pika");
        if launcher.canonicalize()? != executable {
            bail!("The installed Pika launcher does not select this running release");
        }
        return Ok(launcher);
    }
    if managed_release_evidence(&executable)? {
        bail!(
            "This running Pika release is no longer active or its installation cannot be verified. Reopen Pika before pairing your phone."
        );
    }
    Ok(executable)
}
fn managed_release_evidence(executable: &std::path::Path) -> Result<bool> {
    let Some(bin) = executable
        .parent()
        .filter(|path| path.file_name().is_some_and(|name| name == "bin"))
    else {
        return Ok(false);
    };
    let release = bin.parent().context("Pika binary has no release parent")?;
    let mut evidence = vec![release.join(".pika-install.json")];
    if let Some(root) = release
        .parent()
        .filter(|path| path.file_name().is_some_and(|name| name == "releases"))
        .and_then(|path| path.parent())
    {
        evidence.push(root.join(".pika-install-root"));
    }
    for path in evidence {
        match std::fs::symlink_metadata(path) {
            Ok(_) => return Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(false)
}
fn tls_config(address: IpAddr, ttl: u64) -> Result<(Arc<ServerConfig>, [u8; 32])> {
    let key = rcgen::KeyPair::generate()?;
    let mut params = rcgen::CertificateParams::new(vec![address.to_string()])?;
    params.not_before = time::OffsetDateTime::now_utc() - time::Duration::seconds(60);
    params.not_after = time::OffsetDateTime::now_utc() + time::Duration::seconds(ttl as i64);
    let cert = params.self_signed(&key)?;
    let digest = Sha256::digest(cert.der()).into();
    let config =
        ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()?
            .with_no_client_auth()
            .with_single_cert(
                vec![cert.der().clone()],
                PrivatePkcs8KeyDer::from(key.serialize_der()).into(),
            )?;
    Ok((Arc::new(config), digest))
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn read_request(input: &mut impl Read) -> Result<(String, Vec<u8>)> {
    let mut header = Vec::new();
    while !header.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        input.read_exact(&mut byte)?;
        header.push(byte[0]);
        if header.len() > 4096 {
            bail!("Pairing header exceeds its bound");
        }
    }
    let (path, length) = request_header(&header)?;
    let mut body = vec![0; length];
    input.read_exact(&mut body)?;
    Ok((path, body))
}
fn request_header(header: &[u8]) -> Result<(String, usize)> {
    let header = std::str::from_utf8(header)?;
    let mut lines = header.lines();
    let fields: Vec<_> = lines
        .next()
        .context("Missing request")?
        .split(' ')
        .collect();
    if fields.len() != 3 || fields[0] != "POST" || fields[2] != "HTTP/1.1" {
        bail!("Unsupported pairing request");
    }
    let mut length = None;
    for line in lines.filter(|line| !line.is_empty()) {
        let (name, value) = line.split_once(':').context("Invalid pairing header")?;
        if name.eq_ignore_ascii_case("transfer-encoding") {
            bail!("Chunked pairing requests are unsupported");
        }
        if name.eq_ignore_ascii_case("content-length") {
            if length.is_some() {
                bail!("Ambiguous pairing length");
            }
            length = Some(value.trim().parse::<usize>()?);
        }
    }
    let length = length
        .filter(|length| *length <= 4096)
        .context("Invalid pairing body size")?;
    Ok((fields[1].to_owned(), length))
}
fn respond(output: &mut impl Write, status: u16, value: &serde_json::Value) -> Result<()> {
    let body = serde_json::to_vec(value)?;
    write!(
        output,
        "HTTP/1.1 {status} Response\r\nContent-Type: application/json\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    output.write_all(&body)?;
    output.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn enrolled_command_follows_managed_launcher_cutover() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().canonicalize().unwrap();
        let root = base.join("managed");
        let bin = base.join("bin");
        std::fs::create_dir_all(root.join("releases")).unwrap();
        std::fs::create_dir(&bin).unwrap();
        std::fs::write(root.join(".pika-install-root"), crate::update::ROOT_MARKER).unwrap();
        let first = root.join("releases/first");
        let next = root.join("releases/next");
        for (release, output) in [(&first, "first"), (&next, "next")] {
            std::fs::create_dir_all(release.join("bin")).unwrap();
            let executable = release.join("bin/pika");
            std::fs::write(
                &executable,
                format!("#!/bin/sh\n[ \"$1\" = _mobile ] || exit 99\nprintf {output}\n"),
            )
            .unwrap();
            std::fs::set_permissions(executable, std::fs::Permissions::from_mode(0o700)).unwrap();
            let receipt = serde_json::json!({"schema":1,"root":root,"bin_dir":bin,"version":"0.6.38","sha256":"a".repeat(64)});
            std::fs::write(
                release.join(".pika-install.json"),
                serde_json::to_vec(&receipt).unwrap(),
            )
            .unwrap();
        }
        symlink(&first, root.join("current")).unwrap();
        symlink(root.join("current/bin/pika"), bin.join("pika")).unwrap();
        let selected = pairing_executable_at(&first.join("bin/pika")).unwrap();
        assert_eq!(selected, bin.join("pika"));
        let grant =
            crate::mobile_pairing_keys::Grant::prepare(base.join("authorized_keys"), selected)
                .unwrap();
        let mut wire = b"\0\0\0\x0bssh-ed25519\0\0\0\x20".to_vec();
        wire.extend([7; 32]);
        let key = format!(
            "ssh-ed25519 {}",
            base64::engine::general_purpose::STANDARD.encode(wire)
        );
        grant
            .authorize(
                &key,
                Instant::now() + Duration::from_secs(1),
                &AtomicBool::new(false),
                false,
            )
            .unwrap();
        let authorization = std::fs::read_to_string(base.join("authorized_keys")).unwrap();
        let command = authorization.split('"').nth(1).unwrap();
        assert!(!command.contains("releases/first"));
        let run = || {
            std::process::Command::new("/bin/sh")
                .args(["-c", command])
                .output()
                .unwrap()
        };
        assert_eq!(run().stdout, b"first");
        std::fs::remove_file(root.join("current")).unwrap();
        symlink(&next, root.join("current")).unwrap();
        assert!(
            pairing_executable_at(&first.join("bin/pika")).is_err(),
            "an inactive managed process must never enroll its retired binary path"
        );
        assert_eq!(run().stdout, b"next");
        assert_eq!(
            std::fs::read_to_string(base.join("authorized_keys")).unwrap(),
            authorization
        );
    }
    #[test]
    #[ignore = "requires explicitly isolated owned SSH fixture paths"]
    fn held_owned_pairing_server() {
        let root = PathBuf::from(std::env::var("PIKA_QR_FIXTURE_ROOT").unwrap())
            .canonicalize()
            .unwrap();
        assert!(
            root.starts_with("/private/tmp")
                && root
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("pika-qr-native.")
        );
        let pika = crate::core::Pika::discover().unwrap();
        let mut server = Bootstrap::prepare(
            &pika,
            Args {
                address: Some("127.0.0.1".parse().unwrap()),
                ssh_port: 59383,
                host_key: root.join("host-key.pub"),
                authorized_keys: Some(root.join("authorized_keys")),
                json: true,
                ..Args::default()
            },
        )
        .unwrap();
        let wrapper = root.join("pika-mobile-fixture");
        let executable = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("pika");
        let script = format!(
            "#!/bin/sh\n[ \"$#\" = 1 ] && [ \"$1\" = _mobile ] || exit 99\ncd {} || exit 99\nexec /usr/bin/env TMPDIR={} {} _mobile\n",
            shell_words::quote(root.to_str().unwrap()),
            shell_words::quote(root.join("tmp").to_str().unwrap()),
            shell_words::quote(executable.to_str().unwrap())
        );
        std::fs::write(&wrapper, script).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
        server.grant =
            crate::mobile_pairing_keys::Grant::prepare(root.join("authorized_keys"), wrapper)
                .unwrap();
        let mode = std::env::var("PIKA_QR_FIXTURE_MODE").unwrap_or_else(|_| "success".into());
        let node = server.descriptor.node_id.clone();
        let mut expected = None;
        match mode.as_str() {
            "success" => {}
            "lost_receipt" => server.drop_receipt = true,
            "wrong_node" => {
                server.descriptor.node_id = uuid::Uuid::new_v4().to_string();
                expected = Some("This address is not the Pika machine you saved.");
            }
            "expired" => {
                server.descriptor.expires_at = time::OffsetDateTime::now_utc().unix_timestamp() - 1;
                expected =
                    Some("This pairing code expired. Choose Connect phone again on the machine.");
            }
            "wrong_tls" => {
                let (prefix, fragment) = server.uri.split_once('#').unwrap();
                let mut bytes = URL_SAFE_NO_PAD.decode(fragment).unwrap();
                bytes[0] ^= 1;
                server.uri = format!("{prefix}#{}", URL_SAFE_NO_PAD.encode(bytes));
                expected = Some(
                    "The pairing machine's secure identity did not match the code. Nothing was trusted.",
                );
            }
            _ => panic!("unknown fixture mode"),
        }
        let stop = root.join("pair-stop");
        if stop.exists() {
            std::fs::remove_file(&stop).unwrap();
        }
        server.stop_path = Some(stop);
        let fixture = serde_json::json!({"qr":server.uri,"nodeId":node,"expectedName":"Paired Synthetic Board","storeId":uuid::Uuid::new_v4().to_string(),"expectedFailure":expected,"pendingExpected":mode == "wrong_node"});
        std::fs::write(
            root.join("phone-pairing.json"),
            serde_json::to_vec(&fixture).unwrap(),
        )
        .unwrap();
        println!(
            "PAIRING_READY config={} node={}",
            root.join("phone-pairing.json").display(),
            server.descriptor.node_id
        );
        assert_eq!(server.serve(false).unwrap(), 0);
        println!("PAIRING_CLAIM_COMPLETE");
    }
}
