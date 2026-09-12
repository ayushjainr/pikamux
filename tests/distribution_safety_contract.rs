#![cfg(unix)]

use pikamux::update::{
    InstallRequest, ReleaseArtifact, ReleaseManifest, artifact_name, install_staged, native_target,
    prepare_remote_install_bundle, rollback_managed, sha256_file,
};
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

fn candidate(path: &Path, version: &str, marker: Option<&Path>) {
    let marker_command = marker.map_or_else(String::new, |path| {
        format!("printf ran > '{}'\n", path.display())
    });
    fs::write(
        path,
        format!(
            "#!/bin/sh\n{marker_command}case \"$*\" in\n  --version) echo 'pika {version}' ;;\n  --help) exit 0 ;;\n  'skill show') echo skill ;;\n  *) exit 2 ;;\nesac\n"
        ),
    )
    .unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

fn direct_fixture(root: &Path, version: &str) -> (ReleaseManifest, PathBuf, PathBuf) {
    let target = native_target().unwrap();
    let archive = root.join(artifact_name(version, target).unwrap());
    let payload = root.join(format!("payload-{version}"));
    fs::create_dir(&payload).unwrap();
    let binary = payload.join("pika");
    candidate(&binary, version, None);
    fs::copy("LICENSE", payload.join("LICENSE")).unwrap();
    fs::copy("THIRD_PARTY.md", payload.join("THIRD_PARTY.md")).unwrap();
    let archived = Command::new("python3")
        .arg("scripts/archive-release.py")
        .arg("tar.gz")
        .arg(&payload)
        .arg(&binary)
        .arg(&archive)
        .status()
        .unwrap();
    assert!(archived.success());
    let artifact = ReleaseArtifact {
        file: archive.file_name().unwrap().to_string_lossy().into_owned(),
        sha256: sha256_file(&archive).unwrap(),
        bytes: fs::metadata(&archive).unwrap().len(),
    };
    (
        ReleaseManifest {
            schema: 2,
            package: "pikamux".into(),
            version: version.into(),
            channel: "preview".into(),
            artifacts: BTreeMap::from([(target.into(), artifact)]),
        },
        archive,
        binary,
    )
}

fn install_direct(
    manifest: &ReleaseManifest,
    archive: &Path,
    candidate: &Path,
    root: &Path,
    bin: &Path,
) -> pikamux::update::InstallOutcome {
    install_staged(InstallRequest {
        manifest,
        target: native_target().unwrap(),
        artifact: archive,
        candidate,
        root,
        bin_dir: bin,
    })
    .unwrap()
}

fn package(root: &Path, version: &str, binary: &Path, target: &str, name: &str) -> PathBuf {
    let output = root.join(name);
    let result = Command::new("bash")
        .arg("scripts/package-release.sh")
        .arg(version)
        .arg(&output)
        .arg(format!("{target}={}", binary.display()))
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    output
}

fn install_package(bundle: &Path, root: &Path, bin: &Path) -> pikamux::update::InstallOutcome {
    let manifest =
        ReleaseManifest::parse(&fs::read(bundle.join("pika-native-release.json")).unwrap())
            .unwrap();
    let target = native_target().unwrap();
    let artifact = manifest.artifact_for(target).unwrap();
    let extracted = tempfile::tempdir().unwrap();
    let status = Command::new("tar")
        .args(["-xzf"])
        .arg(bundle.join(&artifact.file))
        .arg("-C")
        .arg(extracted.path())
        .status()
        .unwrap();
    assert!(status.success());
    install_staged(InstallRequest {
        manifest: &manifest,
        target,
        artifact: &bundle.join(&artifact.file),
        candidate: &extracted.path().join("pika"),
        root,
        bin_dir: bin,
    })
    .unwrap()
}

fn replace_bundle_notices(bundle: &Path, license: &[u8], third_party: &[u8]) {
    let manifest_path = bundle.join("pika-native-release.json");
    let mut manifest = ReleaseManifest::parse(&fs::read(&manifest_path).unwrap()).unwrap();
    let target = native_target().unwrap();
    let artifact = manifest.artifact_for(target).unwrap().clone();
    let archive = bundle.join(&artifact.file);
    let payload = tempfile::tempdir().unwrap();
    let status = Command::new("tar")
        .args(["-xzf"])
        .arg(&archive)
        .arg("-C")
        .arg(payload.path())
        .status()
        .unwrap();
    assert!(status.success());
    fs::write(payload.path().join("LICENSE"), license).unwrap();
    fs::write(payload.path().join("THIRD_PARTY.md"), third_party).unwrap();
    fs::remove_file(&archive).unwrap();
    let status = Command::new("python3")
        .arg("scripts/archive-release.py")
        .arg("tar.gz")
        .arg(payload.path())
        .arg(payload.path().join("pika"))
        .arg(&archive)
        .status()
        .unwrap();
    assert!(status.success());
    let checksum = sha256_file(&archive).unwrap();
    let row = manifest.artifacts.get_mut(target).unwrap();
    row.sha256.clone_from(&checksum);
    row.bytes = fs::metadata(&archive).unwrap().len();
    fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
    fs::write(
        bundle.join(format!("{}.sha256", artifact.file)),
        format!("{checksum}\n"),
    )
    .unwrap();
    fs::write(bundle.join("LICENSE"), license).unwrap();
    fs::write(bundle.join("THIRD_PARTY.md"), third_party).unwrap();
}

fn shaped_cross_binary(root: &Path, target: &str) -> PathBuf {
    let path = root.join(format!("pika-{}", target.replace('/', "-")));
    let mut bytes = vec![0_u8; 64 * 1024];
    match target {
        "aarch64-apple-darwin" | "x86_64-apple-darwin" => {
            bytes[..4].copy_from_slice(b"\xcf\xfa\xed\xfe");
            let cpu = if target.starts_with("aarch64") {
                0x0100000c_u32
            } else {
                0x01000007_u32
            };
            bytes[4..8].copy_from_slice(&cpu.to_le_bytes());
        }
        "aarch64-unknown-linux-musl" | "x86_64-unknown-linux-musl" => {
            bytes[..6].copy_from_slice(b"\x7fELF\x02\x01");
            let machine = if target.starts_with("aarch64") {
                183_u16
            } else {
                62_u16
            };
            bytes[18..20].copy_from_slice(&machine.to_le_bytes());
        }
        _ => panic!("unsupported bridge fixture target"),
    }
    fs::write(&path, bytes).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    path
}

fn transition_release(root: &Path) -> PathBuf {
    let native_version = env!("CARGO_PKG_VERSION");
    let targets = [
        "aarch64-apple-darwin",
        "x86_64-apple-darwin",
        "aarch64-unknown-linux-musl",
        "x86_64-unknown-linux-musl",
    ];
    let native = root.join("bridge-native");
    let mut package = Command::new("bash");
    package
        .arg("scripts/package-release.sh")
        .arg(native_version)
        .arg(&native)
        .env("PIKA_CROSS_PACKAGE", "1");
    for target in targets {
        package.arg(format!(
            "{target}={}",
            shaped_cross_binary(root, target).display()
        ));
    }
    assert!(package.status().unwrap().success());

    let output = root.join("bridge-release");
    fs::create_dir(&output).unwrap();
    let wheel = output.join("pikamux-0.5.0a5-py3-none-any.whl");
    let mut package = Command::new("python3");
    package
        .arg("scripts/package-python-bridge.py")
        .args(["0.5.0a5", native_version])
        .arg(&wheel);
    for target in targets {
        package.arg(format!(
            "{target}={}",
            native
                .join(artifact_name(native_version, target).unwrap())
                .display()
        ));
    }
    assert!(package.status().unwrap().success());
    output
}

fn rewrite_bridge_outer_checksums(release: &Path) {
    let manifest_path = release.join("pika-release.json");
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    let wheel = manifest["wheel"].as_str().unwrap().to_owned();
    manifest["sha256"] = serde_json::json!(sha256_file(&release.join(&wheel)).unwrap());
    fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
    let mut files = fs::read_dir(release)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.file_name().unwrap() != "SHA256SUMS")
        .collect::<Vec<_>>();
    files.sort();
    let checksums = files
        .iter()
        .map(|path| {
            format!(
                "{}  {}\n",
                sha256_file(path).unwrap(),
                path.file_name().unwrap().to_string_lossy()
            )
        })
        .collect::<String>();
    fs::write(release.join("SHA256SUMS"), checksums).unwrap();
}

