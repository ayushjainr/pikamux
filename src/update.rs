//! Native release manifests and the POSIX managed-installation boundary.
//!
//! Downloading and archive extraction stay outside this module. The installer
//! enters here only after it has an archive and an extracted candidate; this
//! module verifies both before writing the managed installation root.

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use thiserror::Error;
use uuid::Uuid;

pub const MANIFEST_SCHEMA: u32 = 2;
pub const NATIVE_MANIFEST_FILE: &str = "pika-native-release.json";
pub const ROOT_MARKER: &str = "pikamux-installer-v1\n";
pub const MAX_MANIFEST_BYTES: usize = 64 * 1024;
pub const MAX_ARTIFACT_BYTES: u64 = 100 * 1024 * 1024;
pub const MAX_RELEASE_LIST_BYTES: u64 = 2 * 1024 * 1024;
const MAX_CANDIDATE_OUTPUT_BYTES: usize = 1024 * 1024;
const CANDIDATE_PROBE_TIMEOUT: Duration = Duration::from_secs(10);
pub const RELEASE_API: &str =
    "https://api.github.com/repos/ayushjainr/pikamux/releases?per_page=100";
pub const RELEASE_DOWNLOAD_ROOT: &str = "https://github.com/ayushjainr/pikamux/releases/download";

#[derive(Debug, Error)]
pub enum UpdateError {
    #[error("invalid release manifest: {0}")]
    Manifest(String),
    #[error("unsupported native target: {0}")]
    UnsupportedTarget(String),
    #[error("package checksum mismatch; nothing activated")]
    ChecksumMismatch,
    #[error("unsafe archive member: {0}")]
    UnsafeArchiveMember(String),
    #[error("installation safety check failed: {0}")]
    Safety(String),
    #[error("candidate verification failed: {0}")]
    Candidate(String),
    #[error("release lookup failed: {0}")]
    ReleaseLookup(String),
    #[error(
        "this copy is not the active installer-managed Pika; update it with its original installation method"
    )]
    Unmanaged,
    #[error("another Pika installation/update is running; try again after it finishes")]
    Busy,
    #[error(transparent)]
    Io(#[from] io::Error),
}

pub type Result<T> = std::result::Result<T, UpdateError>;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReleaseArtifact {
    pub file: String,
    pub sha256: String,
    pub bytes: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReleaseManifest {
    pub schema: u32,
    pub package: String,
    pub version: String,
    pub channel: String,
    pub artifacts: BTreeMap<String, ReleaseArtifact>,
}

impl ReleaseManifest {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_MANIFEST_BYTES {
            return Err(UpdateError::Manifest("manifest exceeds 64 KiB".into()));
        }
        let value: Self = serde_json::from_slice(bytes)
            .map_err(|error| UpdateError::Manifest(error.to_string()))?;
        value.validate()?;
        Ok(value)
    }

    pub fn validate(&self) -> Result<()> {
        if self.schema != MANIFEST_SCHEMA {
            return Err(UpdateError::Manifest(format!(
                "schema {} is unsupported",
                self.schema
            )));
        }
        if self.package != "pikamux" {
            return Err(UpdateError::Manifest("package must be pikamux".into()));
        }
        let version = parse_version(&self.version)?;
        if !matches!(self.channel.as_str(), "preview" | "stable") {
            return Err(UpdateError::Manifest(
                "channel must be preview or stable".into(),
            ));
        }
        if self.channel == "stable" && version.phase != 3 {
            return Err(UpdateError::Manifest(
                "stable channel cannot contain a prerelease".into(),
            ));
        }
        if self.artifacts.is_empty() {
            return Err(UpdateError::Manifest("no target artifacts".into()));
        }
        for (target, artifact) in &self.artifacts {
            validate_target(target)?;
            let expected = artifact_name(&self.version, target)?;
            if artifact.file != expected {
                return Err(UpdateError::Manifest(format!(
                    "artifact for {target} must be named {expected}"
                )));
            }
            validate_sha256(&artifact.sha256)?;
            if artifact.bytes == 0 || artifact.bytes > MAX_ARTIFACT_BYTES {
                return Err(UpdateError::Manifest(format!(
                    "artifact for {target} has an invalid size"
                )));
            }
        }
        Ok(())
    }

    pub fn artifact_for(&self, target: &str) -> Result<&ReleaseArtifact> {
        validate_target(target)?;
        self.artifacts
            .get(target)
            .ok_or_else(|| UpdateError::Manifest(format!("release has no artifact for {target}")))
    }
}

pub fn native_target() -> Result<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Ok("aarch64-apple-darwin"),
        ("macos", "x86_64") => Ok("x86_64-apple-darwin"),
        ("linux", "aarch64") => Ok("aarch64-unknown-linux-musl"),
        ("linux", "x86_64") => Ok("x86_64-unknown-linux-musl"),
        ("windows", "x86_64") => Ok("x86_64-pc-windows-msvc"),
        (os, architecture) => Err(UpdateError::UnsupportedTarget(format!(
            "{os}/{architecture}"
        ))),
    }
}

pub fn artifact_name(version: &str, target: &str) -> Result<String> {
    parse_version(version)?;
    validate_target(target)?;
    let extension = if target == "x86_64-pc-windows-msvc" {
        "zip"
    } else {
        "tar.gz"
    };
    Ok(format!("pikamux-{version}-{target}.{extension}"))
}

fn validate_target(target: &str) -> Result<()> {
    if matches!(
        target,
        "aarch64-apple-darwin"
            | "x86_64-apple-darwin"
            | "aarch64-unknown-linux-musl"
            | "x86_64-unknown-linux-musl"
            | "x86_64-pc-windows-msvc"
    ) {
        Ok(())
    } else {
        Err(UpdateError::UnsupportedTarget(target.into()))
    }
}

fn validate_sha256(value: &str) -> Result<()> {
    if value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        Ok(())
    } else {
        Err(UpdateError::Manifest(
            "sha256 must be 64 lowercase hexadecimal characters".into(),
        ))
    }
}

/// Archive entries are checked before extraction. Native bundles currently
/// contain one executable, but this helper remains safe if notices are added.
pub fn validate_archive_members<'a>(
    members: impl IntoIterator<Item = &'a str>,
    target: &str,
) -> Result<()> {
    validate_target(target)?;
    let executable = if target == "x86_64-pc-windows-msvc" {
        "pika.exe"
    } else {
        "pika"
    };
    let mut seen = HashSet::new();
    let mut has_executable = false;
    for member in members {
        if member.is_empty()
            || member.contains('\\')
            || member.chars().any(char::is_control)
            || Path::new(member).components().any(|part| {
                matches!(
                    part,
                    Component::CurDir
                        | Component::ParentDir
                        | Component::RootDir
                        | Component::Prefix(_)
                )
            })
            || !seen.insert(member)
        {
            return Err(UpdateError::UnsafeArchiveMember(member.into()));
        }
        if member == executable {
            has_executable = true;
        }
    }
    if !has_executable {
        return Err(UpdateError::UnsafeArchiveMember(format!(
            "missing {executable}"
        )));
    }
    Ok(())
}

pub fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hash.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hash.finalize()))
}

