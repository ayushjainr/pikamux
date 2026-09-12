use pikamux::update::{ReleaseManifest, sha256_file};
use std::fs;
use std::process::Command;

const WINDOWS_TARGET: &str = "x86_64-pc-windows-msvc";

#[test]
fn release_packager_emits_a_single_executable_windows_client_zip() {
    let temporary = tempfile::tempdir().unwrap();
    let binary = temporary.path().join("pika.exe");
    let output = temporary.path().join("release");
    fs::write(&binary, b"synthetic windows client binary").unwrap();
    let packaged = Command::new("bash")
        .arg("scripts/package-release.sh")
        .arg(env!("CARGO_PKG_VERSION"))
        .arg(&output)
        .arg(format!("{WINDOWS_TARGET}={}", binary.display()))
        .env("PIKA_CROSS_PACKAGE", "1")
        .output()
        .unwrap();
    assert!(
        packaged.status.success(),
        "{}",
        String::from_utf8_lossy(&packaged.stderr)
    );

    let manifest =
        ReleaseManifest::parse(&fs::read(output.join("pika-native-release.json")).unwrap())
            .unwrap();
    let artifact = manifest.artifact_for(WINDOWS_TARGET).unwrap();
    assert!(artifact.file.ends_with(".zip"));
    assert_eq!(
        sha256_file(&output.join(&artifact.file)).unwrap(),
        artifact.sha256
    );
    let listing = Command::new("unzip")
        .arg("-Z1")
        .arg(output.join(&artifact.file))
        .output()
        .unwrap();
    assert!(listing.status.success());
    assert_eq!(
        String::from_utf8(listing.stdout).unwrap().trim(),
        "pika.exe"
    );
}

#[test]
fn release_packager_keeps_relative_windows_output_rooted_at_the_caller() {
    let temporary = tempfile::tempdir().unwrap();
    let binary = temporary.path().join("pika.exe");
    fs::write(&binary, b"synthetic windows client binary").unwrap();
    let packaged = Command::new("bash")
        .arg(format!(
            "{}/scripts/package-release.sh",
            env!("CARGO_MANIFEST_DIR")
        ))
        .arg(env!("CARGO_PKG_VERSION"))
        .arg("release")
        .arg(format!("{WINDOWS_TARGET}={}", binary.display()))
        .current_dir(temporary.path())
        .env("PIKA_CROSS_PACKAGE", "1")
        .output()
        .unwrap();
    assert!(
        packaged.status.success(),
        "{}",
        String::from_utf8_lossy(&packaged.stderr)
    );

    let output = temporary.path().join("release");
    let manifest =
        ReleaseManifest::parse(&fs::read(output.join("pika-native-release.json")).unwrap())
            .unwrap();
    let artifact = manifest.artifact_for(WINDOWS_TARGET).unwrap();
    let listing = Command::new("unzip")
        .arg("-Z1")
        .arg(output.join(&artifact.file))
        .output()
        .unwrap();
    assert!(listing.status.success());
    assert_eq!(
        String::from_utf8(listing.stdout).unwrap().trim(),
        "pika.exe"
    );
}
