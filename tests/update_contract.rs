#![cfg(unix)]

use pikamux::update::{
    InstallRequest, ReleaseArtifact, ReleaseManifest, UpdateDisposition, UpdateError,
    UpdateRequest, artifact_name, cached_update_notice, install_staged, native_target,
    prepare_remote_install_bundle, select_latest_release, sha256_file, update_managed,
    validate_archive_members,
};
use pikamux::{
    fleet::{
        CAPABILITIES, FleetError, FleetInstallTransport, FleetManager, FleetTransport,
        PROTOCOL_NAME, PROTOCOL_VERSION, SshTransport,
    },
    model::FleetNode,
    store::Store,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

fn candidate(path: &Path, version: &str, valid: bool) {
    let reported = if valid { version } else { "9.9.9" };
    fs::write(
        path,
        format!(
            "#!/bin/sh\ncase \"$*\" in\n  --version) echo 'pika {reported}' ;;\n  --help) exit 0 ;;\n  'skill show') echo skill ;;\n  _install-native*) [ -z \"${{PIKA_TEST_TRACE:-}}\" ] || printf '%s\\n' \"$@\" > \"$PIKA_TEST_TRACE\" ;;\n  *) exit 2 ;;\nesac\n"
        ),
    )
    .unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

fn fixture(temp: &Path, version: &str, payload: &[u8]) -> (ReleaseManifest, PathBuf, PathBuf) {
    let target = native_target().unwrap();
    let archive = temp.join(artifact_name(version, target).unwrap());
    fs::write(&archive, payload).unwrap();
    let binary = temp.join(format!("candidate-{version}"));
    candidate(&binary, version, true);
    let artifact = ReleaseArtifact {
        file: archive.file_name().unwrap().to_str().unwrap().into(),
        sha256: sha256_file(&archive).unwrap(),
        bytes: payload.len() as u64,
    };
    let manifest = ReleaseManifest {
        schema: 2,
        package: "pikamux".into(),
        version: version.into(),
        channel: "preview".into(),
        artifacts: BTreeMap::from([(target.into(), artifact)]),
    };
    (manifest, archive, binary)
}

fn release_bundle(temp: &Path, version: &str) -> PathBuf {
    let bundle = temp.join(format!("bundle-{version}"));
    let payload = temp.join(format!("payload-{version}"));
    fs::create_dir(&bundle).unwrap();
    fs::create_dir(&payload).unwrap();
    candidate(&payload.join("pika"), version, true);
    let target = native_target().unwrap();
    let name = artifact_name(version, target).unwrap();
    let archive = bundle.join(&name);
    let status = Command::new("tar")
        .args(["-czf"])
        .arg(&archive)
        .args(["-C"])
        .arg(&payload)
        .arg("pika")
        .status()
        .unwrap();
    assert!(status.success());
    let checksum = sha256_file(&archive).unwrap();
    let manifest = ReleaseManifest {
        schema: 2,
        package: "pikamux".into(),
        version: version.into(),
        channel: "preview".into(),
        artifacts: BTreeMap::from([(
            target.into(),
            ReleaseArtifact {
                file: name.clone(),
                sha256: checksum.clone(),
                bytes: fs::metadata(&archive).unwrap().len(),
            },
        )]),
    };
    fs::write(
        bundle.join("pika-release.json"),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
    fs::write(
        bundle.join(format!("{name}.sha256")),
        format!("{checksum}\n"),
    )
    .unwrap();
    fs::write(bundle.join("pika-version"), format!("{version}\n")).unwrap();
    fs::copy("scripts/install.sh", bundle.join("install.sh")).unwrap();
    bundle
}

#[test]
fn board_update_notice_uses_fresh_managed_cache_without_network() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("managed");
    let bin = temp.path().join("bin");
    let (manifest, archive, executable) = fixture(temp.path(), "0.6.0-alpha.1", b"notice");
    let installed = install_staged(InstallRequest {
        manifest: &manifest,
        target: native_target().unwrap(),
        artifact: &archive,
        candidate: &executable,
        root: &root,
        bin_dir: &bin,
    })
    .unwrap();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();
    fs::write(
        root.join(".update-check.json"),
        serde_json::to_vec(&json!({
            "current":"0.6.0-alpha.1",
            "checked_at":now,
            "latest":"0.6.0-alpha.2",
            "failed":false
        }))
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        cached_update_notice(&installed.launcher),
        Some("0.6.0-alpha.2".into())
    );
}

fn fleet_node(id: &str, version: &str) -> FleetNode {
    FleetNode {
        node_id: id.into(),
        alias: "research-node".into(),
        ssh_target: "research-node.example".into(),
        sources: vec!["explicit".into()],
        status: "ready".into(),
        protocol_version: Some(PROTOCOL_VERSION),
        package_version: Some(version.into()),
        capabilities: CAPABILITIES.iter().map(|value| (*value).into()).collect(),
        last_seen: 1.0,
        last_attempt_at: 1.0,
        last_error: None,
        created_at: 1.0,
        updated_at: 1.0,
    }
}

fn hello(id: &str, version: &str) -> Value {
    json!({
        "type":"hello", "protocol":PROTOCOL_NAME, "version":PROTOCOL_VERSION,
        "node_id":id, "machine":"research-node", "package_version":version,
        "capabilities":CAPABILITIES,
    })
}

struct UpgradeTransport {
    responses: Mutex<Vec<Value>>,
    installs: Mutex<Vec<(String, Vec<u8>)>>,
}

impl FleetTransport for &UpgradeTransport {
    fn request(
        &self,
        _target: &str,
        _payload: &Value,
        _mutating: bool,
    ) -> Result<Value, FleetError> {
        Ok(self.responses.lock().unwrap().remove(0))
    }

    fn run_exact(
        &self,
        _node: &FleetNode,
        _arguments: &[String],
        _tty: bool,
    ) -> Result<i32, FleetError> {
        unreachable!("upgrade uses the explicit install capability")
    }
}

impl FleetInstallTransport for &UpgradeTransport {
    fn native_target(&self, _target: &str) -> Result<String, FleetError> {
        Ok(native_target().unwrap().to_owned())
    }

    fn install_bundle(
        &self,
        target: &str,
        bundle: &pikamux::update::RemoteInstallBundle,
    ) -> Result<String, FleetError> {
        self.installs
            .lock()
            .unwrap()
            .push((target.into(), bundle.as_bytes().to_vec()));
        Ok("installed".into())
    }
}

#[test]
fn schema_two_selects_only_exact_target_and_filename() {
    let target = native_target().unwrap();
    let json = format!(
        r#"{{"schema":2,"package":"pikamux","version":"0.6.0-alpha.1","channel":"preview","artifacts":{{"{target}":{{"file":"{}","sha256":"{}","bytes":42}}}}}}"#,
        artifact_name("0.6.0-alpha.1", target).unwrap(),
        "a".repeat(64)
    );
    let manifest = ReleaseManifest::parse(json.as_bytes()).unwrap();
    assert_eq!(manifest.artifact_for(target).unwrap().bytes, 42);
    assert!(manifest.artifact_for("unknown-target").is_err());

    let unsafe_name = json.replace("pikamux-0.6.0-alpha.1-", "../");
    assert!(ReleaseManifest::parse(unsafe_name.as_bytes()).is_err());
    let unknown = json.replacen(
        "\"schema\":2",
        "\"schema\":2,\"url\":\"https://evil.invalid\"",
        1,
    );
    assert!(ReleaseManifest::parse(unknown.as_bytes()).is_err());
    let false_stable = json.replace("\"preview\"", "\"stable\"");
    assert!(ReleaseManifest::parse(false_stable.as_bytes()).is_err());
}

#[test]
fn archive_members_reject_traversal_links_names_and_duplicates() {
    let target = native_target().unwrap();
    validate_archive_members(["pika"], target).unwrap();
    for entries in [
        vec!["../pika"],
        vec!["/pika"],
        vec!["pika", "./notice"],
        vec!["pika\\evil"],
        vec!["pika", "pika"],
        vec!["LICENSE"],
    ] {
        assert!(validate_archive_members(entries, target).is_err());
    }
}

#[test]
fn staged_install_is_atomic_idempotent_and_retains_previous_release() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("managed");
    let bin = temp.path().join("bin");
    let target = native_target().unwrap();
    let (first, first_archive, first_binary) =
        fixture(temp.path(), "0.6.0-alpha.1", b"first archive");
    let outcome = install_staged(InstallRequest {
        manifest: &first,
        target,
        artifact: &first_archive,
        candidate: &first_binary,
        root: &root,
        bin_dir: &bin,
    })
    .unwrap();
    assert!(outcome.activated);
    assert_eq!(
        fs::read_link(bin.join("pika")).unwrap(),
        root.canonicalize().unwrap().join("current/bin/pika")
    );
    assert_eq!(
        root.join("current").canonicalize().unwrap(),
        outcome.release_dir
    );
    assert!(
        outcome
            .release_dir
            .join("bundle/pika-release.json")
            .is_file()
    );
    assert!(
        outcome
            .release_dir
            .join(format!(
                "bundle/{}.sha256",
                first.artifact_for(target).unwrap().file
            ))
            .is_file()
    );
    assert_eq!(
        ReleaseManifest::parse(
            &fs::read(outcome.release_dir.join("bundle/pika-release.json")).unwrap()
        )
        .unwrap(),
        first
    );
    assert_eq!(
        fs::read(
            outcome
                .release_dir
                .join("bundle")
                .join(&first.artifact_for(target).unwrap().file)
        )
        .unwrap(),
        fs::read(&first_archive).unwrap()
    );

    let repeat = install_staged(InstallRequest {
        manifest: &first,
        target,
        artifact: &first_archive,
        candidate: &first_binary,
        root: &root,
        bin_dir: &bin,
    })
    .unwrap();
    assert!(!repeat.activated);

    let old = root.join("current").canonicalize().unwrap();
    let (next, next_archive, next_binary) =
        fixture(temp.path(), "0.6.0-alpha.2", b"second archive");
    let upgraded = install_staged(InstallRequest {
        manifest: &next,
        target,
        artifact: &next_archive,
        candidate: &next_binary,
        root: &root,
        bin_dir: &bin,
    })
    .unwrap();
    assert!(upgraded.activated);
    assert!(old.is_dir());
    assert_ne!(root.join("current").canonicalize().unwrap(), old);
}

#[test]
fn native_install_accepts_a_valid_schema_one_bridge_receipt() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("managed");
    let bin = temp.path().join("bin");
    let target = native_target().unwrap();
    let (bridge, bridge_archive, bridge_binary) =
        fixture(temp.path(), "0.5.0a5", b"bridge archive");
    install_staged(InstallRequest {
        manifest: &bridge,
        target,
        artifact: &bridge_archive,
        candidate: &bridge_binary,
        root: &root,
        bin_dir: &bin,
    })
    .unwrap();
    let current = root.join("current").canonicalize().unwrap();
    fs::write(
        current.join(".pika-install.json"),
        serde_json::json!({
            "schema": 1,
            "root": root.canonicalize().unwrap().to_string_lossy(),
            "bin_dir": bin.canonicalize().unwrap().to_string_lossy(),
            "version": "0.5.0a5",
            "sha256": "a".repeat(64)
        })
        .to_string(),
    )
    .unwrap();

    let (native, archive, binary) = fixture(temp.path(), "0.6.0-alpha.1", b"native archive");
    let outcome = install_staged(InstallRequest {
        manifest: &native,
        target,
        artifact: &archive,
        candidate: &binary,
        root: &root,
        bin_dir: &bin,
    })
    .unwrap();
    assert!(outcome.activated);
    assert!(current.is_dir());
}