pub fn verify_artifact(path: &Path, artifact: &ReleaseArtifact) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() || metadata.len() != artifact.bytes {
        return Err(UpdateError::ChecksumMismatch);
    }
    if sha256_file(path)? != artifact.sha256 {
        return Err(UpdateError::ChecksumMismatch);
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManagedInstallation {
    pub root: PathBuf,
    pub bin_dir: PathBuf,
    pub release_dir: PathBuf,
    pub version: String,
    pub sha256: String,
}

impl ManagedInstallation {
    pub fn channel(&self) -> Result<&'static str> {
        Ok(if parse_version(&self.version)?.phase == 3 {
            "stable"
        } else {
            "preview"
        })
    }
}

#[derive(Clone, Copy, Debug)]
pub struct UpdateRequest<'a> {
    /// The running executable. Production callers should pass
    /// `std::env::current_exe()`; accepting it explicitly keeps tests isolated.
    pub executable: &'a Path,
    pub bundle: Option<&'a Path>,
    pub release: Option<&'a str>,
    pub check: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpdateDisposition {
    AlreadyCurrent,
    Available,
    Installed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdateOutcome {
    pub disposition: UpdateDisposition,
    pub previous_version: String,
    pub version: String,
    pub launcher: PathBuf,
}

#[derive(Debug, Deserialize, Serialize)]
struct UpdateNoticeCache {
    current: String,
    checked_at: f64,
    latest: Option<String>,
    #[serde(default)]
    failed: bool,
}

/// Return a best-effort cached update notice for a managed installation.
/// Unmanaged builds, disabled checks, network failure, and a concurrently
/// running check are all ordinary `None` outcomes for the board.
#[cfg(unix)]
pub fn cached_update_notice(executable: &Path) -> Option<String> {
    if std::env::var("PIKA_UPDATE_CHECK")
        .ok()
        .is_some_and(|value| matches!(value.to_ascii_lowercase().as_str(), "0" | "false" | "off"))
    {
        return None;
    }
    cached_update_notice_inner(executable).ok().flatten()
}

#[cfg(unix)]
fn cached_update_notice_inner(executable: &Path) -> Result<Option<String>> {
    let managed = discover_managed_install(executable)?;
    let target = native_target()?;
    let lock_path = managed.root.join(".update-check.lock");
    if fs::symlink_metadata(&lock_path)
        .ok()
        .is_some_and(|metadata| metadata.file_type().is_symlink())
    {
        return Err(UpdateError::Safety(
            "update-check lock must not be a symlink".into(),
        ));
    }
    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)?;
    if lock.try_lock_exclusive().is_err() {
        return Ok(None);
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |duration| duration.as_secs_f64());
    let cache_path = managed.root.join(".update-check.json");
    let cached = read_update_notice_cache(&cache_path);
    let fresh = cached.as_ref().is_some_and(|cache| {
        let age = now - cache.checked_at;
        cache.current == managed.version
            && age >= 0.0
            && age < if cache.failed { 3600.0 } else { 6.0 * 3600.0 }
    });
    let cache = if fresh {
        cached.expect("fresh cache exists")
    } else {
        let scratch = ScratchDirectory::new("pika-update-check")?;
        let listing = scratch.path.join("releases.json");
        let latest = download(RELEASE_API, &listing, MAX_RELEASE_LIST_BYTES, 10)
            .and_then(|()| select_latest_release(&fs::read(&listing)?, &managed.version, target));
        let cache = match latest {
            Ok(latest) => UpdateNoticeCache {
                current: managed.version.clone(),
                checked_at: now,
                latest,
                failed: false,
            },
            Err(_) => UpdateNoticeCache {
                current: managed.version.clone(),
                checked_at: now,
                latest: None,
                failed: true,
            },
        };
        write_update_notice_cache(&cache_path, &cache)?;
        cache
    };
    let latest = cache.latest.filter(|version| {
        compare_versions(version, &managed.version)
            .is_ok_and(|ordering| ordering == Ordering::Greater)
    });
    Ok(latest)
}

#[cfg(not(unix))]
pub fn cached_update_notice(_executable: &Path) -> Option<String> {
    None
}

