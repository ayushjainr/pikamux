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
    fs::write(&archive, format!("archive {version}\n")).unwrap();
    let binary = root.join(format!("candidate-{version}"));
    candidate(&binary, version, None);
    fs::copy("LICENSE", root.join("LICENSE")).unwrap();
    fs::copy("THIRD_PARTY.md", root.join("THIRD_PARTY.md")).unwrap();
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