#[test]
fn unmodified_v050a4_accepts_the_schema_one_bridge_manifest() {
    let temp = tempfile::tempdir().unwrap();
    let wheel = "pikamux-0.5.0a5-py3-none-any.whl";
    let manifest = temp.path().join("pika-release.json");
    fs::write(
        &manifest,
        serde_json::json!({
            "schema": 1,
            "version": "0.5.0a5",
            "wheel": wheel,
            "sha256": "a".repeat(64),
        })
        .to_string(),
    )
    .unwrap();
    let reference = Path::new(env!("CARGO_MANIFEST_DIR")).join("reference/python/src");
    let output = Command::new("python3")
        .args([
            "-c",
            "from pikamux.installation import read_manifest; import sys; print(read_manifest(__import__('pathlib').Path(sys.argv[1]))['version'])",
        ])
        .arg(&manifest)
        .env("PYTHONPATH", reference)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "0.5.0a5");
}

#[test]
fn latest_selection_requires_a_complete_channel_compatible_release() {
    let target = native_target().unwrap();
    let assets = |version: &str| {
        let artifact = artifact_name(version, target).unwrap();
        serde_json::json!([
            {"name":"pika-release.json","state":"uploaded"},
            {"name":artifact,"state":"uploaded"},
            {"name":format!("{artifact}.sha256"),"state":"uploaded"}
        ])
    };
    let listing = serde_json::json!([
        {"tag_name":"v9.0.0","draft":true,"prerelease":false,"assets":assets("9.0.0")},
        {"tag_name":"v0.7.0-alpha.2","draft":false,"prerelease":true,"assets":assets("0.7.0-alpha.2")},
        {"tag_name":"v0.6.1","draft":false,"prerelease":false,"assets":assets("0.6.1")},
        {"tag_name":"v0.8.0","draft":false,"prerelease":false,"assets":[{"name":"pika-release.json","state":"uploaded"}]},
        {"tag_name":"v0.7.0","draft":false,"prerelease":true,"assets":assets("0.7.0")}
    ]);
    let bytes = serde_json::to_vec(&listing).unwrap();
    assert_eq!(
        select_latest_release(&bytes, "0.6.0", target).unwrap(),
        Some("0.6.1".into())
    );
    assert_eq!(
        select_latest_release(&bytes, "0.6.0-alpha.1", target).unwrap(),
        Some("0.7.0-alpha.2".into())
    );
}