#[cfg(unix)]
fn read_update_notice_cache(path: &Path) -> Option<UpdateNoticeCache> {
    let metadata = fs::symlink_metadata(path).ok()?;
    if !metadata.file_type().is_file() || metadata.len() > 4096 {
        return None;
    }
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

#[cfg(unix)]
fn write_update_notice_cache(path: &Path, cache: &UpdateNoticeCache) -> Result<()> {
    if fs::symlink_metadata(path)
        .ok()
        .is_some_and(|metadata| metadata.file_type().is_symlink())
    {
        return Err(UpdateError::Safety(
            "update-check cache must not be a symlink".into(),
        ));
    }
    let bytes =
        serde_json::to_vec(cache).map_err(|error| UpdateError::Manifest(error.to_string()))?;
    let temporary = path.with_file_name(format!(".update-check-{}.tmp", Uuid::new_v4()));
    write_new_file(&temporary, &bytes, 0o600)?;
    fs::rename(&temporary, path)?;
    Ok(())
}

impl UpdateOutcome {
    pub fn message(&self) -> String {
        match self.disposition {
            UpdateDisposition::AlreadyCurrent => {
                format!("Pika {} is already current.", self.version)
            }
            UpdateDisposition::Available => format!(
                "Pika {} → {} is available. Run `pika update` to install it.",
                self.previous_version, self.version
            ),
            UpdateDisposition::Installed => format!(
                "Updated Pika {} → {}. Reopen the board when convenient; running agents were not restarted.",
                self.previous_version, self.version
            ),
        }
    }
}

#[derive(Debug, Deserialize)]
struct GithubRelease {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    assets: Vec<GithubAsset>,
}

#[derive(Debug, Deserialize)]
struct GithubAsset {
    name: String,
    state: String,
}

/// Select the newest complete release compatible with the current channel.
/// URLs from the API are deliberately ignored; downloads are reconstructed
/// from the validated version and allowlisted filenames.
pub fn select_latest_release(bytes: &[u8], current: &str, target: &str) -> Result<Option<String>> {
    if bytes.len() as u64 > MAX_RELEASE_LIST_BYTES {
        return Err(UpdateError::ReleaseLookup(
            "release listing exceeds 2 MiB".into(),
        ));
    }
    let current_key = parse_version(current)?;
    validate_target(target)?;
    let rows: Vec<GithubRelease> = serde_json::from_slice(bytes)
        .map_err(|error| UpdateError::ReleaseLookup(format!("invalid release listing: {error}")))?;
    let mut selected: Option<(VersionKey, String)> = None;
    for row in rows {
        if row.draft || !row.tag_name.starts_with('v') {
            continue;
        }
        let version = &row.tag_name[1..];
        let Ok(key) = parse_version(version) else {
            continue;
        };
        let is_prerelease = key.phase != 3;
        if row.prerelease != is_prerelease || (current_key.phase == 3 && is_prerelease) {
            continue;
        }
        let Ok(artifact) = artifact_name(version, target) else {
            continue;
        };
        let expected = [
            NATIVE_MANIFEST_FILE.to_owned(),
            artifact.clone(),
            format!("{artifact}.sha256"),
        ];
        let complete = expected.iter().all(|name| {
            row.assets
                .iter()
                .any(|asset| asset.name == *name && asset.state == "uploaded")
        });
        if !complete {
            continue;
        }
        if selected
            .as_ref()
            .is_none_or(|(selected_key, _)| key > *selected_key)
        {
            selected = Some((key, version.to_owned()));
        }
    }
    Ok(selected.map(|(_, version)| version))
}

/// Prove that an executable is the currently active member of the managed
/// installation before an update is allowed to write anything.
pub fn discover_managed_install(executable: &Path) -> Result<ManagedInstallation> {
    let executable = executable
        .canonicalize()
        .map_err(|_| UpdateError::Unmanaged)?;
    let bin = executable.parent().ok_or(UpdateError::Unmanaged)?;
    if bin.file_name().and_then(|name| name.to_str()) != Some("bin") {
        return Err(UpdateError::Unmanaged);
    }
    let release_dir = bin.parent().ok_or(UpdateError::Unmanaged)?.to_path_buf();
    let receipt = current_receipt(&release_dir)
        .map_err(|_| UpdateError::Unmanaged)?
        .ok_or(UpdateError::Unmanaged)?;
    let root = normalized_install_path(&receipt.root, true).map_err(|_| UpdateError::Unmanaged)?;
    let bin_dir =
        normalized_install_path(&receipt.bin_dir, false).map_err(|_| UpdateError::Unmanaged)?;
    initialize_or_validate_root(&root, true).map_err(|_| UpdateError::Unmanaged)?;
    let releases = root.join("releases");
    if release_dir.parent() != Some(releases.as_path())
        || root
            .join("current")
            .canonicalize()
            .map_err(|_| UpdateError::Unmanaged)?
            != release_dir
        || release_dir
            .join("bin/pika")
            .canonicalize()
            .map_err(|_| UpdateError::Unmanaged)?
            != executable
    {
        return Err(UpdateError::Unmanaged);
    }
    validate_launcher(&bin_dir.join("pika"), &root.join("current/bin/pika"))
        .map_err(|_| UpdateError::Unmanaged)?;
    Ok(ManagedInstallation {
        root,
        bin_dir,
        release_dir,
        version: receipt.version,
        sha256: receipt.sha256,
    })
}

/// Check or install a native release. This is the complete public updater;
/// it never invokes Python and it only calls `install_staged` after exact
/// release, archive and candidate verification.
#[cfg(unix)]
pub fn update_managed(request: UpdateRequest<'_>) -> Result<UpdateOutcome> {
    if request.bundle.is_some() && request.release.is_some() {
        return Err(UpdateError::Safety(
            "use --release or --bundle, not both".into(),
        ));
    }
    if let Some(release) = request.release {
        parse_version(release)?;
    }
    let managed = discover_managed_install(request.executable)?;
    let target = native_target()?;
    let scratch = ScratchDirectory::new("pika-update")?;

    let (manifest, artifact_path) = if let Some(bundle) = request.bundle {
        let bundle = checked_bundle(bundle)?;
        let manifest_path = bundle.join(NATIVE_MANIFEST_FILE);
        let manifest = read_manifest_file(&manifest_path)?;
        if let Some(release) = request.release
            && manifest.version != release
        {
            return Err(UpdateError::Safety(
                "release tag and manifest version differ; nothing activated".into(),
            ));
        }
        let artifact = manifest.artifact_for(target)?;
        let source_path = bundle.join(&artifact.file);
        let sidecar_path = bundle.join(format!("{}.sha256", artifact.file));
        let source = checked_regular_file(&source_path)?;
        verify_sidecar(&sidecar_path, artifact)?;
        let copy = scratch.path.join(&artifact.file);
        fs::copy(source, &copy)?;
        verify_artifact(&copy, artifact)?;
        (manifest, copy)
    } else {
        let selected = match request.release {
            Some(release) => release.to_owned(),
            None => {
                let listing = scratch.path.join("releases.json");
                download(RELEASE_API, &listing, MAX_RELEASE_LIST_BYTES, 10)?;
                let selected =
                    select_latest_release(&fs::read(listing)?, &managed.version, target)?;
                let Some(selected) = selected else {
                    return Ok(already_current(&managed));
                };
                selected
            }
        };
        if compare_versions(&selected, &managed.version)? == Ordering::Less {
            if request.release.is_some() {
                return Err(UpdateError::Safety(
                    "refusing a release downgrade; nothing activated".into(),
                ));
            }
            return Ok(already_current(&managed));
        }
        let base = format!("{RELEASE_DOWNLOAD_ROOT}/v{selected}");
        let manifest_path = scratch.path.join(NATIVE_MANIFEST_FILE);
        download(
            &format!("{base}/{NATIVE_MANIFEST_FILE}"),
            &manifest_path,
            MAX_MANIFEST_BYTES as u64,
            30,
        )?;
        let manifest = read_manifest_file(&manifest_path)?;
        if manifest.version != selected {
            return Err(UpdateError::Safety(
                "release tag and manifest version differ; nothing activated".into(),
            ));
        }
        let artifact = manifest.artifact_for(target)?;
        let artifact_path = scratch.path.join(&artifact.file);
        download(
            &format!("{base}/{}", artifact.file),
            &artifact_path,
            MAX_ARTIFACT_BYTES,
            300,
        )?;
        let sidecar = scratch.path.join(format!("{}.sha256", artifact.file));
        download(
            &format!("{base}/{}.sha256", artifact.file),
            &sidecar,
            256,
            30,
        )?;
        verify_sidecar(&sidecar, artifact)?;
        verify_artifact(&artifact_path, artifact)?;
        (manifest, artifact_path)
    };

    match compare_versions(&manifest.version, &managed.version)? {
        Ordering::Less => {
            return Err(UpdateError::Safety(
                "refusing a release downgrade; nothing activated".into(),
            ));
        }
        Ordering::Equal => {
            let artifact = manifest.artifact_for(target)?;
            if managed.sha256 != artifact.sha256 {
                return Err(UpdateError::Safety(
                    "same version has different package bytes; publish a new version".into(),
                ));
            }
            return Ok(already_current(&managed));
        }
        Ordering::Greater => {}
    }
    if request.check {
        return Ok(UpdateOutcome {
            disposition: UpdateDisposition::Available,
            previous_version: managed.version,
            version: manifest.version,
            launcher: managed.bin_dir.join("pika"),
        });
    }

    let candidate_dir = scratch.path.join("candidate");
    fs::create_dir(&candidate_dir)?;
    extract_candidate(&artifact_path, target, &candidate_dir)?;
    let candidate = candidate_dir.join("pika");
    let outcome = install_staged(InstallRequest {
        manifest: &manifest,
        target,
        artifact: &artifact_path,
        candidate: &candidate,
        root: &managed.root,
        bin_dir: &managed.bin_dir,
    })?;
    Ok(UpdateOutcome {
        disposition: if outcome.activated {
            UpdateDisposition::Installed
        } else {
            UpdateDisposition::AlreadyCurrent
        },
        previous_version: managed.version,
        version: manifest.version,
        launcher: outcome.launcher,
    })
}

#[cfg(not(unix))]
pub fn update_managed(_request: UpdateRequest<'_>) -> Result<UpdateOutcome> {
    Err(UpdateError::UnsupportedTarget(
        "managed native updates are not implemented on this platform".into(),
    ))
}

fn already_current(managed: &ManagedInstallation) -> UpdateOutcome {
    UpdateOutcome {
        disposition: UpdateDisposition::AlreadyCurrent,
        previous_version: managed.version.clone(),
        version: managed.version.clone(),
        launcher: managed.bin_dir.join("pika"),
    }
}

fn read_manifest_file(path: &Path) -> Result<ReleaseManifest> {
    let path = checked_regular_file(path)?;
    let metadata = fs::metadata(path)?;
    if metadata.len() > MAX_MANIFEST_BYTES as u64 {
        return Err(UpdateError::Manifest("manifest exceeds 64 KiB".into()));
    }
    ReleaseManifest::parse(&fs::read(path)?)
}

fn checked_bundle(path: &Path) -> Result<PathBuf> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err(UpdateError::Safety(
            "release bundle must be a real directory".into(),
        ));
    }
    path.canonicalize().map_err(UpdateError::Io)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteInstallBundle {
    version: String,
    target: String,
    payload: Vec<u8>,
}