#[test]
fn reused_release_binary_is_compared_before_it_can_run_or_activate() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("managed");
    let bin = temp.path().join("bin");
    let (first_manifest, first_archive, first_binary) =
        direct_fixture(temp.path(), "0.6.0-alpha.1");
    let first = install_direct(&first_manifest, &first_archive, &first_binary, &root, &bin);
    let (second_manifest, second_archive, second_binary) =
        direct_fixture(temp.path(), "0.6.0-alpha.2");
    let second = install_direct(
        &second_manifest,
        &second_archive,
        &second_binary,
        &root,
        &bin,
    );

    fs::remove_file(root.join("current")).unwrap();
    symlink(&first.release_dir, root.join("current")).unwrap();
    let marker = temp.path().join("tampered-ran");
    candidate(
        &second.release_dir.join("bin/pika"),
        "0.6.0-alpha.2",
        Some(&marker),
    );

    let error = install_staged(InstallRequest {
        manifest: &second_manifest,
        target: native_target().unwrap(),
        artifact: &second_archive,
        candidate: &second_binary,
        root: &root,
        bin_dir: &bin,
    })
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("differs from its verified candidate"),
        "{error}"
    );
    assert!(!marker.exists(), "tampered retained binary was executed");
    assert_eq!(
        root.join("current").canonicalize().unwrap(),
        first.release_dir
    );
}