#[test]
fn public_offline_update_checks_then_installs_without_python() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("managed");
    let bin = temp.path().join("bin");
    let target = native_target().unwrap();
    let (first, first_archive, first_binary) =
        fixture(temp.path(), "0.6.0-alpha.1", b"first archive");
    let installed = install_staged(InstallRequest {
        manifest: &first,
        target,
        artifact: &first_archive,
        candidate: &first_binary,
        root: &root,
        bin_dir: &bin,
    })
    .unwrap();
    let executable = installed.release_dir.join("bin/pika");
    let bundle = release_bundle(temp.path(), "0.6.0-alpha.2");

    let checked = update_managed(UpdateRequest {
        executable: &executable,
        bundle: Some(&bundle),
        release: None,
        check: true,
    })
    .unwrap();
    assert_eq!(checked.disposition, UpdateDisposition::Available);
    assert_eq!(
        root.join("current").canonicalize().unwrap(),
        installed.release_dir
    );

    let updated = update_managed(UpdateRequest {
        executable: &executable,
        bundle: Some(&bundle),
        release: None,
        check: false,
    })
    .unwrap();
    assert_eq!(updated.disposition, UpdateDisposition::Installed);
    assert_eq!(updated.version, "0.6.0-alpha.2");
    assert_eq!(
        Command::new(root.join("current/bin/pika"))
            .arg("--version")
            .output()
            .unwrap()
            .stdout,
        b"pika 0.6.0-alpha.2\n"
    );
}