impl RemoteInstallBundle {
    pub fn version(&self) -> &str {
        &self.version
    }

    pub fn target(&self) -> &str {
        &self.target
    }

    pub fn len(&self) -> usize {
        self.payload.len()
    }

    pub fn is_empty(&self) -> bool {
        self.payload.is_empty()
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.payload
    }
}

/// Validate a local native bundle and wrap only its allowlisted files in the
/// tar stream consumed by `SshTransport::install_bundle`. The installer script
/// must be byte-identical to the one embedded in this coordinator release.
pub fn prepare_remote_install_bundle(
    bundle: &Path,
    target: &str,
    expected_version: Option<&str>,
) -> Result<RemoteInstallBundle> {
    let bundle = checked_bundle(bundle)?;
    let manifest = read_manifest_file(&bundle.join(NATIVE_MANIFEST_FILE))?;
    if expected_version.is_some_and(|expected| expected != manifest.version) {
        return Err(UpdateError::Safety(
            "remote bundle does not match the coordinator's pinned version".into(),
        ));
    }
    let artifact = manifest.artifact_for(target)?;
    let artifact_path = bundle.join(&artifact.file);
    checked_regular_file(&artifact_path)?;
    verify_artifact(&artifact_path, artifact)?;
    verify_sidecar(&bundle.join(format!("{}.sha256", artifact.file)), artifact)?;

    let version_path = bundle.join("pika-version");
    checked_regular_file(&version_path)?;
    let version = fs::read_to_string(&version_path)?;
    if version.lines().count() != 1 || version.trim() != manifest.version {
        return Err(UpdateError::Safety(
            "remote bundle has an invalid pika-version file".into(),
        ));
    }
    let installer_path = bundle.join("install.sh");
    checked_regular_file(&installer_path)?;
    if fs::read(&installer_path)? != include_bytes!("../scripts/install.sh") {
        return Err(UpdateError::Safety(
            "remote bundle installer does not match this Pika release".into(),
        ));
    }

    let names = [
        "install.sh".to_owned(),
        "pika-version".to_owned(),
        NATIVE_MANIFEST_FILE.to_owned(),
        artifact.file.clone(),
        format!("{}.sha256", artifact.file),
    ];
    let output = Command::new("tar")
        .args(["-cf", "-", "-C"])
        .arg(&bundle)
        .args(&names)
        .output()
        .map_err(|error| {
            UpdateError::ReleaseLookup(format!("cannot prepare remote bundle: {error}"))
        })?;
    checked_command(&output, "remote bundle preparation")?;
    let limit = MAX_ARTIFACT_BYTES as usize + 1024 * 1024;
    if output.stdout.is_empty() || output.stdout.len() > limit {
        return Err(UpdateError::Safety(
            "remote installation payload exceeds the safety limit".into(),
        ));
    }
    Ok(RemoteInstallBundle {
        version: manifest.version,
        target: target.to_owned(),
        payload: output.stdout,
    })
}

fn checked_regular_file(path: &Path) -> Result<&Path> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(UpdateError::Safety(format!(
            "release asset must be a regular file: {}",
            path.display()
        )));
    }
    Ok(path)
}

fn verify_sidecar(path: &Path, artifact: &ReleaseArtifact) -> Result<()> {
    let path = checked_regular_file(path)?;
    let metadata = fs::metadata(path)?;
    if metadata.len() > 256 {
        return Err(UpdateError::ChecksumMismatch);
    }
    let value = fs::read_to_string(path)?;
    if value.lines().count() != 1 || value.trim() != artifact.sha256 {
        return Err(UpdateError::ChecksumMismatch);
    }
    Ok(())
}

