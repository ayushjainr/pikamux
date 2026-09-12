use assert_cmd::Command;
use predicates::prelude::*;

#[test]
fn version_is_native_and_side_effect_free() {
    let temp = tempfile::tempdir().unwrap();
    let mut command = Command::cargo_bin("pika").unwrap();
    command
        .env("PIKA_CONFIG_HOME", temp.path().join("config"))
        .env("PIKA_STATE_HOME", temp.path().join("state"))
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains("pika 0.6.0-alpha.1"));
    assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 0);
}

#[test]
fn empty_list_does_not_create_state() {
    let temp = tempfile::tempdir().unwrap();
    Command::cargo_bin("pika")
        .unwrap()
        .env("PIKA_CONFIG_HOME", temp.path().join("config"))
        .env("PIKA_STATE_HOME", temp.path().join("state"))
        .args(["list", "--json"])
        .assert()
        .success()
        .stdout("[]\n");
    assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 0);
}