#[test]
fn bad_checksum_or_candidate_writes_nothing() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("managed");
    let bin = temp.path().join("bin");
    let target = native_target().unwrap();
    let (mut manifest, archive, binary) = fixture(temp.path(), "0.6.0-alpha.1", b"archive");
    manifest.artifacts.get_mut(target).unwrap().sha256 = "0".repeat(64);
    assert!(matches!(
        install_staged(InstallRequest {
            manifest: &manifest,
            target,
            artifact: &archive,
            candidate: &binary,
            root: &root,
            bin_dir: &bin,
        }),
        Err(UpdateError::ChecksumMismatch)
    ));
    assert!(!root.exists());

    manifest.artifacts.get_mut(target).unwrap().sha256 = sha256_file(&archive).unwrap();
    candidate(&binary, "0.6.0-alpha.1", false);
    assert!(matches!(
        install_staged(InstallRequest {
            manifest: &manifest,
            target,
            artifact: &archive,
            candidate: &binary,
            root: &root,
            bin_dir: &bin,
        }),
        Err(UpdateError::Candidate(_))
    ));
    assert!(!root.exists());
}

#[test]
fn foreign_root_launcher_and_downgrade_are_refused() {
    let temp = tempfile::tempdir().unwrap();
    let target = native_target().unwrap();
    let (manifest, archive, binary) = fixture(temp.path(), "0.6.0-alpha.2", b"archive");
    let root = temp.path().join("foreign");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("keep"), "mine").unwrap();
    let bin = temp.path().join("bin");
    assert!(
        install_staged(InstallRequest {
            manifest: &manifest,
            target,
            artifact: &archive,
            candidate: &binary,
            root: &root,
            bin_dir: &bin,
        })
        .is_err()
    );
    assert_eq!(fs::read_to_string(root.join("keep")).unwrap(), "mine");

    let managed = temp.path().join("managed");
    fs::create_dir(&bin).unwrap();
    fs::write(bin.join("pika"), "foreign").unwrap();
    assert!(
        install_staged(InstallRequest {
            manifest: &manifest,
            target,
            artifact: &archive,
            candidate: &binary,
            root: &managed,
            bin_dir: &bin,
        })
        .is_err()
    );
    assert_eq!(fs::read_to_string(bin.join("pika")).unwrap(), "foreign");
    assert!(!managed.exists());

    fs::remove_file(bin.join("pika")).unwrap();
    install_staged(InstallRequest {
        manifest: &manifest,
        target,
        artifact: &archive,
        candidate: &binary,
        root: &managed,
        bin_dir: &bin,
    })
    .unwrap();
    let (older, older_archive, older_binary) = fixture(temp.path(), "0.6.0-alpha.1", b"older");
    assert!(
        install_staged(InstallRequest {
            manifest: &older,
            target,
            artifact: &older_archive,
            candidate: &older_binary,
            root: &managed,
            bin_dir: &bin,
        })
        .unwrap_err()
        .to_string()
        .contains("downgrade")
    );
}