#[test]
fn supplied_candidate_must_match_verified_archive_before_any_probe() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("managed");
    let bin = temp.path().join("bin");
    let (manifest, archive, archived_binary) = direct_fixture(temp.path(), "0.6.0-alpha.1");
    let marker = temp.path().join("substituted-candidate-ran");
    let substitute = temp.path().join("substitute-pika");
    candidate(&substitute, "0.6.0-alpha.1", Some(&marker));

    let error = install_staged(InstallRequest {
        manifest: &manifest,
        target: native_target().unwrap(),
        artifact: &archive,
        candidate: &substitute,
        root: &root,
        bin_dir: &bin,
    })
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("differs from the checksum-verified archive"),
        "{error}"
    );
    assert!(!marker.exists(), "substituted candidate was executed");
    assert!(!root.exists(), "managed root was written before binding");

    install_staged(InstallRequest {
        manifest: &manifest,
        target: native_target().unwrap(),
        artifact: &archive,
        candidate: &archived_binary,
        root: &root,
        bin_dir: &bin,
    })
    .unwrap();
}

#[test]
fn same_version_tampered_current_is_rejected_before_it_can_run() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("managed");
    let bin = temp.path().join("bin");
    let (manifest, archive, binary) = direct_fixture(temp.path(), "0.6.0-alpha.1");
    let installed = install_direct(&manifest, &archive, &binary, &root, &bin);
    let marker = temp.path().join("tampered-current-ran");
    candidate(
        &installed.release_dir.join("bin/pika"),
        "0.6.0-alpha.1",
        Some(&marker),
    );

    let error = install_staged(InstallRequest {
        manifest: &manifest,
        target: native_target().unwrap(),
        artifact: &archive,
        candidate: &binary,
        root: &root,
        bin_dir: &bin,
    })
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("differs from its verified candidate"),
        "{error}"
    );
    assert!(!marker.exists(), "tampered current binary was executed");
    assert_eq!(
        root.join("current").canonicalize().unwrap(),
        installed.release_dir
    );

    fs::copy(&binary, installed.release_dir.join("bin/pika")).unwrap();
    fs::write(installed.release_dir.join("LICENSE"), b"tampered notice\n").unwrap();
    let error = install_staged(InstallRequest {
        manifest: &manifest,
        target: native_target().unwrap(),
        artifact: &archive,
        candidate: &binary,
        root: &root,
        bin_dir: &bin,
    })
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("release notice differs from its verified archive"),
        "{error}"
    );
}

