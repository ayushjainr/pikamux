//! Native release manifests and the POSIX managed-installation boundary.
//!
//! Downloading and archive extraction stay outside this module. The installer
//! enters here only after it has an archive and an extracted candidate; this
//! module verifies both before writing the managed installation root.

use crate::consult::{
    CancellablePipe, CancellationToken, OwnedChild, poll_owned_child, terminate_child,
};
use fs2::FileExt;
use serde::{
    Deserialize, Deserializer, Serialize,
    de::{self, MapAccess, Visitor},
};
use sha2::{Digest, Sha256};
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use thiserror::Error;
use uuid::Uuid;

pub const MANIFEST_SCHEMA: u32 = 2;
pub const NATIVE_MANIFEST_FILE: &str = "pika-native-release.json";
pub const ROOT_MARKER: &str = "pikamux-installer-v1\n";
pub const MAX_MANIFEST_BYTES: usize = 64 * 1024;
pub const MAX_ARTIFACT_BYTES: u64 = 20 * 1024 * 1024;
pub const MAX_EXECUTABLE_BYTES: u64 = 50 * 1024 * 1024;
const MAX_NOTICE_BYTES: u64 = 2 * 1024 * 1024;
pub const MAX_RELEASE_LIST_BYTES: u64 = 2 * 1024 * 1024;
const MAX_CANDIDATE_OUTPUT_BYTES: usize = 1024 * 1024;
const MAX_RETAINED_RELEASE_SCAN: usize = 256;
const CANDIDATE_PROBE_TIMEOUT: Duration = Duration::from_secs(10);
const ARCHIVE_OPERATION_TIMEOUT: Duration = Duration::from_secs(60);
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
    #[error("installation interrupted by signal {0}; nothing further was activated")]
    Interrupted(i32),
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
    #[serde(deserialize_with = "deserialize_unique_artifacts")]
    pub artifacts: BTreeMap<String, ReleaseArtifact>,
}

fn deserialize_unique_artifacts<'de, D>(
    deserializer: D,
) -> std::result::Result<BTreeMap<String, ReleaseArtifact>, D::Error>
where
    D: Deserializer<'de>,
{
    struct UniqueArtifacts;

    impl<'de> Visitor<'de> for UniqueArtifacts {
        type Value = BTreeMap<String, ReleaseArtifact>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("an object with unique native artifact targets")
        }

        fn visit_map<A>(self, mut entries: A) -> std::result::Result<Self::Value, A::Error>
        where
            A: MapAccess<'de>,
        {
            let mut artifacts = BTreeMap::new();
            while let Some((target, artifact)) = entries.next_entry::<String, ReleaseArtifact>()? {
                if artifacts.insert(target.clone(), artifact).is_some() {
                    return Err(de::Error::custom(format!(
                        "duplicate native artifact target: {target}"
                    )));
                }
            }
            Ok(artifacts)
        }
    }

    deserializer.deserialize_map(UniqueArtifacts)
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
    RolledBack,
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