#[test]
fn activation_symlink_outside_releases_is_refused() {
    let temp = tempfile::tempdir().unwrap();
    let target = native_target().unwrap();
    let (manifest, archive, binary) = fixture(temp.path(), "0.6.0-alpha.1", b"archive");
    let root = temp.path().join("managed");
    fs::create_dir(&root).unwrap();
    fs::write(root.join(".pika-install-root"), "pikamux-installer-v1\n").unwrap();
    let outside = temp.path().join("outside");
    fs::create_dir(&outside).unwrap();
    symlink(&outside, root.join("current")).unwrap();
    assert!(
        install_staged(InstallRequest {
            manifest: &manifest,
            target,
            artifact: &archive,
            candidate: &binary,
            root: &root,
            bin_dir: &temp.path().join("bin"),
        })
        .is_err()
    );
}

#[test]
fn shell_bootstrap_uses_an_offline_bundle_and_forwards_only_fixed_paths() {
    let temp = tempfile::tempdir().unwrap();
    let bundle = temp.path().join("bundle");
    let payload = temp.path().join("payload");
    fs::create_dir(&bundle).unwrap();
    fs::create_dir(&payload).unwrap();
    let version = env!("CARGO_PKG_VERSION");
    let target = native_target().unwrap();
    let trace = temp.path().join("trace");
    let fake = payload.join("pika");
    fs::write(
        &fake,
        "#!/bin/sh\ncase \"$*\" in\n --version) echo \"pika $PIKA_TEST_VERSION\" ;;\n --help|'skill show') exit 0 ;;\n _install-native*) printf '%s\\n' \"$@\" > \"$PIKA_TEST_TRACE\" ;;\n *) exit 2 ;;\nesac\n",
    )
    .unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).unwrap();
    let archive_name = artifact_name(version, target).unwrap();
    let archive = bundle.join(&archive_name);
    let status = Command::new("tar")
        .args(["-czf"])
        .arg(&archive)
        .args(["-C"])
        .arg(&payload)
        .arg("pika")
        .status()
        .unwrap();
    assert!(status.success());
    let checksum = sha256_file(&archive).unwrap();
    fs::write(
        bundle.join(format!("{archive_name}.sha256")),
        format!("{checksum}\n"),
    )
    .unwrap();
    fs::write(bundle.join("pika-version"), format!("{version}\n")).unwrap();
    fs::write(bundle.join("pika-release.json"), "{}\n").unwrap();

    let output = Command::new("bash")
        .arg("scripts/install.sh")
        .arg("--bundle")
        .arg(&bundle)
        .arg("--root")
        .arg(temp.path().join("root with spaces"))
        .arg("--bin-dir")
        .arg(temp.path().join("bin with spaces"))
        .arg("--no-setup")
        .env("PIKA_TEST_VERSION", version)
        .env("PIKA_TEST_TRACE", &trace)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let forwarded = fs::read_to_string(trace).unwrap();
    assert!(forwarded.contains("_install-native\n"));
    assert!(forwarded.contains("root with spaces"));
    assert!(forwarded.contains("--no-setup"));
}