fn download(url: &str, destination: &Path, max_bytes: u64, timeout_seconds: u64) -> Result<()> {
    if !url.starts_with("https://") || url.chars().any(char::is_control) {
        return Err(UpdateError::ReleaseLookup("unsafe release URL".into()));
    }
    let maximum = max_bytes.to_string();
    let timeout = timeout_seconds.to_string();
    let output = Command::new("curl")
        .args([
            "--disable",
            "--fail",
            "--silent",
            "--show-error",
            "--location",
            "--proto",
            "=https",
            "--proto-redir",
            "=https",
            "--connect-timeout",
            "10",
            "--max-time",
            &timeout,
            "--retry",
            "2",
            "--max-filesize",
            &maximum,
            "--output",
        ])
        .arg(destination)
        .arg(url)
        .output()
        .map_err(|error| UpdateError::ReleaseLookup(format!("cannot run curl: {error}")))?;
    checked_command(&output, "download")?;
    let metadata = fs::metadata(destination).map_err(|error| {
        UpdateError::ReleaseLookup(format!("download produced no file: {error}"))
    })?;
    if !metadata.is_file() || metadata.len() > max_bytes {
        let _ = fs::remove_file(destination);
        return Err(UpdateError::ReleaseLookup(
            "release asset exceeds its size limit".into(),
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn extract_candidate(archive: &Path, target: &str, destination: &Path) -> Result<()> {
    validate_target(target)?;
    if target == "x86_64-pc-windows-msvc" {
        return Err(UpdateError::UnsupportedTarget(target.into()));
    }
    let listing = Command::new("tar")
        .args(["-tzf"])
        .arg(archive)
        .output()
        .map_err(|error| UpdateError::Candidate(format!("cannot inspect archive: {error}")))?;
    checked_command(&listing, "archive inspection")?;
    if listing.stdout.len() > 4096 {
        return Err(UpdateError::UnsafeArchiveMember(
            "archive listing exceeds 4 KiB".into(),
        ));
    }
    let text = String::from_utf8(listing.stdout)
        .map_err(|_| UpdateError::UnsafeArchiveMember("non-UTF-8 path".into()))?;
    let members: Vec<_> = text.lines().collect();
    validate_archive_members(members.iter().copied(), target)?;
    if members != ["pika"] {
        return Err(UpdateError::UnsafeArchiveMember(
            "native archive must contain exactly one executable named pika".into(),
        ));
    }
    let verbose = Command::new("tar")
        .args(["-tvzf"])
        .arg(archive)
        .output()
        .map_err(|error| UpdateError::Candidate(format!("cannot inspect archive: {error}")))?;
    checked_command(&verbose, "archive inspection")?;
    if verbose.stdout.first() != Some(&b'-') {
        return Err(UpdateError::UnsafeArchiveMember(
            "pika is not a regular archive member".into(),
        ));
    }
    let extracted = Command::new("tar")
        .args(["-xzf"])
        .arg(archive)
        .arg("-C")
        .arg(destination)
        .arg("pika")
        .output()
        .map_err(|error| UpdateError::Candidate(format!("cannot extract archive: {error}")))?;
    checked_command(&extracted, "archive extraction")?;
    let candidate_path = destination.join("pika");
    let candidate = checked_regular_file(&candidate_path)?;
    let metadata = fs::metadata(candidate)?;
    if metadata.len() == 0 || metadata.len() > MAX_ARTIFACT_BYTES {
        return Err(UpdateError::Candidate(
            "native executable has an invalid size".into(),
        ));
    }
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(candidate, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

fn checked_command(output: &Output, operation: &str) -> Result<()> {
    if output.status.success() {
        return Ok(());
    }
    let start = output.stderr.len().saturating_sub(2000);
    let stderr = String::from_utf8_lossy(&output.stderr[start..]);
    Err(UpdateError::ReleaseLookup(format!(
        "{operation} failed: {}",
        stderr.trim()
    )))
}

struct ScratchDirectory {
    path: PathBuf,
}

impl ScratchDirectory {
    fn new(prefix: &str) -> Result<Self> {
        for _ in 0..8 {
            let path = std::env::temp_dir().join(format!("{prefix}-{}", Uuid::new_v4()));
            match fs::create_dir(&path) {
                Ok(()) => {
                    #[cfg(unix)]
                    set_private_directory(&path)?;
                    return Ok(Self { path });
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
        }
        Err(UpdateError::Io(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not allocate update workspace",
        )))
    }
}

impl Drop for ScratchDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[derive(Clone, Copy, Debug)]
pub struct InstallRequest<'a> {
    pub manifest: &'a ReleaseManifest,
    pub target: &'a str,
    pub artifact: &'a Path,
    pub candidate: &'a Path,
    pub root: &'a Path,
    pub bin_dir: &'a Path,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstallOutcome {
    pub release_dir: PathBuf,
    pub launcher: PathBuf,
    pub activated: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct InstallReceipt {
    schema: u32,
    kind: String,
    root: PathBuf,
    bin_dir: PathBuf,
    version: String,
    target: String,
    artifact: String,
    sha256: String,
}

#[derive(Clone, Debug, Deserialize)]
struct CurrentReceipt {
    schema: u32,
    root: PathBuf,
    bin_dir: PathBuf,
    version: String,
    sha256: String,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    target: Option<String>,
}

/// Install an already extracted candidate into the existing managed layout.
/// The archive, candidate and command surface are verified before root writes.
#[cfg(unix)]
pub fn install_staged(request: InstallRequest<'_>) -> Result<InstallOutcome> {
    let normalized_root = normalized_install_path(request.root, true)?;
    let normalized_bin_dir = normalized_install_path(request.bin_dir, false)?;
    let request = InstallRequest {
        manifest: request.manifest,
        target: request.target,
        artifact: request.artifact,
        candidate: request.candidate,
        root: &normalized_root,
        bin_dir: &normalized_bin_dir,
    };
    request.manifest.validate()?;
    let local_target = native_target()?;
    if request.target != local_target {
        return Err(UpdateError::Safety(format!(
            "artifact target {} does not match this machine ({local_target})",
            request.target
        )));
    }
    let release_artifact = request.manifest.artifact_for(request.target)?;
    let artifact_file = request
        .artifact
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| UpdateError::Safety("artifact has no safe filename".into()))?;
    if artifact_file != release_artifact.file {
        return Err(UpdateError::Safety(
            "artifact filename does not match manifest".into(),
        ));
    }
    verify_artifact(request.artifact, release_artifact)?;
    validate_install_paths(request.root, request.bin_dir)?;
    if request.root.exists() {
        initialize_or_validate_root(request.root, true)?;
    }
    let preflight_current = request.root.join("current");
    let preflight_launcher = request.bin_dir.join("pika");
    validate_launcher(&preflight_launcher, &preflight_current.join("bin/pika"))?;
    validate_candidate(request.candidate, &request.manifest.version)?;

    let root_existed = request.root.exists();
    if !root_existed {
        fs::create_dir_all(request.root)?;
        set_private_directory(request.root)?;
    }
    if let Err(error) = initialize_or_validate_root(request.root, root_existed) {
        if !root_existed {
            let _ = fs::remove_dir(request.root);
        }
        return Err(error);
    }

    let lock_path = request.root.join(".install.lock");
    reject_symlink(&lock_path)?;
    let lock = open_lock(&lock_path)?;
    lock.try_lock_exclusive().map_err(|error| {
        if error.kind() == io::ErrorKind::WouldBlock {
            UpdateError::Busy
        } else {
            UpdateError::Io(error)
        }
    })?;
    initialize_or_validate_root(request.root, true)?;

    let releases = request.root.join("releases");
    reject_symlink(&releases)?;
    fs::create_dir_all(&releases)?;
    set_private_directory(&releases)?;
    let current = request.root.join("current");
    validate_current(&current, &releases)?;
    let launcher = request.bin_dir.join("pika");
    let expected_launcher_target = current.join("bin/pika");
    validate_launcher(&launcher, &expected_launcher_target)?;

    if let Some(receipt) = current_receipt(&current)? {
        if receipt.root != request.root || receipt.bin_dir != request.bin_dir {
            return Err(UpdateError::Safety(
                "current release receipt does not own this installation".into(),
            ));
        }
        match compare_versions(&request.manifest.version, &receipt.version)? {
            Ordering::Less => {
                return Err(UpdateError::Safety(
                    "refusing a release downgrade; nothing activated".into(),
                ));
            }
            Ordering::Equal => {
                if receipt.schema != MANIFEST_SCHEMA
                    || receipt.kind.as_deref() != Some("native")
                    || receipt.target.as_deref() != Some(request.target)
                    || receipt.sha256 != release_artifact.sha256
                {
                    return Err(UpdateError::Safety(
                        "same version has different package bytes; publish a new version".into(),
                    ));
                }
                ensure_launcher(&launcher, &expected_launcher_target)?;
                return Ok(InstallOutcome {
                    release_dir: current.canonicalize()?,
                    launcher,
                    activated: false,
                });
            }
            Ordering::Greater => {}
        }
    }

    let release_name = format!(
        "{}-{}-{}",
        request.manifest.version,
        request.target,
        &release_artifact.sha256[..12]
    );
    let release_dir = releases.join(release_name);
    let mut prepared_here = false;
    if release_dir.exists() {
        validate_existing_release(&release_dir, request, release_artifact)?;
    } else {
        let stage = releases.join(format!(".stage-{}", Uuid::new_v4()));
        fs::create_dir(&stage)?;
        set_private_directory(&stage)?;
        let prepared = prepare_release(&stage, request, release_artifact);
        if let Err(error) = prepared {
            let _ = fs::remove_dir_all(&stage);
            return Err(error);
        }
        if let Err(error) = fs::rename(&stage, &release_dir) {
            let _ = fs::remove_dir_all(&stage);
            return Err(error.into());
        }
        prepared_here = true;
    }

    let launcher_created = ensure_launcher(&launcher, &expected_launcher_target)?;
    if let Err(error) = atomic_symlink(&release_dir, &current) {
        if launcher_created {
            let _ = fs::remove_file(&launcher);
        }
        if prepared_here {
            let _ = fs::remove_dir_all(&release_dir);
        }
        return Err(error);
    }
    Ok(InstallOutcome {
        release_dir,
        launcher,
        activated: true,
    })
}

#[cfg(not(unix))]
pub fn install_staged(_request: InstallRequest<'_>) -> Result<InstallOutcome> {
    Err(UpdateError::UnsupportedTarget(
        "managed native installation is not implemented on this platform".into(),
    ))
}

#[cfg(unix)]
fn prepare_release(
    stage: &Path,
    request: InstallRequest<'_>,
    artifact: &ReleaseArtifact,
) -> Result<()> {
    let bin = stage.join("bin");
    fs::create_dir(&bin)?;
    set_private_directory(&bin)?;
    let installed = bin.join("pika");
    fs::copy(request.candidate, &installed)?;
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&installed, fs::Permissions::from_mode(0o700))?;
    validate_candidate(&installed, &request.manifest.version)?;

    let receipt = InstallReceipt {
        schema: MANIFEST_SCHEMA,
        kind: "native".into(),
        root: request.root.to_path_buf(),
        bin_dir: request.bin_dir.to_path_buf(),
        version: request.manifest.version.clone(),
        target: request.target.into(),
        artifact: artifact.file.clone(),
        sha256: artifact.sha256.clone(),
    };
    let receipt_bytes = serde_json::to_vec_pretty(&receipt)
        .map_err(|error| UpdateError::Manifest(error.to_string()))?;
    write_new_file(&stage.join(".pika-install.json"), &receipt_bytes, 0o600)?;

    // Retain this exact verified package for explicit, version-pinned fleet
    // installation without a second public download.
    let bundle = stage.join("bundle");
    fs::create_dir(&bundle)?;
    set_private_directory(&bundle)?;
    let bundled_artifact = bundle.join(&artifact.file);
    fs::copy(request.artifact, &bundled_artifact)?;
    fs::set_permissions(&bundled_artifact, fs::Permissions::from_mode(0o600))?;
    let manifest_bytes = serde_json::to_vec_pretty(request.manifest)
        .map_err(|error| UpdateError::Manifest(error.to_string()))?;
    write_new_file(&bundle.join(NATIVE_MANIFEST_FILE), &manifest_bytes, 0o600)?;
    write_new_file(
        &bundle.join(format!("{}.sha256", artifact.file)),
        format!("{}\n", artifact.sha256).as_bytes(),
        0o600,
    )?;
    write_new_file(
        &bundle.join("pika-version"),
        format!("{}\n", request.manifest.version).as_bytes(),
        0o600,
    )?;
    write_new_file(
        &bundle.join("install.sh"),
        include_bytes!("../scripts/install.sh"),
        0o700,
    )?;
    Ok(())
}

fn validate_existing_release(
    release_dir: &Path,
    request: InstallRequest<'_>,
    artifact: &ReleaseArtifact,
) -> Result<()> {
    let metadata = fs::symlink_metadata(release_dir)?;
    if !metadata.file_type().is_dir() {
        return Err(UpdateError::Safety(
            "release path is not a managed directory".into(),
        ));
    }
    let bin_dir = release_dir.join("bin");
    let bin_metadata = fs::symlink_metadata(&bin_dir)?;
    if !bin_metadata.file_type().is_dir() {
        return Err(UpdateError::Safety(
            "release bin path is not a managed directory".into(),
        ));
    }
    let receipt_path = release_dir.join(".pika-install.json");
    if !fs::symlink_metadata(&receipt_path)?.file_type().is_file() {
        return Err(UpdateError::Safety(
            "installation receipt must be a regular file".into(),
        ));
    }
    let receipt = read_receipt(&receipt_path)?;
    if receipt.schema != MANIFEST_SCHEMA
        || receipt.kind != "native"
        || receipt.root != request.root
        || receipt.bin_dir != request.bin_dir
        || receipt.version != request.manifest.version
        || receipt.target != request.target
        || receipt.artifact != artifact.file
        || receipt.sha256 != artifact.sha256
    {
        return Err(UpdateError::Safety(
            "existing release directory has a different receipt".into(),
        ));
    }
    validate_candidate(&release_dir.join("bin/pika"), &request.manifest.version)?;
    let bundle = release_dir.join("bundle");
    if !fs::symlink_metadata(&bundle)?.file_type().is_dir() {
        return Err(UpdateError::Safety(
            "release bundle path is not a managed directory".into(),
        ));
    }
    let bundled_manifest = read_manifest_file(&bundle.join(NATIVE_MANIFEST_FILE))?;
    if &bundled_manifest != request.manifest {
        return Err(UpdateError::Safety(
            "existing release bundle has a different manifest".into(),
        ));
    }
    let bundled_artifact = bundle.join(&artifact.file);
    checked_regular_file(&bundled_artifact)?;
    verify_artifact(&bundled_artifact, artifact)?;
    verify_sidecar(&bundle.join(format!("{}.sha256", artifact.file)), artifact)?;
    prepare_remote_install_bundle(&bundle, request.target, Some(&request.manifest.version))?;
    Ok(())
}

fn validate_candidate(candidate: &Path, version: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(candidate)
        .map_err(|error| UpdateError::Candidate(format!("cannot inspect candidate: {error}")))?;
    if !metadata.file_type().is_file() {
        return Err(UpdateError::Candidate(
            "candidate must be a regular file".into(),
        ));
    }
    let version_output = run_candidate(candidate, &["--version"])?;
    if version_output.trim() != format!("pika {version}") {
        return Err(UpdateError::Candidate(format!(
            "expected `pika {version}`, received {:?}",
            version_output.trim()
        )));
    }
    run_candidate(candidate, &["--help"])?;
    run_candidate(candidate, &["skill", "show"])?;
    Ok(())
}

fn run_candidate(candidate: &Path, arguments: &[&str]) -> Result<String> {
    let output = run_candidate_bounded(candidate, arguments, CANDIDATE_PROBE_TIMEOUT)?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr);
        return Err(UpdateError::Candidate(format!(
            "{} exited {}: {}",
            arguments.join(" "),
            output.status,
            detail.trim()
        )));
    }
    String::from_utf8(output.stdout)
        .map_err(|error| UpdateError::Candidate(format!("non-UTF-8 output: {error}")))
}

fn run_candidate_bounded(
    candidate: &Path,
    arguments: &[&str],
    timeout: Duration,
) -> Result<Output> {
    let mut command = Command::new(candidate);
    command
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command
        .spawn()
        .map_err(|error| UpdateError::Candidate(error.to_string()))?;
    let Some(stdout) = child.stdout.take() else {
        terminate_candidate_tree(&mut child);
        return Err(UpdateError::Candidate(
            "candidate stdout was unavailable".into(),
        ));
    };
    let Some(stderr) = child.stderr.take() else {
        terminate_candidate_tree(&mut child);
        return Err(UpdateError::Candidate(
            "candidate stderr was unavailable".into(),
        ));
    };
    let (sender, receiver) = mpsc::sync_channel(2);
    fn drain_candidate_output<R: Read + Send + 'static>(
        is_stdout: bool,
        mut stream: R,
        sender: mpsc::SyncSender<(bool, io::Result<Vec<u8>>)>,
    ) {
        let sender = sender.clone();
        thread::spawn(move || {
            let mut bytes = Vec::new();
            let result = stream
                .by_ref()
                .take((MAX_CANDIDATE_OUTPUT_BYTES + 1) as u64)
                .read_to_end(&mut bytes)
                .map(|_| bytes);
            let _ = sender.send((is_stdout, result));
        });
    }
    drain_candidate_output(true, stdout, sender.clone());
    drain_candidate_output(false, stderr, sender.clone());
    drop(sender);

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            Ok(None) => {
                terminate_candidate_tree(&mut child);
                return Err(UpdateError::Candidate(format!(
                    "{} exceeded its {} second deadline",
                    arguments.join(" "),
                    timeout.as_secs_f64()
                )));
            }
            Err(error) => {
                terminate_candidate_tree(&mut child);
                return Err(UpdateError::Candidate(format!(
                    "candidate wait failed: {error}"
                )));
            }
        }
    };
    // A validation probe has no reason to leave descendants behind. Closing
    // the process group also guarantees inherited output pipes reach EOF.
    terminate_candidate_tree(&mut child);

    // Pipe closure is part of the same advertised probe deadline. Reuse the
    // original deadline rather than a short post-exit grace that becomes
    // flaky under linker/CI contention.
    let output_deadline = deadline;
    let mut stdout = None;
    let mut stderr = None;
    while stdout.is_none() || stderr.is_none() {
        let remaining = output_deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| UpdateError::Candidate("candidate output did not close".into()))?;
        let (is_stdout, result) = receiver
            .recv_timeout(remaining)
            .map_err(|_| UpdateError::Candidate("candidate output did not close".into()))?;
        let bytes = result.map_err(|error| UpdateError::Candidate(error.to_string()))?;
        if bytes.len() > MAX_CANDIDATE_OUTPUT_BYTES {
            return Err(UpdateError::Candidate(
                "candidate output exceeds 1 MiB".into(),
            ));
        }
        if is_stdout {
            stdout = Some(bytes);
        } else {
            stderr = Some(bytes);
        }
    }
    Ok(Output {
        status,
        stdout: stdout.unwrap_or_default(),
        stderr: stderr.unwrap_or_default(),
    })
}