#[test]
fn rollback_rejects_a_tampered_binary_before_executing_it() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("managed");
    let bin = temp.path().join("bin");
    let first_binary = temp.path().join("first-pika");
    candidate(&first_binary, "0.6.0-alpha.1", None);
    let first_bundle = package(
        temp.path(),
        "0.6.0-alpha.1",
        &first_binary,
        native_target().unwrap(),
        "first-release",
    );
    let first = install_package(&first_bundle, &root, &bin);
    let second_binary = temp.path().join("second-pika");
    candidate(&second_binary, "0.6.0-alpha.2", None);
    let second_bundle = package(
        temp.path(),
        "0.6.0-alpha.2",
        &second_binary,
        native_target().unwrap(),
        "second-release",
    );
    let second = install_package(&second_bundle, &root, &bin);

    let marker = temp.path().join("rollback-tamper-ran");
    candidate(
        &first.release_dir.join("bin/pika"),
        "0.6.0-alpha.1",
        Some(&marker),
    );
    let error =
        rollback_managed(&second.release_dir.join("bin/pika"), Some("0.6.0-alpha.1")).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("is absent or failed validation; nothing activated"),
        "{error}"
    );
    assert!(!marker.exists(), "tampered rollback binary was executed");
    assert_eq!(
        root.join("current").canonicalize().unwrap(),
        second.release_dir
    );
}

#[test]
fn rollback_and_remote_bundle_accept_notices_bound_to_an_older_archive() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("managed");
    let bin = temp.path().join("bin");
    let first_binary = temp.path().join("first-pika");
    candidate(&first_binary, "0.6.0-alpha.1", None);
    let first_bundle = package(
        temp.path(),
        "0.6.0-alpha.1",
        &first_binary,
        native_target().unwrap(),
        "first-release",
    );
    let old_license = b"Historical Pika license text\n";
    let old_third_party = b"Historical dependency notices\n";
    replace_bundle_notices(&first_bundle, old_license, old_third_party);
    let first = install_package(&first_bundle, &root, &bin);

    let second_binary = temp.path().join("second-pika");
    candidate(&second_binary, "0.6.0-alpha.2", None);
    let second_bundle = package(
        temp.path(),
        "0.6.0-alpha.2",
        &second_binary,
        native_target().unwrap(),
        "second-release",
    );
    let second = install_package(&second_bundle, &root, &bin);

    let retained_bundle = first.release_dir.join("bundle");
    let remote = prepare_remote_install_bundle(
        &retained_bundle,
        native_target().unwrap(),
        Some("0.6.0-alpha.1"),
    )
    .unwrap();
    assert!(!remote.is_empty());
    let outcome =
        rollback_managed(&second.release_dir.join("bin/pika"), Some("0.6.0-alpha.1")).unwrap();
    assert_eq!(outcome.version, "0.6.0-alpha.1");
    assert_eq!(
        fs::read(first.release_dir.join("LICENSE")).unwrap(),
        old_license
    );
    assert_eq!(
        fs::read(first.release_dir.join("THIRD_PARTY.md")).unwrap(),
        old_third_party
    );
}

#[test]
fn native_archives_are_reproducible_and_carry_exact_notices() {
    let temp = tempfile::tempdir().unwrap();
    let version = env!("CARGO_PKG_VERSION");
    let target = native_target().unwrap();
    let binary = Path::new(env!("CARGO_BIN_EXE_pika"));
    let first = package(temp.path(), version, binary, target, "first");
    let second = package(temp.path(), version, binary, target, "second");
    let name = artifact_name(version, target).unwrap();
    assert_eq!(
        fs::read(first.join(&name)).unwrap(),
        fs::read(second.join(&name)).unwrap()
    );

    let listing = Command::new("tar")
        .args(["-tzf"])
        .arg(first.join(&name))
        .output()
        .unwrap();
    assert!(listing.status.success());
    assert_eq!(
        String::from_utf8(listing.stdout).unwrap(),
        "LICENSE\nTHIRD_PARTY.md\npika\n"
    );
    for notice in ["LICENSE", "THIRD_PARTY.md"] {
        let extracted = Command::new("tar")
            .args(["-xOzf"])
            .arg(first.join(&name))
            .arg(notice)
            .output()
            .unwrap();
        assert!(extracted.status.success());
        assert_eq!(extracted.stdout, fs::read(notice).unwrap());
    }
    let remote = prepare_remote_install_bundle(&first, target, Some(version)).unwrap();
    assert!(!remote.is_empty());
}