#[test]
fn shell_bootstrap_resolves_latest_then_pins_the_exact_release() {
    let temp = tempfile::tempdir().unwrap();
    let version = env!("CARGO_PKG_VERSION");
    let bundle = release_bundle(temp.path(), version);
    fs::write(bundle.join("pika-version"), format!("{version}\n")).unwrap();
    let fake_bin = temp.path().join("fake-bin");
    fs::create_dir(&fake_bin).unwrap();
    let curl = fake_bin.join("curl");
    fs::write(
        &curl,
        r#"#!/bin/sh
output=''
previous=''
for value in "$@"; do
  if [ "$previous" = '--output' ]; then output=$value; fi
  previous=$value
done
url=$previous
printf '%s\n' "$url" >> "$PIKA_TEST_NETWORK"
case "$url" in
  */latest/download/pika-version) source="$PIKA_TEST_BUNDLE/pika-version" ;;
  *) source="$PIKA_TEST_BUNDLE/${url##*/}" ;;
esac
cp "$source" "$output"
"#,
    )
    .unwrap();
    fs::set_permissions(&curl, fs::Permissions::from_mode(0o700)).unwrap();
    let network = temp.path().join("network");
    let trace = temp.path().join("trace");
    let path = format!("{}:{}", fake_bin.display(), std::env::var("PATH").unwrap());
    let output = Command::new("bash")
        .arg("scripts/install.sh")
        .arg("--root")
        .arg(temp.path().join("managed"))
        .arg("--bin-dir")
        .arg(temp.path().join("bin"))
        .arg("--no-setup")
        .env("PATH", path)
        .env("PIKA_TEST_BUNDLE", &bundle)
        .env("PIKA_TEST_NETWORK", &network)
        .env("PIKA_TEST_TRACE", &trace)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let urls = fs::read_to_string(network).unwrap();
    let mut lines = urls.lines();
    assert_eq!(
        lines.next().unwrap(),
        "https://github.com/ayushjainr/pikamux/releases/latest/download/pika-version"
    );
    assert!(lines.all(|line| line.contains(&format!("/download/v{version}/"))));
    assert!(
        fs::read_to_string(trace)
            .unwrap()
            .contains("_install-native")
    );
}

#[test]
fn release_packager_emits_a_strict_manifest_and_refuses_replacement() {
    let temp = tempfile::tempdir().unwrap();
    let output = temp.path().join("release");
    let version = env!("CARGO_PKG_VERSION");
    let target = native_target().unwrap();
    let binary = Path::new(env!("CARGO_BIN_EXE_pika"));
    let result = Command::new("bash")
        .arg("scripts/package-release.sh")
        .arg(version)
        .arg(&output)
        .arg(format!("{target}={}", binary.display()))
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let manifest =
        ReleaseManifest::parse(&fs::read(output.join("pika-release.json")).unwrap()).unwrap();
    let artifact = manifest.artifact_for(target).unwrap();
    assert_eq!(
        sha256_file(&output.join(&artifact.file)).unwrap(),
        artifact.sha256
    );
    assert_eq!(
        fs::read_to_string(output.join(format!("{}.sha256", artifact.file)))
            .unwrap()
            .trim(),
        artifact.sha256
    );
    let before = fs::read(output.join("SHA256SUMS")).unwrap();
    assert!(
        !Command::new("bash")
            .arg("scripts/package-release.sh")
            .arg(version)
            .arg(&output)
            .arg(format!("{target}={}", binary.display()))
            .output()
            .unwrap()
            .status
            .success()
    );
    assert_eq!(fs::read(output.join("SHA256SUMS")).unwrap(), before);
}