fn terminate_candidate_tree(child: &mut Child) {
    #[cfg(unix)]
    unsafe {
        // `run_candidate_bounded` creates this child as its own process-group
        // leader, so the negative PID cannot target Pika's process group.
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    #[cfg(windows)]
    {
        // Keep the owned root bounded on the experimental client target. The
        // release probe itself is not allowed to delegate validation work.
        let _ = Command::new("taskkill")
            .args(["/T", "/F", "/PID", &child.id().to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn validate_install_paths(root: &Path, bin_dir: &Path) -> Result<()> {
    if !root.is_absolute() || !bin_dir.is_absolute() {
        return Err(UpdateError::Safety(
            "installation root and bin directory must be absolute".into(),
        ));
    }
    for path in [root, bin_dir] {
        if path
            .components()
            .any(|part| matches!(part, Component::ParentDir | Component::CurDir))
        {
            return Err(UpdateError::Safety(
                "installation paths must be lexically normalized".into(),
            ));
        }
    }
    let cwd = std::env::current_dir()?;
    let home = std::env::var_os("HOME").map(PathBuf::from);
    if root == Path::new("/") || root == cwd || home.as_deref() == Some(root) {
        return Err(UpdateError::Safety(
            "choose a dedicated Pika installation directory".into(),
        ));
    }
    if root.exists() && fs::symlink_metadata(root)?.file_type().is_symlink() {
        return Err(UpdateError::Safety(
            "installation root must not be a symlink".into(),
        ));
    }
    Ok(())
}

fn normalized_install_path(path: &Path, reject_final_symlink: bool) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(Path::new("/")),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err(UpdateError::Safety(
                        "installation path escapes its filesystem root".into(),
                    ));
                }
            }
            Component::Normal(part) => normalized.push(part),
        }
    }
    if reject_final_symlink
        && normalized.exists()
        && fs::symlink_metadata(&normalized)?.file_type().is_symlink()
    {
        return Err(UpdateError::Safety(
            "installation root must not be a symlink".into(),
        ));
    }

    // Match the Python installer's resolved paths without requiring the final
    // installation directory to exist yet.
    let mut existing = normalized.as_path();
    let mut suffix = Vec::new();
    while !existing.exists() {
        let name = existing.file_name().ok_or_else(|| {
            UpdateError::Safety("installation path has no existing ancestor".into())
        })?;
        suffix.push(name.to_os_string());
        existing = existing.parent().ok_or_else(|| {
            UpdateError::Safety("installation path has no existing ancestor".into())
        })?;
    }
    let mut resolved = existing.canonicalize()?;
    for part in suffix.into_iter().rev() {
        resolved.push(part);
    }
    Ok(resolved)
}