#[test]
fn windows_archives_are_reproducible_and_carry_exact_notices() {
    let temp = tempfile::tempdir().unwrap();
    let version = env!("CARGO_PKG_VERSION");
    let target = "x86_64-pc-windows-msvc";
    let binary = temp.path().join("pika.exe");
    fs::copy(env!("CARGO_BIN_EXE_pika"), &binary).unwrap();
    let make = |name: &str| {
        let output = temp.path().join(name);
        let result = Command::new("bash")
            .arg("scripts/package-release.sh")
            .arg(version)
            .arg(&output)
            .arg(format!("{target}={}", binary.display()))
            .env("PIKA_CROSS_PACKAGE", "1")
            .output()
            .unwrap();
        assert!(result.status.success());
        output
    };
    let first = make("first");
    let second = make("second");
    let name = artifact_name(version, target).unwrap();
    assert_eq!(
        fs::read(first.join(&name)).unwrap(),
        fs::read(second.join(&name)).unwrap()
    );
    let listing = Command::new("unzip")
        .arg("-Z1")
        .arg(first.join(&name))
        .output()
        .unwrap();
    assert!(listing.status.success());
    assert_eq!(
        String::from_utf8(listing.stdout).unwrap(),
        "LICENSE\nTHIRD_PARTY.md\npika.exe\n"
    );
    for notice in ["LICENSE", "THIRD_PARTY.md"] {
        let extracted = Command::new("unzip")
            .arg("-p")
            .arg(first.join(&name))
            .arg(notice)
            .output()
            .unwrap();
        assert!(extracted.status.success());
        assert_eq!(extracted.stdout, fs::read(notice).unwrap());
    }
    let remote = prepare_remote_install_bundle(&first, target, Some(version)).unwrap();
    assert!(!remote.is_empty());
}