#[test]
fn public_cli_installs_and_checks_a_native_bundle_end_to_end() {
    let temp = tempfile::tempdir().unwrap();
    let bundle = temp.path().join("release");
    let root = temp.path().join("managed");
    let bin = temp.path().join("bin");
    let version = env!("CARGO_PKG_VERSION");
    let target = native_target().unwrap();
    let binary = Path::new(env!("CARGO_BIN_EXE_pika"));
    let packaged = Command::new("bash")
        .arg("scripts/package-release.sh")
        .arg(version)
        .arg(&bundle)
        .arg(format!("{target}={}", binary.display()))
        .output()
        .unwrap();
    assert!(
        packaged.status.success(),
        "{}",
        String::from_utf8_lossy(&packaged.stderr)
    );
    let installed = Command::new("bash")
        .arg("scripts/install.sh")
        .arg("--bundle")
        .arg(&bundle)
        .arg("--root")
        .arg(&root)
        .arg("--bin-dir")
        .arg(&bin)
        .arg("--no-setup")
        .env("HOME", temp.path().join("home"))
        .output()
        .unwrap();
    assert!(
        installed.status.success(),
        "{}",
        String::from_utf8_lossy(&installed.stderr)
    );
    let checked = Command::new(bin.join("pika"))
        .args(["update", "--bundle"])
        .arg(&bundle)
        .env("HOME", temp.path().join("home"))
        .output()
        .unwrap();
    assert!(
        checked.status.success(),
        "{}",
        String::from_utf8_lossy(&checked.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&checked.stdout).trim(),
        format!("Pika {version} is already current.")
    );
}

#[test]
fn remote_payload_is_version_pinned_allowlisted_and_installer_verified() {
    let temp = tempfile::tempdir().unwrap();
    let version = env!("CARGO_PKG_VERSION");
    let target = native_target().unwrap();
    let bundle = temp.path().join("release");
    let binary = Path::new(env!("CARGO_BIN_EXE_pika"));
    let packaged = Command::new("bash")
        .arg("scripts/package-release.sh")
        .arg(version)
        .arg(&bundle)
        .arg(format!("{target}={}", binary.display()))
        .output()
        .unwrap();
    assert!(
        packaged.status.success(),
        "{}",
        String::from_utf8_lossy(&packaged.stderr)
    );
    let remote = prepare_remote_install_bundle(&bundle, target, Some(version)).unwrap();
    let payload = temp.path().join("remote.tar");
    fs::write(&payload, remote.as_bytes()).unwrap();
    let listing = Command::new("tar")
        .args(["-tf"])
        .arg(&payload)
        .output()
        .unwrap();
    assert!(listing.status.success());
    let mut names: Vec<_> = String::from_utf8(listing.stdout)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    names.sort();
    let artifact = artifact_name(version, target).unwrap();
    let mut expected = vec![
        "install.sh".to_owned(),
        "pika-release.json".to_owned(),
        "pika-version".to_owned(),
        artifact.clone(),
        format!("{artifact}.sha256"),
    ];
    expected.sort();
    assert_eq!(names, expected);

    fs::write(bundle.join("install.sh"), "#!/bin/sh\necho replaced\n").unwrap();
    assert!(
        prepare_remote_install_bundle(&bundle, target, Some(version))
            .unwrap_err()
            .to_string()
            .contains("does not match")
    );
}

#[test]
fn fleet_upgrade_proves_the_same_node_before_and_after_install() {
    let temp = tempfile::tempdir().unwrap();
    let next_version = "0.6.0-alpha.1";
    let bundle = release_bundle(temp.path(), next_version);
    let remote =
        prepare_remote_install_bundle(&bundle, native_target().unwrap(), Some(next_version))
            .unwrap();
    let store = Store::at(temp.path().join("state.db"));
    store.initialize().unwrap();
    let node_id = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    store
        .upsert_fleet_node(&fleet_node(node_id, "0.5.0a4"))
        .unwrap();
    let transport = UpgradeTransport {
        responses: Mutex::new(vec![
            hello(node_id, "0.5.0a4"),
            hello(node_id, "0.5.0a4"),
            hello(node_id, next_version),
        ]),
        installs: Mutex::new(Vec::new()),
    };
    let manager = FleetManager::new(&store, &transport);
    let (measured_node, measured_target) = manager.upgrade_target("research-node").unwrap();
    assert_eq!(measured_node.node_id, node_id);
    assert_eq!(measured_target, native_target().unwrap());
    let upgraded = manager.upgrade_bundle("research-node", &remote).unwrap();
    assert_eq!(upgraded.node_id, node_id);
    assert_eq!(upgraded.package_version.as_deref(), Some(next_version));
    assert_eq!(
        transport.installs.lock().unwrap().as_slice(),
        &[(
            "research-node.example".to_owned(),
            remote.as_bytes().to_vec()
        )]
    );
}