fn initialize_or_validate_root(root: &Path, existed: bool) -> Result<()> {
    let marker = root.join(".pika-install-root");
    for path in [
        marker.as_path(),
        &root.join("tools"),
        &root.join("releases"),
        &root.join(".install.lock"),
        &root.join(".update-check.lock"),
    ] {
        reject_symlink(path)?;
    }
    if existed || marker.exists() {
        if !marker.is_file() || fs::read_to_string(&marker)? != ROOT_MARKER {
            return Err(UpdateError::Safety(
                "root is not a Pika-managed installation; nothing overwritten".into(),
            ));
        }
        return Ok(());
    }
    let entries = fs::read_dir(root)?.next().transpose()?;
    if entries.is_some() {
        return Err(UpdateError::Safety(
            "new installation root is not empty".into(),
        ));
    }
    write_new_file(&marker, ROOT_MARKER.as_bytes(), 0o600)
}

fn reject_symlink(path: &Path) -> Result<()> {
    if let Ok(metadata) = fs::symlink_metadata(path)
        && metadata.file_type().is_symlink()
    {
        return Err(UpdateError::Safety(format!(
            "unexpected symlink in managed installation: {}",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(unix)]
fn open_lock(path: &Path) -> Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    Ok(OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)?)
}

fn validate_current(current: &Path, releases: &Path) -> Result<()> {
    match fs::symlink_metadata(current) {
        Ok(metadata) if !metadata.file_type().is_symlink() => Err(UpdateError::Safety(
            "activation path is not a symlink".into(),
        )),
        Ok(_) => {
            let destination = current
                .canonicalize()
                .map_err(|_| UpdateError::Safety("activation symlink is broken".into()))?;
            let parent = destination
                .parent()
                .ok_or_else(|| UpdateError::Safety("activation target has no parent".into()))?;
            if parent != releases.canonicalize()? {
                return Err(UpdateError::Safety(
                    "activation path points outside managed releases".into(),
                ));
            }
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn validate_launcher(launcher: &Path, expected: &Path) -> Result<()> {
    match fs::symlink_metadata(launcher) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            if fs::read_link(launcher)? == expected {
                Ok(())
            } else {
                Err(UpdateError::Safety(format!(
                    "{} belongs to another installation",
                    launcher.display()
                )))
            }
        }
        Ok(_) => Err(UpdateError::Safety(format!(
            "{} belongs to another installation",
            launcher.display()
        ))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn current_receipt(current: &Path) -> Result<Option<CurrentReceipt>> {
    match fs::symlink_metadata(current) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    let path = current.join(".pika-install.json");
    if fs::symlink_metadata(&path)?.file_type().is_symlink() {
        return Err(UpdateError::Safety(
            "installation receipt must not be a symlink".into(),
        ));
    }
    let bytes = read_bounded_receipt(&path)?;
    let receipt: CurrentReceipt = serde_json::from_slice(&bytes)
        .map_err(|error| UpdateError::Safety(format!("invalid installation receipt: {error}")))?;
    if !matches!(receipt.schema, 1 | MANIFEST_SCHEMA)
        || validate_sha256(&receipt.sha256).is_err()
        || parse_version(&receipt.version).is_err()
        || (receipt.schema == MANIFEST_SCHEMA
            && (receipt.kind.as_deref() != Some("native")
                || receipt
                    .target
                    .as_deref()
                    .is_none_or(|target| validate_target(target).is_err())))
    {
        return Err(UpdateError::Safety(
            "invalid current installation receipt".into(),
        ));
    }
    Ok(Some(receipt))
}

fn read_receipt(path: &Path) -> Result<InstallReceipt> {
    let bytes = read_bounded_receipt(path)?;
    serde_json::from_slice(&bytes)
        .map_err(|error| UpdateError::Safety(format!("invalid installation receipt: {error}")))
}

fn read_bounded_receipt(path: &Path) -> Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() || metadata.len() > MAX_MANIFEST_BYTES as u64 {
        return Err(UpdateError::Safety(
            "installation receipt is not a small regular file".into(),
        ));
    }
    Ok(fs::read(path)?)
}

#[cfg(unix)]
fn ensure_launcher(launcher: &Path, expected: &Path) -> Result<bool> {
    validate_launcher(launcher, expected)?;
    if fs::symlink_metadata(launcher).is_ok() {
        return Ok(false);
    }
    let parent = launcher
        .parent()
        .ok_or_else(|| UpdateError::Safety("launcher has no parent directory".into()))?;
    fs::create_dir_all(parent)?;
    use std::os::unix::fs::symlink;
    symlink(expected, launcher)?;
    Ok(true)
}

#[cfg(unix)]
fn atomic_symlink(target: &Path, link: &Path) -> Result<()> {
    use std::os::unix::fs::symlink;
    let parent = link
        .parent()
        .ok_or_else(|| UpdateError::Safety("activation path has no parent".into()))?;
    let temporary = parent.join(format!(".current-{}", Uuid::new_v4()));
    symlink(target, &temporary)?;
    match fs::rename(&temporary, link) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = fs::remove_file(&temporary);
            Err(error.into())
        }
    }
}

#[cfg(unix)]
fn set_private_directory(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

#[cfg(unix)]
fn write_new_file(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct VersionKey {
    major: u64,
    minor: u64,
    patch: u64,
    phase: u8,
    phase_number: u64,
}

impl Ord for VersionKey {
    fn cmp(&self, other: &Self) -> Ordering {
        (
            self.major,
            self.minor,
            self.patch,
            self.phase,
            self.phase_number,
        )
            .cmp(&(
                other.major,
                other.minor,
                other.patch,
                other.phase,
                other.phase_number,
            ))
    }
}

impl PartialOrd for VersionKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

fn compare_versions(left: &str, right: &str) -> Result<Ordering> {
    Ok(parse_version(left)?.cmp(&parse_version(right)?))
}

fn parse_version(value: &str) -> Result<VersionKey> {
    // Accept the historical PEP 440 spelling and Cargo's SemVer spelling
    // during the one-time transition. Build metadata and post/dev releases are
    // deliberately unsupported.
    let (core, suffix) = value
        .find(|character: char| character.is_ascii_alphabetic() || character == '-')
        .map_or((value, ""), |index| value.split_at(index));
    let numbers: Vec<_> = core.split('.').collect();
    if numbers.len() != 3 || numbers.iter().any(|part| part.is_empty()) {
        return Err(UpdateError::Manifest("unsupported version".into()));
    }
    let major = number(numbers[0])?;
    let minor = number(numbers[1])?;
    let patch = number(numbers[2])?;
    let (phase, phase_number) = if suffix.is_empty() {
        (3, 0)
    } else if let Some(suffix_number) = suffix.strip_prefix('a') {
        (0, number(suffix_number)?)
    } else if let Some(suffix_number) = suffix.strip_prefix('b') {
        (1, number(suffix_number)?)
    } else if let Some(suffix_number) = suffix.strip_prefix("rc") {
        (2, number(suffix_number)?)
    } else if let Some(suffix_number) = suffix.strip_prefix("-alpha.") {
        (0, number(suffix_number)?)
    } else if let Some(suffix_number) = suffix.strip_prefix("-beta.") {
        (1, number(suffix_number)?)
    } else if let Some(suffix_number) = suffix.strip_prefix("-rc.") {
        (2, number(suffix_number)?)
    } else {
        return Err(UpdateError::Manifest("unsupported version".into()));
    };
    Ok(VersionKey {
        major,
        minor,
        patch,
        phase,
        phase_number,
    })
}

fn number(value: &str) -> Result<u64> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(UpdateError::Manifest("unsupported version".into()));
    }
    value
        .parse()
        .map_err(|_| UpdateError::Manifest("version number is too large".into()))
}

#[cfg(all(test, unix))]
mod bounded_candidate_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn script(body: &str) -> (tempfile::TempDir, PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("candidate");
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        (directory, path)
    }

    #[test]
    fn candidate_probe_deadline_includes_a_stalled_process() {
        let (_directory, candidate) = script("printf 'started\\n'\nsleep 30");
        let started = Instant::now();
        let error = run_candidate_bounded(&candidate, &["--version"], Duration::from_millis(150))
            .unwrap_err();
        assert!(error.to_string().contains("deadline"));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn candidate_probe_reaps_descendants_holding_output_open() {
        // Replacing the shell makes the root process exit immediately while
        // the background child deliberately retains both inherited pipes.
        // Falling off the end of a non-interactive shell is not portable: some
        // shells wait for background jobs and would test the root deadline
        // instead of descendant cleanup.
        let (_directory, candidate) =
            script("sleep 30 &\nprintf 'pika 0.0.0\\n'\nexec /usr/bin/true");
        let started = Instant::now();
        let output =
            run_candidate_bounded(&candidate, &["--version"], Duration::from_secs(1)).unwrap();
        assert!(output.status.success());
        assert_eq!(String::from_utf8(output.stdout).unwrap(), "pika 0.0.0\n");
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn candidate_probe_rejects_unbounded_output() {
        let (_directory, candidate) = script("dd if=/dev/zero bs=1048577 count=1 2>/dev/null");
        let error = run_candidate_bounded(&candidate, &[], Duration::from_secs(2)).unwrap_err();
        assert!(error.to_string().contains("exceeds 1 MiB"));
    }
}