#[test]
fn transition_wheel_is_reproducible_and_carries_notices_for_wheel_and_native_install() {
    let temp = tempfile::tempdir().unwrap();
    let native_version = env!("CARGO_PKG_VERSION");
    let binary = Path::new(env!("CARGO_BIN_EXE_pika"));
    let native = temp.path().join("native");
    let targets = [
        "aarch64-apple-darwin",
        "x86_64-apple-darwin",
        "aarch64-unknown-linux-musl",
        "x86_64-unknown-linux-musl",
    ];
    let mut packager = Command::new("bash");
    packager
        .arg("scripts/package-release.sh")
        .arg(native_version)
        .arg(&native)
        .env("PIKA_CROSS_PACKAGE", "1");
    for target in targets {
        packager.arg(format!("{target}={}", binary.display()));
    }
    assert!(packager.status().unwrap().success());

    let make_wheel = |directory: &str| {
        let output = temp.path().join(directory);
        fs::create_dir(&output).unwrap();
        let wheel = output.join("pikamux-0.5.0a5-py3-none-any.whl");
        let mut command = Command::new("python3");
        command
            .arg("scripts/package-python-bridge.py")
            .args(["0.5.0a5", native_version])
            .arg(&wheel);
        for target in targets {
            command.arg(format!(
                "{target}={}",
                native
                    .join(artifact_name(native_version, target).unwrap())
                    .display()
            ));
        }
        let result = command.output().unwrap();
        assert!(
            result.status.success(),
            "{}{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        wheel
    };
    let first = make_wheel("bridge-first");
    let second = make_wheel("bridge-second");
    assert_eq!(fs::read(&first).unwrap(), fs::read(&second).unwrap());
    for (member, source) in [
        ("pikamux_bridge/native/LICENSE", "LICENSE"),
        ("pikamux_bridge/native/THIRD_PARTY.md", "THIRD_PARTY.md"),
        ("pikamux-0.5.0a5.dist-info/LICENSE", "LICENSE"),
        ("pikamux-0.5.0a5.dist-info/THIRD_PARTY.md", "THIRD_PARTY.md"),
    ] {
        let extracted = Command::new("unzip")
            .arg("-p")
            .arg(&first)
            .arg(member)
            .output()
            .unwrap();
        assert!(extracted.status.success(), "missing wheel member {member}");
        assert_eq!(extracted.stdout, fs::read(source).unwrap());
    }
}

#[test]
fn transition_verifier_rejects_extra_bomb_and_rehashed_source_mutant_members() {
    for mode in ["extra", "bomb", "source-mutant"] {
        let temp = tempfile::tempdir().unwrap();
        let release = transition_release(temp.path());
        let baseline = Command::new("bash")
            .arg("scripts/verify-release.sh")
            .arg(&release)
            .output()
            .unwrap();
        assert!(
            baseline.status.success(),
            "{}",
            String::from_utf8_lossy(&baseline.stderr)
        );

        let wheel = release.join("pikamux-0.5.0a5-py3-none-any.whl");
        let mutator = temp.path().join("mutate-wheel.py");
        fs::write(
            &mutator,
            r#"import base64, hashlib, pathlib, sys, zipfile
wheel = pathlib.Path(sys.argv[1])
mode = sys.argv[2]
temporary = wheel.with_suffix('.new')
with zipfile.ZipFile(wheel, 'r') as source:
    infos = source.infolist()
    files = {info.filename: source.read(info.filename) for info in infos}
if mode == 'bomb':
    files['pikamux_bridge/cli.py'] = b'0' * (3 * 1024 * 1024)
if mode == 'source-mutant':
    files['pikamux_bridge/cli.py'] += b'\n# recomputed-hash mutant\n'
    record = next(name for name in files if name.endswith('.dist-info/RECORD'))
    rows = []
    for name, data in sorted(files.items()):
        if name == record:
            continue
        digest = base64.urlsafe_b64encode(hashlib.sha256(data).digest()).rstrip(b'=').decode()
        rows.append(f'{name},sha256={digest},{len(data)}')
    rows.append(f'{record},,')
    files[record] = ('\n'.join(rows) + '\n').encode()
with zipfile.ZipFile(temporary, 'w') as target:
    for info in infos:
        data = files[info.filename]
        target.writestr(info, data, compress_type=zipfile.ZIP_DEFLATED, compresslevel=9)
    if mode == 'extra':
        info = zipfile.ZipInfo('pika_bootstrap.pth', (2020, 1, 1, 0, 0, 0))
        info.compress_type = zipfile.ZIP_DEFLATED
        info.external_attr = 0o100644 << 16
        target.writestr(info, b'import pika_bootstrap\n', compresslevel=9)
temporary.replace(wheel)
"#,
        )
        .unwrap();
        assert!(
            Command::new("python3")
                .arg(&mutator)
                .arg(&wheel)
                .arg(mode)
                .status()
                .unwrap()
                .success()
        );
        rewrite_bridge_outer_checksums(&release);

        let rejected = Command::new("bash")
            .arg("scripts/verify-release.sh")
            .arg(&release)
            .output()
            .unwrap();
        assert!(!rejected.status.success(), "hostile {mode} wheel passed");
        let stderr = String::from_utf8_lossy(&rejected.stderr);
        if mode == "extra" {
            assert!(stderr.contains("member allowlist mismatch"), "{stderr}");
        } else if mode == "bomb" {
            assert!(stderr.contains("member exceeds its size limit"), "{stderr}");
        } else {
            assert!(stderr.contains("differs from audited source"), "{stderr}");
        }
    }
}

#[test]
fn transition_verifier_binds_top_level_version_to_bridge_manifest() {
    let temp = tempfile::tempdir().unwrap();
    let release = transition_release(temp.path());
    fs::write(release.join("pika-version"), "0.5.0a4\n").unwrap();
    rewrite_bridge_outer_checksums(&release);
    let rejected = Command::new("bash")
        .arg("scripts/verify-release.sh")
        .arg(&release)
        .output()
        .unwrap();
    assert!(!rejected.status.success());
    assert!(
        String::from_utf8_lossy(&rejected.stderr)
            .contains("pika-version does not match the bridge manifest"),
        "{}",
        String::from_utf8_lossy(&rejected.stderr)
    );
}

#[test]
fn install_rejects_permissive_managed_directories_before_writes_and_creates_private_ones() {
    let temp = tempfile::tempdir().unwrap();
    let (manifest, archive, binary) = direct_fixture(temp.path(), "0.6.0-alpha.1");

    let permissive_root = temp.path().join("permissive-root");
    fs::create_dir(&permissive_root).unwrap();
    fs::set_permissions(&permissive_root, fs::Permissions::from_mode(0o777)).unwrap();
    let error = install_staged(InstallRequest {
        manifest: &manifest,
        target: native_target().unwrap(),
        artifact: &archive,
        candidate: &binary,
        root: &permissive_root,
        bin_dir: &temp.path().join("safe-bin"),
    })
    .unwrap_err();
    assert!(
        error.to_string().contains("group/other-writable"),
        "{error}"
    );
    assert!(fs::read_dir(&permissive_root).unwrap().next().is_none());

    fs::set_permissions(&permissive_root, fs::Permissions::from_mode(0o700)).unwrap();
    fs::remove_dir(&permissive_root).unwrap();
    let permissive_bin = temp.path().join("permissive-bin");
    fs::create_dir(&permissive_bin).unwrap();
    fs::set_permissions(&permissive_bin, fs::Permissions::from_mode(0o777)).unwrap();
    let fresh_root = temp.path().join("fresh-root");
    let error = install_staged(InstallRequest {
        manifest: &manifest,
        target: native_target().unwrap(),
        artifact: &archive,
        candidate: &binary,
        root: &fresh_root,
        bin_dir: &permissive_bin,
    })
    .unwrap_err();
    assert!(
        error.to_string().contains("group/other-writable"),
        "{error}"
    );
    assert!(
        !fresh_root.exists(),
        "root was written before bin ownership proof"
    );

    fs::set_permissions(&permissive_bin, fs::Permissions::from_mode(0o700)).unwrap();
    fs::remove_dir(&permissive_bin).unwrap();
    let private_root = temp.path().join("private-root");
    let private_bin = temp.path().join("private-bin");
    let installed = install_direct(&manifest, &archive, &binary, &private_root, &private_bin);
    for directory in [
        private_root.clone(),
        private_root.join("releases"),
        installed.release_dir.clone(),
        installed.release_dir.join("bin"),
        installed.release_dir.join("bundle"),
        private_bin.clone(),
    ] {
        assert_eq!(
            fs::symlink_metadata(&directory)
                .unwrap()
                .permissions()
                .mode()
                & 0o077,
            0,
            "{} was not created private",
            directory.display()
        );
    }

    fs::set_permissions(
        private_root.join("releases"),
        fs::Permissions::from_mode(0o777),
    )
    .unwrap();
    let error = install_staged(InstallRequest {
        manifest: &manifest,
        target: native_target().unwrap(),
        artifact: &archive,
        candidate: &binary,
        root: &private_root,
        bin_dir: &private_bin,
    })
    .unwrap_err();
    assert!(
        error.to_string().contains("group/other-writable"),
        "{error}"
    );
}

#[test]
fn offline_bootstrap_rejects_oversized_bundle_files_before_copying_or_writing_root() {
    let temp = tempfile::tempdir().unwrap();
    let bundle = temp.path().join("bundle");
    fs::create_dir(&bundle).unwrap();
    let version = env!("CARGO_PKG_VERSION");
    fs::write(bundle.join("pika-version"), format!("{version}\n")).unwrap();
    fs::write(
        bundle.join("pika-native-release.json"),
        vec![b'x'; 64 * 1024 + 1],
    )
    .unwrap();
    let root = temp.path().join("managed");
    let output = Command::new("bash")
        .arg("scripts/install.sh")
        .arg("--bundle")
        .arg(&bundle)
        .arg("--root")
        .arg(&root)
        .arg("--bin-dir")
        .arg(temp.path().join("bin"))
        .arg("--no-setup")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("Native release manifest exceeds its safety limit"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!root.exists());
}

#[test]
fn install_native_reaps_archive_subprocess_groups_on_int_and_term() {
    for (signal, expected_code) in [(libc::SIGINT, 130), (libc::SIGTERM, 143)] {
        let temp = tempfile::tempdir().unwrap();
        let target = native_target().unwrap();
        let version = env!("CARGO_PKG_VERSION");
        let artifact_name = artifact_name(version, target).unwrap();
        let artifact = temp.path().join(&artifact_name);
        fs::write(&artifact, b"checksum-bound archive fixture\n").unwrap();
        let checksum = sha256_file(&artifact).unwrap();
        let manifest = ReleaseManifest {
            schema: 2,
            package: "pikamux".into(),
            version: version.into(),
            channel: "preview".into(),
            artifacts: BTreeMap::from([(
                target.into(),
                ReleaseArtifact {
                    file: artifact_name,
                    sha256: checksum,
                    bytes: fs::metadata(&artifact).unwrap().len(),
                },
            )]),
        };
        let manifest_path = temp.path().join("pika-native-release.json");
        fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();

        let fake_bin = temp.path().join("fake-bin");
        fs::create_dir(&fake_bin).unwrap();
        let descendant_pid = temp.path().join("archive-descendant.pid");
        let fake_tar = fake_bin.join("tar");
        fs::write(
            &fake_tar,
            "#!/bin/sh\nsleep 300 &\nprintf '%s\\n' \"$!\" > \"$PIKA_TEST_ARCHIVE_DESCENDANT\"\nwait\n",
        )
        .unwrap();
        fs::set_permissions(&fake_tar, fs::Permissions::from_mode(0o700)).unwrap();
        let path = format!("{}:/usr/bin:/bin", fake_bin.display());
        let executable = Path::new(env!("CARGO_BIN_EXE_pika"));
        let mut process = Command::new(executable)
            .arg("_install-native")
            .arg("--manifest")
            .arg(&manifest_path)
            .arg("--artifact")
            .arg(&artifact)
            .arg("--candidate")
            .arg(executable)
            .arg("--target")
            .arg(target)
            .arg("--root")
            .arg(temp.path().join("managed"))
            .arg("--bin-dir")
            .arg(temp.path().join("bin"))
            .arg("--no-setup")
            .env("PATH", path)
            .env("PIKA_TEST_ARCHIVE_DESCENDANT", &descendant_pid)
            .spawn()
            .unwrap();
        let appeared = Instant::now() + Duration::from_secs(5);
        while !descendant_pid.exists() && Instant::now() < appeared {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(descendant_pid.exists(), "archive subprocess never started");
        let descendant: i32 = fs::read_to_string(&descendant_pid)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_eq!(unsafe { libc::kill(process.id() as i32, signal) }, 0);
        let status = process.wait().unwrap();
        assert_eq!(status.code(), Some(expected_code), "{status}");

        let reaped = Instant::now() + Duration::from_secs(3);
        while unsafe { libc::kill(descendant, 0) } == 0 && Instant::now() < reaped {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(
            unsafe { libc::kill(descendant, 0) },
            -1,
            "archive descendant {descendant} survived signal {signal}"
        );
        assert!(!temp.path().join("managed").exists());
    }
}

#[test]
fn release_publication_is_blocked_until_macos_signing_and_notarization_exist() {
    let workflow = fs::read_to_string(".github/workflows/release.yml").unwrap();
    let block = workflow
        .find("Block unsigned macOS publication")
        .expect("release workflow must retain the unsigned-macOS gate");
    let failure = workflow[block..]
        .find("exit 1")
        .map(|offset| block + offset)
        .expect("unsigned-macOS gate must fail closed");
    let publish = workflow
        .find("gh release create")
        .expect("managed publication command must remain reviewable");
    assert!(block < failure && failure < publish);
    assert!(workflow.contains("signed and notarized macOS artifacts"));
}