#[test]
fn fleet_upgrade_never_installs_before_identity_and_quarantines_a_changed_node() {
    let temp = tempfile::tempdir().unwrap();
    let next_version = "0.6.0-alpha.1";
    let bundle = release_bundle(temp.path(), next_version);
    let remote =
        prepare_remote_install_bundle(&bundle, native_target().unwrap(), Some(next_version))
            .unwrap();
    let store = Store::at(temp.path().join("state.db"));
    store.initialize().unwrap();
    let expected = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    let changed = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
    store
        .upsert_fleet_node(&fleet_node(expected, "0.5.0a4"))
        .unwrap();

    let before = UpgradeTransport {
        responses: Mutex::new(vec![hello(changed, "0.5.0a4")]),
        installs: Mutex::new(Vec::new()),
    };
    let error = FleetManager::new(&store, &before)
        .upgrade_bundle("research-node", &remote)
        .unwrap_err();
    assert_eq!(error.kind.as_str(), "quarantined");
    assert!(before.installs.lock().unwrap().is_empty());

    let after = UpgradeTransport {
        responses: Mutex::new(vec![
            hello(expected, "0.5.0a4"),
            hello(changed, next_version),
        ]),
        installs: Mutex::new(Vec::new()),
    };
    let error = FleetManager::new(&store, &after)
        .upgrade_bundle("research-node", &remote)
        .unwrap_err();
    assert_eq!(error.kind.as_str(), "quarantined");
    assert_eq!(after.installs.lock().unwrap().len(), 1);
}

#[test]
fn ssh_remote_install_keeps_payload_on_stdin_and_command_fixed() {
    let temp = tempfile::tempdir().unwrap();
    let version = "0.6.0-alpha.1";
    let bundle = release_bundle(temp.path(), version);
    let remote =
        prepare_remote_install_bundle(&bundle, native_target().unwrap(), Some(version)).unwrap();
    let ssh = temp.path().join("ssh");
    let arguments = temp.path().join("arguments");
    let payload = temp.path().join("payload");
    fs::write(
        &ssh,
        "#!/bin/sh\nbase=$(dirname \"$0\")\nprintf '%s\\n' \"$@\" > \"$base/arguments\"\ncat > \"$base/payload\"\nprintf installed\n",
    )
    .unwrap();
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();
    let transport = SshTransport::new(&ssh, Duration::from_secs(1), Duration::from_secs(5));
    let detail = transport
        .install_bundle("research-node.example", &remote)
        .unwrap();
    assert_eq!(detail, "installed");
    assert_eq!(fs::read(payload).unwrap(), remote.as_bytes());
    let arguments = fs::read_to_string(arguments).unwrap();
    assert!(arguments.lines().any(|line| line == "BatchMode=yes"));
    assert!(
        arguments
            .lines()
            .any(|line| line == "research-node.example")
    );
    assert!(arguments.contains("--no-setup"));
    assert!(!arguments.contains(temp.path().to_string_lossy().as_ref()));
}

#[test]
fn ssh_remote_target_probe_is_fixed_bounded_and_platform_mapped() {
    let temp = tempfile::tempdir().unwrap();
    let ssh = temp.path().join("ssh");
    fs::write(&ssh, "#!/bin/sh\nprintf 'Darwin\\narm64\\n'\n").unwrap();
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();
    let transport = SshTransport::new(&ssh, Duration::from_secs(1), Duration::from_secs(5));
    assert_eq!(
        transport.native_target("mac.example").unwrap(),
        "aarch64-apple-darwin"
    );

    fs::write(&ssh, "#!/bin/sh\nprintf 'Windows_NT\\nx86_64\\n'\n").unwrap();
    assert_eq!(
        transport
            .native_target("windows.example")
            .unwrap_err()
            .kind
            .as_str(),
        "incompatible"
    );
}