/// Return a best-effort, strictly offline update notice for a managed
/// installation. The interactive board must never own an unjoinable network
/// child: explicit `pika update --check` is the network refresh boundary.
/// Unmanaged builds, disabled checks, missing cache and stale cache are all
/// ordinary `None` outcomes.
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
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |duration| duration.as_secs_f64());
    let cache_path = managed.root.join(".update-check.json");
    let Some(cache) = read_update_notice_cache(&cache_path) else {
        return Ok(None);
    };
    let fresh = {
        let age = now - cache.checked_at;
        cache.current == managed.version
            && age >= 0.0
            && age < if cache.failed { 3600.0 } else { 6.0 * 3600.0 }
    };
    if !fresh {
        return Ok(None);
    }
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
            UpdateDisposition::RolledBack => format!(
                "Rolled Pika back {} → {}. Reopen the board when convenient; running agents were not restarted.",
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
    validate_path_owner(&executable).map_err(|_| UpdateError::Unmanaged)?;
    validate_path_owner(bin).map_err(|_| UpdateError::Unmanaged)?;
    let release_dir = bin.parent().ok_or(UpdateError::Unmanaged)?.to_path_buf();
    let receipt = current_receipt(&release_dir)
        .map_err(|_| UpdateError::Unmanaged)?
        .ok_or(UpdateError::Unmanaged)?;
    let root = normalized_install_path(&receipt.root, true).map_err(|_| UpdateError::Unmanaged)?;
    let bin_dir =
        normalized_install_path(&receipt.bin_dir, false).map_err(|_| UpdateError::Unmanaged)?;
    initialize_or_validate_root(&root, true).map_err(|_| UpdateError::Unmanaged)?;
    let releases = root.join("releases");
    validate_current(&root.join("current"), &releases).map_err(|_| UpdateError::Unmanaged)?;
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
        if let Some(outcome) =
            metadata_update_outcome(&managed, &manifest, artifact, request.check)?
        {
            return Ok(outcome);
        }
        let source_path = bundle.join(&artifact.file);
        let sidecar_path = bundle.join(format!("{}.sha256", artifact.file));
        verify_sidecar(&sidecar_path, artifact)?;
        verify_artifact(&source_path, artifact)?;
        let copy = scratch.path.join(&artifact.file);
        copy_regular_bounded(
            &source_path,
            &copy,
            artifact.bytes,
            "native release archive",
        )?;
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
        if let Some(outcome) =
            metadata_update_outcome(&managed, &manifest, artifact, request.check)?
        {
            return Ok(outcome);
        }
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

/// Atomically activate an already retained, fully revalidated native release.
/// With no version, the newest validated release older than the current one is
/// selected. Rollback never downloads, extracts, or guesses from directory
/// names; the immutable receipt, bundle, checksum, and executable are proven
/// again while the installation lock is held.
#[cfg(unix)]
pub fn rollback_managed(executable: &Path, requested: Option<&str>) -> Result<UpdateOutcome> {
    if let Some(version) = requested {
        parse_version(version)?;
    }
    let managed = discover_managed_install(executable)?;
    let target = native_target()?;
    let lock_path = managed.root.join(".install.lock");
    reject_symlink(&lock_path)?;
    let lock = open_lock(&lock_path)?;
    lock.try_lock_exclusive().map_err(|error| {
        if error.kind() == io::ErrorKind::WouldBlock {
            UpdateError::Busy
        } else {
            UpdateError::Io(error)
        }
    })?;

    initialize_or_validate_root(&managed.root, true)?;
    let releases = managed.root.join("releases");
    let current = managed.root.join("current");
    validate_current(&current, &releases)?;
    if current.canonicalize()? != managed.release_dir {
        return Err(UpdateError::Safety(
            "active release changed while rollback was starting; retry explicitly".into(),
        ));
    }
    validate_launcher(&managed.bin_dir.join("pika"), &current.join("bin/pika"))?;

    let mut candidates = Vec::new();
    for (index, entry) in fs::read_dir(&releases)?.enumerate() {
        if index >= MAX_RETAINED_RELEASE_SCAN {
            return Err(UpdateError::Safety(format!(
                "retained release inventory exceeds {MAX_RETAINED_RELEASE_SCAN} entries; nothing activated"
            )));
        }
        let entry = entry?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if !metadata.file_type().is_dir() || path == managed.release_dir {
            continue;
        }
        let Ok((version, release)) =
            validate_retained_release(&path, &managed.root, &managed.bin_dir, target)
        else {
            continue;
        };
        if compare_versions(&version, &managed.version)? != Ordering::Less {
            continue;
        }
        if requested.is_none_or(|wanted| wanted == version) {
            candidates.push((parse_version(&version)?, version, release));
        }
    }
    candidates.sort_by(|left, right| left.0.cmp(&right.0));
    let Some((_, version, release_dir)) = candidates.pop() else {
        let detail = requested.map_or_else(
            || "no prior validated native release is retained".to_owned(),
            |version| format!("retained release {version} is absent or failed validation"),
        );
        return Err(UpdateError::Safety(format!("{detail}; nothing activated")));
    };
    let _activation = atomic_symlink(&release_dir, &current)?;
    validate_launcher(&managed.bin_dir.join("pika"), &current.join("bin/pika"))?;
    Ok(UpdateOutcome {
        disposition: UpdateDisposition::RolledBack,
        previous_version: managed.version,
        version,
        launcher: managed.bin_dir.join("pika"),
    })
}

#[cfg(not(unix))]
pub fn rollback_managed(_executable: &Path, _requested: Option<&str>) -> Result<UpdateOutcome> {
    Err(UpdateError::UnsupportedTarget(
        "managed native rollback is not implemented on this platform".into(),
    ))
}

#[cfg(unix)]
fn validate_retained_release(
    release_dir: &Path,
    root: &Path,
    bin_dir: &Path,
    target: &str,
) -> Result<(String, PathBuf)> {
    validate_path_owner(release_dir)?;
    let canonical = release_dir.canonicalize()?;
    if canonical.parent() != Some(root.join("releases").canonicalize()?.as_path()) {
        return Err(UpdateError::Safety(
            "retained release escapes the managed release directory".into(),
        ));
    }
    let receipt_path = canonical.join(".pika-install.json");
    let receipt = read_receipt(&receipt_path)?;
    if receipt.schema != MANIFEST_SCHEMA
        || receipt.kind != "native"
        || receipt.root != root
        || receipt.bin_dir != bin_dir
        || receipt.target != target
    {
        return Err(UpdateError::Safety(
            "retained release receipt does not own this installation".into(),
        ));
    }
    let bundle = canonical.join("bundle");
    validate_path_owner(&bundle)?;
    let manifest = read_manifest_file(&bundle.join(NATIVE_MANIFEST_FILE))?;
    if manifest.version != receipt.version {
        return Err(UpdateError::Safety(
            "retained release manifest and receipt differ".into(),
        ));
    }
    let artifact = manifest.artifact_for(target)?;
    if artifact.file != receipt.artifact || artifact.sha256 != receipt.sha256 {
        return Err(UpdateError::Safety(
            "retained release artifact and receipt differ".into(),
        ));
    }
    let scratch = ScratchDirectory::new("pika-rollback")?;
    extract_candidate(&bundle.join(&artifact.file), target, &scratch.path)?;
    let archived_candidate = scratch.path.join("pika");
    let notices = read_sibling_notices(&archived_candidate)?;
    validate_existing_release(
        &canonical,
        InstallRequest {
            manifest: &manifest,
            target,
            artifact: &bundle.join(&artifact.file),
            candidate: &archived_candidate,
            root,
            bin_dir,
        },
        artifact,
        &notices,
    )?;
    validate_candidate(&archived_candidate, &receipt.version)?;
    Ok((receipt.version, canonical))
}

fn metadata_update_outcome(
    managed: &ManagedInstallation,
    manifest: &ReleaseManifest,
    artifact: &ReleaseArtifact,
    check: bool,
) -> Result<Option<UpdateOutcome>> {
    match compare_versions(&manifest.version, &managed.version)? {
        Ordering::Less => Err(UpdateError::Safety(
            "refusing a release downgrade; nothing activated".into(),
        )),
        Ordering::Equal => {
            if managed.sha256 != artifact.sha256 {
                return Err(UpdateError::Safety(
                    "same version has different package bytes; publish a new version".into(),
                ));
            }
            Ok(Some(already_current(managed)))
        }
        Ordering::Greater if check => Ok(Some(UpdateOutcome {
            disposition: UpdateDisposition::Available,
            previous_version: managed.version.clone(),
            version: manifest.version.clone(),
            launcher: managed.bin_dir.join("pika"),
        })),
        Ordering::Greater => Ok(None),
    }
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
    let bytes = read_regular_bounded(path, MAX_MANIFEST_BYTES as u64, "release manifest")?;
    ReleaseManifest::parse(&bytes)
}

/// Read one local release manifest through the same regular-file and size
/// boundary used by update bundles.
pub fn read_release_manifest(path: &Path) -> Result<ReleaseManifest> {
    read_manifest_file(path)
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
    let version = String::from_utf8(read_regular_bounded(&version_path, 128, "pika-version")?)
        .map_err(|_| UpdateError::Safety("pika-version is not UTF-8".into()))?;
    if version.lines().count() != 1 || version.trim() != manifest.version {
        return Err(UpdateError::Safety(
            "remote bundle has an invalid pika-version file".into(),
        ));
    }
    let installer_path = bundle.join("install.sh");
    if read_regular_bounded(&installer_path, 1024 * 1024, "bundle installer")?
        != include_bytes!("../scripts/install.sh")
    {
        return Err(UpdateError::Safety(
            "remote bundle installer does not match this Pika release".into(),
        ));
    }
    // The archive hash in the manifest is the trust anchor. Compare the
    // transport siblings with that exact archive rather than this build's
    // notices, so a retained older release remains installable after its
    // dependency notices legitimately change in a newer Pika version.
    let notices = read_archive_notices(&artifact_path, target)?;
    validate_notice_file(&bundle.join("LICENSE"), &notices.license)?;
    validate_notice_file(&bundle.join("THIRD_PARTY.md"), &notices.third_party)?;

    // Freeze the validated semantic payload in an owned directory. The
    // caller-controlled bundle is never handed to tar, so a replacement race
    // cannot substitute installer or notice bytes after validation.
    let transport = ScratchDirectory::new("pika-remote-bundle")?;
    copy_regular_bounded(
        &artifact_path,
        &transport.path.join(&artifact.file),
        artifact.bytes,
        "native release archive",
    )?;
    fs::write(
        transport.path.join(NATIVE_MANIFEST_FILE),
        serde_json::to_vec_pretty(&manifest)
            .map_err(|error| UpdateError::Manifest(error.to_string()))?,
    )?;
    fs::write(
        transport.path.join(format!("{}.sha256", artifact.file)),
        format!("{}\n", artifact.sha256),
    )?;
    fs::write(
        transport.path.join("pika-version"),
        format!("{}\n", manifest.version),
    )?;
    fs::write(
        transport.path.join("install.sh"),
        include_bytes!("../scripts/install.sh"),
    )?;
    fs::write(transport.path.join("LICENSE"), &notices.license)?;
    fs::write(transport.path.join("THIRD_PARTY.md"), &notices.third_party)?;

    let names = [
        "install.sh".to_owned(),
        "pika-version".to_owned(),
        NATIVE_MANIFEST_FILE.to_owned(),
        artifact.file.clone(),
        format!("{}.sha256", artifact.file),
        "LICENSE".to_owned(),
        "THIRD_PARTY.md".to_owned(),
    ];
    let limit = MAX_ARTIFACT_BYTES as usize + 3 * 1024 * 1024;
    let mut command = Command::new("tar");
    command
        .args(["-cf", "-", "-C"])
        .arg(&transport.path)
        .args(&names);
    let output = run_command_bounded(
        command,
        ARCHIVE_OPERATION_TIMEOUT,
        limit,
        "remote bundle preparation",
    )?;
    checked_command(&output, "remote bundle preparation")?;
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

fn read_archive_notices(archive: &Path, target: &str) -> Result<ReleaseNotices> {
    validate_target(target)?;
    if target != "x86_64-pc-windows-msvc" {
        #[cfg(unix)]
        {
            let scratch = ScratchDirectory::new("pika-notices")?;
            extract_candidate(archive, target, &scratch.path)?;
            return read_sibling_notices(&scratch.path.join("pika"));
        }
        #[cfg(not(unix))]
        {
            return Err(UpdateError::UnsupportedTarget(target.into()));
        }
    }

    let mut listing = Command::new("unzip");
    listing.arg("-Z1").arg(archive);
    let listing = run_command_bounded(
        listing,
        ARCHIVE_OPERATION_TIMEOUT,
        4096,
        "archive inspection",
    )?;
    checked_command(&listing, "archive inspection")?;
    let text = String::from_utf8(listing.stdout)
        .map_err(|_| UpdateError::UnsafeArchiveMember("non-UTF-8 path".into()))?;
    let members = text.lines().collect::<Vec<_>>();
    validate_archive_members(members.iter().copied(), target)?;
    let expected = std::collections::BTreeSet::from(["LICENSE", "THIRD_PARTY.md", "pika.exe"]);
    if members.len() != 3
        || members
            .iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>()
            != expected
    {
        return Err(UpdateError::UnsafeArchiveMember(
            "native archive must contain exactly pika.exe, LICENSE, and THIRD_PARTY.md".into(),
        ));
    }
    let mut verbose = Command::new("unzip");
    verbose.args(["-Z", "-l"]).arg(archive).env("LC_ALL", "C");
    let verbose = run_command_bounded(
        verbose,
        ARCHIVE_OPERATION_TIMEOUT,
        8192,
        "archive inspection",
    )?;
    checked_command(&verbose, "archive inspection")?;
    let regular_members = verbose
        .stdout
        .split(|byte| *byte == b'\n')
        .filter(|line| {
            expected.iter().any(|name| {
                line.strip_suffix(name.as_bytes())
                    .is_some_and(|prefix| prefix.last().is_some_and(u8::is_ascii_whitespace))
            })
        })
        .collect::<Vec<_>>();
    if regular_members.len() != 3
        || regular_members
            .iter()
            .any(|line| line.first() != Some(&b'-'))
    {
        return Err(UpdateError::UnsafeArchiveMember(
            "native archive contains a non-regular member".into(),
        ));
    }
    let read_member = |name: &str| -> Result<Vec<u8>> {
        let mut command = Command::new("unzip");
        command.args(["-p"]).arg(archive).arg(name);
        let output = run_command_bounded(
            command,
            ARCHIVE_OPERATION_TIMEOUT,
            MAX_NOTICE_BYTES as usize,
            "archive notice inspection",
        )?;
        checked_command(&output, "archive notice inspection")?;
        if output.stdout.is_empty() {
            return Err(UpdateError::Safety(format!(
                "release notice is empty: {name}"
            )));
        }
        Ok(output.stdout)
    };
    Ok(ReleaseNotices {
        license: read_member("LICENSE")?,
        third_party: read_member("THIRD_PARTY.md")?,
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

#[cfg(unix)]
fn open_regular_read(path: &Path, description: &str) -> Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)
        .map_err(|error| {
            UpdateError::Safety(format!(
                "cannot open {description} as a regular file: {error}"
            ))
        })?;
    if !file.metadata()?.file_type().is_file() {
        return Err(UpdateError::Safety(format!(
            "{description} must be a regular file: {}",
            path.display()
        )));
    }
    Ok(file)
}

#[cfg(not(unix))]
fn open_regular_read(path: &Path, description: &str) -> Result<File> {
    checked_regular_file(path)?;
    File::open(path).map_err(|error| {
        UpdateError::Safety(format!(
            "cannot open {description} as a regular file: {error}"
        ))
    })
}

fn read_regular_bounded(path: &Path, limit: u64, description: &str) -> Result<Vec<u8>> {
    let mut file = open_regular_read(path, description)?;
    let metadata = file.metadata()?;
    if metadata.len() == 0 || metadata.len() > limit {
        return Err(UpdateError::Safety(format!(
            "{description} has an invalid size: {}",
            path.display()
        )));
    }
    let capacity = usize::try_from(metadata.len()).map_err(|_| {
        UpdateError::Safety(format!("{description} exceeds this platform's size limit"))
    })?;
    let mut bytes = Vec::with_capacity(capacity);
    Read::by_ref(&mut file)
        .take(limit + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 != metadata.len() || bytes.len() as u64 > limit {
        return Err(UpdateError::Safety(format!(
            "{description} changed while it was being read; nothing activated"
        )));
    }
    Ok(bytes)
}

fn copy_regular_bounded(
    source: &Path,
    destination: &Path,
    exact_bytes: u64,
    description: &str,
) -> Result<()> {
    if exact_bytes == 0 || exact_bytes > MAX_ARTIFACT_BYTES {
        return Err(UpdateError::Safety(format!(
            "{description} has an invalid declared size"
        )));
    }
    let mut input = open_regular_read(source, description)?;
    if input.metadata()?.len() != exact_bytes {
        return Err(UpdateError::ChecksumMismatch);
    }
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)?;
    let copied = io::copy(
        &mut Read::by_ref(&mut input).take(exact_bytes + 1),
        &mut output,
    );
    let result = match copied {
        Ok(bytes) if bytes == exact_bytes => {
            output.sync_all()?;
            Ok(())
        }
        Ok(_) => Err(UpdateError::Safety(format!(
            "{description} changed while it was being copied; nothing activated"
        ))),
        Err(error) => Err(UpdateError::Io(error)),
    };
    if result.is_err() {
        drop(output);
        let _ = fs::remove_file(destination);
    }
    result
}

fn verify_sidecar(path: &Path, artifact: &ReleaseArtifact) -> Result<()> {
    let value = String::from_utf8(
        read_regular_bounded(path, 256, "artifact checksum")
            .map_err(|_| UpdateError::ChecksumMismatch)?,
    )
    .map_err(|_| UpdateError::ChecksumMismatch)?;
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
    // Prove the only payload is bounded before any filesystem extraction.
    // This reader closes at the first byte over the limit and the owned child
    // group is then reaped, so a high-ratio archive cannot fill the disk.
    validate_archive_expanded_size(
        archive,
        (MAX_EXECUTABLE_BYTES + 2 * MAX_NOTICE_BYTES) as usize,
    )?;
    let mut command = Command::new("tar");
    command.args(["-tzf"]).arg(archive);
    let listing = run_command_bounded(
        command,
        ARCHIVE_OPERATION_TIMEOUT,
        4096,
        "archive inspection",
    )?;
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
    let member_set = members
        .iter()
        .copied()
        .collect::<std::collections::BTreeSet<_>>();
    if member_set != std::collections::BTreeSet::from(["LICENSE", "THIRD_PARTY.md", "pika"])
        || members.len() != 3
    {
        return Err(UpdateError::UnsafeArchiveMember(
            "native archive must contain exactly pika, LICENSE, and THIRD_PARTY.md".into(),
        ));
    }
    let mut command = Command::new("tar");
    command.args(["-tvzf"]).arg(archive);
    let verbose = run_command_bounded(
        command,
        ARCHIVE_OPERATION_TIMEOUT,
        4096,
        "archive inspection",
    )?;
    checked_command(&verbose, "archive inspection")?;
    let verbose_lines = verbose
        .stdout
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    if verbose_lines.len() != 3 || verbose_lines.iter().any(|line| line.first() != Some(&b'-')) {
        return Err(UpdateError::UnsafeArchiveMember(
            "native archive contains a non-regular member".into(),
        ));
    }
    let mut command = Command::new("tar");
    command
        .args(["-xzf"])
        .arg(archive)
        .arg("-C")
        .arg(destination)
        .args(["LICENSE", "THIRD_PARTY.md", "pika"]);
    let extracted = run_command_bounded(
        command,
        ARCHIVE_OPERATION_TIMEOUT,
        4096,
        "archive extraction",
    )?;
    checked_command(&extracted, "archive extraction")?;
    let candidate_path = destination.join("pika");
    let candidate = checked_regular_file(&candidate_path)?;
    let metadata = fs::metadata(candidate)?;
    if metadata.len() == 0 || metadata.len() > MAX_EXECUTABLE_BYTES {
        return Err(UpdateError::Candidate(
            "native executable has an invalid size".into(),
        ));
    }
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(candidate, fs::Permissions::from_mode(0o700))?;
    read_notice_file(&destination.join("LICENSE"))?;
    read_notice_file(&destination.join("THIRD_PARTY.md"))?;
    Ok(())
}

#[cfg(unix)]
fn validate_archive_expanded_size(archive: &Path, limit: usize) -> Result<()> {
    let mut command = Command::new("tar");
    command.args(["-xOzf"]).arg(archive);
    let output = run_command_bounded(
        command,
        ARCHIVE_OPERATION_TIMEOUT,
        limit,
        "archive expansion preflight",
    )?;
    checked_command(&output, "archive expansion preflight")?;
    if output.stdout.is_empty() {
        return Err(UpdateError::Candidate(
            "native executable has an invalid size".into(),
        ));
    }
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

#[derive(Debug)]
struct ReleaseNotices {
    license: Vec<u8>,
    third_party: Vec<u8>,
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
    let unbound_request = InstallRequest {
        manifest: request.manifest,
        target: request.target,
        artifact: request.artifact,
        candidate: request.candidate,
        root: &normalized_root,
        bin_dir: &normalized_bin_dir,
    };
    unbound_request.manifest.validate()?;
    let local_target = native_target()?;
    if unbound_request.target != local_target {
        return Err(UpdateError::Safety(format!(
            "artifact target {} does not match this machine ({local_target})",
            unbound_request.target
        )));
    }
    let release_artifact = unbound_request
        .manifest
        .artifact_for(unbound_request.target)?;
    let artifact_file = unbound_request
        .artifact
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| UpdateError::Safety("artifact has no safe filename".into()))?;
    if artifact_file != release_artifact.file {
        return Err(UpdateError::Safety(
            "artifact filename does not match manifest".into(),
        ));
    }
    verify_artifact(unbound_request.artifact, release_artifact)?;

    // The archive checksum is the installation trust boundary. Re-extract it
    // here from an owned byte-for-byte copy, even if an outer installer already
    // extracted and probed a candidate. Reverification after the bounded copy
    // closes the path-replacement window before extraction and retention.
    let bound_scratch = ScratchDirectory::new("pika-install-candidate")?;
    let bound_artifact = bound_scratch.path.join(&release_artifact.file);
    copy_regular_bounded(
        unbound_request.artifact,
        &bound_artifact,
        release_artifact.bytes,
        "native release archive",
    )?;
    verify_artifact(&bound_artifact, release_artifact)?;
    extract_candidate(&bound_artifact, unbound_request.target, &bound_scratch.path)?;
    let bound_candidate = bound_scratch.path.join("pika");
    compare_candidate_bytes(unbound_request.candidate, &bound_candidate)?;
    let request = InstallRequest {
        manifest: unbound_request.manifest,
        target: unbound_request.target,
        artifact: &bound_artifact,
        candidate: &bound_candidate,
        root: unbound_request.root,
        bin_dir: unbound_request.bin_dir,
    };

    validate_install_paths(request.root, request.bin_dir)?;
    if request.root.exists() {
        initialize_or_validate_root(request.root, true)?;
    }
    let preflight_current = request.root.join("current");
    let preflight_launcher = request.bin_dir.join("pika");
    validate_launcher(&preflight_launcher, &preflight_current.join("bin/pika"))?;
    validate_candidate(request.candidate, &request.manifest.version)?;
    let notices = read_sibling_notices(request.candidate)?;

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
    sync_directory(&releases)?;
    sync_directory(request.root)?;
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
                let active_release = current.canonicalize()?;
                validate_existing_release(&active_release, request, release_artifact, &notices)?;
                ensure_launcher(&launcher, &expected_launcher_target)?;
                return Ok(InstallOutcome {
                    release_dir: active_release,
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
    if release_dir.exists() {
        validate_existing_release(&release_dir, request, release_artifact, &notices)?;
    } else {
        let stage = releases.join(format!(".stage-{}", Uuid::new_v4()));
        fs::create_dir(&stage)?;
        set_private_directory(&stage)?;
        let prepared = prepare_release(&stage, request, release_artifact, &notices);
        if let Err(error) = prepared {
            let _ = fs::remove_dir_all(&stage);
            return Err(error);
        }
        if let Err(error) = fs::rename(&stage, &release_dir) {
            let _ = fs::remove_dir_all(&stage);
            return Err(error.into());
        }
        sync_directory(&releases)?;
    }

    // On first install, stage the launcher under a private temporary name. The
    // public `pika` path must never be made durable while `current` is absent:
    // an interruption may leave no launcher, but never a broken one.
    let staged_launcher = stage_launcher(&launcher, &expected_launcher_target)?;
    if let Err(error) = atomic_symlink(&release_dir, &current) {
        if let Some(staged) = &staged_launcher {
            let _ = fs::remove_file(staged);
            if let Some(parent) = staged.parent() {
                let _ = sync_directory(parent);
            }
        }
        // The fully synced release is deliberately retained. If the current
        // symlink was renamed before a directory-sync failure, removing its
        // target would turn a recoverable activation error into a broken home.
        return Err(error);
    }
    if let Some(staged) = staged_launcher {
        publish_staged_launcher(&staged, &launcher, &expected_launcher_target)?;
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
    notices: &ReleaseNotices,
) -> Result<()> {
    let bin = stage.join("bin");
    fs::create_dir(&bin)?;
    set_private_directory(&bin)?;
    let installed = bin.join("pika");
    fs::copy(request.candidate, &installed)?;
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&installed, fs::Permissions::from_mode(0o700))?;
    validate_candidate(&installed, &request.manifest.version)?;
    sync_file(&installed)?;

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
    write_new_file(&stage.join("LICENSE"), &notices.license, 0o600)?;
    write_new_file(&stage.join("THIRD_PARTY.md"), &notices.third_party, 0o600)?;

    // Retain this exact verified package for explicit, version-pinned fleet
    // installation without a second public download.
    let bundle = stage.join("bundle");
    fs::create_dir(&bundle)?;
    set_private_directory(&bundle)?;
    let bundled_artifact = bundle.join(&artifact.file);
    fs::copy(request.artifact, &bundled_artifact)?;
    fs::set_permissions(&bundled_artifact, fs::Permissions::from_mode(0o600))?;
    sync_file(&bundled_artifact)?;
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
    write_new_file(&bundle.join("LICENSE"), &notices.license, 0o600)?;
    write_new_file(&bundle.join("THIRD_PARTY.md"), &notices.third_party, 0o600)?;
    // Persist every payload before the stage name can become a release, then
    // persist the directory hierarchy bottom-up.
    sync_directory(&bin)?;
    sync_directory(&bundle)?;
    sync_directory(stage)?;
    Ok(())
}

fn validate_existing_release(
    release_dir: &Path,
    request: InstallRequest<'_>,
    artifact: &ReleaseArtifact,
    notices: &ReleaseNotices,
) -> Result<()> {
    let metadata = fs::symlink_metadata(release_dir)?;
    if !metadata.file_type().is_dir() {
        return Err(UpdateError::Safety(
            "release path is not a managed directory".into(),
        ));
    }
    validate_metadata_owner(&metadata, release_dir)?;
    let bin_dir = release_dir.join("bin");
    let bin_metadata = fs::symlink_metadata(&bin_dir)?;
    if !bin_metadata.file_type().is_dir() {
        return Err(UpdateError::Safety(
            "release bin path is not a managed directory".into(),
        ));
    }
    validate_metadata_owner(&bin_metadata, &bin_dir)?;
    let receipt_path = release_dir.join(".pika-install.json");
    let receipt_metadata = fs::symlink_metadata(&receipt_path)?;
    if !receipt_metadata.file_type().is_file() {
        return Err(UpdateError::Safety(
            "installation receipt must be a regular file".into(),
        ));
    }
    validate_metadata_owner(&receipt_metadata, &receipt_path)?;
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
    let installed_path = release_dir.join("bin/pika");
    validate_path_owner(&installed_path)?;
    let installed = checked_regular_file(&installed_path)?;
    let expected = checked_regular_file(request.candidate)?;
    if fs::metadata(installed)?.len() > MAX_EXECUTABLE_BYTES
        || sha256_file(installed)? != sha256_file(expected)?
    {
        return Err(UpdateError::Safety(
            "retained executable differs from its verified candidate".into(),
        ));
    }
    validate_notice_file(&release_dir.join("LICENSE"), &notices.license)?;
    validate_notice_file(&release_dir.join("THIRD_PARTY.md"), &notices.third_party)?;
    validate_path_owner(&release_dir.join("LICENSE"))?;
    validate_path_owner(&release_dir.join("THIRD_PARTY.md"))?;
    let bundle = release_dir.join("bundle");
    let bundle_metadata = fs::symlink_metadata(&bundle)?;
    if !bundle_metadata.file_type().is_dir() {
        return Err(UpdateError::Safety(
            "release bundle path is not a managed directory".into(),
        ));
    }
    validate_metadata_owner(&bundle_metadata, &bundle)?;
    let bundled_manifest = read_manifest_file(&bundle.join(NATIVE_MANIFEST_FILE))?;
    validate_path_owner(&bundle.join(NATIVE_MANIFEST_FILE))?;
    if &bundled_manifest != request.manifest {
        return Err(UpdateError::Safety(
            "existing release bundle has a different manifest".into(),
        ));
    }
    let bundled_artifact = bundle.join(&artifact.file);
    validate_path_owner(&bundled_artifact)?;
    checked_regular_file(&bundled_artifact)?;
    verify_artifact(&bundled_artifact, artifact)?;
    verify_sidecar(&bundle.join(format!("{}.sha256", artifact.file)), artifact)?;
    validate_path_owner(&bundle.join(format!("{}.sha256", artifact.file)))?;
    validate_notice_file(&bundle.join("LICENSE"), &notices.license)?;
    validate_notice_file(&bundle.join("THIRD_PARTY.md"), &notices.third_party)?;
    for name in ["LICENSE", "THIRD_PARTY.md", "pika-version", "install.sh"] {
        validate_path_owner(&bundle.join(name))?;
    }
    prepare_remote_install_bundle(&bundle, request.target, Some(&request.manifest.version))?;
    Ok(())
}

fn read_sibling_notices(candidate: &Path) -> Result<ReleaseNotices> {
    let parent = candidate
        .parent()
        .ok_or_else(|| UpdateError::Safety("verified candidate has no archive directory".into()))?;
    Ok(ReleaseNotices {
        license: read_notice_file(&parent.join("LICENSE"))?,
        third_party: read_notice_file(&parent.join("THIRD_PARTY.md"))?,
    })
}

fn read_notice_file(path: &Path) -> Result<Vec<u8>> {
    read_regular_bounded(path, MAX_NOTICE_BYTES, "release notice")
}

fn compare_candidate_bytes(supplied: &Path, archived: &Path) -> Result<()> {
    let mut supplied = open_regular_read(supplied, "supplied native candidate")?;
    let mut archived = open_regular_read(archived, "archive-bound native candidate")?;
    let supplied_metadata = supplied.metadata()?;
    let archived_metadata = archived.metadata()?;
    if supplied_metadata.len() == 0
        || supplied_metadata.len() > MAX_EXECUTABLE_BYTES
        || supplied_metadata.len() != archived_metadata.len()
    {
        return Err(UpdateError::Safety(
            "supplied executable differs from the checksum-verified archive; nothing executed"
                .into(),
        ));
    }
    let digest = |file: &mut File| -> Result<([u8; 32], u64)> {
        let mut hash = Sha256::new();
        let mut buffer = [0_u8; 64 * 1024];
        let mut bytes = 0_u64;
        let mut reader = Read::by_ref(file).take(MAX_EXECUTABLE_BYTES + 1);
        loop {
            let read = reader.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            bytes += read as u64;
            hash.update(&buffer[..read]);
        }
        Ok((hash.finalize().into(), bytes))
    };
    let supplied_digest = digest(&mut supplied)?;
    let archived_digest = digest(&mut archived)?;
    if supplied_digest.1 != supplied_metadata.len()
        || archived_digest.1 != archived_metadata.len()
        || supplied_digest.0 != archived_digest.0
    {
        return Err(UpdateError::Safety(
            "supplied executable differs from the checksum-verified archive; nothing executed"
                .into(),
        ));
    }
    Ok(())
}

/// Prove that the hidden installation command is running from the exact
/// executable path supplied by the outer bootstrap. This closes the last
/// substitution gap before `install_staged` rebinds that executable to the
/// checksum-verified archive.
#[cfg(unix)]
pub fn verify_running_install_candidate(candidate: &Path) -> Result<()> {
    let running = std::env::current_exe().map_err(|error| {
        UpdateError::Safety(format!("cannot identify the running installer: {error}"))
    })?;
    compare_candidate_bytes(&running, candidate)
}

fn validate_notice_file(path: &Path, expected: &[u8]) -> Result<()> {
    if read_notice_file(path)? != expected {
        return Err(UpdateError::Safety(format!(
            "release notice differs from its verified archive: {}",
            path.display()
        )));
    }
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
    run_command_bounded(
        command,
        timeout,
        MAX_CANDIDATE_OUTPUT_BYTES,
        "candidate probe",
    )
}

fn run_command_bounded(
    mut command: Command,
    timeout: Duration,
    stdout_limit: usize,
    operation: &str,
) -> Result<Output> {
    #[cfg(unix)]
    let interrupts = ScopedCommandInterrupts::install()?;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = OwnedChild::spawn(&mut command)
        .map_err(|error| UpdateError::Candidate(format!("cannot run {operation}: {error}")))?;
    let stop = CancellationToken::default();
    let Some(stdout) = child.stdout.take() else {
        return Err(UpdateError::Candidate(
            "candidate stdout was unavailable".into(),
        ));
    };
    let Some(stderr) = child.stderr.take() else {
        return Err(UpdateError::Candidate(
            "candidate stderr was unavailable".into(),
        ));
    };
    let stdout = CancellablePipe::new(stdout, stop.clone())?;
    let stderr = CancellablePipe::new(stderr, stop.clone())?;
    let (sender, receiver) = mpsc::sync_channel(2);
    let stdout_worker = drain_bounded_output(true, stdout, sender.clone(), stdout_limit);
    let stderr_worker =
        drain_bounded_output(false, stderr, sender.clone(), MAX_CANDIDATE_OUTPUT_BYTES);
    drop(sender);

    let deadline = Instant::now() + timeout;
    let outcome = (|| {
        let status = loop {
            #[cfg(unix)]
            if let Some(code) = interrupts.exit_code() {
                return Err(UpdateError::Interrupted(code));
            }
            // This observes exit without reaping, kills the pinned group,
            // then caches the reaped status. No stale numeric PGID is used.
            match poll_owned_child(&mut child) {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
                Ok(None) => {
                    return Err(UpdateError::Candidate(format!(
                        "{} exceeded its {} second deadline",
                        operation,
                        timeout.as_secs_f64()
                    )));
                }
                Err(error) => {
                    return Err(UpdateError::Candidate(format!(
                        "candidate wait failed: {error}"
                    )));
                }
            }
        };
        // A descendant outside the owned group may retain a pipe. It is not
        // ours to signal; the same probe deadline bounds waiting for its EOF.
        let mut stdout = None;
        let mut stderr = None;
        while stdout.is_none() || stderr.is_none() {
            #[cfg(unix)]
            if let Some(code) = interrupts.exit_code() {
                return Err(UpdateError::Interrupted(code));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            let (is_stdout, result) =
                match receiver.recv_timeout(remaining.min(Duration::from_millis(25))) {
                    Ok(value) => value,
                    Err(mpsc::RecvTimeoutError::Timeout) if !remaining.is_zero() => continue,
                    Err(_) => {
                        return Err(UpdateError::Candidate(
                            "candidate output did not close".into(),
                        ));
                    }
                };
            let bytes = result.map_err(|error| UpdateError::Candidate(error.to_string()))?;
            let limit = if is_stdout {
                stdout_limit
            } else {
                MAX_CANDIDATE_OUTPUT_BYTES
            };
            if bytes.len() > limit {
                if operation == "candidate probe" && limit == MAX_CANDIDATE_OUTPUT_BYTES {
                    return Err(UpdateError::Candidate(
                        "candidate output exceeds 1 MiB".into(),
                    ));
                }
                return Err(UpdateError::Candidate(format!(
                    "{operation} output exceeds its {limit} byte safety limit"
                )));
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
    })();
    let cleanup = terminate_child(&mut child);
    stop.cancel();
    // Unix pipes are nonblocking, so cancellation bounds these joins even
    // when a foreign process keeps its inherited descriptor open.
    #[cfg(unix)]
    {
        let _ = stdout_worker.join();
        let _ = stderr_worker.join();
    }
    #[cfg(not(unix))]
    drop((stdout_worker, stderr_worker));
    cleanup
        .map_err(|error| UpdateError::Candidate(format!("candidate cleanup failed: {error}")))?;
    #[cfg(unix)]
    if let Some(code) = interrupts.exit_code() {
        return Err(UpdateError::Interrupted(code));
    }
    outcome
}

#[cfg(unix)]
struct ScopedCommandInterrupts {
    signal: Arc<AtomicUsize>,
    registrations: Vec<signal_hook::SigId>,
}

#[cfg(unix)]
impl ScopedCommandInterrupts {
    fn install() -> Result<Self> {
        let signal = Arc::new(AtomicUsize::new(0));
        let mut registrations = Vec::with_capacity(2);
        for value in [libc::SIGINT, libc::SIGTERM] {
            match signal_hook::flag::register_usize(value, Arc::clone(&signal), value as usize) {
                Ok(registration) => registrations.push(registration),
                Err(error) => {
                    for registration in registrations.drain(..) {
                        signal_hook::low_level::unregister(registration);
                    }
                    return Err(UpdateError::Safety(format!(
                        "cannot install scoped update signal handling: {error}"
                    )));
                }
            }
        }
        Ok(Self {
            signal,
            registrations,
        })
    }

    fn exit_code(&self) -> Option<i32> {
        match self.signal.load(AtomicOrdering::SeqCst) as i32 {
            libc::SIGINT => Some(130),
            libc::SIGTERM => Some(143),
            _ => None,
        }
    }
}

#[cfg(unix)]
impl Drop for ScopedCommandInterrupts {
    fn drop(&mut self) {
        for registration in self.registrations.drain(..) {
            signal_hook::low_level::unregister(registration);
        }
    }
}

#[cfg(test)]
fn drain_candidate_output<R: Read + Send + 'static>(
    is_stdout: bool,
    stream: R,
    sender: mpsc::SyncSender<(bool, io::Result<Vec<u8>>)>,
) -> thread::JoinHandle<()> {
    drain_bounded_output(is_stdout, stream, sender, MAX_CANDIDATE_OUTPUT_BYTES)
}

fn drain_bounded_output<R: Read + Send + 'static>(
    is_stdout: bool,
    mut stream: R,
    sender: mpsc::SyncSender<(bool, io::Result<Vec<u8>>)>,
    limit: usize,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = stream
            .by_ref()
            .take((limit + 1) as u64)
            .read_to_end(&mut bytes)
            .map(|_| bytes);
        let _ = sender.send((is_stdout, result));
    })
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
    if root.exists() {
        validate_path_owner(root)?;
    }
    for path in [
        marker.as_path(),
        &root.join("tools"),
        &root.join("releases"),
        &root.join(".install.lock"),
        &root.join(".update-check.lock"),
    ] {
        reject_symlink(path)?;
        if fs::symlink_metadata(path).is_ok() {
            validate_path_owner(path)?;
        }
    }
    let current = root.join("current");
    if fs::symlink_metadata(&current).is_ok() {
        validate_path_owner(&current)?;
    }
    if existed || marker.exists() {
        let marker_bytes = read_regular_bounded(&marker, 64, "managed root marker")?;
        if marker_bytes != ROOT_MARKER.as_bytes() {
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
    write_new_file(&marker, ROOT_MARKER.as_bytes(), 0o600)?;
    sync_directory(root)?;
    if let Some(parent) = root.parent() {
        sync_directory(parent)?;
    }
    Ok(())
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
fn validate_metadata_owner(metadata: &fs::Metadata, path: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    validate_uid(metadata.uid(), unsafe { libc::geteuid() }, path)
}

#[cfg(not(unix))]
fn validate_metadata_owner(_metadata: &fs::Metadata, _path: &Path) -> Result<()> {
    Ok(())
}

fn validate_uid(actual: u32, expected: u32, path: &Path) -> Result<()> {
    if actual != expected {
        return Err(UpdateError::Safety(format!(
            "managed installation component is owned by another user: {}",
            path.display()
        )));
    }
    Ok(())
}

fn validate_path_owner(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    validate_metadata_owner(&metadata, path)
}

#[cfg(unix)]
fn open_lock(path: &Path) -> Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(path)?;
    validate_metadata_owner(&file.metadata()?, path)?;
    Ok(file)
}

fn validate_current(current: &Path, releases: &Path) -> Result<()> {
    match fs::symlink_metadata(current) {
        Ok(metadata) if !metadata.file_type().is_symlink() => Err(UpdateError::Safety(
            "activation path is not a symlink".into(),
        )),
        Ok(_) => {
            validate_path_owner(current)?;
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
            validate_path_owner(releases)?;
            validate_path_owner(&destination)?;
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn validate_launcher(launcher: &Path, expected: &Path) -> Result<()> {
    match fs::symlink_metadata(launcher) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            validate_metadata_owner(&metadata, launcher)?;
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
    validate_path_owner(&path)?;
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
    validate_metadata_owner(&metadata, path)?;
    read_regular_bounded(path, MAX_MANIFEST_BYTES as u64, "installation receipt")
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
    sync_directory(parent)?;
    Ok(true)
}

#[cfg(unix)]
fn stage_launcher(launcher: &Path, expected: &Path) -> Result<Option<PathBuf>> {
    validate_launcher(launcher, expected)?;
    if fs::symlink_metadata(launcher).is_ok() {
        return Ok(None);
    }
    let parent = launcher
        .parent()
        .ok_or_else(|| UpdateError::Safety("launcher has no parent directory".into()))?;
    fs::create_dir_all(parent)?;
    let staged = parent.join(format!(".pika-launcher-{}", Uuid::new_v4()));
    use std::os::unix::fs::symlink;
    symlink(expected, &staged)?;
    sync_directory(parent)?;
    Ok(Some(staged))
}

#[cfg(unix)]
fn publish_staged_launcher(staged: &Path, launcher: &Path, expected: &Path) -> Result<()> {
    match fs::symlink_metadata(launcher) {
        Ok(_) => {
            validate_launcher(launcher, expected)?;
            fs::remove_file(staged)?;
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::rename(staged, launcher)?;
        }
        Err(error) => return Err(error.into()),
    }
    let parent = launcher
        .parent()
        .ok_or_else(|| UpdateError::Safety("launcher has no parent directory".into()))?;
    if let Err(error) = sync_directory(parent) {
        if fs::read_link(launcher).ok().as_deref() != Some(expected) {
            return Err(error);
        }
    }
    Ok(())
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LinkActivation {
    Durable,
    SwitchedDurabilityUnconfirmed,
}

#[cfg(unix)]
fn atomic_symlink(target: &Path, link: &Path) -> Result<LinkActivation> {
    atomic_symlink_with_sync(target, link, sync_directory)
}

#[cfg(unix)]
fn atomic_symlink_with_sync(
    target: &Path,
    link: &Path,
    mut sync_parent: impl FnMut(&Path) -> Result<()>,
) -> Result<LinkActivation> {
    use std::os::unix::fs::symlink;
    let parent = link
        .parent()
        .ok_or_else(|| UpdateError::Safety("activation path has no parent".into()))?;
    let temporary = parent.join(format!(".current-{}", Uuid::new_v4()));
    symlink(target, &temporary)?;
    // Make both the old activation and the temporary replacement durable
    // before rename. After the atomic swap, persist the selected name.
    if let Err(error) = sync_parent(parent) {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    match fs::rename(&temporary, link) {
        Ok(()) => match sync_parent(parent) {
            Ok(()) => Ok(LinkActivation::Durable),
            Err(error) => {
                // rename(2) already chose the new release. Re-read the link so
                // callers never report "nothing activated" or remove the new
                // launcher after a post-rename fsync error. Durability across
                // an immediate power loss is unconfirmed, but current state is
                // exact and usable.
                if fs::read_link(link).ok().as_deref() == Some(target) {
                    Ok(LinkActivation::SwitchedDurabilityUnconfirmed)
                } else {
                    Err(error)
                }
            }
        },
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

#[cfg(unix)]
fn sync_file(path: &Path) -> Result<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_dir() {
        return Err(UpdateError::Safety(format!(
            "durability boundary is not a directory: {}",
            path.display()
        )));
    }
    File::open(path)?.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<()> {
    // Managed activation is Unix-only; keep shared receipt validation
    // buildable for the Windows client without claiming directory fsync.
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
        // instead of descendant cleanup. Five seconds leaves scheduler
        // headroom in a parallel suite while remaining far below the child's
        // 30-second lifetime and the production probe deadline.
        let (_directory, candidate) = script(
            "sleep 30 &\nprintf 'pika 0.0.0\\n'\nprintf 'diagnostic\\n' >&2\nexec /usr/bin/true",
        );
        let started = Instant::now();
        let output =
            run_candidate_bounded(&candidate, &["--version"], Duration::from_secs(5)).unwrap();
        assert!(output.status.success());
        assert_eq!(String::from_utf8(output.stdout).unwrap(), "pika 0.0.0\n");
        assert_eq!(String::from_utf8(output.stderr).unwrap(), "diagnostic\n");
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn candidate_probe_rejects_unbounded_output() {
        let (_directory, candidate) = script("dd if=/dev/zero bs=1048577 count=1 2>/dev/null");
        let error = run_candidate_bounded(&candidate, &[], Duration::from_secs(2)).unwrap_err();
        assert!(error.to_string().contains("exceeds 1 MiB"));
    }

    #[test]
    fn candidate_probe_preserves_reaped_failure_status_and_both_streams() {
        let (_directory, candidate) = script("printf out\nprintf err >&2\nexit 7");
        for _ in 0..10 {
            // This checks status/stream preservation, not scheduler latency.
            // Use the unchanged production budget so parallel-suite contention
            // cannot turn it into an accidental one-second performance gate.
            // The stalled-process regression separately verifies deadlines.
            let output = run_candidate_bounded(&candidate, &[], CANDIDATE_PROBE_TIMEOUT).unwrap();
            assert_eq!(output.status.code(), Some(7));
            assert_eq!(output.stdout, b"out");
            assert_eq!(output.stderr, b"err");
        }
    }

    #[test]
    fn candidate_pipe_workers_cancel_without_foreign_holder_eof() {
        use std::os::unix::net::UnixStream;

        let stop = CancellationToken::default();
        let (reader, mut held_writer) = UnixStream::pair().unwrap();
        held_writer.write_all(b"partial").unwrap();
        let reader = CancellablePipe::new(reader, stop.clone()).unwrap();
        let (sender, receiver) = mpsc::sync_channel(2);
        let worker = drain_candidate_output(true, reader, sender);
        // Simulate the output deadline expiring while a foreign process
        // still owns a writer. No PID is needed or safe to signal here.
        assert!(receiver.recv_timeout(Duration::from_millis(30)).is_err());
        let started = Instant::now();
        stop.cancel();
        worker.join().unwrap();
        let (is_stdout, result) = receiver.recv().unwrap();
        assert!(is_stdout);
        assert_eq!(result.unwrap(), b"partial");
        assert!(started.elapsed() < Duration::from_secs(1));
        drop(held_writer);
    }

    #[test]
    fn activation_reports_the_link_as_switched_after_post_rename_sync_failure() {
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("first");
        let second = directory.path().join("second");
        let current = directory.path().join("current");
        fs::create_dir(&first).unwrap();
        fs::create_dir(&second).unwrap();
        std::os::unix::fs::symlink(&first, &current).unwrap();
        let mut syncs = 0;
        let outcome = atomic_symlink_with_sync(&second, &current, |_parent| {
            syncs += 1;
            if syncs == 2 {
                Err(UpdateError::Io(io::Error::other(
                    "injected post-rename fsync failure",
                )))
            } else {
                Ok(())
            }
        })
        .unwrap();
        assert_eq!(outcome, LinkActivation::SwitchedDurabilityUnconfirmed);
        assert_eq!(fs::read_link(&current).unwrap(), second);
    }

    #[test]
    fn activation_remains_unswitched_after_pre_rename_sync_failure() {
        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("first");
        let second = directory.path().join("second");
        let current = directory.path().join("current");
        fs::create_dir(&first).unwrap();
        fs::create_dir(&second).unwrap();
        std::os::unix::fs::symlink(&first, &current).unwrap();
        let error = atomic_symlink_with_sync(&second, &current, |_parent| {
            Err(UpdateError::Io(io::Error::other(
                "injected pre-rename fsync failure",
            )))
        })
        .unwrap_err();
        assert!(error.to_string().contains("pre-rename"));
        assert_eq!(fs::read_link(&current).unwrap(), first);
    }

    #[test]
    fn first_install_keeps_the_public_launcher_absent_until_current_exists() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("managed");
        let release = root.join("releases/0.6.0-alpha.1-test");
        let current = root.join("current");
        let bin = directory.path().join("bin");
        let launcher = bin.join("pika");
        let expected = current.join("bin/pika");
        fs::create_dir_all(release.join("bin")).unwrap();
        fs::write(release.join("bin/pika"), b"candidate").unwrap();

        let staged = stage_launcher(&launcher, &expected).unwrap().unwrap();
        assert!(
            fs::symlink_metadata(&staged)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(fs::symlink_metadata(&launcher).is_err());
        assert!(fs::symlink_metadata(&current).is_err());

        atomic_symlink(&release, &current).unwrap();
        assert!(current.join("bin").is_dir());
        publish_staged_launcher(&staged, &launcher, &expected).unwrap();
        assert_eq!(fs::read_link(&launcher).unwrap(), expected);
        assert_eq!(
            launcher.canonicalize().unwrap(),
            release.join("bin/pika").canonicalize().unwrap()
        );
    }

    #[test]
    fn archive_expansion_is_rejected_before_extraction_when_it_exceeds_limit() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("pika"), vec![0_u8; 128 * 1024]).unwrap();
        let archive = directory.path().join("pika.tar.gz");
        let status = Command::new("tar")
            .args(["-czf"])
            .arg(&archive)
            .arg("-C")
            .arg(&source)
            .arg("pika")
            .status()
            .unwrap();
        assert!(status.success());

        let error = validate_archive_expanded_size(&archive, 64 * 1024).unwrap_err();
        assert!(error.to_string().contains("safety limit"));
    }

    #[test]
    fn managed_component_owner_mismatch_fails_closed() {
        let path = Path::new("/managed/component");
        validate_uid(501, 501, path).unwrap();
        let error = validate_uid(0, 501, path).unwrap_err();
        assert!(error.to_string().contains("owned by another user"));
        assert!(error.to_string().contains("/managed/component"));
    }
}
